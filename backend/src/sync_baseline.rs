//! Startup pass that records the rows a data folder already holds in the sync
//! change log, so the log describes the whole current state instead of
//! starting mid history.
//!
//! [`baseline_change_log`] runs before the backend serves requests, after
//! [`crate::row_id_migration::migrate_row_ids`]: a row recorded before it has
//! an ID could not be merged with the same row on another device. It holds
//! the log's single writer lock for the life of the process, so a second
//! program writing the same data folder fails at startup instead of making
//! changes nobody records.
//!
//! # Running once, and the four times it runs again
//!
//! A run leaves a marker file, [`baseline_marker_path`], written and flushed
//! after the last entry. The marker, not the log, is what says this folder
//! has been recorded, and it holds what the run did: how much it recorded and
//! which files it could not read. A non-empty log cannot stand in for it. A
//! run killed part way, which on Android means the system stopping a process
//! that went to the background, leaves a log that is merely short and looks
//! exactly like a finished one, and that folder would then be served, and
//! later synced, missing rows nobody would ever record.
//!
//! So [`baseline_change_log`] records the whole folder again when the marker
//! is missing, and when it is there but cannot be read, which covers a
//! truncated write and a marker from a version this one does not understand.
//! It also records the whole folder again when the marker says entries were
//! written and the log holds nothing at all: deleting the log is not
//! something this app does, but it is the obvious first repair for a corrupt
//! one, and a folder must not be left with a marker and no history. When the
//! marker lists files an earlier run could not read, the run tries those
//! files and only those, because every other row was either recorded then or
//! has been recorded by the data layer since.
//!
//! # Never on a paired device
//!
//! Recording again is only safe while nothing syncs. A device with a sync
//! peer ([`crate::sync_peers::is_paired`]) never records anything here: when
//! any of the four conditions above fires on it, the run records nothing,
//! marks the log incomplete ([`crate::sync::log_incomplete_path`]), and
//! reports the cause in [`ChangeLogBaseline::needs_repair`]. The reason is
//! the stamps: a re-recorded row gets a fresh stamp, which outranks every
//! edit the peer made since this device last merged, and the next sync
//! would silently undo them. The device is repaired through the hub instead
//! (see [`crate::sync_exchange`]). The run still returns `Ok`, because the
//! app must keep serving its data; only sync stops. For a listed file, the
//! run first checks whether the file now yields any row: one that still
//! cannot be read, or reads as empty, records nothing either way and is
//! handled as on an unpaired device.
//!
//! Recording again on an unpaired device is safe, and this is the reason: the baseline records the
//! state the data is in now, which is the newest local truth. A row recorded
//! a second time gets a newer stamp than the copy already in the log and says
//! the same thing, so the merge keeps the newer one and nothing changes. A
//! row the user deleted in between is not in the data any more, so it is
//! never recorded again and the delete stands. The cost of a second run is a
//! longer log, not a wrong one. What this must not do is grow the log on
//! every start, which is why a file that stays unreadable is retried by
//! itself rather than by re-recording the folder around it.
//!
//! It reads every year folder under [`crate::paths::get_dbs_root`] through
//! the loaders in [`crate::df_operations`], not the raw files, so a row it
//! records has the same shape a live edit of that row records: a legacy `"E"`
//! currency is already `"EUR"`, `expense_date` is already a date, and a file
//! without row IDs is refused by the loader rather than by a second copy of
//! that check. Each row of the six tables that carry a
//! [`crate::df_operations::ROW_ID_COLUMN`] becomes one
//! [`ChangeOp::Upsert`], and each income category and month of
//! `cashflow.parquet` becomes one [`ChangeOp::Cell`]. `primaries.parquet` and
//! `secondaries.parquet` are derived summaries, are not among the tables the
//! step 7 design syncs, and get nothing.
//!
//! A file that cannot be read or loaded is skipped: it gets a warning on
//! stderr naming it, is listed in [`ChangeLogBaseline::skipped_paths`] and in
//! the marker, and contributes no entries, so one damaged file costs its own
//! rows and not the whole app. This matches [`crate::row_id_migration`],
//! which skips the same way. Because the marker carries it, a file repaired
//! or restored later is recorded at the next start rather than left out
//! forever. A failed append is different and stops the run, because a log
//! missing entries must not be marked done; the next start finds no marker
//! and records the folder again.
//!
//! The report holds counts and file names, never row contents. A warning
//! holds a file name and the reason it was skipped, and that reason is an
//! error message from this crate, from Polars, or from `serde_json`. The
//! lower two can quote the value they could not handle, so a warning is
//! diagnostic output for the user's own machine and belongs nowhere else.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use polars::prelude::DataFrame;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::df_operations::{
    Cashflow, CreditsDebts, DetailedExpenses, INCOME_CATEGORIES, InvestmentHoldings, Liquidity,
    ROW_ID_COLUMN, RecurringExpenses, has_column, month_labels, row_json, str_col_to_vec,
};
use crate::error::{Error, Result};
use crate::paths::{
    CASHFLOW_FILENAME, CREDITS_DEBTS_FILENAME, INVESTMENTS_FILENAME, INVESTMENTS_PRICES_FILENAME,
    LIQUIDITY_FILENAME, RECURRING_EXPENSES_FILENAME, get_dbs_root, get_monthly_parquet_path,
    get_sync_dir, get_year_summary_path, month_from_parquet_path, year_from_parquet_path,
};
use crate::sync::{self, ChangeLog, ChangeOp, ChangeTable, IncompleteReason};
use crate::sync_peers;

/// The plural ending for `count`, so a report reads as English.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Milliseconds in a day, for reporting how far a stamp runs ahead of the
/// wall clock in a unit a reader can judge.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

const BASELINE_MARKER_FILE_NAME: &str = "baseline_done";

/// Which of the conditions that make [`baseline_change_log`] record the data
/// again fired. On a paired device it records nothing instead, and reports
/// this in [`ChangeLogBaseline::needs_repair`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerunCause {
    /// There is no baseline marker.
    MarkerMissing,
    /// The baseline marker exists and cannot be read.
    MarkerUnreadable,
    /// The marker says rows were recorded and the log holds nothing.
    LogGone,
    /// A file an earlier run could not read now yields rows.
    SkippedFileReadable,
}

impl fmt::Display for RerunCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RerunCause::MarkerMissing => "the record of its first baseline is missing",
            RerunCause::MarkerUnreadable => "the record of its first baseline cannot be read",
            RerunCause::LogGone => "its change log is empty although rows were recorded before",
            RerunCause::SkippedFileReadable => {
                "a data file an earlier start could not read now holds rows the log lacks"
            }
        })
    }
}

/// Counts from one [`baseline_change_log`] run. It holds no row contents, so
/// it is safe to print.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChangeLogBaseline {
    /// Rows recorded as an upsert, across every table and year.
    pub rows_recorded: usize,
    /// Cashflow income cells recorded.
    pub income_cells_recorded: usize,
    /// Files that could not be read or loaded. Each got a warning on stderr
    /// naming it and contributed nothing. They go in the marker, and the next
    /// start tries them again.
    pub skipped_paths: Vec<PathBuf>,
    /// How many files an earlier run had skipped and this run tried again. 0
    /// for a run that recorded the whole folder.
    pub retried_files: usize,
    /// Set when this folder was already recorded and had no skipped file left
    /// to try, so this run recorded nothing. See [`baseline_marker_path`].
    pub already_recorded: bool,
    /// Set when this device has a sync peer and a condition fired that would
    /// have recorded the data again. Nothing was recorded, the log is marked
    /// incomplete, and the device needs a repair through the hub. See the
    /// module docs.
    pub needs_repair: Option<RerunCause>,
    /// From [`crate::sync::LogRead::clock_ahead_of_wall_by`]: how far the
    /// newest stamp in the log runs ahead of this machine's clock, in
    /// milliseconds, when that is more than a day.
    pub clock_ahead_of_wall_by: Option<i64>,
}

