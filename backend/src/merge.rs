//! The merge decision engine: what a batch of changes from another device
//! should do to this device's data.
//!
//! [`plan_merge`] is the heart of it. It takes this device's change log and
//! the batch that arrived from another device, both as slices of
//! [`ChangeEntry`], and returns a [`MergePlan`]. It opens no file, writes
//! nothing, and never reads a clock, so every rule below can be tested without
//! a disk. [`crate::merge_apply::apply_remote_batch`] carries a plan out.
//!
//! [`generated_row_stays_deleted`] answers one of these rules on its own, for
//! the other caller that needs it: `/api/recurring/apply`, which decides what
//! to regenerate and must not bring back a row the user deleted.
//!
//! # What a caller does with a plan
//!
//! 1. Apply [`MergePlan::actions`] in the order they are given. Each one names
//!    a table, a row, and the write that row needs.
//! 2. Append every entry from [`MergePlan::entries_to_store`] to this device's
//!    log **verbatim**, keeping its original stamp and device id. A log that
//!    held only locally made changes could not tell a third device what it has
//!    already seen. Keeping foreign entries is also what makes the same batch
//!    arriving twice cost nothing: see [`MergeOutcome::AlreadyKnown`].
//! 3. Hand [`MergeSummary::max_remote_stamp`] to
//!    [`crate::sync::ChangeLog::observe_remote`] once, so every stamp this
//!    device issues afterwards is greater than everything in the batch.
//!
//! A plan holds row values, which are amounts, names, and categories. Only
//! [`MergePlan::summary`] is safe to print: it is counts and one stamp.
//!
//! # The rules
//!
//! Order is [`ChangeEntry::order_key`] everywhere: the stamp, then the device
//! id, so two devices never tie. "Newest" below always means largest by that
//! key. A row is identified by its table and its `row_id`, and the table
//! carries the year, and the month for expenses, so one key names one row of
//! one file.
//!
//! **Whole row tables** ([`ChangeTable::Expenses`] and
//! [`ChangeTable::Recurring`]). The newest entry for a row wins outright,
//! upsert or delete. A field by field merge could produce a row nobody typed,
//! which the user rejected for a finance record. The losing version is not
//! applied but stays in the log, so nothing is destroyed.
//!
//! **Per cell tables** (the four net worth tables and
//! [`ChangeTable::CashflowIncome`]). Two devices filling different months of
//! one row is ordinary use, not a conflict, so these merge per column:
//!
//! 1. Whether the row exists is decided by the newest row level entry, an
//!    [`ChangeOp::Upsert`] or a [`ChangeOp::Delete`], for that row.
//! 2. When the row exists, each column takes the newest entry that affects it,
//!    either a [`ChangeOp::Cell`] for that column or the value the newest
//!    upsert carries, whichever ranks higher.
//! 3. A cell that does not outrank the newest upsert loses to it, and a cell
//!    of a row whose newest row level entry is a delete is dropped: a cell
//!    cannot say "this row exists", so it cannot bring a row back.
//!
//! A row with no row level entry at all exists. That is the normal shape of
//! [`ChangeTable::CashflowIncome`], whose rows are fixed categories that are
//! only ever written cell by cell.
//!
//! **Edit against delete.** The newest action wins, the same rule as
//! everywhere else, with one exception: an upsert carrying
//! [`ChangeOrigin::RecurringApply`] never overrides a delete of the same row,
//! whatever the stamps say. That is what stops a generated row the user
//! deleted from returning every month. It holds back only the generated
//! upsert: an ordinary upsert of the same row, which is the user re-creating
//! it by hand, wins on stamp like anything else, and still wins when a
//! generated upsert ranks above it.
//!
//! A merge is only half of that rule. It fires when the other device sends an
//! entry for the row, and the row also comes back with no sync in sight: apply
//! reads the month's dataframe, where a deleted row is simply absent and so
//! looks due to be generated again. [`generated_row_stays_deleted`] is the
//! same rule for that caller, asked of the log instead of the dataframe.
//!
//! **A cell edit of a row nothing creates is neither applied nor lost.** It
//! becomes [`MergeOutcome::Unplaceable`]: both logs are missing the change
//! that would say whether the row exists, so no write follows, and the count
//! in [`MergeSummary::unplaceable`] tells part 3 the history has a hole.
//!
//! # Decisions this module makes
//!
//! **A derived row id is not treated as a mark of a generated row.** Rows
//! generated before the change log existed carry no origin, because no Parquet
//! file records where a row came from, and the baseline recorded them as
//! ordinary rows. Their ids still have the shape `<template row id>:<YYYY>-<MM>`,
//! so that shape could stand in for the missing origin. This module does not
//! use it, for one reason: editing a generated row through the expense form
//! records an ordinary upsert under that same derived id. Treating the shape
//! as the mark would make every such edit lose to any older delete of the row,
//! silently discarding the user's most recent action, and editing a generated
//! row is far more common than the case the fallback would cover. That case is
//! also already closed from both ends: the phone starts empty and takes the
//! desktop's rows rather than baselining its own, and part 3 must refuse to
//! baseline a device that has a sync partner. If a belt and braces guard is
//! wanted later, the honest form is a new origin on baseline entries, not a
//! guess from the id.
//!
//! **An entry the local log already holds is recognized by its stamp and
//! device id**, the pair the log format guarantees names one change forever.
//! Contents are not compared. A batch that arrives twice therefore applies
//! nothing the second time, which is what makes applying a plan idempotent in
//! effect.
//!
//! **A cell edit on a whole row table is kept but not applied**, with
//! [`SkipReason::CellOnWholeRowTable`]. This version merges those tables as
//! whole rows, and folding one column of a foreign entry into a local row is
//! exactly the invented row the whole row rule exists to prevent. The entry is
//! still stored, so a later version, or a third device, still has it.
//!
//! An unknown [`ChangeOrigin`] is an ordinary change: only
//! [`ChangeOrigin::RecurringApply`] triggers the exception above, and every
//! other value, including one from a newer version, merges on stamp. An
//! unknown table cannot reach here at all: [`ChangeTable`] is a closed set, so
//! a line naming a table this version does not know fails to decode and
//! [`crate::sync::read_log`] reports it as a corrupt line instead.
//!
//! # What this module assumes
//!
//! The local log describes the local data. A change that reached a Parquet
//! file and never reached the log is invisible here, and the merge can then
//! let the other device's older version win. Part 1b accepted that risk on
//! purpose, and detecting it belongs to part 3, along with every other reason
//! to refuse an exchange: a log with corrupt lines, and a device that never
//! finished its baseline. This module judges no input, it only merges.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Map, Value};

use crate::sync::{ChangeEntry, ChangeOp, ChangeOrigin, ChangeTable, Hlc};

/// Names one change by the pair that identifies it for good: the stamp and the
/// device that issued it. A plan uses it to say which entry beat the one it
/// did not apply, so a caller can report the conflict instead of a bare
/// "skipped".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryId {
    /// The winning entry's stamp.
    pub stamp: Hlc,
    /// The device that issued it.
    pub device_id: String,
}

impl EntryId {
    fn of(entry: &ChangeEntry) -> Self {
        EntryId {
            stamp: entry.stamp,
            device_id: entry.device_id.clone(),
        }
    }
}

/// Why a remote entry changes nothing on this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// A newer row level entry for the same row, an upsert or a delete, won.
    /// Also covers a cell that does not outrank the winning upsert, whose
    /// value for that column is therefore the newer one.
    SupersededByRow(EntryId),
    /// A newer cell edit for the same column won.
    SupersededByCell(EntryId),
    /// This delete is the newest row level entry for the row, so the row does
    /// not exist and the cell edit has nothing to edit. A cell cannot bring a
    /// row back, whichever side of the delete it falls on.
    RowIsDeleted(EntryId),
    /// The entry is an upsert from `/api/recurring/apply` and this delete of
    /// the same row ranks below it. The generated row stays deleted rather
    /// than returning every month. An ordinary upsert is never held back this
    /// way.
    GeneratedRowStaysDeleted(EntryId),
    /// A cell edit on a table this version merges as whole rows. The entry is
    /// stored, not applied.
    CellOnWholeRowTable,
}

