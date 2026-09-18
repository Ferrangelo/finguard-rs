//! Carry out a [`MergePlan`] against this device's Parquet data and change
//! log.
//!
//! [`apply_remote_batch`] is the whole of it. It takes the process-wide data
//! write lock ([`crate::write_lock`]), reads the local log, asks
//! [`merge::plan_merge`] what the batch does, moves the clock past the
//! largest remote stamp, and then works in three stages, each of which
//! finishes before the next one starts:
//!
//! 1. **Prepare, in memory.** Every file an action targets is loaded once,
//!    through the same loaders the handlers use, so their normalizations
//!    apply: a missing `currency` or `link` column is added, a legacy `"E"`
//!    currency becomes `"EUR"`, `expense_date` becomes a date, and a file or
//!    year folder that does not exist yet starts as an empty table. Each
//!    action is checked against the schema of the file as loaded and applied
//!    to the dataframe. Every entry to be stored is checked against the log's
//!    line rules too. Any failure here returns an error and writes nothing.
//! 2. **Write the data.** Each file an action actually changed is written
//!    once, through a temporary file and a rename, and without recording a
//!    local change: the other device's entries are stored instead. A year
//!    folder the merge created and wrote nothing into is removed again.
//! 3. **Store the entries** verbatim, with one flush for the whole batch.
//!
//! That order is the plan's contract: the log may only claim what a file
//! holds. A failure in stage 2 or 3 stores nothing more and returns the
//! error. A later run of the same batch re-plans against whatever reached
//! the log and re-applies the rest, which changes nothing that is already in
//! place, so retrying is the repair.
//!
//! # The column policy
//!
//! Decided by the user on 2026-09-18. A column the target file lacks rejects
//! the whole batch, naming the table and the column: the file's schema is
//! never grown to fit, and an unknown column is never dropped. A column the
//! row lacks gets the value the app gives that column in a new row of the
//! same table (see [`default_value`]), or null. Each JSON value is converted
//! to its column's type, and a value that does not fit rejects the batch,
//! naming the column and the type it needed.
//!
//! No message from this module, error or report, holds a row value, a name,
//! an amount, or a category. Row ids appear, as they do in the data layer's
//! own errors: they are random ids, not user data.
//!
//! # What this module does not judge
//!
//! Whether it is safe to sync at all, given a log with corrupt lines or a
//! device that never finished its baseline, belongs to the transport in part
//! 3. [`MergeReport`] carries what the local log read found, and nothing is
//! refused for it here.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use chrono::NaiveDate;
use polars::prelude::*;
use serde_json::{Map, Value};

use crate::df_operations::{
    Cashflow, CreditsDebts, DetailedExpenses, INCOME_CATEGORIES, InvestmentHoldings, Liquidity,
    ROW_ID_COLUMN, RecurringExpenses, has_column, month_labels,
};
use crate::error::{Error, Result};
use crate::merge::{self, CellValue, MergeAction, MergeSummary, RowAction};
use crate::paths::{
    CASHFLOW_FILENAME, CREDITS_DEBTS_FILENAME, INVESTMENTS_FILENAME, INVESTMENTS_PRICES_FILENAME,
    LIQUIDITY_FILENAME, PARQUET_SUFFIX, RECURRING_EXPENSES_FILENAME, get_dbs_root,
};
use crate::sync::{self, ChangeEntry, ChangeTable};
use crate::write_lock;

/// What one [`apply_remote_batch`] run did, in counts. Holds no row values,
/// so it is safe to print or log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergeReport {
    /// The plan's own counts, including the largest remote stamp.
    pub summary: MergeSummary,
    /// Parquet files written. An investments action can write
    /// `investments.parquet`, `investments_prices.parquet`, or both, and each
    /// counts once.
    pub files_written: usize,
    /// Remote entries appended to this device's log.
    pub entries_stored: usize,
    /// How many lines of the local log could not be read. Each one is a
    /// change the plan could not see. See [`sync::LogRead::corrupt_lines`].
    pub local_corrupt_lines: usize,
    /// How many of those were skipped for being too long, which also costs
    /// the clock a stamp. See [`sync::LogRead::oversized_lines`].
    pub local_oversized_lines: usize,
    /// Whether the local log ended in an append that did not finish.
    pub local_unfinished_tail: bool,
    /// From [`sync::LogRead::clock_ahead_of_wall_by`], read before the merge.
    pub clock_ahead_of_wall_by: Option<i64>,
}

impl fmt::Display for MergeReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let summary = &self.summary;
        write!(
            f,
            "Merge: {} remote changes, {} applied, {} already known, {} skipped, {} unplaceable; \
             {} rows and {} files written, {} changes stored.",
            summary.remote_entries,
            summary.applied,
            summary.already_known,
            summary.skipped,
            summary.unplaceable,
            summary.rows_changed,
            self.files_written,
            self.entries_stored,
        )?;
        if self.local_corrupt_lines > 0 {
            write!(
                f,
                " The local change log has {} unreadable lines.",
                self.local_corrupt_lines
            )?;
        }
        Ok(())
    }
}

/// Apply `remote`, a batch of changes from another device, to this device's
/// data and change log.
///
/// Holds the data write lock for the whole run, from reading the local log
/// to storing the last entry, so no local edit can land between planning and
/// storing. Blocking: call it from a plain thread or from
/// [`tokio::task::spawn_blocking`], never directly inside an async task (see
/// [`write_lock::lock_blocking`]).
///
/// See the module docs for the stages, the order of writes, and the column
/// policy. The same batch applied twice changes nothing the second time.
///
/// # Errors
///
/// - [`Error::MergeRejected`] when an action does not fit the file it
///   targets: a column the file lacks, a value of the wrong type, or a
///   cashflow change that is not an income cell. Nothing is written.
/// - [`Error::NotFound`] when a cell change addresses a row this device does
///   not hold, and [`Error::AlreadyExists`] when a row id matches more than
///   one row. Nothing is written.
/// - [`Error::InvalidArgument`] or [`Error::Json`] when an entry cannot be
///   stored as a log line, before anything is written.
/// - [`Error::Io`], [`Error::Polars`], or [`Error::RowIdsMissing`] from
///   loading or writing a file, or from appending to the log. After a write
///   or an append has failed, some files or entries may already be in
///   place. Do not undo them: run the same batch again. The clock has
///   already taken in the batch's stamps by then, so a local edit made
///   before the retry still outranks the batch, unless the process restarts
///   first (see the comment in the body).
pub fn apply_remote_batch(remote: &[ChangeEntry]) -> Result<MergeReport> {
    let guard = write_lock::lock_blocking();
    apply_remote_batch_holding(&guard, remote)
}

/// [`apply_remote_batch`] for a caller that already holds the data write
/// lock for a longer run, such as a phone reset from the hub. Taking the
/// lock again here would wait forever, because it is not reentrant.
pub(crate) fn apply_remote_batch_holding(
    _lock: &write_lock::WriteGuard,
    remote: &[ChangeEntry],
) -> Result<MergeReport> {
    let log = sync::shared_log()?;
    let read = sync::read_log()?;
    let plan = merge::plan_merge(&read.entries, remote);

    // Before any file holds a remote value, so a local edit made after a
    // failed run, and before its retry, still outranks everything in the
    // batch. The clock state lives in memory until an entry carrying it
    // reaches the log: if the process restarts between a failed run and the
    // retry, the clock recovers from the log alone, and an edit made in that
    // window can still rank below a remote value already written to a file.
    if let Some(stamp) = plan.summary.max_remote_stamp {
        log.observe_remote(stamp);
    }

    let to_store: Vec<&ChangeEntry> = plan.entries_to_store().collect();
    for entry in &to_store {
        sync::check_storable(entry)?;
    }

    let mut files = PreparedFiles::default();
    let written = files.prepare(&plan.actions).and_then(|()| files.write());
    // A year folder a load created and nothing was written into, whether
    // because every action was a no-op or because the run failed, would show
    // up in the year list as an empty year.
    files.remove_created_year_dirs();
    let files_written = written?;

    log.append_remote_batch(&to_store)?;

    Ok(MergeReport {
        summary: plan.summary.clone(),
        files_written,
        entries_stored: to_store.len(),
        local_corrupt_lines: read.corrupt_lines.len(),
        local_oversized_lines: read.oversized_lines.len(),
        local_unfinished_tail: read.unfinished_tail,
        clock_ahead_of_wall_by: read.clock_ahead_of_wall_by,
    })
}