impl fmt::Display for ChangeLogBaseline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(cause) = self.needs_repair {
            write!(
                f,
                "Change log: this device has a sync peer and {cause}, so nothing was recorded. \
                 The app works as usual; sync stops until this device is repaired from the \
                 desktop."
            )?;
        } else if self.already_recorded {
            write!(
                f,
                "Change log: already describes this data, so nothing was added."
            )?;
        } else if self.retried_files > 0 {
            write!(
                f,
                "Change log: tried {} file{} again that an earlier run could not read, and \
                 recorded {} rows and {} income cells from {}.",
                self.retried_files,
                plural(self.retried_files),
                self.rows_recorded,
                self.income_cells_recorded,
                if self.retried_files == 1 {
                    "it"
                } else {
                    "them"
                }
            )?;
        } else {
            write!(
                f,
                "Change log: recorded the {} rows and {} income cells this data folder already \
                 held.",
                self.rows_recorded, self.income_cells_recorded
            )?;
        }
        if !self.skipped_paths.is_empty() {
            write!(
                f,
                " {} file{} could not be read and will be tried again at the next start; {}",
                self.skipped_paths.len(),
                plural(self.skipped_paths.len()),
                if self.skipped_paths.len() == 1 {
                    "the warning above names it."
                } else {
                    "the warnings above name them."
                }
            )?;
        }
        if let Some(ahead) = self.clock_ahead_of_wall_by {
            write!(
                f,
                " Changes here are stamped about {} days in the future; see the warning above.",
                ahead / DAY_MS
            )?;
        }
        Ok(())
    }
}

/// Record every row this data folder already holds. See the module docs for
/// the full contract. Needs no network listener, so any program that serves
/// this data, including the Android app, calls it at startup.
///
/// A file that cannot be read is skipped, not an error: see
/// [`ChangeLogBaseline::skipped_paths`]. A paired device that would have to
/// record again is not an error either: see
/// [`ChangeLogBaseline::needs_repair`].
///
/// # Errors
///
/// Returns [`Error::SyncLogLocked`] when another program holds the log, and
/// [`Error::Io`] or [`Error::Json`] when the data folder cannot be listed, a
/// line cannot be written, or the marker cannot be written. The entries
/// written before the failure stay in the log and no marker is left, so the
/// next run records the folder again. The caller must not serve requests
/// after an error.
pub fn baseline_change_log() -> Result<ChangeLogBaseline> {
    // Opening first takes the single writer lock before anything reads the
    // file, so no second program can append between this read and the
    // entries below.
    let log = sync::shared_log()?;
    let read = sync::read_log()?;

    let mut report = ChangeLogBaseline {
        clock_ahead_of_wall_by: read.clock_ahead_of_wall_by,
        ..ChangeLogBaseline::default()
    };
    if let Some(ahead) = read.clock_ahead_of_wall_by {
        eprintln!(
            "Change log: the newest change recorded on this device is dated about {} days from \
             now. A device with a wrong clock wrote that date, and every change made here from \
             now on carries it, so changes will look newer than they are until that date passes. \
             Nothing is lost and the app works as usual.",
            ahead / DAY_MS
        );
    }

    let marker_path = baseline_marker_path()?;
    let paired = paired_for_baseline();
    // What the marker says decides what this run does. See the module docs
    // for each case; the `carried` value is the part of the old marker that
    // still describes entries in the log after this run.
    let carried = match load_marker(&marker_path) {
        MarkerRead::Present(marker) if log_is_gone(&marker, &read) => {
            if paired {
                return Ok(refuse_on_paired_device(report, RerunCause::LogGone));
            }
            eprintln!(
                "Change log: {} holds nothing, but this folder was recorded before. The log was \
                 deleted or replaced, so the data is being recorded again from the start.",
                sync::changelog_path()?.display()
            );
            record_every_year(&log, &mut report)?;
            // The old counts described entries that are gone with the file.
            BaselineMarker::default()
        }
        MarkerRead::Present(marker) if !marker.skipped_paths.is_empty() => {
            let pending = retry_skipped(&marker.skipped_paths, &mut report);
            // Rows found in a listed file are rows the log lacks. On a
            // paired device they cannot be recorded with fresh stamps, and
            // leaving them out would let the peer's older state win.
            if paired && !pending.is_empty() {
                return Ok(refuse_on_paired_device(
                    report,
                    RerunCause::SkippedFileReadable,
                ));
            }
            append_pending(&log, pending, &mut report)?;
            BaselineMarker {
                skipped_paths: Vec::new(),
                ..marker
            }
        }
        MarkerRead::Present(_) => {
            report.already_recorded = true;
            return Ok(report);
        }
        MarkerRead::Missing => {
            if paired {
                return Ok(refuse_on_paired_device(report, RerunCause::MarkerMissing));
            }
            record_every_year(&log, &mut report)?;
            BaselineMarker::default()
        }
        MarkerRead::Unreadable(reason) => {
            if paired {
                return Ok(refuse_on_paired_device(
                    report,
                    RerunCause::MarkerUnreadable,
                ));
            }
            eprintln!(
                "Change log: {} {reason}, so this folder is being recorded again and that file \
                 rewritten.",
                marker_path.display()
            );
            record_every_year(&log, &mut report)?;
            BaselineMarker::default()
        }
    };

    write_baseline_marker(
        &marker_path,
        &BaselineMarker {
            rows_recorded: carried.rows_recorded + report.rows_recorded,
            income_cells_recorded: carried.income_cells_recorded + report.income_cells_recorded,
            skipped_paths: report.skipped_paths.clone(),
        },
    )?;
    Ok(report)
}

/// Whether this device has a sync peer, for the guard above. A peers file
/// that cannot be read counts as paired: it may name a peer, and recording
/// again on a paired device is the one outcome the guard exists to prevent.
fn paired_for_baseline() -> bool {
    match sync_peers::is_paired() {
        Ok(paired) => paired,
        Err(err) => {
            eprintln!(
                "Change log: the sync peers file cannot be read ({err}), so this device is \
                 treated as paired and its data is not recorded again."
            );
            true
        }
    }
}

/// Record nothing on a paired device, mark the log incomplete, and report
/// why. Marking is best effort: [`crate::sync_exchange::log_health`] also
/// reports a missing or unreadable marker and an emptied log by itself, and
/// the warning says when marking failed.
fn refuse_on_paired_device(report: ChangeLogBaseline, cause: RerunCause) -> ChangeLogBaseline {
    let marked = match sync::mark_log_incomplete(IncompleteReason::BaselineRefused) {
        Ok(()) => "The log is marked incomplete.".to_string(),
        Err(err) => format!("Marking the log incomplete failed too ({err})."),
    };
    eprintln!(
        "Change log: this device has a sync peer and {cause}. Recording its data again would \
         outrank the peer's newer edits, so nothing was recorded. {marked} The app works as \
         usual; sync stops until this device is repaired from the desktop."
    );
    ChangeLogBaseline {
        needs_repair: Some(cause),
        clock_ahead_of_wall_by: report.clock_ahead_of_wall_by,
        ..ChangeLogBaseline::default()
    }
}