/// What a plan does with one remote entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The entry decides part of the row's final state, so it appears in
    /// [`MergePlan::actions`] and must be stored in the log.
    Applied,
    /// This device's log already holds a change with this stamp and device id,
    /// or the same change appeared earlier in this batch. It is neither
    /// applied nor stored again.
    AlreadyKnown,
    /// The entry is stored in the log but changes no data, for this reason.
    Skipped(SkipReason),
    /// The entry edits one cell of a row that no entry on either side ever
    /// creates, so this device cannot place it. It is stored and not applied.
    ///
    /// This is not a change that lost, and a caller that reports conflicts
    /// should keep the two apart. A row of these tables exists because some
    /// entry says so, either an [`ChangeOp::Upsert`] from the edit that
    /// created it or one from the baseline, which records every row a data
    /// folder already held. A cell with no such entry anywhere means a row
    /// level change never reached either log, which is the failed append part
    /// 1b decided to accept, so the history both devices hold is incomplete.
    /// Part 3 owns the answer, most likely re-seeding from the other device.
    ///
    /// The plan will not invent the row from the cells it was given. A row
    /// built out of one column is the row nobody typed that the whole row rule
    /// exists to prevent.
    ///
    /// [`ChangeTable::CashflowIncome`] is never unplaceable: its rows are the
    /// fixed income categories, they always exist, and the baseline records
    /// them cell by cell with no row level entry at all.
    Unplaceable,
}

/// One remote entry and what the plan does with it.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeDecision {
    /// The remote entry, exactly as it arrived. Store it unchanged: its stamp
    /// and device id are what a third device matches against later.
    pub entry: ChangeEntry,
    /// What to do with it.
    pub outcome: MergeOutcome,
}

/// One column of a row and the value it takes.
#[derive(Debug, Clone, PartialEq)]
pub struct CellValue {
    /// Column name as it appears in the Parquet file.
    pub column: String,
    /// The merged value, `null` for a cleared cell.
    pub value: Value,
}

/// The write one row needs.
#[derive(Debug, Clone, PartialEq)]
pub enum RowAction {
    /// Replace the row with exactly these columns and values, or add it when
    /// it is absent. For a per cell table the map is already the merged row:
    /// the winning upsert with every newer cell value laid over it, including
    /// cells this device wrote. Writing it column by column instead would drop
    /// whichever part the caller left out.
    Upsert {
        /// Every column of the merged row, including `row_id`.
        row: Map<String, Value>,
    },
    /// Remove the row. The row may already be absent, when this device never
    /// held it or deleted it first, and removing it then is a no-op rather
    /// than an error.
    Delete,
    /// Set these columns of an existing row and leave every other column
    /// alone. Sorted by column name.
    ///
    /// Only for per cell tables, and only where the row is known to exist:
    /// either an upsert on one side or the other creates it and this device
    /// already holds that row, or the table's rows are the fixed income
    /// categories. A cell for a row nothing creates is
    /// [`MergeOutcome::Unplaceable`] instead and produces no action at all.
    SetCells {
        /// The columns to write.
        cells: Vec<CellValue>,
    },
}

/// One row to write, with the table and location that name its file or config item.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeAction {
    /// The table, carrying the year and, for expenses, the month.
    pub table: ChangeTable,
    /// The `row_id` of the row to write. For [`ChangeTable::CashflowIncome`]
    /// this is the income category name, which that table uses as its row id.
    pub row_id: String,
    /// The write itself.
    pub action: RowAction,
}

/// Counts from one plan, plus the stamp the local clock has to absorb. Holds
/// no row values, so it is safe to print or log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergeSummary {
    /// Entries in the batch, which is also the number of decisions.
    pub remote_entries: usize,
    /// Entries that decide part of the merged state.
    pub applied: usize,
    /// Entries this device already had.
    pub already_known: usize,
    /// Entries stored but not applied, each with a reason in its decision.
    pub skipped: usize,
    /// Entries this device could not place, counted apart from `skipped`
    /// because they report an incomplete history rather than a lost change.
    /// See [`MergeOutcome::Unplaceable`]. Any value above zero is something
    /// part 3 has to answer.
    pub unplaceable: usize,
    /// Rows to write, which is the length of [`MergePlan::actions`].
    pub rows_changed: usize,
    /// The largest stamp anywhere in the batch, including entries this device
    /// already had. Pass it to [`crate::sync::ChangeLog::observe_remote`]
    /// once. Absorbing a stamp the log already holds is harmless: the clock
    /// only moves forwards.
    pub max_remote_stamp: Option<Hlc>,
}

/// What a batch of remote changes does to this device: the decision on every
/// entry, the writes that follow, and the counts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MergePlan {
    /// One decision per remote entry, ordered by [`ChangeEntry::order_key`],
    /// whatever order the batch arrived in.
    pub decisions: Vec<MergeDecision>,
    /// The writes to perform, in the order the winning changes were made.
    /// Each row appears at most once.
    pub actions: Vec<MergeAction>,
    /// Counts and the largest remote stamp.
    pub summary: MergeSummary,
}

impl MergePlan {
    /// The entries to append to this device's log, verbatim and in order:
    /// every entry the device did not already have, whether it was applied or
    /// skipped. A skipped entry is still history, and a third device may need
    /// it.
    pub fn entries_to_store(&self) -> impl Iterator<Item = &ChangeEntry> {
        self.decisions
            .iter()
            .filter(|decision| decision.outcome != MergeOutcome::AlreadyKnown)
            .map(|decision| &decision.entry)
    }

    /// Whether carrying out this plan would write nothing at all, to the data
    /// or to the log. True for a batch this device has already merged.
    pub fn changes_nothing(&self) -> bool {
        self.actions.is_empty() && self.entries_to_store().next().is_none()
    }
}

/// Merge `remote`, a batch of changes from another device, against `local`,
/// this device's change log.
///
/// Reads nothing outside the two slices and writes nothing at all. The result
/// depends only on their contents, not on their order: entries are sorted by
/// [`ChangeEntry::order_key`] first. See the module docs for the rules, what a
/// caller does with the plan, and what it assumes about `local`.
pub fn plan_merge(local: &[ChangeEntry], remote: &[ChangeEntry]) -> MergePlan {
    // Ordered rather than hashed, because a stamp orders but does not hash.
    let known: BTreeSet<(Hlc, &str)> = local.iter().map(ChangeEntry::order_key).collect();

    // Sorting here, rather than trusting the caller, is what makes the plan
    // depend on the batch's contents alone.
    let mut batch: Vec<&ChangeEntry> = remote.iter().collect();
    batch.sort_by(|left, right| left.order_key().cmp(&right.order_key()));

    let mut decisions: Vec<OrderedDecision> = Vec::with_capacity(batch.len());
    let mut groups: HashMap<(&ChangeTable, &str), Vec<Candidate>> = HashMap::new();
    let mut seen: BTreeSet<(Hlc, &str)> = BTreeSet::new();
    let mut max_remote_stamp: Option<Hlc> = None;

    for (index, entry) in batch.iter().enumerate() {
        max_remote_stamp = max_remote_stamp.max(Some(entry.stamp));
        if known.contains(&entry.order_key()) || !seen.insert(entry.order_key()) {
            decisions.push((
                index,
                MergeDecision {
                    entry: (*entry).clone(),
                    outcome: MergeOutcome::AlreadyKnown,
                },
            ));
            continue;
        }
        groups
            .entry((&entry.table, entry.row_id.as_str()))
            .or_default()
            .push(Candidate {
                entry,
                order_index: Some(index),
            });
    }

    // Only a row the batch touches can change, so the local log is read once
    // and only for those rows.
    for entry in local {
        if let Some(group) = groups.get_mut(&(&entry.table, entry.row_id.as_str())) {
            group.push(Candidate {
                entry,
                order_index: None,
            });
        }
    }

    let mut actions: Vec<OrderedAction> = Vec::new();
    for ((table, row_id), candidates) in groups {
        let (group_decisions, action) = plan_row(table, row_id, &candidates);
        decisions.extend(group_decisions);
        actions.extend(action);
    }

    // Both orders come from each entry's position in the sorted batch, so the
    // grouping above, which runs in hash order, cannot reach the result.
    decisions.sort_by_key(|(index, _)| *index);
    actions.sort_by_key(|(index, _)| *index);

    let decisions: Vec<MergeDecision> = decisions.into_iter().map(|(_, it)| it).collect();
    let actions: Vec<MergeAction> = actions.into_iter().map(|(_, it)| it).collect();
    let summary = MergeSummary {
        remote_entries: decisions.len(),
        applied: count(&decisions, |outcome| *outcome == MergeOutcome::Applied),
        already_known: count(&decisions, |outcome| *outcome == MergeOutcome::AlreadyKnown),
        skipped: count(&decisions, |outcome| {
            matches!(outcome, MergeOutcome::Skipped(_))
        }),
        unplaceable: count(&decisions, |outcome| *outcome == MergeOutcome::Unplaceable),
        rows_changed: actions.len(),
        max_remote_stamp,
    };
    MergePlan {
        decisions,
        actions,
        summary,
    }
}