/// One file, or for investments the pair of files one loader reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FileKey {
    Expenses(i32, u32),
    Recurring(i32),
    /// `investments.parquet` and `investments_prices.parquet` together,
    /// because [`InvestmentHoldings::new`] loads both.
    Investments(i32),
    Liquidity(i32),
    CreditsDebts(i32),
    Cashflow(i32),
}

impl FileKey {
    fn of(table: &ChangeTable) -> Self {
        match *table {
            ChangeTable::Expenses { year, month } => FileKey::Expenses(year, month),
            ChangeTable::Recurring { year } => FileKey::Recurring(year),
            ChangeTable::Investments { year } | ChangeTable::InvestmentsPrices { year } => {
                FileKey::Investments(year)
            }
            ChangeTable::Liquidity { year } => FileKey::Liquidity(year),
            ChangeTable::CreditsDebts { year } => FileKey::CreditsDebts(year),
            ChangeTable::CashflowIncome { year } => FileKey::Cashflow(year),
        }
    }

    fn year(&self) -> i32 {
        match *self {
            FileKey::Expenses(year, _)
            | FileKey::Recurring(year)
            | FileKey::Investments(year)
            | FileKey::Liquidity(year)
            | FileKey::CreditsDebts(year)
            | FileKey::Cashflow(year) => year,
        }
    }

    fn load(&self) -> Result<Loaded> {
        Ok(match *self {
            FileKey::Expenses(year, month) => Loaded::Expenses(DetailedExpenses::new(year, month)?),
            FileKey::Recurring(year) => Loaded::Recurring(RecurringExpenses::new(year)?),
            FileKey::Investments(year) => Loaded::Investments(InvestmentHoldings::new(year)?),
            FileKey::Liquidity(year) => Loaded::Liquidity(Liquidity::new(year)?),
            FileKey::CreditsDebts(year) => Loaded::CreditsDebts(CreditsDebts::new(year)?),
            FileKey::Cashflow(year) => Loaded::Cashflow(Cashflow::new(year)?),
        })
    }
}

/// A loaded table and the changes applied to it so far, not yet written.
enum Loaded {
    Expenses(DetailedExpenses),
    Recurring(RecurringExpenses),
    /// Holdings and prices, which one loader reads and which are written
    /// separately.
    Investments(InvestmentHoldings),
    Liquidity(Liquidity),
    CreditsDebts(CreditsDebts),
    Cashflow(Cashflow),
}

/// A loaded table and which of its frames an action actually changed. Only
/// a changed frame is written.
struct Prepared {
    loaded: Loaded,
    /// The table's frame changed, or for investments the holdings frame.
    changed: bool,
    /// The investments prices frame changed. Unused for other tables.
    prices_changed: bool,
}

impl Prepared {
    /// Apply one action to the dataframe it targets, in memory, and note
    /// whether the frame changed.
    fn apply(&mut self, action: &MergeAction) -> Result<()> {
        let table = &action.table;
        let changed = match &mut self.loaded {
            Loaded::Expenses(de) => apply_row_action(&mut de.expense_df, table, action)?,
            Loaded::Recurring(rec) => apply_row_action(&mut rec.df, table, action)?,
            Loaded::Investments(holdings) => {
                if matches!(table, ChangeTable::InvestmentsPrices { .. }) {
                    let changed = apply_row_action(&mut holdings.df_prices, table, action)?;
                    self.prices_changed |= changed;
                    false
                } else {
                    apply_row_action(&mut holdings.df, table, action)?
                }
            }
            Loaded::Liquidity(liq) => apply_row_action(&mut liq.df, table, action)?,
            Loaded::CreditsDebts(cd) => apply_row_action(&mut cd.df, table, action)?,
            Loaded::Cashflow(cf) => apply_income_action(cf, action)?,
        };
        self.changed |= changed;
        Ok(())
    }

    /// Work that follows the last action on this file and still writes
    /// nothing: when an income cell changed, the cashflow `Income` row is
    /// recomputed from the income cells, as [`Cashflow::set_income`] does
    /// after each edit.
    fn finish(&mut self) -> Result<()> {
        if let Loaded::Cashflow(cf) = &mut self.loaded
            && self.changed
        {
            cf.recompute_income()?;
        }
        Ok(())
    }

    /// Write every changed frame without recording a local change, and
    /// return how many files that was.
    fn write(&self) -> Result<usize> {
        let mut written = 0;
        if self.changed {
            match &self.loaded {
                Loaded::Expenses(de) => de.save_merged()?,
                Loaded::Recurring(rec) => rec.save_merged()?,
                Loaded::Investments(holdings) => holdings.save_merged_holdings()?,
                Loaded::Liquidity(liq) => liq.save_merged()?,
                Loaded::CreditsDebts(cd) => cd.save_merged()?,
                Loaded::Cashflow(cf) => cf.save_merged()?,
            }
            written += 1;
        }
        if self.prices_changed
            && let Loaded::Investments(holdings) = &self.loaded
        {
            holdings.save_merged_prices()?;
            written += 1;
        }
        Ok(written)
    }
}

/// Every file a plan touches, loaded once, in the order the plan first
/// touches it.
#[derive(Default)]
struct PreparedFiles {
    files: Vec<Prepared>,
    index: HashMap<FileKey, usize>,
    /// Year folders that did not exist before this merge loaded a table in
    /// them. The loaders create the folder, and an empty one would show up
    /// in the year list, so the merge removes each one it wrote nothing
    /// into.
    created_year_dirs: Vec<PathBuf>,
}

impl PreparedFiles {
    /// Load every targeted file and apply every action to it in memory, in
    /// the plan's order. Writes no file.
    fn prepare(&mut self, actions: &[MergeAction]) -> Result<()> {
        for action in actions {
            let key = FileKey::of(&action.table);
            let slot = match self.index.get(&key) {
                Some(slot) => *slot,
                None => {
                    let year_dir = get_dbs_root()?.join(key.year().to_string());
                    if !year_dir.exists() && !self.created_year_dirs.contains(&year_dir) {
                        self.created_year_dirs.push(year_dir);
                    }
                    self.files.push(Prepared {
                        loaded: key.load()?,
                        changed: false,
                        prices_changed: false,
                    });
                    self.index.insert(key, self.files.len() - 1);
                    self.files.len() - 1
                }
            };
            self.files[slot].apply(action)?;
        }
        for file in &mut self.files {
            file.finish()?;
        }
        Ok(())
    }

    /// Write every changed file and return how many were written.
    fn write(&self) -> Result<usize> {
        let mut written = 0;
        for file in &self.files {
            written += file.write()?;
        }
        Ok(written)
    }