/// Record every year folder under the data root.
fn record_every_year(log: &ChangeLog, report: &mut ChangeLogBaseline) -> Result<()> {
    for year in years_on_disk(report)? {
        baseline_year(log, year, report)?;
    }
    Ok(())
}

/// Record the whole data folder again as a fresh baseline and write the
/// marker, whatever the marker said before and whether or not the device is
/// paired. Only a hub repair calls this, holding the data write lock, right
/// after it rewrote the log without its damaged lines.
///
/// Every entry gets a fresh stamp above everything the log holds, so the
/// current data outranks every older entry, and the phone then resets from
/// this log. The marker's totals are this run's, because they only need to
/// say whether anything was recorded (see [`log_is_gone`]).
///
/// # Errors
///
/// As for [`baseline_change_log`]. No marker is written after an error.
pub(crate) fn record_fresh_baseline(log: &ChangeLog) -> Result<ChangeLogBaseline> {
    let mut report = ChangeLogBaseline::default();
    record_every_year(log, &mut report)?;
    write_baseline_marker(
        &baseline_marker_path()?,
        &BaselineMarker {
            rows_recorded: report.rows_recorded,
            income_cells_recorded: report.income_cells_recorded,
            skipped_paths: report.skipped_paths.clone(),
        },
    )?;
    Ok(report)
}

/// Write the marker for a folder that a phone reset filled from the hub, so
/// the baseline never runs on it. `entries_stored` is how many entries the
/// reset stored; it stands in for the recorded row count, whose only use is
/// telling a log that was never written from one that went missing.
pub(crate) fn write_marker_after_reset(entries_stored: usize) -> Result<()> {
    write_baseline_marker(
        &baseline_marker_path()?,
        &BaselineMarker {
            rows_recorded: entries_stored,
            income_cells_recorded: 0,
            skipped_paths: Vec::new(),
        },
    )
}

/// Remove the marker, so a reset that stops part way leaves a folder the
/// baseline does not consider recorded. A missing marker is not an error.
pub(crate) fn remove_baseline_marker() -> Result<()> {
    match std::fs::remove_file(baseline_marker_path()?) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// What the marker says about the log, for the health check in
/// [`crate::sync_exchange::log_health`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkerState {
    /// The marker is there, can be read, and the log it describes is too.
    Present,
    /// There is no marker.
    Missing,
    /// The marker exists and cannot be read.
    Unreadable,
    /// The marker says rows were recorded and `read` holds nothing.
    DescribesGoneLog,
}

/// Judge the marker against `read`, the current log, without writing or
/// warning.
pub(crate) fn marker_state(read: &sync::LogRead) -> Result<MarkerState> {
    Ok(match load_marker(&baseline_marker_path()?) {
        MarkerRead::Present(marker) if log_is_gone(&marker, read) => MarkerState::DescribesGoneLog,
        MarkerRead::Present(_) => MarkerState::Present,
        MarkerRead::Missing => MarkerState::Missing,
        MarkerRead::Unreadable(_) => MarkerState::Unreadable,
    })
}

/// What the marker file holds, and what a later start needs from it.
///
/// The counts are totals for the folder, added up over every run that
/// recorded anything, not the counts of the last run. A run that repairs one
/// file must not make the totals say this folder holds one file's worth of
/// rows, because [`log_is_gone`] asks them whether anything was ever
/// recorded.
///
/// Unknown fields are ignored, so a marker from a later version is still
/// read as far as this version understands it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BaselineMarker {
    /// Rows recorded in this folder, over every run.
    rows_recorded: usize,
    /// Cashflow income cells recorded in this folder, over every run.
    income_cells_recorded: usize,
    /// Files no run has managed to read yet. The next start tries these and
    /// nothing else. Absent in a marker written before this field existed,
    /// which reads as an empty list and means the same thing.
    #[serde(default)]
    skipped_paths: Vec<PathBuf>,
}

/// Return `<data_home>/finguard/sync/baseline_done`, creating its directory
/// if necessary.
///
/// The file exists only after a [`baseline_change_log`] run, and it sits
/// beside the log, so deleting one means deleting the other. It holds one
/// line of JSON: how many rows and income cells this folder has recorded, and
/// the files no run could read yet. The next run reads it back, so it is both
/// readable by eye and part of how this decides what to do.
pub fn baseline_marker_path() -> Result<PathBuf> {
    Ok(get_sync_dir()?.join(BASELINE_MARKER_FILE_NAME))
}

/// The marker as a read found it.
enum MarkerRead {
    Present(BaselineMarker),
    Missing,
    /// Why it cannot be read, as a phrase that follows the file name.
    Unreadable(String),
}

/// Read the marker. Writes nothing and warns about nothing.
///
/// A marker that cannot be read, from a write that was cut off or from a
/// version that wrote a different shape, is treated by an unpaired
/// [`baseline_change_log`] as no marker at all: the folder is recorded again
/// and the marker rewritten. That repairs itself and cannot repeat, because
/// the rewrite replaces the file that caused it.
fn load_marker(path: &Path) -> MarkerRead {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return MarkerRead::Missing,
        Err(err) => return MarkerRead::Unreadable(format!("cannot be read ({err})")),
    };
    match serde_json::from_str(&text) {
        Ok(marker) => MarkerRead::Present(marker),
        Err(err) => MarkerRead::Unreadable(format!("does not say what it should ({err})")),
    }
}

/// Whether the log this marker describes has gone missing: the marker says
/// entries were written, and the file now holds nothing at all, not one
/// damaged line and not an unfinished one.
///
/// Nothing in this app deletes the log. It is the obvious first repair for a
/// corrupt one, though, so a folder that loses its log must record itself
/// again rather than serve and sync with no history behind it.
fn log_is_gone(marker: &BaselineMarker, read: &sync::LogRead) -> bool {
    marker.rows_recorded + marker.income_cells_recorded > 0
        && read.entries.is_empty()
        && read.corrupt_lines.is_empty()
        && !read.unfinished_tail
}

/// Collect the rows of the files an earlier run could not read, and nothing
/// else, without recording them.
///
/// Only those can be missing from the log: every other row was recorded by
/// that run or by the data layer since. A file that is no longer there is
/// dropped rather than retried forever, because [`collect_table`] finds
/// nothing in it and only a failed load puts a path back in the list. A file
/// this version does not recognize is kept, so a later version can try it.
fn retry_skipped(paths: &[PathBuf], report: &mut ChangeLogBaseline) -> Vec<PendingChange> {
    report.retried_files = paths.len();
    let mut pending = Vec::new();
    for path in paths {
        match table_at(path) {
            Some((year, table)) => pending.extend(collect_table(year, table, path, report)),
            None => {
                eprintln!(
                    "Change log: an earlier run could not read {}, and this version does not know \
                     that file, so it is kept for a later one to try.",
                    path.display()
                );
                report.skipped_paths.push(path.clone());
            }
        }
    }
    pending
}