/// Whether `entries` leave this row deleted, so regenerating it from a
/// recurring template would bring back a row the user removed.
///
/// True when a [`ChangeOp::Delete`] for `table` and `row_id` ranks above every
/// [`ChangeOp::Upsert`] for it that is allowed to count. The exception in
/// [`plan_merge`] applies here too: an upsert marked
/// [`ChangeOrigin::RecurringApply`] above that delete does not revive the row,
/// so an earlier apply cannot make a later one look legitimate. An ordinary
/// upsert above the delete does revive it, because that is the user creating
/// the row again by hand, and the answer is then false.
///
/// Call this before generating a row whose id a template and a month derive,
/// which is what `/api/recurring/apply` does. The caller supplies the
/// "generated" half of the question: this function answers only whether the
/// deletion of that row still stands. The check belongs on the log rather than
/// on the month's dataframe, because a deleted row is absent from the
/// dataframe, which is exactly the state that makes it look missing and due to
/// be regenerated. [`plan_merge`] repairs this only when the other device
/// sends an entry for the same row, so without this check on the apply path
/// the row returns on a device that never syncs, or between two syncs.
///
/// Pass the whole log as `entries`, from [`crate::sync::read_log`]. This
/// filters it once per call, so a caller asking about many rows should read
/// the log once and pass the same slice.
///
/// `table` must name the file the row would be written to, month included for
/// [`ChangeTable::Expenses`], because the same row id in another month is
/// another row.
pub fn generated_row_stays_deleted(
    entries: &[ChangeEntry],
    table: &ChangeTable,
    row_id: &str,
) -> bool {
    let candidates: Vec<Candidate<'_>> = entries
        .iter()
        .filter(|entry| &entry.table == table && entry.row_id == row_id)
        .map(|entry| Candidate {
            entry,
            order_index: None,
        })
        .collect();
    // The same resolution the merge uses, so the rule has one implementation
    // and the two callers cannot drift apart.
    RowState::resolve(table, &candidates).deleted.is_some()
}

fn count(decisions: &[MergeDecision], want: impl Fn(&MergeOutcome) -> bool) -> usize {
    decisions
        .iter()
        .filter(|decision| want(&decision.outcome))
        .count()
}

/// Whether the table merges one column at a time rather than as a whole row.
///
/// The match is exhaustive on purpose: a new table added to [`ChangeTable`]
/// has to be classified here, and in [`has_fixed_rows`], before this crate
/// compiles again.
fn merges_per_cell(table: &ChangeTable) -> bool {
    match table {
        ChangeTable::Expenses { .. } | ChangeTable::Recurring { .. } => false,
        ChangeTable::Investments { .. }
        | ChangeTable::InvestmentsPrices { .. }
        | ChangeTable::Liquidity { .. }
        | ChangeTable::CreditsDebts { .. }
        | ChangeTable::CashflowIncome { .. }
        | ChangeTable::CurrencySettings => true,
        ChangeTable::CategoryMappings | ChangeTable::KnownCategories => false,
    }
}

/// Whether the table's rows exist without any entry saying so.
///
/// True only for [`ChangeTable::CashflowIncome`], whose rows are the fixed
/// income categories of `cashflow.parquet`: no route creates or removes one,
/// and the baseline records them cell by cell. Every other table's rows come
/// from an upsert, so a cell edit of a row with no upsert anywhere is
/// [`MergeOutcome::Unplaceable`] rather than an ordinary edit.
fn has_fixed_rows(table: &ChangeTable) -> bool {
    match table {
        ChangeTable::CashflowIncome { .. } => true,
        ChangeTable::CategoryMappings
        | ChangeTable::KnownCategories
        | ChangeTable::CurrencySettings => false,
        ChangeTable::Expenses { .. }
        | ChangeTable::Recurring { .. }
        | ChangeTable::Investments { .. }
        | ChangeTable::InvestmentsPrices { .. }
        | ChangeTable::Liquidity { .. }
        | ChangeTable::CreditsDebts { .. } => false,
    }
}

/// One entry competing for a row, from either side.
#[derive(Debug, Clone, Copy)]
struct Candidate<'a> {
    entry: &'a ChangeEntry,
    /// Position in the sorted batch, or `None` for an entry from this
    /// device's log. It is both the "this came from the other device" flag and
    /// the tie free sort key for the decisions and actions built from it.
    order_index: Option<usize>,
}

impl<'a> Candidate<'a> {
    fn key(&self) -> (Hlc, &'a str) {
        self.entry.order_key()
    }

    fn is_remote(&self) -> bool {
        self.order_index.is_some()
    }

    fn id(&self) -> EntryId {
        EntryId::of(self.entry)
    }
}

fn newest<'a>(candidates: impl Iterator<Item = Candidate<'a>>) -> Option<Candidate<'a>> {
    candidates.max_by(|left, right| left.key().cmp(&right.key()))
}

/// A decision and the position in the sorted batch that orders it.
type OrderedDecision = (usize, MergeDecision);

/// A write and the position in the sorted batch that orders it.
type OrderedAction = (usize, MergeAction);

/// The delete a generated upsert must not override, if there is one.
///
/// Only an upsert marked [`ChangeOrigin::RecurringApply`] is ever held back,
/// and only above a delete: below one it is an ordinary older version and
/// loses on stamp like anything else.
fn held_back_by<'a>(
    candidate: Candidate<'a>,
    newest_delete: Option<Candidate<'a>>,
) -> Option<Candidate<'a>> {
    if !matches!(candidate.entry.op, ChangeOp::Upsert { .. })
        || candidate.entry.origin != Some(ChangeOrigin::RecurringApply)
    {
        return None;
    }
    newest_delete.filter(|delete| delete.key() < candidate.key())
}

/// Decide one row: an outcome for every remote entry touching it, and the
/// write it needs, if any.
fn plan_row(
    table: &ChangeTable,
    row_id: &str,
    candidates: &[Candidate<'_>],
) -> (Vec<OrderedDecision>, Option<OrderedAction>) {
    let state = RowState::resolve(table, candidates);

    let mut decisions = Vec::new();
    let mut applied_index: Option<usize> = None;
    for candidate in candidates {
        let Some(index) = candidate.order_index else {
            continue;
        };
        let outcome = state.outcome_for(*candidate);
        if outcome == MergeOutcome::Applied {
            applied_index = applied_index.max(Some(index));
        }
        decisions.push((
            index,
            MergeDecision {
                entry: candidate.entry.clone(),
                outcome,
            },
        ));
    }

    // A write exists only where a remote entry won something, so an action
    // without an applied entry would be a write nothing asked for.
    let action = match (applied_index, state.action()) {
        (Some(index), Some(action)) => Some((
            index,
            MergeAction {
                table: table.clone(),
                row_id: row_id.to_string(),
                action,
            },
        )),
        _ => None,
    };
    (decisions, action)
}

/// One row after every entry from both devices has been weighed: who decides
/// whether it exists, and who decides each of its columns.
struct RowState<'a> {
    /// Whether this row's table merges one column at a time.
    per_cell: bool,
    /// The newest delete of the row, which the generated row rule needs even
    /// when a later upsert outranks it.
    newest_delete: Option<Candidate<'a>>,
    /// The newest row level entry that is allowed to decide the row.
    row_winner: Option<Candidate<'a>>,
    /// `row_winner` when it is a delete: the row does not exist.
    deleted: Option<Candidate<'a>>,
    /// `row_winner` when it is an upsert: the row exists and this entry holds
    /// the value of every column no newer cell overrides.
    base_upsert: Option<Candidate<'a>>,
    /// Which entry decides each column that outranks `base_upsert`. Empty for
    /// a whole row table and for a deleted row.
    cell_winners: HashMap<&'a str, Candidate<'a>>,
    /// Set when nothing on either side says whether this row exists: a per
    /// cell table whose rows come from upserts, and no row level entry at all.
    /// Its cell edits are [`MergeOutcome::Unplaceable`] and it gets no write.
    unplaceable: bool,
}