    /// Remove the year folders this merge caused to exist and left empty.
    /// `remove_dir` refuses a folder that holds anything, so a folder a file
    /// was written into stays.
    fn remove_created_year_dirs(&self) {
        for dir in &self.created_year_dirs {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// The file a table's rows live in, for messages: a file name and a year,
/// never a value.
fn describe(table: &ChangeTable) -> String {
    let (file, year) = match table {
        ChangeTable::Expenses { year, month } => (format!("{month:02}{PARQUET_SUFFIX}"), year),
        ChangeTable::Recurring { year } => (RECURRING_EXPENSES_FILENAME.to_string(), year),
        ChangeTable::Investments { year } => (INVESTMENTS_FILENAME.to_string(), year),
        ChangeTable::InvestmentsPrices { year } => (INVESTMENTS_PRICES_FILENAME.to_string(), year),
        ChangeTable::Liquidity { year } => (LIQUIDITY_FILENAME.to_string(), year),
        ChangeTable::CreditsDebts { year } => (CREDITS_DEBTS_FILENAME.to_string(), year),
        ChangeTable::CashflowIncome { year } => (CASHFLOW_FILENAME.to_string(), year),
    };
    format!("{file} of {year}")
}

/// The value a column gets when an upserted row does not carry it: what the
/// app puts in that column of a new row of this table, or null where the
/// app has no default.
///
/// The defaults mirror the data layer: `DetailedExpenses::add_row` stores an
/// empty `secondary_category` when neither the caller nor a mapping gives
/// one, `wide_row` zeroes every month column of a new investment, price,
/// liquidity, or credit and debt row, and `POST /api/investments` stores an
/// empty `link` when none is given. Recurring templates have no default: the
/// app requires every field. Changing a default in the data layer means
/// changing it here too.
fn default_value(table: &ChangeTable, column: &str) -> Value {
    let is_month = month_labels().iter().any(|month| month == column);
    match table {
        ChangeTable::Expenses { .. } if column == "secondary_category" => Value::from(""),
        ChangeTable::Investments { .. } | ChangeTable::InvestmentsPrices { .. }
            if column == "link" =>
        {
            Value::from("")
        }
        ChangeTable::Investments { .. }
        | ChangeTable::InvestmentsPrices { .. }
        | ChangeTable::Liquidity { .. }
        | ChangeTable::CreditsDebts { .. }
            if is_month =>
        {
            Value::from(0.0)
        }
        _ => Value::Null,
    }
}

/// Positions of the rows of `df` whose `key_column` equals `key`.
fn positions(df: &DataFrame, key_column: &str, key: &str) -> Result<Vec<usize>> {
    Ok(df
        .column(key_column)?
        .str()?
        .iter()
        .enumerate()
        .filter(|(_, value)| *value == Some(key))
        .map(|(index, _)| index)
        .collect())
}

/// Fail when several rows share `row_id`: writing one of them, or all of
/// them, would each be a guess. The same rule as the data layer's own edits.
fn at_most_one(found: &[usize], table: &ChangeTable, row_id: &str) -> Result<Option<usize>> {
    match found {
        [] => Ok(None),
        [index] => Ok(Some(*index)),
        _ => Err(Error::AlreadyExists(format!(
            "{} rows of {} share the id '{row_id}', so the merge changed nothing. Each row ID \
             must be unique; the data file needs repair.",
            found.len(),
            describe(table)
        ))),
    }
}

/// Fail naming the table and the column when `df` has no such column.
fn require_column(df: &DataFrame, table: &ChangeTable, column: &str) -> Result<()> {
    if has_column(df, column) {
        return Ok(());
    }
    Err(Error::MergeRejected(format!(
        "{} has no column '{column}', so the batch from the other device was not applied. The \
         two devices store this table with different columns.",
        describe(table)
    )))
}

/// Apply one action to a table keyed by [`ROW_ID_COLUMN`], and return
/// whether the frame changed. Only a delete of a row this device does not
/// hold leaves it unchanged; an upsert or a cell write always counts, even
/// when it writes the value the cell already held.
fn apply_row_action(df: &mut DataFrame, table: &ChangeTable, action: &MergeAction) -> Result<bool> {
    let row_id = action.row_id.as_str();
    let found = at_most_one(&positions(df, ROW_ID_COLUMN, row_id)?, table, row_id)?;
    match &action.action {
        RowAction::Upsert { row } => upsert(df, table, row_id, row, found).map(|()| true),
        RowAction::Delete => match found {
            Some(index) => {
                *df = remove_at(df, index)?;
                Ok(true)
            }
            None => Ok(false),
        },
        RowAction::SetCells { cells } => {
            let index = found.ok_or_else(|| {
                Error::NotFound(format!(
                    "No row with id '{row_id}' in {}, so the merge cannot set cells of it.",
                    describe(table)
                ))
            })?;
            set_cells(df, table, index, cells).map(|()| true)
        }
    }
}

/// Replace the row at `found`, or append it when `found` is `None`, with
/// exactly the columns of `df`: the row's own values, and the table's
/// defaults for the columns it lacks.
fn upsert(
    df: &mut DataFrame,
    table: &ChangeTable,
    row_id: &str,
    row: &Map<String, Value>,
    found: Option<usize>,
) -> Result<()> {
    for column in row.keys() {
        require_column(df, table, column)?;
    }
    match row.get(ROW_ID_COLUMN) {
        None | Some(Value::Null) => {}
        Some(Value::String(carried)) if carried == row_id => {}
        Some(_) => {
            return Err(Error::MergeRejected(format!(
                "a row for {} carries a '{ROW_ID_COLUMN}' that is not the id of the change \
                 holding it, so the batch was not applied",
                describe(table)
            )));
        }
    }

    let mut new_columns = Vec::with_capacity(df.width());
    for column in df.columns() {
        let name = column.name().as_str();
        let value = if name == ROW_ID_COLUMN {
            Value::from(row_id)
        } else {
            row.get(name)
                .cloned()
                .unwrap_or_else(|| default_value(table, name))
        };
        let single = one_value(name, column.dtype(), &value, table)?;
        new_columns.push(match found {
            Some(index) => replace_at(column.as_materialized_series(), index, &single)?,
            None => {
                let mut appended = column.as_materialized_series().clone();
                appended.append(&single)?;
                appended
            }
        });
    }
    *df = DataFrame::new_infer_height(new_columns.into_iter().map(Column::from).collect())?;
    Ok(())
}

/// Set `cells` of the row at `index`, leaving every other column alone.
fn set_cells(
    df: &mut DataFrame,
    table: &ChangeTable,
    index: usize,
    cells: &[CellValue],
) -> Result<()> {
    for cell in cells {
        require_column(df, table, &cell.column)?;
        if cell.column == ROW_ID_COLUMN {
            return Err(Error::MergeRejected(format!(
                "a cell change for {} names the '{ROW_ID_COLUMN}' column, which only a whole \
                 row may set, so the batch was not applied",
                describe(table)
            )));
        }
        let current = df.column(&cell.column)?;
        let single = one_value(&cell.column, current.dtype(), &cell.value, table)?;
        let replaced = replace_at(current.as_materialized_series(), index, &single)?;
        df.with_column(Column::from(replaced))?;
    }
    Ok(())
}

/// Apply one action to a cashflow table, and return whether it changed. Its
/// rows are the fixed income categories, keyed by the `category` column, and
/// only their month cells are ever changed by hand.
fn apply_income_action(cf: &mut Cashflow, action: &MergeAction) -> Result<bool> {
    let table = &action.table;
    let RowAction::SetCells { cells } = &action.action else {
        return Err(Error::MergeRejected(format!(
            "a change for {} adds or removes a whole row, but only income cells of that table \
             are synced, so the batch was not applied",
            describe(table)
        )));
    };
    // The derived `Income` row and any other label would be overwritten by
    // the recompute, or would edit a row the app never lets a user edit.
    if !INCOME_CATEGORIES.contains(&action.row_id.as_str()) {
        return Err(Error::MergeRejected(format!(
            "a change for {} names a row that is not an income category, so the batch was not \
             applied",
            describe(table)
        )));
    }
    let found = positions(&cf.df, "category", &action.row_id)?;
    let index = match found.as_slice() {
        [index] => *index,
        [] => {
            return Err(Error::NotFound(format!(
                "{} has no row for an income category the batch changes, so the merge changed \
                 nothing",
                describe(table)
            )));
        }
        _ => {
            return Err(Error::AlreadyExists(format!(
                "{} rows of {} hold the same income category, so the merge changed nothing. The \
                 data file needs repair.",
                found.len(),
                describe(table)
            )));
        }
    };
    let months = month_labels();
    for cell in cells {
        if !months.contains(&cell.column) {
            require_column(&cf.df, table, &cell.column)?;
            return Err(Error::MergeRejected(format!(
                "a change for {} sets the '{}' column, but only month columns of income rows are \
                 synced, so the batch was not applied",
                describe(table),
                cell.column
            )));
        }
    }
    set_cells(&mut cf.df, table, index, cells)?;
    Ok(!cells.is_empty())
}

/// `series` with the value at `index` replaced by the single value in
/// `value`, which has the same type.
fn replace_at(series: &Series, index: usize, value: &Series) -> Result<Series> {
    let mut out = series.slice(0, index);
    out.append(value)?;
    out.append(&series.slice(index as i64 + 1, series.len() - index - 1))?;
    Ok(out)
}

/// `df` without the row at `index`.
fn remove_at(df: &DataFrame, index: usize) -> Result<DataFrame> {
    let before = df.slice(0, index);
    let after = df.slice(index as i64 + 1, df.height() - index - 1);
    Ok(before.vstack(&after)?)
}

/// A one-value series named `column` of type `dtype`, holding `value`
/// converted to that type.
///
/// `expense_date` is stored as `Date` in some monthly files and as `Int32`
/// days since 1970-01-01 in others, and the change log encodes the two
/// differently: an ISO day such as `"2026-03-04"` for `Date`, an integer for
/// `Int32`. [`DetailedExpenses`] loads both as `Date`, so a `Date` column
/// takes both encodings. An `Int32` column takes whole numbers only: an ISO
/// day there would be stored as a day count in a column that is not a date.
fn one_value(column: &str, dtype: &DataType, value: &Value, table: &ChangeTable) -> Result<Series> {
    let name: PlSmallStr = column.into();
    let mismatch = |expected: &str| {
        Error::MergeRejected(format!(
            "the '{column}' column of {} holds {expected}, and the batch from the other device \
             has a value of another kind for it, so the batch was not applied",
            describe(table)
        ))
    };
    let series = match dtype {
        DataType::String => {
            let text = match value {
                Value::Null => None,
                Value::String(text) => Some(text.as_str()),
                _ => return Err(mismatch("text")),
            };
            Series::new(name, &[text])
        }
        DataType::Float64 => {
            let number = match value {
                Value::Null => None,
                Value::Number(number) => number.as_f64(),
                _ => return Err(mismatch("numbers")),
            };
            Series::new(name, &[number])
        }
        DataType::Float32 => {
            let number = match value {
                Value::Null => None,
                Value::Number(number) => number.as_f64().map(|number| number as f32),
                _ => return Err(mismatch("numbers")),
            };
            Series::new(name, &[number])
        }
        DataType::Int64 => {
            let number = match value {
                Value::Null => None,
                _ => Some(whole_number(value).ok_or_else(|| mismatch("whole numbers"))?),
            };
            Series::new(name, &[number])
        }
        DataType::Int32 => {
            let number = match value {
                Value::Null => None,
                _ => Some(
                    whole_number(value)
                        .and_then(|number| i32::try_from(number).ok())
                        .ok_or_else(|| mismatch("whole numbers"))?,
                ),
            };
            Series::new(name, &[number])
        }
        DataType::Date => {
            let days = date_days(value).ok_or_else(|| mismatch("dates"))?;
            Series::new(name, &[days]).cast(&DataType::Date)?
        }
        DataType::Boolean => {
            let flag = match value {
                Value::Null => None,
                Value::Bool(flag) => Some(*flag),
                _ => return Err(mismatch("true or false")),
            };
            Series::new(name, &[flag])
        }
        other => {
            return Err(Error::MergeRejected(format!(
                "the '{column}' column of {} holds {other} values, which a merge cannot write, so \
                 the batch was not applied",
                describe(table)
            )));
        }
    };
    Ok(series)
}

/// A JSON integer, or a float with no fractional part, as an `i64`.
fn whole_number(value: &Value) -> Option<i64> {
    let Value::Number(number) = value else {
        return None;
    };
    if let Some(whole) = number.as_i64() {
        return Some(whole);
    }
    let float = number.as_f64()?;
    // The range check keeps a huge float from saturating to i64::MAX.
    (float.fract() == 0.0 && float >= i64::MIN as f64 && float < i64::MAX as f64)
        .then_some(float as i64)
}

/// A `Date` column value as days since 1970-01-01: `None` inside for a
/// null, `None` outside for a value that is neither an ISO day nor a whole
/// number in the `i32` range.
fn date_days(value: &Value) -> Option<Option<i32>> {
    match value {
        Value::Null => Some(None),
        Value::String(text) => {
            let day = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)?;
            i32::try_from((day - epoch).num_days()).ok().map(Some)
        }
        _ => i32::try_from(whole_number(value)?).ok().map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::df_operations::{read_parquet, str_col_to_vec};
    use crate::paths::{get_monthly_parquet_path, get_year_summary_path};
    use crate::sync::{ChangeOp, Hlc};

    /// Point `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `HOME` at three folders
    /// inside `dir`, and force offline FX mode, so no test reads or writes
    /// the real data or config or reaches the network.
    ///
    /// Two devices are two such folders: switching between them switches
    /// the data, the config holding the device id, and the change log, which
    /// [`sync::shared_log`] keys by path.
    ///
    /// # Safety
    ///
    /// `std::env::set_var` is not thread-safe; callers hold
    /// `#[serial_test::serial]`.
    fn use_device(dir: &std::path::Path) {
        for name in ["data", "config", "home"] {
            std::fs::create_dir_all(dir.join(name)).expect("create a root");
        }
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.join("data"));
            std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
            std::env::set_var("HOME", dir.join("home"));
            std::env::set_var("FINGUARD_FX_OFFLINE", "1");
        }
    }

    fn with_temp_env() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        use_device(dir.path());
        dir
    }