/// Write the marker, and flush it.
///
/// Called only after the last entry is on disk, because a marker written
/// earlier would seal a log that is still incomplete. The flush is what makes
/// it worth writing at all: without it, the crash the marker exists for could
/// take the marker with it.
///
/// The file's own bytes are flushed, but the folder entry naming it is not,
/// so a crash in the moment after this can still leave a folder with no
/// marker. That fails the safe way: the next start finds no marker and
/// records the folder again, which the module docs explain is harmless.
fn write_baseline_marker(path: &Path, marker: &BaselineMarker) -> Result<()> {
    let mut file = std::fs::File::create(path)?;
    std::io::Write::write_all(
        &mut file,
        format!("{}\n", serde_json::to_string(marker)?).as_bytes(),
    )?;
    file.sync_all()?;
    Ok(())
}

/// The years that have a folder under the data root, ascending. Mirrors the
/// walk in [`crate::row_id_migration`]: a folder whose name is not a year is
/// not data, and symbolic links are followed.
fn years_on_disk(report: &mut ChangeLogBaseline) -> Result<Vec<i32>> {
    let dbs_root = get_dbs_root()?;
    let mut years = Vec::new();
    for entry in std::fs::read_dir(&dbs_root)? {
        let entry = entry?;
        let Some(year) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => years.push(year),
            Ok(_) => {}
            Err(err) => skip_in_baseline(report, &path, err),
        }
    }
    years.sort_unstable();
    Ok(years)
}

/// One table file inside a year folder.
///
/// A path names exactly one of these, which is what lets a single file be
/// recorded on its own when [`retry_skipped`] comes back to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YearTable {
    /// One month of detailed expenses, by month number.
    Expenses(u32),
    /// The recurring expense templates.
    Recurring,
    /// Investment holdings.
    Investments,
    /// Investment prices.
    InvestmentsPrices,
    /// Liquidity accounts.
    Liquidity,
    /// Credits and debts.
    CreditsDebts,
    /// The income rows of the cashflow table.
    CashflowIncome,
}

/// Every table file a year folder can hold, with its path, in the order a
/// run records them.
fn year_tables(year: i32) -> Result<Vec<(YearTable, PathBuf)>> {
    let mut tables = Vec::new();
    for month in 1..=12u32 {
        tables.push((
            YearTable::Expenses(month),
            get_monthly_parquet_path(year, month)?,
        ));
    }
    for (table, file_name) in [
        (YearTable::Recurring, RECURRING_EXPENSES_FILENAME),
        (YearTable::Investments, INVESTMENTS_FILENAME),
        (YearTable::InvestmentsPrices, INVESTMENTS_PRICES_FILENAME),
        (YearTable::Liquidity, LIQUIDITY_FILENAME),
        (YearTable::CreditsDebts, CREDITS_DEBTS_FILENAME),
        (YearTable::CashflowIncome, CASHFLOW_FILENAME),
    ] {
        tables.push((table, get_year_summary_path(year, file_name)?));
    }
    Ok(tables)
}

/// The year and the table a path names, or `None` when this version does not
/// know that file. A path from the marker is the only caller, so `None` means
/// a marker written by a later version that skipped a file this one has never
/// heard of.
fn table_at(path: &Path) -> Option<(i32, YearTable)> {
    let year = year_from_parquet_path(path).ok()?;
    let table = match path.file_name()?.to_str()? {
        RECURRING_EXPENSES_FILENAME => YearTable::Recurring,
        INVESTMENTS_FILENAME => YearTable::Investments,
        INVESTMENTS_PRICES_FILENAME => YearTable::InvestmentsPrices,
        LIQUIDITY_FILENAME => YearTable::Liquidity,
        CREDITS_DEBTS_FILENAME => YearTable::CreditsDebts,
        CASHFLOW_FILENAME => YearTable::CashflowIncome,
        _ => YearTable::Expenses(month_from_parquet_path(path).ok()?),
    };
    Some((year, table))
}

/// Record every table of one year folder, one file at a time, so memory
/// holds at most one file's entries.
fn baseline_year(log: &ChangeLog, year: i32, report: &mut ChangeLogBaseline) -> Result<()> {
    for (table, path) in year_tables(year)? {
        let pending = collect_table(year, table, &path, report);
        append_pending(log, pending, report)?;
    }
    Ok(())
}

/// One change the baseline is about to record: an upsert of a whole row, or
/// one cashflow income cell.
type PendingChange = (ChangeTable, String, ChangeOp);

/// Append `pending` to the log in order, counting each entry once it is
/// written.
fn append_pending(
    log: &ChangeLog,
    pending: Vec<PendingChange>,
    report: &mut ChangeLogBaseline,
) -> Result<()> {
    for (table, row_id, op) in pending {
        let is_cell = matches!(op, ChangeOp::Cell { .. });
        log.append(table, row_id, op)?;
        if is_cell {
            report.income_cells_recorded += 1;
        } else {
            report.rows_recorded += 1;
        }
    }
    Ok(())
}

/// A table file as its loader returned it.
enum LoadedTable {
    /// A table with a [`ROW_ID_COLUMN`], recorded row by row.
    Rows(ChangeTable, DataFrame),
    /// The cashflow table of this year, whose income cells are recorded.
    Income(i32, DataFrame),
}

/// Load one table file through the data layer's loader.
///
/// The two investment files are loaded separately, once each, although one
/// [`InvestmentHoldings::new`] reads both. That costs a second read of a
/// small file and buys the property the whole retry path needs: one path,
/// one table, one outcome. Their loader fails together, so a damaged
/// `investments.parquet` still skips both, and both are then retried.
///
/// The caller checks that the file is there. That check matters most for
/// cashflow: [`Cashflow::new`] builds a zeroed table in memory for a year
/// that has no `cashflow.parquet`, and those zeros are not data the user
/// entered.
fn load_table(year: i32, table: YearTable) -> Result<LoadedTable> {
    Ok(match table {
        YearTable::Expenses(month) => LoadedTable::Rows(
            ChangeTable::Expenses { year, month },
            DetailedExpenses::new(year, month)?.expense_df,
        ),
        YearTable::Recurring => LoadedTable::Rows(
            ChangeTable::Recurring { year },
            RecurringExpenses::new(year)?.df,
        ),
        YearTable::Investments => LoadedTable::Rows(
            ChangeTable::Investments { year },
            InvestmentHoldings::new(year)?.df,
        ),
        YearTable::InvestmentsPrices => LoadedTable::Rows(
            ChangeTable::InvestmentsPrices { year },
            InvestmentHoldings::new(year)?.df_prices,
        ),
        YearTable::Liquidity => {
            LoadedTable::Rows(ChangeTable::Liquidity { year }, Liquidity::new(year)?.df)
        }
        YearTable::CreditsDebts => LoadedTable::Rows(
            ChangeTable::CreditsDebts { year },
            CreditsDebts::new(year)?.df,
        ),
        YearTable::CashflowIncome => LoadedTable::Income(year, Cashflow::new(year)?.df),
    })
}

/// Every change one table file holds, without recording any: one upsert per
/// row, or for cashflow one cell per income category and month. A file that
/// is not there yields nothing and is not a skip. A file that cannot be
/// loaded, or holds a value the log cannot carry, is skipped with a warning
/// and yields nothing, so it is never recorded by halves.
fn collect_table(
    year: i32,
    table: YearTable,
    path: &Path,
    report: &mut ChangeLogBaseline,
) -> Vec<PendingChange> {
    if !path.exists() {
        return Vec::new();
    }
    let changes = load_table(year, table).and_then(|loaded| match loaded {
        LoadedTable::Rows(table, df) => row_changes(table, &df),
        LoadedTable::Income(year, df) => income_changes(year, &df),
    });
    match changes {
        Ok(changes) => changes,
        Err(err) => {
            skip_in_baseline(report, path, err);
            Vec::new()
        }
    }
}