impl<'a> RowState<'a> {
    fn resolve(table: &ChangeTable, candidates: &[Candidate<'a>]) -> Self {
        let per_cell = merges_per_cell(table);
        let newest_delete = newest(
            candidates
                .iter()
                .copied()
                .filter(|it| matches!(it.entry.op, ChangeOp::Delete)),
        );
        // The row's existence is decided by the newest row level entry, after
        // the generated upserts that must not override a delete are taken out.
        let row_winner = newest(candidates.iter().copied().filter(|it| {
            matches!(it.entry.op, ChangeOp::Upsert { .. } | ChangeOp::Delete)
                && held_back_by(*it, newest_delete).is_none()
        }));
        let deleted = row_winner.filter(|it| matches!(it.entry.op, ChangeOp::Delete));
        let base_upsert = row_winner.filter(|it| matches!(it.entry.op, ChangeOp::Upsert { .. }));

        let mut cell_winners: HashMap<&'a str, Candidate<'a>> = HashMap::new();
        if per_cell && deleted.is_none() {
            for candidate in candidates {
                let ChangeOp::Cell { column, .. } = &candidate.entry.op else {
                    continue;
                };
                if base_upsert.is_some_and(|upsert| candidate.key() <= upsert.key()) {
                    continue;
                }
                cell_winners
                    .entry(column.as_str())
                    .and_modify(|winner| {
                        if winner.key() < candidate.key() {
                            *winner = *candidate;
                        }
                    })
                    .or_insert(*candidate);
            }
        }

        RowState {
            per_cell,
            newest_delete,
            row_winner,
            deleted,
            base_upsert,
            cell_winners,
            // `row_winner` is `None` only when no entry on either side is an
            // upsert or a delete: a held back generated upsert needs a delete
            // to be held back by, and that delete would win the row itself.
            unplaceable: per_cell && !has_fixed_rows(table) && row_winner.is_none(),
        }
    }

    /// What one remote entry does to this row.
    fn outcome_for(&self, candidate: Candidate<'_>) -> MergeOutcome {
        match &candidate.entry.op {
            ChangeOp::Cell { .. } if !self.per_cell => {
                MergeOutcome::Skipped(SkipReason::CellOnWholeRowTable)
            }
            ChangeOp::Cell { column, .. } => {
                if self.unplaceable {
                    return MergeOutcome::Unplaceable;
                }
                if let Some(delete) = self.deleted {
                    return MergeOutcome::Skipped(SkipReason::RowIsDeleted(delete.id()));
                }
                if let Some(upsert) = self.base_upsert
                    && candidate.key() <= upsert.key()
                {
                    return MergeOutcome::Skipped(SkipReason::SupersededByRow(upsert.id()));
                }
                match self.cell_winners.get(column.as_str()) {
                    Some(winner) if winner.key() != candidate.key() => {
                        MergeOutcome::Skipped(SkipReason::SupersededByCell(winner.id()))
                    }
                    _ => MergeOutcome::Applied,
                }
            }
            ChangeOp::Upsert { .. } | ChangeOp::Delete => {
                if let Some(delete) = held_back_by(candidate, self.newest_delete) {
                    return MergeOutcome::Skipped(SkipReason::GeneratedRowStaysDeleted(
                        delete.id(),
                    ));
                }
                match self.row_winner {
                    Some(winner) if winner.key() != candidate.key() => {
                        MergeOutcome::Skipped(SkipReason::SupersededByRow(winner.id()))
                    }
                    _ => MergeOutcome::Applied,
                }
            }
        }
    }

    /// The write this row needs, or `None` when this device's data already
    /// holds the merged state.
    fn action(&self) -> Option<RowAction> {
        if !self.per_cell {
            // Whole row: the winner is the row, and a local winner is already
            // on disk.
            let winner = self.row_winner.filter(Candidate::is_remote)?;
            return match &winner.entry.op {
                ChangeOp::Upsert { row } => Some(RowAction::Upsert { row: row.clone() }),
                ChangeOp::Delete => Some(RowAction::Delete),
                // A cell is never a row winner. Writing nothing is the only
                // safe reading of one here.
                ChangeOp::Cell { .. } => None,
            };
        }
        if let Some(delete) = self.deleted {
            return delete.is_remote().then_some(RowAction::Delete);
        }
        // Nothing says this row exists, so there is no row to set a column on
        // and no honest way to build one from the columns at hand.
        if self.unplaceable {
            return None;
        }

        // Sorted so the merged row and the cell list do not depend on hash
        // order.
        let mut winners: Vec<(&str, Candidate<'_>)> = self
            .cell_winners
            .iter()
            .map(|(column, winner)| (*column, *winner))
            .collect();
        winners.sort_by_key(|(column, _)| *column);

        match self.base_upsert {
            Some(upsert) if upsert.is_remote() => {
                let ChangeOp::Upsert { row } = &upsert.entry.op else {
                    return None;
                };
                let mut merged = row.clone();
                for (column, winner) in winners {
                    if let ChangeOp::Cell { value, .. } = &winner.entry.op {
                        merged.insert(column.to_string(), value.clone());
                    }
                }
                Some(RowAction::Upsert { row: merged })
            }
            // The row's existence is settled locally, either by a local upsert
            // or because the table's rows always exist, so only the columns
            // the other device won need writing.
            _ => {
                let cells: Vec<CellValue> = winners
                    .into_iter()
                    .filter(|(_, winner)| winner.is_remote())
                    .filter_map(|(column, winner)| match &winner.entry.op {
                        ChangeOp::Cell { value, .. } => Some(CellValue {
                            column: column.to_string(),
                            value: value.clone(),
                        }),
                        _ => None,
                    })
                    .collect();
                (!cells.is_empty()).then_some(RowAction::SetCells { cells })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HERE: &str = "desktop";
    const THERE: &str = "phone";

    fn stamp(ms: i64) -> Hlc {
        Hlc { ms, counter: 0 }
    }

    fn entry(ms: i64, device: &str, table: ChangeTable, row_id: &str, op: ChangeOp) -> ChangeEntry {
        ChangeEntry {
            stamp: stamp(ms),
            device_id: device.to_string(),
            table,
            row_id: row_id.to_string(),
            origin: None,
            op,
        }
    }

    fn expenses() -> ChangeTable {
        ChangeTable::Expenses {
            year: 2026,
            month: 3,
        }
    }

    fn liquidity() -> ChangeTable {
        ChangeTable::Liquidity { year: 2026 }
    }

    /// A whole expense row, as part 1b records one.
    fn expense_row(row_id: &str, name: &str, amount: f64) -> Map<String, Value> {
        let mut row = Map::new();
        row.insert("row_id".to_string(), Value::from(row_id));
        row.insert("expense_name".to_string(), Value::from(name));
        row.insert("expense_amount".to_string(), Value::from(amount));
        row
    }

    fn upsert(row: Map<String, Value>) -> ChangeOp {
        ChangeOp::Upsert { row }
    }

    fn cell(column: &str, value: f64) -> ChangeOp {
        ChangeOp::Cell {
            column: column.to_string(),
            value: Value::from(value),
        }
    }

    /// A net worth row: one asset, one value per month.
    fn asset_row(row_id: &str, march: f64) -> Map<String, Value> {
        let mut row = Map::new();
        row.insert("row_id".to_string(), Value::from(row_id));
        row.insert("name".to_string(), Value::from("Broker"));
        row.insert("03".to_string(), Value::from(march));
        row.insert("04".to_string(), Value::Null);
        row
    }

    fn id(entry: &ChangeEntry) -> EntryId {
        EntryId::of(entry)
    }

    /// One entry as a whole log or a whole batch, left where the test can
    /// still name it afterwards.
    fn one(entry: &ChangeEntry) -> &[ChangeEntry] {
        std::slice::from_ref(entry)
    }

    fn outcomes(plan: &MergePlan) -> Vec<MergeOutcome> {
        plan.decisions
            .iter()
            .map(|decision| decision.outcome.clone())
            .collect()
    }

    fn stored(plan: &MergePlan) -> Vec<ChangeEntry> {
        plan.entries_to_store().cloned().collect()
    }

    /// The decisions as the pairs that identify them in these tests, in the
    /// order the plan puts them.
    fn decision_order(plan: &MergePlan) -> Vec<(i64, &str)> {
        plan.decisions
            .iter()
            .map(|decision| (decision.entry.stamp.ms, decision.entry.row_id.as_str()))
            .collect()
    }

    fn action_order(plan: &MergePlan) -> Vec<&str> {
        plan.actions
            .iter()
            .map(|action| action.row_id.as_str())
            .collect()
    }

    /// The other device's newer version of an expense replaces this device's.
    #[test]
    fn a_newer_remote_expense_replaces_the_local_row() {
        let mine = entry(
            100,
            HERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );
        let theirs = entry(
            200,
            THERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Coffee", 4.0)),
        );

        let plan = plan_merge(&[mine], one(&theirs));

        assert_eq!(outcomes(&plan), vec![MergeOutcome::Applied]);
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: expenses(),
                row_id: "row-1".to_string(),
                action: RowAction::Upsert {
                    row: expense_row("row-1", "Coffee", 4.0)
                },
            }]
        );
        assert_eq!(stored(&plan), vec![theirs]);
    }

    /// The same two versions the other way round: this device's edit is the
    /// newer one, so nothing is written and the loser is still stored.
    #[test]
    fn an_older_remote_expense_is_stored_but_not_applied() {
        let theirs = entry(
            100,
            THERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Coffee", 4.0)),
        );
        let mine = entry(
            200,
            HERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );

        let plan = plan_merge(one(&mine), one(&theirs));

        assert_eq!(
            outcomes(&plan),
            vec![MergeOutcome::Skipped(SkipReason::SupersededByRow(id(
                &mine
            )))]
        );
        assert!(plan.actions.is_empty());
        assert_eq!(stored(&plan), vec![theirs]);
    }

    /// Two devices stamping the same millisecond are separated by device id,
    /// not by which side the entry came from.
    #[test]
    fn a_tie_on_the_stamp_is_broken_by_device_id() {
        let mine = entry(
            100,
            "aaa",
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );
        let theirs = entry(
            100,
            "zzz",
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Coffee", 4.0)),
        );

        let remote_wins = plan_merge(one(&mine), one(&theirs));
        assert_eq!(outcomes(&remote_wins), vec![MergeOutcome::Applied]);
        assert_eq!(remote_wins.actions.len(), 1);

        // Swap the ids and the same stamps go the other way.
        let mut mine = mine;
        mine.device_id = "zzz".to_string();
        let mut theirs = theirs;
        theirs.device_id = "aaa".to_string();
        let local_wins = plan_merge(one(&mine), &[theirs]);
        assert_eq!(
            outcomes(&local_wins),
            vec![MergeOutcome::Skipped(SkipReason::SupersededByRow(id(
                &mine
            )))]
        );
        assert!(local_wins.actions.is_empty());
    }

    /// A delete made after this device's edit removes the row.
    #[test]
    fn a_remote_delete_after_a_local_edit_removes_the_row() {
        let mine = entry(
            100,
            HERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );
        let theirs = entry(200, THERE, expenses(), "row-1", ChangeOp::Delete);

        let plan = plan_merge(&[mine], &[theirs]);

        assert_eq!(outcomes(&plan), vec![MergeOutcome::Applied]);
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: expenses(),
                row_id: "row-1".to_string(),
                action: RowAction::Delete,
            }]
        );
    }

    /// An edit made after this device's delete brings the row back. Nothing
    /// but the generated row rule changes that.
    #[test]
    fn a_remote_edit_after_a_local_delete_brings_the_row_back() {
        let mine = entry(100, HERE, expenses(), "row-1", ChangeOp::Delete);
        let theirs = entry(
            200,
            THERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );

        let plan = plan_merge(&[mine], &[theirs]);

        assert_eq!(outcomes(&plan), vec![MergeOutcome::Applied]);
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: expenses(),
                row_id: "row-1".to_string(),
                action: RowAction::Upsert {
                    row: expense_row("row-1", "Tea", 3.0)
                },
            }]
        );
    }

    /// The one exception: a row `/api/recurring/apply` regenerated does not
    /// override a delete, however much newer its stamp is. Without this the
    /// row the user deleted comes back every month.
    #[test]
    fn a_generated_row_does_not_come_back_after_a_delete() {
        let deleted = entry(100, HERE, expenses(), "tpl-1:2026-03", ChangeOp::Delete);
        let mut regenerated = entry(
            900,
            THERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent", 1000.0)),
        );
        regenerated.origin = Some(ChangeOrigin::RecurringApply);

        let plan = plan_merge(one(&deleted), one(&regenerated));

        assert_eq!(
            outcomes(&plan),
            vec![MergeOutcome::Skipped(SkipReason::GeneratedRowStaysDeleted(
                id(&deleted)
            ))]
        );
        assert!(plan.actions.is_empty());
        // Held back, not dropped: a third device still gets the entry.
        assert_eq!(stored(&plan), vec![regenerated]);
    }

    /// The same batch without the mark is an ordinary re-creation and wins on
    /// stamp. This and the test above fail together if the rule is deleted or
    /// applied to everything.
    #[test]
    fn an_ordinary_upsert_after_a_delete_is_not_held_back() {
        let deleted = entry(100, HERE, expenses(), "tpl-1:2026-03", ChangeOp::Delete);
        let typed = entry(
            900,
            THERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent", 1000.0)),
        );

        let plan = plan_merge(&[deleted], &[typed]);

        assert_eq!(outcomes(&plan), vec![MergeOutcome::Applied]);
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: expenses(),
                row_id: "tpl-1:2026-03".to_string(),
                action: RowAction::Upsert {
                    row: expense_row("tpl-1:2026-03", "Rent", 1000.0)
                },
            }]
        );
    }

    /// An origin from a newer version is an ordinary change here. Only
    /// `recurring_apply` holds an upsert back.
    #[test]
    fn an_unknown_origin_merges_on_stamp() {
        let deleted = entry(100, HERE, expenses(), "row-1", ChangeOp::Delete);
        let mut theirs = entry(
            900,
            THERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );
        theirs.origin = Some(ChangeOrigin::Unknown("imported_from_bank".to_string()));

        let plan = plan_merge(&[deleted], one(&theirs));

        assert_eq!(outcomes(&plan), vec![MergeOutcome::Applied]);
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(stored(&plan), vec![theirs]);
    }

    /// The user deletes a generated row, types it again by hand, and the next
    /// apply regenerates it. The hand typed row is what survives: the rule
    /// holds back the generated upsert alone.
    #[test]
    fn a_hand_typed_row_survives_a_later_generated_upsert() {
        let deleted = entry(100, HERE, expenses(), "tpl-1:2026-03", ChangeOp::Delete);
        let typed = entry(
            200,
            THERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent, corrected", 950.0)),
        );
        let mut regenerated = entry(
            300,
            THERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent", 1000.0)),
        );
        regenerated.origin = Some(ChangeOrigin::RecurringApply);

        let plan = plan_merge(one(&deleted), &[typed.clone(), regenerated]);

        assert_eq!(
            outcomes(&plan),
            vec![
                MergeOutcome::Applied,
                MergeOutcome::Skipped(SkipReason::GeneratedRowStaysDeleted(id(&deleted))),
            ]
        );
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: expenses(),
                row_id: "tpl-1:2026-03".to_string(),
                action: RowAction::Upsert {
                    row: expense_row("tpl-1:2026-03", "Rent, corrected", 950.0)
                },
            }]
        );
    }

    /// A generated upsert older than the delete is an ordinary loser, not a
    /// held back one. It is the apply that came after the delete that must not
    /// win.
    #[test]
    fn a_generated_upsert_below_the_delete_loses_on_stamp() {
        let mut generated = entry(
            100,
            THERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent", 1000.0)),
        );
        generated.origin = Some(ChangeOrigin::RecurringApply);
        let deleted = entry(200, HERE, expenses(), "tpl-1:2026-03", ChangeOp::Delete);

        let plan = plan_merge(one(&deleted), &[generated]);

        assert_eq!(
            outcomes(&plan),
            vec![MergeOutcome::Skipped(SkipReason::SupersededByRow(id(
                &deleted
            )))]
        );
        assert!(plan.actions.is_empty());
    }

    /// Two applies around a hand typed row: delete, regenerate, type by hand,
    /// regenerate again. The hand typed row is what stands, and both generated
    /// upserts are held back by the same delete, including the one that ranks
    /// above the hand typed row. This is the shape a later change is most
    /// likely to break, because each rule looks right on a pair of entries.
    #[test]
    fn two_generated_upserts_around_a_hand_typed_row_both_lose_to_it() {
        let deleted = entry(100, HERE, expenses(), "tpl-1:2026-03", ChangeOp::Delete);
        let mut first_apply = entry(
            200,
            THERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent", 1000.0)),
        );
        first_apply.origin = Some(ChangeOrigin::RecurringApply);
        let typed = entry(
            300,
            THERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent, corrected", 950.0)),
        );
        let mut second_apply = entry(
            400,
            THERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent", 1000.0)),
        );
        second_apply.origin = Some(ChangeOrigin::RecurringApply);

        let plan = plan_merge(one(&deleted), &[first_apply, typed, second_apply]);

        assert_eq!(
            outcomes(&plan),
            vec![
                MergeOutcome::Skipped(SkipReason::GeneratedRowStaysDeleted(id(&deleted))),
                MergeOutcome::Applied,
                MergeOutcome::Skipped(SkipReason::GeneratedRowStaysDeleted(id(&deleted))),
            ]
        );
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: expenses(),
                row_id: "tpl-1:2026-03".to_string(),
                action: RowAction::Upsert {
                    row: expense_row("tpl-1:2026-03", "Rent, corrected", 950.0)
                },
            }]
        );
        // The row exists again, so the next apply has nothing to skip.
        let mut log = vec![deleted];
        log.extend(plan.entries_to_store().cloned());
        assert!(!generated_row_stays_deleted(
            &log,
            &expenses(),
            "tpl-1:2026-03"
        ));
    }

    /// Applying the same month twice after a delete: neither generated upsert
    /// revives the row, so it stays deleted however many times apply runs.
    #[test]
    fn repeated_applies_after_a_delete_leave_the_row_deleted() {
        let deleted = entry(100, HERE, expenses(), "tpl-1:2026-03", ChangeOp::Delete);
        let generated = |ms: i64| {
            let mut it = entry(
                ms,
                THERE,
                expenses(),
                "tpl-1:2026-03",
                upsert(expense_row("tpl-1:2026-03", "Rent", 1000.0)),
            );
            it.origin = Some(ChangeOrigin::RecurringApply);
            it
        };

        let plan = plan_merge(one(&deleted), &[generated(200), generated(300)]);

        assert_eq!(
            outcomes(&plan),
            vec![
                MergeOutcome::Skipped(SkipReason::GeneratedRowStaysDeleted(id(&deleted))),
                MergeOutcome::Skipped(SkipReason::GeneratedRowStaysDeleted(id(&deleted))),
            ]
        );
        assert!(plan.actions.is_empty());
        let mut log = vec![deleted];
        log.extend(plan.entries_to_store().cloned());
        assert!(generated_row_stays_deleted(
            &log,
            &expenses(),
            "tpl-1:2026-03"
        ));
    }

    /// The predicate `/api/recurring/apply` asks before regenerating a row:
    /// does the user's deletion of it still stand? A deleted row is absent
    /// from the month's dataframe, so the dataframe cannot answer this and the
    /// log has to.
    #[test]
    fn the_deletion_predicate_answers_for_the_apply_path() {
        let created = entry(
            100,
            HERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent", 1000.0)),
        );
        let deleted = entry(200, HERE, expenses(), "tpl-1:2026-03", ChangeOp::Delete);

        // Nothing recorded: there is no deletion to stand.
        assert!(!generated_row_stays_deleted(
            &[],
            &expenses(),
            "tpl-1:2026-03"
        ));
        // The row exists.
        assert!(!generated_row_stays_deleted(
            one(&created),
            &expenses(),
            "tpl-1:2026-03"
        ));
        // The user deleted it: apply must not put it back.
        let log = vec![created.clone(), deleted.clone()];
        assert!(generated_row_stays_deleted(
            &log,
            &expenses(),
            "tpl-1:2026-03"
        ));
        // An earlier apply that already regenerated it does not make the row
        // look legitimate: the deletion still stands.
        let mut regenerated = entry(
            300,
            THERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent", 1000.0)),
        );
        regenerated.origin = Some(ChangeOrigin::RecurringApply);
        let mut with_apply = log.clone();
        with_apply.push(regenerated);
        assert!(generated_row_stays_deleted(
            &with_apply,
            &expenses(),
            "tpl-1:2026-03"
        ));
        // The user typing the row again by hand does revive it.
        let mut with_typed = log.clone();
        with_typed.push(entry(
            400,
            HERE,
            expenses(),
            "tpl-1:2026-03",
            upsert(expense_row("tpl-1:2026-03", "Rent", 950.0)),
        ));
        assert!(!generated_row_stays_deleted(
            &with_typed,
            &expenses(),
            "tpl-1:2026-03"
        ));
        // Another row and another month are other rows.
        assert!(!generated_row_stays_deleted(
            &log,
            &expenses(),
            "tpl-2:2026-03"
        ));
        assert!(!generated_row_stays_deleted(
            &log,
            &ChangeTable::Expenses {
                year: 2026,
                month: 4
            },
            "tpl-1:2026-03"
        ));
    }

    /// Two devices filling different months of one asset is ordinary use. Both
    /// months survive, and only the other device's column is written.
    #[test]
    fn two_devices_fill_different_months_of_one_row() {
        let created = entry(
            100,
            HERE,
            liquidity(),
            "asset-1",
            upsert(asset_row("asset-1", 10.0)),
        );
        let march = entry(200, HERE, liquidity(), "asset-1", cell("03", 11.0));
        let april = entry(300, THERE, liquidity(), "asset-1", cell("04", 20.0));

        let plan = plan_merge(&[created, march], &[april]);

        assert_eq!(outcomes(&plan), vec![MergeOutcome::Applied]);
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: liquidity(),
                row_id: "asset-1".to_string(),
                action: RowAction::SetCells {
                    cells: vec![CellValue {
                        column: "04".to_string(),
                        value: Value::from(20.0),
                    }],
                },
            }]
        );
    }

    /// A cashflow income row has no row level entry at all, because its rows
    /// are fixed categories written cell by cell. It still merges per column.
    #[test]
    fn income_cells_merge_without_any_row_level_entry() {
        let table = ChangeTable::CashflowIncome { year: 2026 };
        let mine = entry(100, HERE, table.clone(), "Salary", cell("03", 2000.0));
        let theirs = entry(200, THERE, table.clone(), "Salary", cell("04", 2100.0));

        let plan = plan_merge(&[mine], &[theirs]);

        assert_eq!(outcomes(&plan), vec![MergeOutcome::Applied]);
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table,
                row_id: "Salary".to_string(),
                action: RowAction::SetCells {
                    cells: vec![CellValue {
                        column: "04".to_string(),
                        value: Value::from(2100.0),
                    }],
                },
            }]
        );
        // The income categories are fixed rows, so a cell with no upsert
        // behind it is ordinary here and not a hole in the history.
        assert_eq!(plan.summary.unplaceable, 0);
    }

    /// A cell edit of a row no entry on either side creates cannot be placed:
    /// the change that would say whether the row exists reached neither log.
    /// The plan will not guess the row into existence from one column, and it
    /// says so apart from the ordinary losers, because part 3 has to answer
    /// this rather than report a conflict.
    #[test]
    fn a_cell_for_a_row_nothing_creates_cannot_be_placed() {
        let theirs = entry(200, THERE, liquidity(), "ghost", cell("03", 20.0));

        let plan = plan_merge(&[], one(&theirs));

        assert_eq!(outcomes(&plan), vec![MergeOutcome::Unplaceable]);
        assert!(plan.actions.is_empty());
        assert_eq!(plan.summary.unplaceable, 1);
        assert_eq!(plan.summary.skipped, 0);
        assert_eq!(plan.summary.applied, 0);
        // Stored anyway: the entry is history, and a re-seed may place it.
        assert_eq!(stored(&plan), vec![theirs]);
    }

    /// The same cell is placeable as soon as any entry on either side settles
    /// whether the row exists, wherever that entry ranks.
    #[test]
    fn a_cell_is_placeable_once_an_upsert_exists_on_either_side() {
        let edit = entry(200, THERE, liquidity(), "asset-1", cell("03", 20.0));

        let created_here = entry(
            100,
            HERE,
            liquidity(),
            "asset-1",
            upsert(asset_row("asset-1", 10.0)),
        );
        let local = plan_merge(one(&created_here), one(&edit));
        assert_eq!(outcomes(&local), vec![MergeOutcome::Applied]);
        assert_eq!(local.summary.unplaceable, 0);

        // The creating upsert arriving in the same batch settles it too.
        let created_there = entry(
            100,
            THERE,
            liquidity(),
            "asset-1",
            upsert(asset_row("asset-1", 10.0)),
        );
        let remote = plan_merge(&[], &[created_there, edit]);
        assert_eq!(
            outcomes(&remote),
            vec![MergeOutcome::Applied, MergeOutcome::Applied]
        );
        assert_eq!(remote.summary.unplaceable, 0);
        assert_eq!(remote.actions.len(), 1);
    }

    /// A cell edit of a row that was deleted later has nothing to edit, and a
    /// cell cannot bring the row back.
    #[test]
    fn a_cell_below_a_newer_delete_is_dropped() {
        let created = entry(
            100,
            HERE,
            liquidity(),
            "asset-1",
            upsert(asset_row("asset-1", 10.0)),
        );
        let removed = entry(300, HERE, liquidity(), "asset-1", ChangeOp::Delete);
        let edit = entry(200, THERE, liquidity(), "asset-1", cell("04", 20.0));

        let plan = plan_merge(&[created, removed.clone()], one(&edit));

        assert_eq!(
            outcomes(&plan),
            vec![MergeOutcome::Skipped(SkipReason::RowIsDeleted(id(
                &removed
            )))]
        );
        assert!(plan.actions.is_empty());
        assert_eq!(stored(&plan), vec![edit]);
    }

    /// The same holds for a cell stamped after the delete: a cell says nothing
    /// about whether the row exists, so it cannot recreate one.
    #[test]
    fn a_cell_above_the_newest_delete_is_still_dropped() {
        let removed = entry(100, HERE, liquidity(), "asset-1", ChangeOp::Delete);
        let edit = entry(200, THERE, liquidity(), "asset-1", cell("04", 20.0));

        let plan = plan_merge(one(&removed), &[edit]);

        assert_eq!(
            outcomes(&plan),
            vec![MergeOutcome::Skipped(SkipReason::RowIsDeleted(id(
                &removed
            )))]
        );
        assert!(plan.actions.is_empty());
    }

    /// A remote delete newer than everything removes the row, and the cell
    /// edits under it go nowhere.
    #[test]
    fn a_remote_delete_wins_over_older_cells() {
        let created = entry(
            100,
            HERE,
            liquidity(),
            "asset-1",
            upsert(asset_row("asset-1", 10.0)),
        );
        let mine = entry(200, HERE, liquidity(), "asset-1", cell("03", 11.0));
        let theirs = entry(300, THERE, liquidity(), "asset-1", cell("04", 20.0));
        let removed = entry(400, THERE, liquidity(), "asset-1", ChangeOp::Delete);

        let plan = plan_merge(&[created, mine], &[theirs, removed.clone()]);

        assert_eq!(
            outcomes(&plan),
            vec![
                MergeOutcome::Skipped(SkipReason::RowIsDeleted(id(&removed))),
                MergeOutcome::Applied,
            ]
        );
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: liquidity(),
                row_id: "asset-1".to_string(),
                action: RowAction::Delete,
            }]
        );
    }

    /// A cell newer than the winning upsert decides its own column, and the
    /// merged row carries both: the upsert's other columns and the cell's
    /// value. The cell here is this device's, so applying the remote upsert on
    /// its own would undo a local edit.
    #[test]
    fn a_newer_cell_overrides_the_upserts_value_for_its_column() {
        let mine = entry(300, HERE, liquidity(), "asset-1", cell("03", 99.0));
        let theirs = entry(
            200,
            THERE,
            liquidity(),
            "asset-1",
            upsert(asset_row("asset-1", 10.0)),
        );

        let plan = plan_merge(&[mine], &[theirs]);

        let mut expected = asset_row("asset-1", 10.0);
        expected.insert("03".to_string(), Value::from(99.0));
        assert_eq!(outcomes(&plan), vec![MergeOutcome::Applied]);
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: liquidity(),
                row_id: "asset-1".to_string(),
                action: RowAction::Upsert { row: expected },
            }]
        );
    }

    /// A cell older than the winning upsert loses to it: the upsert carries a
    /// value for that column too, and it is the newer one.
    #[test]
    fn a_cell_older_than_the_winning_upsert_loses_to_it() {
        let theirs_cell = entry(100, THERE, liquidity(), "asset-1", cell("03", 99.0));
        let theirs_row = entry(
            200,
            THERE,
            liquidity(),
            "asset-1",
            upsert(asset_row("asset-1", 10.0)),
        );

        let plan = plan_merge(&[], &[theirs_cell, theirs_row.clone()]);

        assert_eq!(
            outcomes(&plan),
            vec![
                MergeOutcome::Skipped(SkipReason::SupersededByRow(id(&theirs_row))),
                MergeOutcome::Applied,
            ]
        );
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: liquidity(),
                row_id: "asset-1".to_string(),
                action: RowAction::Upsert {
                    row: asset_row("asset-1", 10.0)
                },
            }]
        );
    }

    /// A row edited three times on the phone arrives as three entries. Only
    /// the last one is written, and all three are stored.
    #[test]
    fn several_entries_for_one_row_write_only_the_last() {
        let first = entry(
            100,
            THERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );
        let second = entry(
            200,
            THERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.5)),
        );
        let third = entry(
            300,
            THERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea, large", 4.0)),
        );

        let plan = plan_merge(&[], &[first.clone(), second.clone(), third.clone()]);

        assert_eq!(
            outcomes(&plan),
            vec![
                MergeOutcome::Skipped(SkipReason::SupersededByRow(id(&third))),
                MergeOutcome::Skipped(SkipReason::SupersededByRow(id(&third))),
                MergeOutcome::Applied,
            ]
        );
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: expenses(),
                row_id: "row-1".to_string(),
                action: RowAction::Upsert {
                    row: expense_row("row-1", "Tea, large", 4.0)
                },
            }]
        );
        assert_eq!(stored(&plan), vec![first, second, third]);
    }

    /// Several cells for one column in one batch: the newest decides, the
    /// others are stored and not applied.
    #[test]
    fn several_cells_for_one_column_keep_only_the_newest() {
        let created = entry(
            100,
            HERE,
            liquidity(),
            "asset-1",
            upsert(asset_row("asset-1", 10.0)),
        );
        let older = entry(200, THERE, liquidity(), "asset-1", cell("04", 20.0));
        let newer = entry(300, THERE, liquidity(), "asset-1", cell("04", 25.0));

        let plan = plan_merge(&[created], &[newer.clone(), older]);

        assert_eq!(
            outcomes(&plan),
            vec![
                MergeOutcome::Skipped(SkipReason::SupersededByCell(id(&newer))),
                MergeOutcome::Applied,
            ]
        );
        assert_eq!(
            plan.actions,
            vec![MergeAction {
                table: liquidity(),
                row_id: "asset-1".to_string(),
                action: RowAction::SetCells {
                    cells: vec![CellValue {
                        column: "04".to_string(),
                        value: Value::from(25.0),
                    }],
                },
            }]
        );
    }

    /// The same batch arriving again, after part 2b stored the first one,
    /// writes nothing at all. This is the ordinary case, not an error.
    #[test]
    fn a_batch_that_arrives_twice_applies_nothing_the_second_time() {
        let mine = entry(
            100,
            HERE,
            liquidity(),
            "asset-1",
            upsert(asset_row("asset-1", 10.0)),
        );
        let batch = vec![
            entry(200, THERE, liquidity(), "asset-1", cell("04", 20.0)),
            entry(300, THERE, expenses(), "row-1", ChangeOp::Delete),
        ];

        let first = plan_merge(one(&mine), &batch);
        assert_eq!(first.summary.applied, 2);
        assert_eq!(first.actions.len(), 2);

        let mut log = vec![mine];
        log.extend(first.entries_to_store().cloned());
        let second = plan_merge(&log, &batch);

        assert_eq!(
            outcomes(&second),
            vec![MergeOutcome::AlreadyKnown, MergeOutcome::AlreadyKnown]
        );
        assert!(second.actions.is_empty());
        assert!(second.entries_to_store().next().is_none());
        assert!(second.changes_nothing());
    }

    /// One batch holding the same entry twice counts it once: the first copy
    /// decides, the repeat is already accounted for.
    #[test]
    fn a_repeat_inside_one_batch_is_counted_once() {
        let theirs = entry(
            200,
            THERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );

        let plan = plan_merge(&[], &[theirs.clone(), theirs.clone()]);

        assert_eq!(
            outcomes(&plan),
            vec![MergeOutcome::Applied, MergeOutcome::AlreadyKnown]
        );
        assert_eq!(stored(&plan), vec![theirs]);
        assert_eq!(plan.actions.len(), 1);
    }

    /// A cell edit on a table this version merges as whole rows is stored and
    /// not applied, rather than folded into a row the user never typed.
    #[test]
    fn a_cell_on_a_whole_row_table_is_stored_but_not_applied() {
        let mine = entry(
            100,
            HERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );
        let theirs = entry(
            200,
            THERE,
            expenses(),
            "row-1",
            ChangeOp::Cell {
                column: "expense_amount".to_string(),
                value: Value::from(9.0),
            },
        );

        let plan = plan_merge(&[mine], one(&theirs));

        assert_eq!(
            outcomes(&plan),
            vec![MergeOutcome::Skipped(SkipReason::CellOnWholeRowTable)]
        );
        assert!(plan.actions.is_empty());
        assert_eq!(stored(&plan), vec![theirs]);
    }

    /// The same row id in two different months is two rows in two files, and
    /// neither decides the other.
    #[test]
    fn the_same_row_id_in_another_month_is_another_row() {
        let march = ChangeTable::Expenses {
            year: 2026,
            month: 3,
        };
        let april = ChangeTable::Expenses {
            year: 2026,
            month: 4,
        };
        let mine = entry(900, HERE, march.clone(), "row-1", ChangeOp::Delete);
        let theirs = entry(
            100,
            THERE,
            april.clone(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );

        let plan = plan_merge(&[mine], &[theirs]);

        assert_eq!(outcomes(&plan), vec![MergeOutcome::Applied]);
        assert_eq!(plan.actions[0].table, april);
    }

    /// The plan depends on the batch's contents, not on the order the entries
    /// arrive in.
    #[test]
    fn the_plan_does_not_depend_on_the_batch_order() {
        let local = vec![
            entry(
                100,
                HERE,
                liquidity(),
                "asset-1",
                upsert(asset_row("asset-1", 10.0)),
            ),
            entry(150, HERE, expenses(), "row-1", ChangeOp::Delete),
        ];
        let mut batch = vec![
            entry(200, THERE, liquidity(), "asset-1", cell("04", 20.0)),
            entry(300, THERE, liquidity(), "asset-1", cell("03", 30.0)),
            entry(
                400,
                THERE,
                expenses(),
                "row-1",
                upsert(expense_row("row-1", "Tea", 3.0)),
            ),
            entry(500, THERE, liquidity(), "asset-2", ChangeOp::Delete),
        ];

        let forwards = plan_merge(&local, &batch);
        batch.reverse();
        let backwards = plan_merge(&local, &batch);

        assert_eq!(forwards, backwards);
        assert_eq!(forwards.summary.applied, 4);
        // Actions follow the order the winning changes were made.
        let rows: Vec<&str> = forwards
            .actions
            .iter()
            .map(|action| action.row_id.as_str())
            .collect();
        assert_eq!(rows, vec!["asset-1", "row-1", "asset-2"]);
    }

    /// The literal order of the output, over four rows whose entries
    /// interleave. Comparing two plans is not enough on its own: both are
    /// built by the same process, so two hash maps iterate alike and a plan
    /// that followed group order would still equal itself.
    ///
    /// The decisions here cannot come out in any group order, because the two
    /// entries for `row-1` sit at the ends of the expected list with three
    /// other rows between them. So this pins both sorts: drop the sort of the
    /// batch and the order becomes the order of arrival, drop the sort of the
    /// output and entries for one row clump together.
    #[test]
    fn decisions_and_actions_come_out_in_one_fixed_order() {
        let local = vec![
            entry(
                50,
                HERE,
                liquidity(),
                "asset-1",
                upsert(asset_row("asset-1", 10.0)),
            ),
            entry(
                60,
                HERE,
                liquidity(),
                "asset-2",
                upsert(asset_row("asset-2", 20.0)),
            ),
        ];
        // Given in an order that is neither the stamp order nor grouped by row.
        let batch = vec![
            entry(150, THERE, liquidity(), "asset-2", cell("04", 21.0)),
            entry(
                400,
                THERE,
                expenses(),
                "row-1",
                upsert(expense_row("row-1", "Tea, large", 4.0)),
            ),
            entry(200, THERE, expenses(), "row-2", ChangeOp::Delete),
            entry(300, THERE, liquidity(), "asset-1", cell("03", 11.0)),
            entry(
                100,
                THERE,
                expenses(),
                "row-1",
                upsert(expense_row("row-1", "Tea", 3.0)),
            ),
        ];

        let plan = plan_merge(&local, &batch);

        assert_eq!(
            decision_order(&plan),
            vec![
                (100, "row-1"),
                (150, "asset-2"),
                (200, "row-2"),
                (300, "asset-1"),
                (400, "row-1"),
            ]
        );
        assert_eq!(
            outcomes(&plan),
            vec![
                MergeOutcome::Skipped(SkipReason::SupersededByRow(id(&batch[1]))),
                MergeOutcome::Applied,
                MergeOutcome::Applied,
                MergeOutcome::Applied,
                MergeOutcome::Applied,
            ]
        );
        // Each row is written where its own winning change falls in that order.
        assert_eq!(
            action_order(&plan),
            vec!["asset-2", "row-2", "asset-1", "row-1"]
        );
        // The entries to store keep the same order, since part 2b appends them
        // as one run.
        assert_eq!(
            stored(&plan)
                .iter()
                .map(|entry| entry.stamp.ms)
                .collect::<Vec<i64>>(),
            vec![100, 150, 200, 300, 400]
        );

        let mut shuffled = batch.clone();
        shuffled.rotate_left(3);
        shuffled.reverse();
        assert_eq!(plan_merge(&local, &shuffled), plan);
    }

    /// The summary counts what the decisions say, and reports the largest
    /// stamp in the batch, which the caller hands to the local clock.
    #[test]
    fn the_summary_matches_the_decisions() {
        let mine = entry(
            400,
            HERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );
        let known = entry(100, THERE, expenses(), "row-2", ChangeOp::Delete);
        let batch = vec![
            known.clone(),
            entry(
                200,
                THERE,
                expenses(),
                "row-1",
                upsert(expense_row("row-1", "Coffee", 4.0)),
            ),
            entry(
                300,
                THERE,
                expenses(),
                "row-3",
                upsert(expense_row("row-3", "Bus", 2.0)),
            ),
        ];

        let plan = plan_merge(&[mine, known], &batch);

        assert_eq!(
            plan.summary,
            MergeSummary {
                remote_entries: 3,
                applied: 1,
                already_known: 1,
                skipped: 1,
                unplaceable: 0,
                rows_changed: 1,
                max_remote_stamp: Some(stamp(300)),
            }
        );
        assert!(!plan.changes_nothing());
    }

    /// An empty batch decides nothing and asks for nothing.
    #[test]
    fn an_empty_batch_plans_nothing() {
        let mine = entry(
            100,
            HERE,
            expenses(),
            "row-1",
            upsert(expense_row("row-1", "Tea", 3.0)),
        );

        let plan = plan_merge(&[mine], &[]);

        assert_eq!(plan, MergePlan::default());
        assert!(plan.changes_nothing());
    }

    #[test]
    fn settings_newest_mapping_and_category_entries_win_per_item() {
        let mapping = ChangeTable::CategoryMappings;
        let category = ChangeTable::KnownCategories;
        let mut old_row = Map::new();
        old_row.insert("primary_category".into(), Value::from("old"));
        old_row.insert("secondary_category".into(), Value::from("old"));
        let mut new_row = Map::new();
        new_row.insert("primary_category".into(), Value::from("new"));
        new_row.insert("secondary_category".into(), Value::from("new"));
        let mapping_plan = plan_merge(
            &[entry(100, HERE, mapping.clone(), "coffee", upsert(old_row))],
            &[entry(
                200,
                THERE,
                mapping,
                "coffee",
                upsert(new_row.clone()),
            )],
        );
        assert_eq!(mapping_plan.summary.applied, 1);
        assert!(
            matches!(mapping_plan.actions[0].action, RowAction::Upsert { ref row } if row == &new_row)
        );
        let category_plan = plan_merge(
            &[entry(
                100,
                HERE,
                category.clone(),
                "primary:Food",
                ChangeOp::Upsert { row: Map::new() },
            )],
            &[entry(
                200,
                THERE,
                category,
                "primary:Food",
                ChangeOp::Delete,
            )],
        );
        assert_eq!(category_plan.summary.applied, 1);
        assert!(matches!(category_plan.actions[0].action, RowAction::Delete));
    }

    #[test]
    fn currency_settings_merge_newest_value_per_field() {
        let table = ChangeTable::CurrencySettings;
        let mut reference = Map::new();
        reference.insert("value".into(), Value::from("EUR"));
        let mut mode = Map::new();
        mode.insert("value".into(), Value::from("previous_month_end"));
        let local = vec![
            entry(
                100,
                HERE,
                table.clone(),
                "reference_currency",
                upsert(reference),
            ),
            entry(
                100,
                HERE,
                table.clone(),
                "current_month_rate_mode",
                upsert(mode),
            ),
        ];
        let remote = vec![
            entry(
                200,
                THERE,
                table.clone(),
                "reference_currency",
                ChangeOp::Cell {
                    column: "value".into(),
                    value: Value::from("USD"),
                },
            ),
            entry(
                50,
                THERE,
                table,
                "current_month_rate_mode",
                ChangeOp::Cell {
                    column: "value".into(),
                    value: Value::from("live"),
                },
            ),
        ];
        let plan = plan_merge(&local, &remote);
        assert_eq!(plan.summary.applied, 1);
        assert_eq!(plan.summary.skipped, 1);
        assert!(
            matches!(plan.actions[0].action, RowAction::SetCells { ref cells } if cells[0].value == Value::from("USD"))
        );
    }
}