    fn log_entries() -> Vec<ChangeEntry> {
        sync::read_log().expect("read the change log").entries
    }

    fn log_bytes() -> Vec<u8> {
        std::fs::read(sync::changelog_path().unwrap()).unwrap_or_default()
    }

    /// A stamp in 2096, so it is larger than anything the wall clock issues
    /// during a test.
    fn far_future(counter: u32) -> Hlc {
        Hlc {
            ms: 4_000_000_000_000,
            counter,
        }
    }

    fn remote(ms: i64, table: ChangeTable, row_id: &str, op: ChangeOp) -> ChangeEntry {
        ChangeEntry {
            stamp: Hlc { ms, counter: 0 },
            device_id: "phone".to_string(),
            table,
            row_id: row_id.to_string(),
            origin: None,
            op,
        }
    }

    fn march() -> ChangeTable {
        ChangeTable::Expenses {
            year: 2026,
            month: 3,
        }
    }

    fn expense_row(row_id: &str, name: &str, date: Value, amount: f64) -> ChangeOp {
        let mut row = Map::new();
        row.insert("expense_name".into(), Value::from(name));
        row.insert("expense_date".into(), date);
        row.insert("expense_amount".into(), Value::from(amount));
        row.insert("currency".into(), Value::from("EUR"));
        row.insert("primary_category".into(), Value::from("Housing"));
        row.insert("secondary_category".into(), Value::from("Rent"));
        row.insert(ROW_ID_COLUMN.into(), Value::from(row_id));
        ChangeOp::Upsert { row }
    }

    fn recurring_row(row_id: &str, amount: f64) -> ChangeOp {
        let mut row = Map::new();
        row.insert("expense_name".into(), Value::from("Gym"));
        row.insert("expense_day".into(), Value::from(5));
        row.insert("expense_amount".into(), Value::from(amount));
        row.insert("currency".into(), Value::from("EUR"));
        row.insert("primary_category".into(), Value::from("Health"));
        row.insert("secondary_category".into(), Value::from("Gym"));
        row.insert(ROW_ID_COLUMN.into(), Value::from(row_id));
        ChangeOp::Upsert { row }
    }

    fn cell(column: &str, value: f64) -> ChangeOp {
        ChangeOp::Cell {
            column: column.to_string(),
            value: Value::from(value),
        }
    }

    fn march_expenses() -> DataFrame {
        DetailedExpenses::new(2026, 3).unwrap().expense_df
    }

    fn amounts(df: &DataFrame) -> Vec<f64> {
        df.column("expense_amount")
            .unwrap()
            .f64()
            .unwrap()
            .iter()
            .map(|amount| amount.unwrap())
            .collect()
    }

    fn f64_at(df: &DataFrame, key_column: &str, key: &str, column: &str) -> Option<f64> {
        let index = positions(df, key_column, key).unwrap()[0];
        df.column(column).unwrap().f64().unwrap().get(index)
    }

    /// `df` with its rows in `key_column` order, so two devices that hold
    /// the same rows in a different order compare equal.
    fn sorted(df: &DataFrame, key_column: &str) -> DataFrame {
        df.sort([key_column], SortMultipleOptions::default())
            .unwrap()
    }