/// One upsert per row of `df`.
fn row_changes(table: ChangeTable, df: &DataFrame) -> Result<Vec<PendingChange>> {
    Ok(baseline_rows_json(df)?
        .into_iter()
        .map(|(row_id, row)| (table.clone(), row_id, ChangeOp::Upsert { row }))
        .collect())
}

/// Every row of `df` as its row ID and its JSON object (see
/// [`crate::df_operations::row_json`]).
fn baseline_rows_json(df: &DataFrame) -> Result<Vec<(String, Map<String, Value>)>> {
    str_col_to_vec(df, ROW_ID_COLUMN)?
        .into_iter()
        .enumerate()
        .map(|(index, row_id)| Ok((row_id, row_json(df, index)?)))
        .collect()
}

/// One cell per income category and month of `year`'s cashflow table. The
/// derived rows are not recorded: every device recomputes `Income` from
/// these cells, and this branch no longer writes `Spending`, `Saving`, or
/// `Saving %`.
fn income_changes(year: i32, df: &DataFrame) -> Result<Vec<PendingChange>> {
    Ok(income_cells(df)?
        .into_iter()
        .map(|(category, column, value)| {
            (
                ChangeTable::CashflowIncome { year },
                category,
                ChangeOp::Cell { column, value },
            )
        })
        .collect())
}

/// Every `(category, month column, value)` an income row of `df` holds, in
/// category then month order. An empty cell becomes `null`, the same value a
/// cleared cell is recorded with.
fn income_cells(df: &DataFrame) -> Result<Vec<(String, String, Value)>> {
    let categories = str_col_to_vec(df, "category")?;
    let mut cells = Vec::new();
    for (index, category) in categories.iter().enumerate() {
        if !INCOME_CATEGORIES.contains(&category.as_str()) {
            continue;
        }
        for column in month_labels() {
            if !has_column(df, &column) {
                continue;
            }
            let value = match df.column(&column)?.f64()?.get(index) {
                Some(value) => sync::cell_number(value)?,
                None => Value::Null,
            };
            cells.push((category.clone(), column, value));
        }
    }
    Ok(cells)
}

/// What a data folder holds, in counts, for the confirmation a phone shows
/// before a reset deletes its data.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DataCounts {
    /// Rows per table, keyed by the table's name in the change log, such as
    /// `expenses` or `liquidity`, summed over every year. For
    /// `cashflow_income`, the income cells holding a value other than zero.
    pub rows_per_table: BTreeMap<String, usize>,
    /// Year folders under the data root.
    pub year_folders: usize,
    /// Table files that exist and could not be loaded, and so are not
    /// counted in `rows_per_table`.
    pub unreadable_files: usize,
}

/// Count what this data folder holds, through the same loaders the baseline
/// uses. Writes nothing and warns about nothing.
///
/// # Errors
///
/// [`Error::Io`] when the data folder cannot be listed. A file that cannot
/// be loaded is counted in [`DataCounts::unreadable_files`] instead.
pub(crate) fn count_data() -> Result<DataCounts> {
    let mut counts = DataCounts::default();
    let mut unlisted = ChangeLogBaseline::default();
    let years = years_on_disk(&mut unlisted)?;
    counts.year_folders = years.len();
    counts.unreadable_files = unlisted.skipped_paths.len();
    for year in years {
        for (table, path) in year_tables(year)? {
            if !path.exists() {
                continue;
            }
            let counted = load_table(year, table).and_then(|loaded| match loaded {
                LoadedTable::Rows(table, df) => Ok((table_name(&table), df.height())),
                LoadedTable::Income(_, df) => Ok((
                    "cashflow_income",
                    income_cells(&df)?
                        .iter()
                        .filter(|(_, _, value)| {
                            !value.is_null() && value.as_f64().is_none_or(|number| number != 0.0)
                        })
                        .count(),
                )),
            });
            match counted {
                Ok((name, rows)) => {
                    *counts.rows_per_table.entry(name.to_string()).or_default() += rows
                }
                Err(_) => counts.unreadable_files += 1,
            }
        }
    }
    Ok(counts)
}

/// The name a table has in the change log's `table` field.
fn table_name(table: &ChangeTable) -> &'static str {
    match table {
        ChangeTable::Expenses { .. } => "expenses",
        ChangeTable::Recurring { .. } => "recurring",
        ChangeTable::Investments { .. } => "investments",
        ChangeTable::InvestmentsPrices { .. } => "investments_prices",
        ChangeTable::Liquidity { .. } => "liquidity",
        ChangeTable::CreditsDebts { .. } => "credits_debts",
        ChangeTable::CashflowIncome { .. } => "cashflow_income",
    }
}