    /// The phone starts with no data folder at all. Applying the desktop's
    /// whole history creates every file, in a year folder that did not
    /// exist, and stores every entry.
    #[test]
    #[serial_test::serial]
    fn a_first_sync_fills_an_empty_data_folder() {
        let _temp = with_temp_env();
        let batch = vec![
            remote(
                1,
                march(),
                "rent",
                expense_row("rent", "Rent", Value::from("2026-03-01"), 900.0),
            ),
            remote(
                2,
                ChangeTable::Recurring { year: 2026 },
                "gym",
                recurring_row("gym", 30.0),
            ),
            remote(3, ChangeTable::Liquidity { year: 2026 }, "bank", {
                let mut row = Map::new();
                row.insert("asset_name".into(), Value::from("Bank"));
                row.insert("category".into(), Value::from("Cash"));
                row.insert("currency".into(), Value::from("EUR"));
                row.insert("03".into(), Value::from(1200.0));
                row.insert(ROW_ID_COLUMN.into(), Value::from("bank"));
                ChangeOp::Upsert { row }
            }),
            remote(
                4,
                ChangeTable::CashflowIncome { year: 2026 },
                "Salary",
                cell("03", 2500.0),
            ),
        ];
        assert!(!get_dbs_root().unwrap().join("2026").exists());

        let report = apply_remote_batch(&batch).expect("apply into an empty folder");

        assert_eq!(report.summary.applied, 4);
        assert_eq!(report.files_written, 4);
        assert_eq!(report.entries_stored, 4);
        assert_eq!(amounts(&march_expenses()), vec![900.0]);
        assert_eq!(RecurringExpenses::new(2026).unwrap().df.height(), 1);
        let liquidity = Liquidity::new(2026).unwrap().df;
        assert_eq!(
            f64_at(&liquidity, ROW_ID_COLUMN, "bank", "03"),
            Some(1200.0)
        );
        assert_eq!(
            f64_at(&liquidity, ROW_ID_COLUMN, "bank", "04"),
            Some(0.0),
            "a month the row does not carry gets the new row default"
        );
        let cashflow = Cashflow::new(2026).unwrap().df;
        assert_eq!(f64_at(&cashflow, "category", "Salary", "03"), Some(2500.0));
        assert_eq!(f64_at(&cashflow, "category", "Income", "03"), Some(2500.0));
        assert_eq!(log_entries(), batch);
    }

    /// Upsert replaces the whole row in place, a delete removes it, and a
    /// delete of a row this device never held is a no-op, for both whole
    /// row tables.
    #[test]
    #[serial_test::serial]
    fn whole_rows_are_upserted_and_deleted() {
        let _temp = with_temp_env();
        let mut de = DetailedExpenses::new(2026, 3).unwrap();
        let rent = de
            .add_row("Rent", 1, 900.0, Some("Housing"), "EUR", Some("Rent"))
            .unwrap();
        let tea = de
            .add_row("Tea", 2, 3.0, Some("Out"), "EUR", Some("Cafe"))
            .unwrap();
        let mut rec = RecurringExpenses::new(2026).unwrap();
        let gym = rec.add("Gym", 5, 30.0, "EUR", "Health", "Gym").unwrap();
        let later = far_future(0).ms;

        apply_remote_batch(&[
            remote(
                later,
                march(),
                &rent,
                expense_row(&rent, "Rent", Value::from("2026-03-01"), 950.0),
            ),
            remote(later + 1, march(), &tea, ChangeOp::Delete),
            remote(later + 2, march(), "never-here", ChangeOp::Delete),
            remote(
                later + 3,
                ChangeTable::Recurring { year: 2026 },
                &gym,
                recurring_row(&gym, 35.0),
            ),
            remote(
                later + 4,
                ChangeTable::Recurring { year: 2026 },
                "new-template",
                recurring_row("new-template", 10.0),
            ),
        ])
        .expect("apply");

        let expenses = march_expenses();
        assert_eq!(
            str_col_to_vec(&expenses, ROW_ID_COLUMN).unwrap(),
            vec![rent]
        );
        assert_eq!(amounts(&expenses), vec![950.0]);
        let recurring = RecurringExpenses::new(2026).unwrap().df;
        assert_eq!(
            str_col_to_vec(&recurring, ROW_ID_COLUMN).unwrap(),
            vec![gym.clone(), "new-template".to_string()],
            "the replaced row keeps its place and the new one is appended"
        );
        assert_eq!(
            f64_at(&recurring, ROW_ID_COLUMN, &gym, "expense_amount"),
            Some(35.0)
        );

        apply_remote_batch(&[remote(
            later + 5,
            ChangeTable::Recurring { year: 2026 },
            &gym,
            ChangeOp::Delete,
        )])
        .expect("apply a delete");
        assert_eq!(
            str_col_to_vec(&RecurringExpenses::new(2026).unwrap().df, ROW_ID_COLUMN).unwrap(),
            vec!["new-template".to_string()]
        );
    }

    /// Two devices fill different months of one liquidity row: each keeps
    /// the other's month, and a newer remote cell beats an older local one.
    #[test]
    #[serial_test::serial]
    fn net_worth_cells_merge_per_cell() {
        let _temp = with_temp_env();
        let mut liq = Liquidity::new(2026).unwrap();
        liq.add_asset("Bank", "Cash", "EUR").unwrap();
        liq.set_value("Bank", 1, 100.0).unwrap();
        liq.set_value("Bank", 2, 200.0).unwrap();
        let id = str_col_to_vec(&liq.df, ROW_ID_COLUMN).unwrap()[0].clone();
        let table = ChangeTable::Liquidity { year: 2026 };
        let later = far_future(0).ms;

        let report = apply_remote_batch(&[
            remote(later, table.clone(), &id, cell("02", 250.0)),
            remote(later + 1, table.clone(), &id, cell("03", 300.0)),
        ])
        .expect("apply");

        assert_eq!(report.files_written, 1);
        let df = Liquidity::new(2026).unwrap().df;
        assert_eq!(f64_at(&df, ROW_ID_COLUMN, &id, "01"), Some(100.0));
        assert_eq!(f64_at(&df, ROW_ID_COLUMN, &id, "02"), Some(250.0));
        assert_eq!(f64_at(&df, ROW_ID_COLUMN, &id, "03"), Some(300.0));
        assert_eq!(str_col_to_vec(&df, "asset_name").unwrap(), vec!["Bank"]);

        // An older remote cell loses to the local edit and changes nothing.
        let before = log_entries().len();
        let report = apply_remote_batch(&[remote(5, table, &id, cell("01", 1.0))]).expect("apply");
        assert_eq!(report.summary.skipped, 1);
        assert_eq!(report.files_written, 0);
        assert_eq!(
            log_entries().len(),
            before + 1,
            "a skipped entry is still stored"
        );
        let df = Liquidity::new(2026).unwrap().df;
        assert_eq!(f64_at(&df, ROW_ID_COLUMN, &id, "01"), Some(100.0));
    }

    /// Investments and prices are two tables in one loader. A prices change
    /// writes only the prices file.
    #[test]
    #[serial_test::serial]
    fn a_price_change_writes_only_the_prices_file() {
        let _temp = with_temp_env();
        let mut inv = InvestmentHoldings::new(2026).unwrap();
        inv.add_asset("Fund", "Stocks/ETF", "", "EUR").unwrap();
        let id = str_col_to_vec(&inv.df, ROW_ID_COLUMN).unwrap()[0].clone();
        let holdings_path = get_year_summary_path(2026, INVESTMENTS_FILENAME).unwrap();
        let holdings_before = std::fs::read(&holdings_path).unwrap();

        let report = apply_remote_batch(&[remote(
            far_future(0).ms,
            ChangeTable::InvestmentsPrices { year: 2026 },
            &id,
            cell("04", 12.5),
        )])
        .expect("apply");

        assert_eq!(report.files_written, 1);
        assert_eq!(std::fs::read(&holdings_path).unwrap(), holdings_before);
        let prices = InvestmentHoldings::new(2026).unwrap().df_prices;
        assert_eq!(f64_at(&prices, ROW_ID_COLUMN, &id, "04"), Some(12.5));
    }