/// Record that `path` contributed nothing to the baseline, and warn on
/// stderr.
///
/// The warning names the file and the reason it could not be read. That
/// reason is an error message from this crate, from Polars, or from
/// `serde_json`, and the lower two can quote the value they could not handle,
/// so the warning stays on stderr and goes nowhere else. The report itself
/// carries only the path and the counts.
fn skip_in_baseline(report: &mut ChangeLogBaseline, path: &Path, err: impl Into<Error>) {
    eprintln!(
        "Change log: skipped {} while recording what this data folder already holds, because it \
         could not be read ({}). Its rows are not in the log yet, so a sync would not carry them. \
         The next start tries this file again.",
        path.display(),
        err.into()
    );
    report.skipped_paths.push(path.to_path_buf());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use chrono::NaiveDate;
    use polars::prelude::*;

    use crate::paths::PRIMARIES_FILENAME;

    /// Point `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `HOME` (as a guard) at a
    /// fresh temp directory, so a test never touches the user's real data
    /// under `~/.local/share/finguard/`, its change log, or its config under
    /// `~/.config/finguard/`. Matches `df_operations.rs`'s own helper.
    ///
    /// # Safety
    ///
    /// `std::env::set_var` is not thread-safe; callers hold
    /// `#[serial_test::serial]`.
    fn with_temp_data_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            std::env::set_var("HOME", dir.path());
        }
        dir
    }

    /// The marker, or `None` when it is missing or cannot be read.
    fn read_marker(path: &Path) -> Option<BaselineMarker> {
        match load_marker(path) {
            MarkerRead::Present(marker) => Some(marker),
            MarkerRead::Missing | MarkerRead::Unreadable(_) => None,
        }
    }

    /// Every change recorded so far, in the order it was written.
    fn log_entries() -> Vec<sync::ChangeEntry> {
        sync::read_log().expect("read the change log").entries
    }

    /// The row an [`ChangeOp::Upsert`] carries, or a failure naming what the
    /// entry held instead.
    fn upserted_row(op: &ChangeOp) -> &Map<String, Value> {
        match op {
            ChangeOp::Upsert { row } => row,
            other => panic!("expected an upsert, got {other:?}"),
        }
    }

    /// Write `df` at `path`, creating the year folder. A test builds its
    /// files this way rather than through the data layer, which would record
    /// them in the change log and leave the baseline nothing to do.
    fn write_table(df: &DataFrame, path: &Path) {
        std::fs::create_dir_all(path.parent().expect("a file has a folder"))
            .expect("create the year folder");
        let mut df = df.clone();
        let file = std::fs::File::create(path).expect("create the parquet file");
        ParquetWriter::new(file)
            .finish(&mut df)
            .expect("write the parquet file");
    }

    /// Two expense rows as a file already on disk holds them.
    fn expenses_on_disk() -> DataFrame {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let day = (NaiveDate::from_ymd_opt(2026, 3, 4).unwrap() - epoch).num_days() as i32;
        DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Rent", "Tea"]),
            Int32Chunked::from_iter([Some(day), Some(day)])
                .into_series()
                .cast(&DataType::Date)
                .unwrap()
                .with_name("expense_date".into())
                .into(),
            Column::new("expense_amount".into(), &[900.0, 3.0]),
            Column::new("currency".into(), &["EUR", "EUR"]),
            Column::new("primary_category".into(), &["Housing", "Out"]),
            Column::new("secondary_category".into(), &["Rent", "Cafe"]),
            Column::new(ROW_ID_COLUMN.into(), &["rent-id", "tea-id"]),
        ])
        .expect("build the expenses frame")
    }

    /// One liquidity row with zeroed months, as a file already on disk holds
    /// it.
    fn liquidity_on_disk() -> DataFrame {
        let mut columns = vec![
            Column::new("asset_name".into(), &["Main"]),
            Column::new("category".into(), &["Cash"]),
            Column::new("currency".into(), &["EUR"]),
        ];
        for month in month_labels() {
            columns.push(Column::new(month.as_str().into(), &[0.0_f64]));
        }
        columns.push(Column::new(ROW_ID_COLUMN.into(), &["liquidity-id"]));
        DataFrame::new_infer_height(columns).expect("build the liquidity frame")
    }

    /// A cashflow table with one income value set. `Income` and `Spending`
    /// are named here on purpose: they are derived rows the baseline must
    /// leave out.
    fn cashflow_on_disk() -> DataFrame {
        let mut categories: Vec<&str> = INCOME_CATEGORIES.to_vec();
        categories.extend(["Income", "Spending"]);
        let mut columns: Vec<Column> = vec![Column::new("category".into(), &categories)];
        for month in month_labels() {
            let values: Vec<f64> = categories
                .iter()
                .map(|category| match *category {
                    "Salary" if month == "03" => 1_000.0,
                    "Income" if month == "03" => 1_000.0,
                    _ => 0.0,
                })
                .collect();
            columns.push(Column::new(month.as_str().into(), values));
        }
        DataFrame::new_infer_height(columns).expect("build the cashflow frame")
    }

    /// The baseline records every row a data folder already holds, once, and
    /// a second run adds nothing. A phone pairing later builds its first copy
    /// of the data from these entries, so a row missing here is a row that
    /// never reaches it.
    #[test]
    #[serial_test::serial]
    fn the_baseline_records_what_is_already_on_disk_once() {
        let _temp = with_temp_data_home();
        write_table(
            &expenses_on_disk(),
            &get_monthly_parquet_path(2026, 3).expect("march path"),
        );
        write_table(
            &liquidity_on_disk(),
            &get_year_summary_path(2026, LIQUIDITY_FILENAME).expect("liquidity path"),
        );
        write_table(
            &cashflow_on_disk(),
            &get_year_summary_path(2026, CASHFLOW_FILENAME).expect("cashflow path"),
        );
        // A derived summary, which the design leaves out of sync entirely.
        write_table(
            &DataFrame::new_infer_height(vec![
                Column::new("primary_category".into(), &["Housing"]),
                Column::new("01".into(), &[900.0]),
            ])
            .expect("build a primaries frame"),
            &get_year_summary_path(2026, PRIMARIES_FILENAME).expect("primaries path"),
        );

        let report = baseline_change_log().expect("record the baseline");

        assert!(!report.already_recorded);
        assert_eq!(
            report.rows_recorded, 3,
            "two expenses and one liquidity row"
        );
        assert_eq!(
            report.income_cells_recorded,
            INCOME_CATEGORIES.len() * 12,
            "every income category, every month"
        );
        assert!(report.skipped_paths.is_empty());

        let entries = log_entries();
        assert_eq!(entries.len(), 3 + INCOME_CATEGORIES.len() * 12);
        assert_eq!(
            entries[0].table,
            ChangeTable::Expenses {
                year: 2026,
                month: 3
            }
        );
        assert_eq!(entries[0].row_id, "rent-id");
        assert_eq!(
            upserted_row(&entries[0].op)["expense_date"],
            Value::from("2026-03-04")
        );
        assert_eq!(entries[2].table, ChangeTable::Liquidity { year: 2026 });
        assert_eq!(entries[2].row_id, "liquidity-id");
        assert!(
            entries.iter().all(|entry| entry.origin.is_none()),
            "a baseline row is not a generated row"
        );
        // Only the three tables that have a file here appear, so
        // `primaries.parquet`, a derived summary, recorded nothing.
        let tables: HashSet<&ChangeTable> = entries.iter().map(|entry| &entry.table).collect();
        assert_eq!(
            tables,
            HashSet::from([
                &ChangeTable::Expenses {
                    year: 2026,
                    month: 3
                },
                &ChangeTable::Liquidity { year: 2026 },
                &ChangeTable::CashflowIncome { year: 2026 },
            ])
        );
        assert!(
            entries.iter().all(|entry| entry.row_id != "Income"),
            "a derived cashflow row records nothing"
        );
        let salary_march = entries
            .iter()
            .find(|entry| {
                entry.row_id == "Salary"
                    && matches!(&entry.op, ChangeOp::Cell { column, .. } if column == "03")
            })
            .expect("the salary cell for march");
        assert_eq!(
            salary_march.op,
            ChangeOp::Cell {
                column: "03".to_string(),
                value: Value::from(1_000.0),
            }
        );

        let marker = baseline_marker_path().expect("marker path");
        assert!(marker.exists(), "a finished run leaves its marker");

        let second = baseline_change_log().expect("run the baseline again");
        assert!(second.already_recorded);
        assert_eq!(second.rows_recorded, 0);
        assert_eq!(
            log_entries().len(),
            3 + INCOME_CATEGORIES.len() * 12,
            "a second run must not add a second copy"
        );
    }

    /// A run that was cut short leaves lines in the log and no marker, and the
    /// next run records the folder again instead of trusting those lines.
    ///
    /// Without this, one killed start, which on Android is the system
    /// stopping a process that went to the background, would leave rows out
    /// of the log permanently and silently: every later start would see a
    /// non-empty log and report the folder as recorded.
    #[test]
    #[serial_test::serial]
    fn an_interrupted_run_is_recorded_again() {
        let _temp = with_temp_data_home();
        write_table(
            &expenses_on_disk(),
            &get_monthly_parquet_path(2026, 3).expect("march path"),
        );
        // What a run that stopped after its first entry leaves behind: one
        // complete line, and no marker. Its stamp is a second ahead of this
        // machine's clock, close enough not to count as a broken clock, so
        // the assertion below only holds if the new entries recover the
        // clock from this line rather than reading the wall clock alone.
        let ahead = chrono::Utc::now().timestamp_millis() + 1_000;
        let partial = format!(
            "{{\"stamp\":{{\"ms\":{ahead},\"counter\":0}},\"device_id\":\"dev-1\",\
             \"table\":\"expenses\",\"year\":2026,\"month\":3,\"row_id\":\"rent-id\",\
             \"op\":\"delete\"}}\n"
        );
        std::fs::write(
            sync::changelog_path().expect("changelog path"),
            partial.as_bytes(),
        )
        .expect("write the partial log");
        assert!(
            !baseline_marker_path().expect("marker path").exists(),
            "an interrupted run leaves no marker"
        );

        let report = baseline_change_log().expect("record the baseline");

        assert!(
            !report.already_recorded,
            "a log with lines but no marker is not a finished baseline"
        );
        assert_eq!(report.rows_recorded, 2, "both rows are recorded");
        let entries = log_entries();
        assert_eq!(entries.len(), 3, "the partial line is kept, not replaced");
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.row_id == "rent-id")
                .count(),
            2,
            "the row from the partial line is recorded again, with a newer stamp"
        );
        assert!(
            entries[1].stamp > entries[0].stamp && entries[2].stamp > entries[1].stamp,
            "a re-recorded row wins on stamp, which is why re-running is safe"
        );

        let third = baseline_change_log().expect("run the baseline once more");
        assert!(third.already_recorded, "now the marker says it is done");
        assert_eq!(log_entries().len(), 3);
    }

    /// The baseline records a legacy file the way the app reads it, not the
    /// way the bytes sit on disk: `expense_date` stored as a day count comes
    /// out as the day it names, and the pre-2024 `"E"` currency comes out as
    /// `"EUR"`. A live edit of the same row records exactly this, because
    /// both go through the loader and the same encoder, and a receiving
    /// device must not have to guess which of two shapes it was sent.
    #[test]
    #[serial_test::serial]
    fn the_baseline_records_a_legacy_file_the_way_the_app_reads_it() {
        let _temp = with_temp_data_home();
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let day = (NaiveDate::from_ymd_opt(2026, 3, 4).unwrap() - epoch).num_days() as i32;
        let legacy = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Rent"]),
            // An older file stores the date as a plain day count, not a date.
            Column::new("expense_date".into(), &[day]),
            Column::new("expense_amount".into(), &[900.0]),
            Column::new("currency".into(), &["E"]),
            Column::new("primary_category".into(), &["Housing"]),
            Column::new("secondary_category".into(), &["Rent"]),
            Column::new(ROW_ID_COLUMN.into(), &["rent-id"]),
        ])
        .expect("build a legacy expenses frame");
        write_table(
            &legacy,
            &get_monthly_parquet_path(2026, 3).expect("march path"),
        );

        let report = baseline_change_log().expect("record the baseline");

        assert_eq!(report.rows_recorded, 1);
        assert!(report.skipped_paths.is_empty());
        let entries = log_entries();
        let row = upserted_row(&entries[0].op);
        assert_eq!(row["expense_date"], Value::from("2026-03-04"));
        assert_eq!(row["currency"], Value::from("EUR"));
        assert_eq!(row["expense_amount"], Value::from(900.0));
        assert_eq!(row["row_id"], Value::from("rent-id"));
    }

    /// A file the baseline cannot read is skipped with a warning naming it,
    /// and the rest of the folder is still recorded. Failing startup instead
    /// would leave the user with no app at all over one damaged file.
    #[test]
    #[serial_test::serial]
    fn the_baseline_skips_a_file_it_cannot_read() {
        let _temp = with_temp_data_home();
        let damaged = get_monthly_parquet_path(2026, 3).expect("march path");
        write_table(&expenses_on_disk(), &damaged);
        std::fs::write(&damaged, b"not a parquet file").expect("damage the file");
        write_table(
            &liquidity_on_disk(),
            &get_year_summary_path(2026, LIQUIDITY_FILENAME).expect("liquidity path"),
        );

        let report = baseline_change_log().expect("record the baseline");

        assert_eq!(report.skipped_paths, vec![damaged.clone()]);
        assert_eq!(
            report.rows_recorded, 1,
            "the readable table is still recorded"
        );
        let entries = log_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].table, ChangeTable::Liquidity { year: 2026 });
        assert_eq!(
            read_marker(&baseline_marker_path().expect("marker path"))
                .expect("a marker")
                .skipped_paths,
            vec![damaged],
            "the marker carries the file, or nothing will ever come back to it"
        );
    }

    /// A file that could not be read at the first run is recorded once it is
    /// repaired, and the marker stops listing it.
    ///
    /// Without this the marker would say a run finished, which is not the
    /// same as saying it recorded everything, and the rows of a file restored
    /// from a backup would stay outside the log for good, with no warning
    /// after the first one and nothing on a phone but a line in logcat.
    #[test]
    #[serial_test::serial]
    fn a_repaired_file_is_recorded_at_the_next_start() {
        let _temp = with_temp_data_home();
        let damaged = get_monthly_parquet_path(2026, 3).expect("march path");
        write_table(&expenses_on_disk(), &damaged);
        std::fs::write(&damaged, b"not a parquet file").expect("damage the file");
        write_table(
            &liquidity_on_disk(),
            &get_year_summary_path(2026, LIQUIDITY_FILENAME).expect("liquidity path"),
        );
        let first = baseline_change_log().expect("record the baseline");
        assert_eq!(first.rows_recorded, 1);
        assert_eq!(first.skipped_paths, vec![damaged.clone()]);

        write_table(&expenses_on_disk(), &damaged);
        let second = baseline_change_log().expect("run after the repair");

        assert!(
            !second.already_recorded,
            "a folder with a file still to record is not finished"
        );
        assert_eq!(second.retried_files, 1, "only the skipped file is tried");
        assert_eq!(second.rows_recorded, 2, "the repaired file's two rows");
        assert!(second.skipped_paths.is_empty());
        let entries = log_entries();
        assert_eq!(
            entries.len(),
            3,
            "the retry adds the repaired rows and nothing else"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.table
                    == ChangeTable::Expenses {
                        year: 2026,
                        month: 3
                    })
                .count(),
            2
        );
        let marker = read_marker(&baseline_marker_path().expect("marker path")).expect("a marker");
        assert!(
            marker.skipped_paths.is_empty(),
            "a file that now loads stops being listed"
        );
        assert_eq!(
            marker.rows_recorded, 3,
            "the marker counts the whole folder, not the last run"
        );

        let third = baseline_change_log().expect("run once more");
        assert!(third.already_recorded, "nothing is left to try");
        assert_eq!(log_entries().len(), 3);
    }

    /// A folder whose log is gone, marker and all its data still in place,
    /// records itself again instead of serving with no history.
    ///
    /// Nothing in the app deletes the log. Deleting it is the obvious first
    /// repair for a corrupt one, though, and part 2 still owes that policy,
    /// so this has to survive it.
    #[test]
    #[serial_test::serial]
    fn a_marker_whose_log_vanished_records_the_folder_again() {
        let _temp = with_temp_data_home();
        write_table(
            &expenses_on_disk(),
            &get_monthly_parquet_path(2026, 3).expect("march path"),
        );
        write_baseline_marker(
            &baseline_marker_path().expect("marker path"),
            &BaselineMarker {
                rows_recorded: 2,
                income_cells_recorded: 0,
                skipped_paths: Vec::new(),
            },
        )
        .expect("write the marker of a run whose log was later deleted");
        assert!(log_entries().is_empty(), "the log is gone");

        let report = baseline_change_log().expect("record the baseline");

        assert!(
            !report.already_recorded,
            "a marker with no log behind it does not describe this data"
        );
        assert_eq!(report.rows_recorded, 2);
        assert_eq!(log_entries().len(), 2);
        assert_eq!(
            read_marker(&baseline_marker_path().expect("marker path"))
                .expect("a marker")
                .rows_recorded,
            2,
            "the totals start again from what the log now holds"
        );

        let again = baseline_change_log().expect("run once more");
        assert!(again.already_recorded, "and it settles");
        assert_eq!(log_entries().len(), 2);
    }

    /// A marker this version cannot read is treated as no marker: the folder
    /// is recorded again and the marker rewritten, which repairs itself and
    /// cannot repeat.
    #[test]
    #[serial_test::serial]
    fn an_unreadable_marker_records_the_folder_again_and_is_rewritten() {
        let _temp = with_temp_data_home();
        write_table(
            &expenses_on_disk(),
            &get_monthly_parquet_path(2026, 3).expect("march path"),
        );
        let marker_path = baseline_marker_path().expect("marker path");
        std::fs::write(&marker_path, b"{\"rows_recorded\":")
            .expect("leave a marker cut off mid write");

        let report = baseline_change_log().expect("record the baseline");

        assert!(!report.already_recorded);
        assert_eq!(report.rows_recorded, 2);
        assert_eq!(log_entries().len(), 2);
        assert_eq!(
            read_marker(&marker_path)
                .expect("the marker reads back now")
                .rows_recorded,
            2
        );

        let again = baseline_change_log().expect("run once more");
        assert!(
            again.already_recorded,
            "the rewrite is what stops this repeating at every start"
        );
        assert_eq!(log_entries().len(), 2);
    }

    /// Pair this device with a hub, as part 3b's pairing will.
    fn pair_with_a_hub() {
        crate::sync_peers::record_peer(crate::sync_peers::PeerRecord::new(
            "hub-device",
            crate::sync_peers::PeerRole::Hub,
        ))
        .expect("record a peer");
    }

    /// A paired device where `cause` fired recorded nothing, marked its log
    /// incomplete, reports the cause, and its log is unreliable.
    fn assert_refused(report: &ChangeLogBaseline, cause: RerunCause, entries_before: usize) {
        assert_eq!(report.needs_repair, Some(cause));
        assert_eq!(report.rows_recorded + report.income_cells_recorded, 0);
        assert!(!report.already_recorded);
        assert_eq!(log_entries().len(), entries_before, "nothing was recorded");
        assert_eq!(
            sync::read_log_incomplete().expect("read the incomplete marker"),
            Some(vec![IncompleteReason::BaselineRefused])
        );
        assert!(
            !crate::sync_exchange::log_health()
                .expect("judge the log")
                .is_reliable()
        );
        assert!(report.to_string().contains("sync peer"), "{report}");
    }

    /// With no marker, a paired device records nothing, at every start.
    #[test]
    #[serial_test::serial]
    fn a_paired_device_without_a_marker_records_nothing() {
        let _temp = with_temp_data_home();
        write_table(
            &expenses_on_disk(),
            &get_monthly_parquet_path(2026, 3).expect("march path"),
        );
        pair_with_a_hub();

        let report = baseline_change_log().expect("startup keeps going");
        assert_refused(&report, RerunCause::MarkerMissing, 0);
        assert!(!baseline_marker_path().expect("marker path").exists());

        let again = baseline_change_log().expect("startup keeps going");
        assert_refused(&again, RerunCause::MarkerMissing, 0);
    }

    /// With a marker it cannot read, a paired device records nothing and
    /// leaves the marker as it was.
    #[test]
    #[serial_test::serial]
    fn a_paired_device_with_an_unreadable_marker_records_nothing() {
        let _temp = with_temp_data_home();
        write_table(
            &expenses_on_disk(),
            &get_monthly_parquet_path(2026, 3).expect("march path"),
        );
        let marker_path = baseline_marker_path().expect("marker path");
        std::fs::write(&marker_path, b"{\"rows_recorded\":").expect("damage the marker");
        pair_with_a_hub();

        let report = baseline_change_log().expect("startup keeps going");

        assert_refused(&report, RerunCause::MarkerUnreadable, 0);
        assert_eq!(
            std::fs::read(&marker_path).expect("read the marker"),
            b"{\"rows_recorded\":"
        );
    }

    /// With a marker whose log has gone, a paired device records nothing.
    #[test]
    #[serial_test::serial]
    fn a_paired_device_whose_log_vanished_records_nothing() {
        let _temp = with_temp_data_home();
        write_table(
            &expenses_on_disk(),
            &get_monthly_parquet_path(2026, 3).expect("march path"),
        );
        write_baseline_marker(
            &baseline_marker_path().expect("marker path"),
            &BaselineMarker {
                rows_recorded: 2,
                income_cells_recorded: 0,
                skipped_paths: Vec::new(),
            },
        )
        .expect("write the marker of a run whose log was later deleted");
        pair_with_a_hub();

        let report = baseline_change_log().expect("startup keeps going");

        assert_refused(&report, RerunCause::LogGone, 0);
    }

    /// A file skipped before pairing and repaired after it holds rows the
    /// log lacks, and a paired device records none of them.
    #[test]
    #[serial_test::serial]
    fn a_paired_device_does_not_record_a_repaired_file() {
        let _temp = with_temp_data_home();
        let damaged = get_monthly_parquet_path(2026, 3).expect("march path");
        write_table(&expenses_on_disk(), &damaged);
        std::fs::write(&damaged, b"not a parquet file").expect("damage the file");
        write_table(
            &liquidity_on_disk(),
            &get_year_summary_path(2026, LIQUIDITY_FILENAME).expect("liquidity path"),
        );
        let first = baseline_change_log().expect("record the baseline");
        assert_eq!(first.skipped_paths, vec![damaged.clone()]);
        pair_with_a_hub();
        write_table(&expenses_on_disk(), &damaged);

        let report = baseline_change_log().expect("startup keeps going");

        assert_refused(&report, RerunCause::SkippedFileReadable, 1);
        assert_eq!(
            read_marker(&baseline_marker_path().expect("marker path"))
                .expect("a marker")
                .skipped_paths,
            vec![damaged],
            "the marker still lists the file"
        );
    }

    /// A listed file that still cannot be read records nothing on any
    /// device, so a paired device retries it as usual and needs no repair.
    #[test]
    #[serial_test::serial]
    fn a_paired_device_retries_a_file_that_is_still_damaged() {
        let _temp = with_temp_data_home();
        let damaged = get_monthly_parquet_path(2026, 3).expect("march path");
        write_table(&expenses_on_disk(), &damaged);
        std::fs::write(&damaged, b"not a parquet file").expect("damage the file");
        baseline_change_log().expect("record the baseline");
        pair_with_a_hub();

        let report = baseline_change_log().expect("startup keeps going");

        assert_eq!(report.needs_repair, None);
        assert_eq!(report.retried_files, 1);
        assert_eq!(report.skipped_paths, vec![damaged]);
        assert_eq!(sync::read_log_incomplete().expect("read the marker"), None);
    }

    /// A peers file that cannot be read may name a peer, so the baseline
    /// treats the device as paired and records nothing.
    #[test]
    #[serial_test::serial]
    fn an_unreadable_peers_file_counts_as_paired() {
        let _temp = with_temp_data_home();
        write_table(
            &expenses_on_disk(),
            &get_monthly_parquet_path(2026, 3).expect("march path"),
        );
        std::fs::write(
            crate::sync_peers::sync_peers_path().expect("peers path"),
            b"{not json",
        )
        .expect("damage the peers file");

        let report = baseline_change_log().expect("startup keeps going");

        assert_refused(&report, RerunCause::MarkerMissing, 0);
    }
}