    /// An income cell from the other device lands in its category, and the
    /// derived `Income` row is recomputed from all the income cells.
    #[test]
    #[serial_test::serial]
    fn income_cells_merge_and_income_is_recomputed() {
        let _temp = with_temp_env();
        let mut cf = Cashflow::new(2026).unwrap();
        cf.set_income(3, "Salary", 2000.0).unwrap();
        let table = ChangeTable::CashflowIncome { year: 2026 };
        let later = far_future(0).ms;

        apply_remote_batch(&[
            remote(later, table.clone(), "Other", cell("03", 150.0)),
            remote(later + 1, table.clone(), "Salary", cell("04", 2100.0)),
        ])
        .expect("apply");

        let df = Cashflow::new(2026).unwrap().df;
        assert_eq!(f64_at(&df, "category", "Salary", "03"), Some(2000.0));
        assert_eq!(f64_at(&df, "category", "Other", "03"), Some(150.0));
        assert_eq!(f64_at(&df, "category", "Income", "03"), Some(2150.0));
        assert_eq!(f64_at(&df, "category", "Income", "04"), Some(2100.0));

        let err = apply_remote_batch(&[remote(later + 2, table, "Income", cell("05", 1.0))])
            .expect_err("the derived row is not synced");
        assert!(matches!(err, Error::MergeRejected(_)), "{err}");
    }

    /// A column the file lacks rejects the whole batch: no file changes, no
    /// year folder is left behind, and no entry is stored. The message names
    /// the table and the column and holds no value.
    #[test]
    #[serial_test::serial]
    fn an_unknown_column_rejects_the_whole_batch() {
        let _temp = with_temp_env();
        let mut liq = Liquidity::new(2026).unwrap();
        liq.add_asset("Bank", "Cash", "EUR").unwrap();
        let id = str_col_to_vec(&liq.df, ROW_ID_COLUMN).unwrap()[0].clone();
        let liquidity_path = get_year_summary_path(2026, LIQUIDITY_FILENAME).unwrap();
        let liquidity_before = std::fs::read(&liquidity_path).unwrap();
        let log_before = log_bytes();
        let later = far_future(0).ms;

        let mut row = Map::new();
        row.insert("asset_name".into(), Value::from("SecretBank"));
        row.insert("category".into(), Value::from("Cash"));
        row.insert("currency".into(), Value::from("EUR"));
        row.insert("bogus".into(), Value::from(4242.5));
        let batch = vec![
            remote(
                later,
                ChangeTable::Expenses {
                    year: 2031,
                    month: 1,
                },
                "rent",
                expense_row("rent", "Rent", Value::from("2031-01-01"), 900.0),
            ),
            remote(
                later + 1,
                ChangeTable::Liquidity { year: 2026 },
                &id,
                ChangeOp::Upsert { row },
            ),
        ];

        let err = apply_remote_batch(&batch).expect_err("an unknown column is refused");

        let message = err.to_string();
        assert!(matches!(err, Error::MergeRejected(_)), "{message}");
        assert!(message.contains("'bogus'"), "{message}");
        assert!(message.contains(LIQUIDITY_FILENAME), "{message}");
        assert!(!message.contains("SecretBank") && !message.contains("4242"));
        assert_eq!(std::fs::read(&liquidity_path).unwrap(), liquidity_before);
        assert!(!get_dbs_root().unwrap().join("2031").exists());
        assert_eq!(log_bytes(), log_before);
    }

    /// A value that does not fit its column is refused the same way, naming
    /// the column and the type it needed.
    #[test]
    #[serial_test::serial]
    fn a_value_of_the_wrong_type_rejects_the_batch() {
        let _temp = with_temp_env();
        let mut op = expense_row("rent", "Rent", Value::from("2026-03-01"), 0.0);
        if let ChangeOp::Upsert { row } = &mut op {
            row.insert("expense_amount".into(), Value::from("nine hundred"));
        }

        let err = apply_remote_batch(&[remote(1, march(), "rent", op)])
            .expect_err("text is not an amount");

        let message = err.to_string();
        assert!(message.contains("'expense_amount'"), "{message}");
        assert!(message.contains("numbers"), "{message}");
        assert!(!message.contains("nine hundred"));
        assert!(!get_monthly_parquet_path(2026, 3).unwrap().exists());
        assert!(log_entries().is_empty());
    }

    fn expense_dates(df: &DataFrame) -> Vec<NaiveDate> {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        df.column("expense_date")
            .unwrap()
            .cast(&DataType::Int32)
            .unwrap()
            .i32()
            .unwrap()
            .iter()
            .map(|days| epoch + chrono::Duration::days(days.unwrap() as i64))
            .collect()
    }

    fn write_month(date_column: Column) {
        let mut df = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Rent"]),
            date_column,
            Column::new("expense_amount".into(), &[900.0]),
            Column::new("currency".into(), &["EUR"]),
            Column::new("primary_category".into(), &["Housing"]),
            Column::new("secondary_category".into(), &["Rent"]),
            Column::new(ROW_ID_COLUMN.into(), &["rent"]),
        ])
        .unwrap();
        let file = std::fs::File::create(get_monthly_parquet_path(2026, 3).unwrap()).unwrap();
        ParquetWriter::new(file).finish(&mut df).unwrap();
    }

    /// Monthly files store `expense_date` as `Date` or as `Int32` days, and
    /// the log encodes those as an ISO day or an integer. The loader reads
    /// both files as `Date`, so both encodings land on the same day in
    /// either file, and the merged file is saved with a `Date` column, as an
    /// ordinary save of that month would.
    #[test]
    #[serial_test::serial]
    fn both_date_encodings_fit_int32_and_date_files() {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 3, d).unwrap();
        let days = |d: u32| (day(d) - epoch).num_days() as i32;
        let columns = [
            Column::new("expense_date".into(), &[days(1)]),
            Column::new("expense_date".into(), &[days(1)])
                .cast(&DataType::Date)
                .unwrap(),
        ];
        for date_column in columns {
            let _temp = with_temp_env();
            write_month(date_column);

            apply_remote_batch(&[
                remote(
                    1,
                    march(),
                    "by-text",
                    expense_row("by-text", "A", Value::from("2026-03-07"), 1.0),
                ),
                remote(
                    2,
                    march(),
                    "by-number",
                    expense_row("by-number", "B", Value::from(days(9)), 2.0),
                ),
            ])
            .expect("apply both encodings");

            assert_eq!(
                expense_dates(&march_expenses()),
                vec![day(1), day(7), day(9)]
            );
            let on_disk = read_parquet(&get_monthly_parquet_path(2026, 3).unwrap()).unwrap();
            assert_eq!(
                on_disk.column("expense_date").unwrap().dtype(),
                &DataType::Date
            );
        }
    }

    /// Called directly: a `Date` column takes an ISO day and an integer day
    /// count, an `Int32` column takes only whole numbers, and a date string
    /// sent to an `Int32` column is refused without quoting it.
    #[test]
    fn a_date_column_takes_both_encodings_and_an_int32_column_only_numbers() {
        let table = march();
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let days = (NaiveDate::from_ymd_opt(2026, 3, 7).unwrap() - epoch).num_days() as i32;

        let by_text = one_value("d", &DataType::Date, &Value::from("2026-03-07"), &table).unwrap();
        let by_number = one_value("d", &DataType::Date, &Value::from(days), &table).unwrap();
        assert_eq!(by_text.dtype(), &DataType::Date);
        assert_eq!(by_number.dtype(), &DataType::Date);
        assert_eq!(
            by_text
                .cast(&DataType::Int32)
                .unwrap()
                .i32()
                .unwrap()
                .get(0),
            Some(days)
        );
        assert!(by_text.equals(&by_number));
        let null_date = one_value("d", &DataType::Date, &Value::Null, &table).unwrap();
        assert_eq!(null_date.null_count(), 1);
        assert!(one_value("d", &DataType::Date, &Value::from("7 March"), &table).is_err());

        let number = one_value("n", &DataType::Int32, &Value::from(days), &table).unwrap();
        assert_eq!(number.dtype(), &DataType::Int32);
        assert_eq!(number.i32().unwrap().get(0), Some(days));
        let whole_float = one_value("n", &DataType::Int32, &Value::from(5.0), &table).unwrap();
        assert_eq!(whole_float.i32().unwrap().get(0), Some(5));
        let null_number = one_value("n", &DataType::Int32, &Value::Null, &table).unwrap();
        assert_eq!(null_number.null_count(), 1);
        for refused in [
            Value::from("2026-03-07"),
            Value::from(5.5),
            Value::from(i64::from(i32::MAX) + 1),
        ] {
            let err = one_value("n", &DataType::Int32, &refused, &table)
                .expect_err("not a whole number in range");
            let message = err.to_string();
            assert!(matches!(err, Error::MergeRejected(_)), "{message}");
            assert!(message.contains("'n'") && message.contains("whole numbers"));
            assert!(!message.contains("2026-03-07"), "{message}");
        }
    }

    /// A delete of a row this device never held changes no file: it creates
    /// no monthly file and no year folder, rewrites no existing file, and is
    /// still stored.
    #[test]
    #[serial_test::serial]
    fn a_delete_of_an_absent_row_writes_nothing() {
        let _temp = with_temp_env();
        DetailedExpenses::new(2026, 3)
            .unwrap()
            .add_row("Rent", 1, 900.0, Some("Housing"), "EUR", Some("Rent"))
            .unwrap();
        let march_path = get_monthly_parquet_path(2026, 3).unwrap();
        let march_before = std::fs::read(&march_path).unwrap();
        let later = far_future(0).ms;

        let report = apply_remote_batch(&[
            remote(later, march(), "ghost", ChangeOp::Delete),
            remote(
                later + 1,
                ChangeTable::Expenses {
                    year: 2031,
                    month: 1,
                },
                "ghost",
                ChangeOp::Delete,
            ),
        ])
        .expect("apply");

        assert_eq!(report.summary.rows_changed, 2);
        assert_eq!(report.files_written, 0);
        assert_eq!(report.entries_stored, 2);
        assert_eq!(std::fs::read(&march_path).unwrap(), march_before);
        assert!(!get_dbs_root().unwrap().join("2031").exists());
    }

    fn bank_upsert() -> ChangeOp {
        let mut row = Map::new();
        row.insert("asset_name".into(), Value::from("Bank"));
        row.insert("category".into(), Value::from("Cash"));
        row.insert("currency".into(), Value::from("EUR"));
        row.insert("03".into(), Value::from(1200.0));
        row.insert(ROW_ID_COLUMN.into(), Value::from("bank"));
        ChangeOp::Upsert { row }
    }

    /// A write that fails part way leaves the files written before it and
    /// stores no entry, and a local edit made before the retry still ranks
    /// above the batch. Once the obstacle is gone, running the same batch
    /// again ends with the data and the stored entries a clean run produces.
    #[test]
    #[serial_test::serial]
    fn a_failed_write_stores_nothing_and_a_retry_completes() {
        let root = tempfile::tempdir().unwrap();
        let later = far_future(0).ms;
        let batch = vec![
            remote(
                later,
                march(),
                "rent",
                expense_row("rent", "Rent", Value::from("2026-03-01"), 900.0),
            ),
            remote(
                later + 1,
                ChangeTable::Liquidity { year: 2026 },
                "bank",
                bank_upsert(),
            ),
        ];
        let state = || {
            (
                march_expenses(),
                Liquidity::new(2026).unwrap().df,
                log_entries()
                    .into_iter()
                    .filter(|entry| entry.device_id == "phone")
                    .collect::<Vec<_>>(),
            )
        };

        use_device(&root.path().join("failing"));
        // A folder where the liquidity write puts its temporary file makes
        // that write fail after the monthly file, the first target, is done.
        let liquidity_path = get_year_summary_path(2026, LIQUIDITY_FILENAME).unwrap();
        let obstacle = liquidity_path.with_file_name(format!(".{LIQUIDITY_FILENAME}.tmp"));
        std::fs::create_dir(&obstacle).unwrap();

        let err = apply_remote_batch(&batch).expect_err("the liquidity write fails");

        assert!(matches!(err, Error::Io(_)), "{err}");
        assert_eq!(amounts(&march_expenses()), vec![900.0]);
        assert!(!liquidity_path.exists());
        assert!(
            log_entries().is_empty(),
            "no entry is stored after a failure"
        );

        // The clock took in the batch before the failed write, so an edit
        // made before the retry outranks the remote values already on disk.
        CreditsDebts::new(2026)
            .unwrap()
            .add_entry("Loan", "EUR")
            .unwrap();
        let local_edit = log_entries().last().unwrap().stamp;
        assert!(
            local_edit > batch[1].stamp,
            "the local edit ranks above the batch"
        );

        std::fs::remove_dir(&obstacle).unwrap();
        let report = apply_remote_batch(&batch).expect("the retry succeeds");
        assert_eq!(report.entries_stored, 2);
        let retried = state();

        use_device(&root.path().join("clean"));
        apply_remote_batch(&batch).expect("a clean run");
        assert_eq!(state(), retried);
    }

    /// The investments files keep the shapes found in real data: a holdings
    /// file with no `currency` column and prices with `link` after the month
    /// columns. The merge writes into the frames as the loader shapes them,
    /// and the result loads again with the merged values.
    #[test]
    #[serial_test::serial]
    fn investments_in_real_file_shapes_take_a_merge() {
        let _temp = with_temp_env();
        let file = |with_currency: bool| {
            let mut columns = vec![
                Column::new("asset_name".into(), &["Fund"]),
                Column::new("category".into(), &["Stocks/ETF"]),
            ];
            if with_currency {
                columns.push(Column::new("currency".into(), &["EUR"]));
            }
            for month in month_labels() {
                columns.push(Column::new(month.as_str().into(), &[1.0_f64]));
            }
            columns.push(Column::new("link".into(), &[""]));
            columns.push(Column::new(ROW_ID_COLUMN.into(), &["fund"]));
            DataFrame::new_infer_height(columns).unwrap()
        };
        for (name, mut df) in [
            (INVESTMENTS_FILENAME, file(false)),
            (INVESTMENTS_PRICES_FILENAME, file(false)),
        ] {
            let path = get_year_summary_path(2026, name).unwrap();
            ParquetWriter::new(std::fs::File::create(path).unwrap())
                .finish(&mut df)
                .unwrap();
        }
        let fund_row = |with_currency: bool, march: f64| {
            let df = file(with_currency);
            let mut row = crate::df_operations::row_json(&df, 0).unwrap();
            row.insert("03".into(), Value::from(march));
            ChangeOp::Upsert { row }
        };
        let later = far_future(0).ms;

        let report = apply_remote_batch(&[
            remote(
                later,
                ChangeTable::Investments { year: 2026 },
                "fund",
                fund_row(true, 7.0),
            ),
            remote(
                later + 1,
                ChangeTable::InvestmentsPrices { year: 2026 },
                "fund",
                fund_row(false, 20.0),
            ),
            remote(
                later + 2,
                ChangeTable::InvestmentsPrices { year: 2026 },
                "fund",
                cell("04", 21.0),
            ),
        ])
        .expect("apply into the real shapes");

        assert_eq!(report.files_written, 2);
        let inv = InvestmentHoldings::new(2026).expect("the merged files load");
        assert_eq!(f64_at(&inv.df, ROW_ID_COLUMN, "fund", "03"), Some(7.0));
        assert_eq!(f64_at(&inv.df, ROW_ID_COLUMN, "fund", "01"), Some(1.0));
        assert_eq!(str_col_to_vec(&inv.df, "currency").unwrap(), vec!["EUR"]);
        assert_eq!(
            f64_at(&inv.df_prices, ROW_ID_COLUMN, "fund", "03"),
            Some(20.0)
        );
        assert_eq!(
            f64_at(&inv.df_prices, ROW_ID_COLUMN, "fund", "04"),
            Some(21.0)
        );
        assert!(!has_column(&inv.df_prices, "currency"));
        let prices_on_disk =
            read_parquet(&get_year_summary_path(2026, INVESTMENTS_PRICES_FILENAME).unwrap())
                .unwrap();
        let names: Vec<String> = prices_on_disk
            .get_column_names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        assert_eq!(names.iter().position(|name| name == "link"), Some(14));
    }

    /// The same batch applied a second time changes no file and stores no
    /// entry. Stored entries keep their original stamp and device id, and
    /// the local clock moves past the largest remote stamp.
    #[test]
    #[serial_test::serial]
    fn a_batch_is_stored_verbatim_and_applies_once() {
        let _temp = with_temp_env();
        let stamp = far_future(7);
        let mut entry = remote(
            0,
            march(),
            "rent",
            expense_row("rent", "Rent", Value::from("2026-03-01"), 900.0),
        );
        entry.stamp = stamp;
        let batch = vec![entry];

        apply_remote_batch(&batch).expect("first apply");
        let path = get_monthly_parquet_path(2026, 3).unwrap();
        let file_after_first = std::fs::read(&path).unwrap();
        let log_after_first = log_bytes();

        let report = apply_remote_batch(&batch).expect("second apply");

        assert_eq!(report.summary.already_known, 1);
        assert_eq!(report.files_written, 0);
        assert_eq!(report.entries_stored, 0);
        assert_eq!(std::fs::read(&path).unwrap(), file_after_first);
        assert_eq!(log_bytes(), log_after_first);
        let stored = log_entries();
        assert_eq!(stored, batch, "stored with its own stamp and device id");
        assert_eq!(stored[0].device_id, "phone");

        let local = DetailedExpenses::new(2026, 3).unwrap().add_row(
            "Tea",
            2,
            3.0,
            Some("Out"),
            "EUR",
            Some("Cafe"),
        );
        local.expect("a local edit after the merge");
        let newest = log_entries().last().unwrap().clone();
        assert!(
            newest.stamp > stamp,
            "the local clock passed the remote stamp"
        );
        assert_ne!(newest.device_id, "phone");
    }

    /// Everything under the three folders of the current device, as data a
    /// test can compare between two devices.
    fn snapshot() -> Vec<DataFrame> {
        vec![
            sorted(&march_expenses(), ROW_ID_COLUMN),
            sorted(&RecurringExpenses::new(2026).unwrap().df, ROW_ID_COLUMN),
            sorted(&Liquidity::new(2026).unwrap().df, ROW_ID_COLUMN),
            Cashflow::new(2026).unwrap().df,
        ]
    }

    /// A desktop and a phone drift apart, then sync both ways, and end with
    /// the same data. Syncing again changes nothing on either side.
    #[test]
    #[serial_test::serial]
    fn a_desktop_and_a_phone_converge() {
        let root = tempfile::tempdir().unwrap();
        let desktop = root.path().join("desktop");
        let phone = root.path().join("phone");

        // The desktop has history.
        use_device(&desktop);
        let mut de = DetailedExpenses::new(2026, 3).unwrap();
        let rent = de
            .add_row("Rent", 1, 900.0, Some("Housing"), "EUR", Some("Rent"))
            .unwrap();
        let tea = de
            .add_row("Tea", 2, 3.0, Some("Out"), "EUR", Some("Cafe"))
            .unwrap();
        RecurringExpenses::new(2026)
            .unwrap()
            .add("Gym", 5, 30.0, "EUR", "Health", "Gym")
            .unwrap();
        let mut liq = Liquidity::new(2026).unwrap();
        liq.add_asset("Bank", "Cash", "EUR").unwrap();
        liq.set_value("Bank", 1, 100.0).unwrap();
        Cashflow::new(2026)
            .unwrap()
            .set_income(1, "Salary", 2000.0)
            .unwrap();
        let desktop_history = log_entries();

        // The phone starts empty and takes all of it.
        use_device(&phone);
        apply_remote_batch(&desktop_history).expect("first sync on the phone");
        assert_eq!(snapshot(), {
            use_device(&desktop);
            snapshot()
        });

        // They drift apart.
        use_device(&desktop);
        DetailedExpenses::new(2026, 3)
            .unwrap()
            .edit_row(&rent, None, None, Some(950.0), None, None, None)
            .unwrap();
        Liquidity::new(2026)
            .unwrap()
            .set_value("Bank", 3, 300.0)
            .unwrap();
        use_device(&phone);
        DetailedExpenses::new(2026, 3)
            .unwrap()
            .delete_row(&tea)
            .unwrap();
        Liquidity::new(2026)
            .unwrap()
            .set_value("Bank", 5, 500.0)
            .unwrap();
        Cashflow::new(2026)
            .unwrap()
            .set_income(2, "Other", 40.0)
            .unwrap();
        DetailedExpenses::new(2026, 4)
            .unwrap()
            .add_row("Book", 3, 12.0, Some("Culture"), "EUR", Some("Books"))
            .unwrap();
        let phone_history = log_entries();

        // They sync both ways.
        use_device(&desktop);
        let desktop_history = log_entries();
        apply_remote_batch(&phone_history).expect("desktop takes the phone's changes");
        let desktop_data = snapshot();
        let desktop_april = DetailedExpenses::new(2026, 4).unwrap().expense_df;
        use_device(&phone);
        apply_remote_batch(&desktop_history).expect("phone takes the desktop's changes");
        let phone_data = snapshot();

        assert_eq!(phone_data, desktop_data);
        assert_eq!(
            DetailedExpenses::new(2026, 4).unwrap().expense_df,
            desktop_april
        );
        assert_eq!(amounts(&phone_data[0]), vec![950.0]);
        let liquidity = &phone_data[2];
        let bank = str_col_to_vec(liquidity, ROW_ID_COLUMN).unwrap()[0].clone();
        for (month, value) in [("01", 100.0), ("03", 300.0), ("05", 500.0)] {
            assert_eq!(f64_at(liquidity, ROW_ID_COLUMN, &bank, month), Some(value));
        }
        assert_eq!(
            f64_at(&phone_data[3], "category", "Income", "02"),
            Some(40.0)
        );

        // A further exchange of whole logs changes nothing on either side.
        let phone_log = log_entries();
        use_device(&desktop);
        let report = apply_remote_batch(&phone_log).expect("desktop again");
        assert_eq!(report.files_written, 0);
        assert_eq!(report.entries_stored, 0);
        let desktop_log = log_entries();
        use_device(&phone);
        let report = apply_remote_batch(&desktop_log).expect("phone again");
        assert_eq!(report.files_written, 0);
        assert_eq!(report.entries_stored, 0);
    }

    /// The write lock serializes a merge and a request in both directions: a
    /// data-changing request waits while the lock is held, a read does not,
    /// and a merge waits while a request holds it.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn the_write_lock_serializes_a_merge_and_a_request() {
        let _temp = with_temp_env();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, crate::api::router()).await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let wait = std::time::Duration::from_millis(300);

        // Held here as a merge would hold it.
        let merge_holds = write_lock::lock().await;
        let post = tokio::spawn(
            client
                .post(format!("http://{address}/api/credits_debts"))
                .json(&serde_json::json!({"year": 2026, "name": "Loan", "currency": "EUR"}))
                .send(),
        );
        let read = client
            .get(format!("http://{address}/api/years"))
            .send()
            .await
            .expect("a read does not wait for the lock");
        assert!(read.status().is_success());
        tokio::time::sleep(wait).await;
        assert!(!post.is_finished(), "the write waits for the merge");
        assert_eq!(CreditsDebts::new(2026).unwrap().df.height(), 0);
        drop(merge_holds);
        let response = post.await.unwrap().expect("the write runs after the merge");
        assert!(response.status().is_success());
        assert_eq!(CreditsDebts::new(2026).unwrap().df.height(), 1);

        // Held here as a request would hold it.
        let request_holds = write_lock::lock().await;
        let batch = vec![remote(
            far_future(0).ms,
            march(),
            "rent",
            expense_row("rent", "Rent", Value::from("2026-03-01"), 900.0),
        )];
        let merge = tokio::task::spawn_blocking(move || apply_remote_batch(&batch));
        tokio::time::sleep(wait).await;
        assert!(!merge.is_finished(), "the merge waits for the request");
        assert!(!get_monthly_parquet_path(2026, 3).unwrap().exists());
        drop(request_holds);
        merge
            .await
            .unwrap()
            .expect("the merge runs after the request");
        assert_eq!(amounts(&march_expenses()), vec![900.0]);
    }
}
