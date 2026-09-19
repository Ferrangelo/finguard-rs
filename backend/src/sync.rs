//! The sync change log: what changed, on which device, and in what order.
//!
//! Every change to a synced table appends one JSON object, on one line, to
//! `<data_home>/finguard/sync/changelog.jsonl`. Appending is the only write
//! in ordinary use, so the file stays readable by eye and a failure can damage
//! at most the line being written. Two sync repairs in
//! [`crate::sync_exchange`] also change the file, both through the open
//! handle: a phone reset empties it (`ChangeLog::empty_for_reset`) and a hub
//! repair drops its damaged lines (`ChangeLog::rewrite_readable`).
//! Nothing in this module reads or writes a Parquet file: a
//! later part calls [`ChangeLog::append`] from the data layer, and a later step
//! ships the entries between devices.
//!
//! The log sits beside the data in [`get_sync_dir`], not inside `dbs/`, so the
//! year folder walkers and the backup copier never see it.
//!
//! # One stamp, one change
//!
//! Everything here serves one property: a `(stamp, device id)` pair names
//! exactly one change, forever. Three rules keep it.
//!
//! - [`ChangeLog::open`] recovers the clock from the largest stamp anywhere in
//!   the file, taken with a probe that reads the `stamp` field and ignores
//!   every other field. A line this version cannot decode, because a newer
//!   version wrote a table, an operation, or an origin it does not know, still
//!   moves the clock. Recovering from the last line this version can fully
//!   read would reissue that line's stamp.
//! - One process at a time may append. [`ChangeLog::open`] takes an exclusive
//!   advisory lock on the file and fails while another handle holds it, so the
//!   backend and `seed_mock` cannot both recover the same stamp and then issue
//!   it twice. The lock is per open handle and the operating system drops it
//!   when the process ends, so a crash leaves nothing to clean up.
//! - A stamp is consumed before its line is written, so a failed write never
//!   hands the same stamp to the next change.
//!
//! The on-disk shape of `stamp` is frozen, because that recovery depends on
//! it: a JSON object under the key `stamp`, holding the integers `ms` and
//! `counter`. A later clock change adds a field inside `stamp`, which this
//! version already ignores, and never renames or retypes those two. Renaming
//! them would make every older line invisible to the clock and start handing
//! out stamps that are already in the file. It is the one format change this
//! design cannot survive.
//!
//! # Ordering
//!
//! Entries carry a hybrid logical clock stamp ([`Hlc`]) rather than a plain
//! wall clock reading. A device whose clock is hours fast or slow would
//! otherwise win or lose every conflict until someone fixed the clock.
//! [`Hlc::next`] is the send half: the stamp advances by at least one counter
//! step per change even when the wall clock stands still or jumps backwards.
//! [`Hlc::observe`] is the receive half: after seeing a stamp from another
//! device, this device's clock passes it, so a device stuck in 2099 cannot win
//! every conflict until 2099. [`ChangeEntry::order_key`] breaks a tie between
//! two devices by device id, so a change from any device has exactly one place
//! in the total order.
//!
//! # Identity
//!
//! The device id lives in the **config** directory, not beside the data. A
//! copied data folder must not clone a device identity: two devices claiming
//! the same id would hand out colliding stamps and merge each other's changes
//! as their own.
//!
//! # Damaged lines
//!
//! A read never fails on the contents of a line. [`read_log`] reports what it
//! could not use instead, and the caller decides:
//!
//! - A final line with no terminating newline is an append that did not
//!   finish, which is what a crash or a full disk leaves. It records no change
//!   and costs nothing. [`LogRead::unfinished_tail`] marks it, and the next
//!   append closes it with a newline before writing, so the two lines do not
//!   run together.
//! - Any other line that cannot be read is corruption, and the change it held
//!   is lost. Its number goes in [`LogRead::corrupt_lines`]. A lost `delete`
//!   never reaches the other device, so part 2 is expected to refuse to sync a
//!   log that reports corruption. This module does not refuse anything: part
//!   1b must be able to open a damaged log and keep the app working.
//!
//! A line that is not UTF-8, or longer than [`MAX_LINE_BYTES`], is damaged in
//! the same way as a line that is not JSON. The warnings name the file, the
//! line number, and the error category. They never contain the line's
//! contents, because a line holds row values.
//!
//! # Cost
//!
//! [`ChangeLog::open`] and [`read_log`] both read the whole file, and part 1b
//! records every existing row as a baseline, so the log is large from the
//! first run. Startup therefore grows with the log. Compaction and a shorter
//! tail read are later work, not part of this format.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{CurrentMonthRateMode, SUPPORTED_REFERENCE_CURRENCIES, get_config_dir};
use crate::error::{Error, Result};
use crate::paths::get_sync_dir;

const CHANGELOG_FILE_NAME: &str = "changelog.jsonl";
const DEVICE_ID_FILE_NAME: &str = "device_id";
const LOG_INCOMPLETE_FILE_NAME: &str = "log_incomplete";

/// The longest line a read will hold in memory. A corrupt run of bytes with no
/// newline in it must not make the reader allocate the rest of the file. An
/// entry is a few hundred bytes, and a whole row upsert is a few thousand, so
/// this leaves room for a table far wider than any of ours.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// How far the newest stamp may run ahead of the wall clock before a read
/// reports it in [`LogRead::clock_ahead_of_wall_by`]. A day is far more than
/// the gap two working clocks and a slow sync can produce, and far less than
/// the jump a broken clock or a wrong unit makes.
pub const CLOCK_AHEAD_LIMIT_MS: i64 = 24 * 60 * 60 * 1000;

// ------------------------------------------------------------------
// Clock
// ------------------------------------------------------------------

/// A hybrid logical clock stamp: a wall clock reading in milliseconds since
/// the Unix epoch, plus a counter that separates changes made inside the same
/// millisecond or while the wall clock is not moving forwards.
///
/// The field order is the comparison order, because [`Ord`] is derived.
/// Reordering the fields silently changes how every entry sorts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hlc {
    /// Milliseconds since the Unix epoch, never smaller than the previous
    /// stamp's value even when the wall clock moves backwards.
    pub ms: i64,
    /// Number of stamps already issued for this `ms` value.
    pub counter: u32,
}

impl Hlc {
    /// Return the next stamp after `last`, reading the wall clock. This is the
    /// send half of the clock.
    ///
    /// `last` is `None` only for the first stamp of an empty log. The result
    /// is always greater than `last`.
    pub fn next(last: Option<Hlc>) -> Hlc {
        Hlc::after(last, wall_clock_ms())
    }

    /// Return a stamp greater than both `last` and `remote`, reading the wall
    /// clock. This is the receive half: call it for a stamp that arrived from
    /// another device, before issuing the next local stamp.
    ///
    /// Without it, local stamps only track the local clock, and a device whose
    /// clock reads years ahead wins every conflict until the other clocks
    /// catch up. Use [`ChangeLog::observe_remote`] to apply this to an open
    /// log, or [`ChangeLog::append_remote_batch`] to store remote entries and
    /// observe their stamps in one step: this function only computes the
    /// stamp.
    pub fn observe(last: Option<Hlc>, remote: Hlc) -> Hlc {
        Hlc::observed_at(last, remote, wall_clock_ms())
    }

    /// [`Hlc::next`] with the wall clock reading supplied, so a test can move
    /// the clock forwards, hold it still, or push it backwards.
    fn after(last: Option<Hlc>, wall_ms: i64) -> Hlc {
        let Some(last) = last else {
            return Hlc {
                ms: wall_ms,
                counter: 0,
            };
        };
        if wall_ms > last.ms {
            return Hlc {
                ms: wall_ms,
                counter: 0,
            };
        }
        bump(last.ms, last.counter)
    }

    /// [`Hlc::observe`] with the wall clock reading supplied.
    fn observed_at(last: Option<Hlc>, remote: Hlc, wall_ms: i64) -> Hlc {
        // An empty log is older than any stamp that can exist.
        let last = last.unwrap_or(Hlc {
            ms: i64::MIN,
            counter: 0,
        });
        // The wall clock only resets the counter when it has passed both
        // stamps. Otherwise the result keeps the larger millisecond and steps
        // past the counter that goes with it.
        if wall_ms > last.ms && wall_ms > remote.ms {
            return Hlc {
                ms: wall_ms,
                counter: 0,
            };
        }
        if last.ms == remote.ms {
            return bump(last.ms, last.counter.max(remote.counter));
        }
        if last.ms > remote.ms {
            return bump(last.ms, last.counter);
        }
        bump(remote.ms, remote.counter)
    }
}

/// The stamp after `(ms, counter)` without moving the wall clock reading.
fn bump(ms: i64, counter: u32) -> Hlc {
    match counter.checked_add(1) {
        Some(counter) => Hlc { ms, counter },
        // Four billion changes inside one millisecond is not reachable in
        // practice. Borrowing the next millisecond still beats wrapping the
        // counter to zero, which would reissue a used stamp. `ms` can come
        // from a line in the file, so it can be anything an i64 holds:
        // saturating keeps a hostile value from panicking here.
        None => Hlc {
            ms: ms.saturating_add(1),
            counter: 0,
        },
    }
}

/// Milliseconds since the Unix epoch.
fn wall_clock_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ------------------------------------------------------------------
// Device identity
// ------------------------------------------------------------------

/// Return the path of the device id file, creating its directory if
/// necessary.
pub fn device_id_path() -> Result<PathBuf> {
    Ok(get_config_dir()?.join(DEVICE_ID_FILE_NAME))
}

/// Return this device's id, writing a fresh UUID v4 on the first call.
///
/// Every later call returns the stored value, on this run and on every run
/// afterwards. The file lives in the config directory on purpose: see the
/// module docs. A new file and its directory entry are flushed to disk before
/// this returns, so a crash straight after the first call cannot lose the
/// identity and let the device mint a second one.
pub fn device_id() -> Result<String> {
    let path = device_id_path()?;
    if let Some(id) = read_device_id(&path)? {
        return Ok(id);
    }

    let fresh = uuid::Uuid::new_v4().to_string();
    // `create_new` loses the race to another process that is creating the
    // file at the same moment, instead of overwriting the id it wrote.
    match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(mut file) => {
            file.write_all(fresh.as_bytes())?;
            file.sync_all()?;
            sync_parent_dir(&path);
            Ok(fresh)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            match read_device_id(&path)? {
                Some(id) => Ok(id),
                // The file exists but holds no id, so a previous run was
                // interrupted between creating and writing it. No device has
                // ever used that empty file as an identity, so filling it now
                // cannot collide with anything.
                None => {
                    let mut file = OpenOptions::new().write(true).truncate(true).open(&path)?;
                    file.write_all(fresh.as_bytes())?;
                    file.sync_all()?;
                    Ok(fresh)
                }
            }
        }
        Err(err) => Err(err.into()),
    }
}

/// Read the stored device id, or `None` when the file is absent or holds no
/// non-blank text.
fn read_device_id(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let id = contents.trim();
            Ok((!id.is_empty()).then(|| id.to_string()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Flush the entries of the folder holding `path`, so a file just created in
/// it survives a crash. `sync_all` on the file makes its contents durable but
/// not its name. Only Unix can open a folder to flush it, so elsewhere this
/// does nothing.
///
/// Best effort: a failure is warned about, not returned. This narrows the
/// window in which a crash loses a brand new file, and nothing here is wrong
/// without it. On a filesystem that refuses to flush a directory, returning
/// the error would instead stop the log from opening at all and leave the app
/// unable to record anything.
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(folder) = path.parent()
        && let Err(err) = File::open(folder).and_then(|folder| folder.sync_all())
    {
        eprintln!(
            "Change log: could not flush the folder {} ({err}). The file is written; a crash in \
             the next moment could still lose it.",
            folder.display()
        );
    }
    #[cfg(not(unix))]
    let _ = path;
}

// ------------------------------------------------------------------
// Entries
// ------------------------------------------------------------------

/// Which table a change belongs to, and where that table's file is.
///
/// Parquet variants carry a year, because those tables are stored under
/// `<dbs_root>/<year>/`. Settings are config files and therefore carry no year.
///
/// The variants match the files on disk. Adding one also needs a case
/// wherever the data layer maps a table to a change, and a round trip test
/// below.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "table", rename_all = "snake_case")]
pub enum ChangeTable {
    /// One month of detailed expenses.
    Expenses {
        /// Year folder holding the monthly file.
        year: i32,
        /// Month number, 1 to 12.
        month: u32,
    },
    /// The recurring expense templates.
    Recurring {
        /// Year folder holding the file.
        year: i32,
    },
    /// Investment holdings.
    Investments {
        /// Year folder holding the file.
        year: i32,
    },
    /// Investment prices.
    InvestmentsPrices {
        /// Year folder holding the file.
        year: i32,
    },
    /// Liquidity accounts.
    Liquidity {
        /// Year folder holding the file.
        year: i32,
    },
    /// Credits and debts.
    CreditsDebts {
        /// Year folder holding the file.
        year: i32,
    },
    /// The income rows of the cashflow table. Only the income cells of
    /// `cashflow.parquet` are edited by hand; every other row of that file is
    /// recomputed from the expenses.
    CashflowIncome {
        /// Year folder holding the file.
        year: i32,
    },
    CategoryMappings,
    KnownCategories,
    CurrencySettings,
}

impl ChangeTable {
    /// The year folder this table's file lives in.
    pub fn year(&self) -> Option<i32> {
        match self {
            ChangeTable::Expenses { year, .. }
            | ChangeTable::Recurring { year }
            | ChangeTable::Investments { year }
            | ChangeTable::InvestmentsPrices { year }
            | ChangeTable::Liquidity { year }
            | ChangeTable::CreditsDebts { year }
            | ChangeTable::CashflowIncome { year } => Some(*year),
            ChangeTable::CategoryMappings
            | ChangeTable::KnownCategories
            | ChangeTable::CurrencySettings => None,
        }
    }

    /// Reject a location no file can have, so a bad month is caught when it is
    /// written and when it is read, not when the data layer asks
    /// [`crate::paths::get_monthly_parquet_path`] for a file it refuses to
    /// name.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::InvalidArgument`] when the month is outside 1
    /// to 12.
    pub fn validate(&self) -> Result<()> {
        if let ChangeTable::Expenses { month, .. } = self
            && !(1..=12).contains(month)
        {
            return Err(Error::InvalidArgument(format!(
                "month must be between 1 and 12, got {month}"
            )));
        }
        Ok(())
    }
}

impl ChangeEntry {
    pub fn validate(&self) -> Result<()> {
        self.table.validate()?;
        match &self.table {
            ChangeTable::CategoryMappings => {
                if self.row_id.is_empty() || self.row_id != self.row_id.to_lowercase() {
                    return Err(Error::InvalidArgument(
                        "mapping row id must be a lower-case expense name".into(),
                    ));
                }
                if let ChangeOp::Upsert { row } = &self.op {
                    for field in ["primary_category", "secondary_category"] {
                        if !row.get(field).is_some_and(Value::is_string) {
                            return Err(Error::InvalidArgument(format!(
                                "mapping upsert must contain string field {field}"
                            )));
                        }
                    }
                } else if !matches!(self.op, ChangeOp::Delete) {
                    return Err(Error::InvalidArgument(
                        "mapping changes must be upserts or deletes".into(),
                    ));
                }
            }
            ChangeTable::KnownCategories => {
                if !(self.row_id.starts_with("primary:") || self.row_id.starts_with("secondary:"))
                    || self.row_id.len() <= self.row_id.find(':').unwrap_or(usize::MAX) + 1
                {
                    return Err(Error::InvalidArgument(
                        "known category row id must be primary:<name> or secondary:<name>".into(),
                    ));
                }
                if !matches!(self.op, ChangeOp::Upsert { ref row } if row.is_empty())
                    && !matches!(self.op, ChangeOp::Delete)
                {
                    return Err(Error::InvalidArgument(
                        "known category changes must be empty upserts or deletes".into(),
                    ));
                }
            }
            ChangeTable::CurrencySettings => {
                if !matches!(
                    self.row_id.as_str(),
                    "reference_currency" | "current_month_rate_mode"
                ) {
                    return Err(Error::InvalidArgument(
                        "unknown currency setting field".into(),
                    ));
                }
                match &self.op {
                    ChangeOp::Cell { column, value } if column == "value" => {
                        validate_currency_value(&self.row_id, value)?;
                    }
                    ChangeOp::Upsert { row } => {
                        let value = row.get("value").ok_or_else(|| {
                            Error::InvalidArgument("currency upsert must contain value".into())
                        })?;
                        validate_currency_value(&self.row_id, value)?;
                    }
                    ChangeOp::Delete => {
                        return Err(Error::InvalidArgument(
                            "currency settings do not support delete".into(),
                        ));
                    }
                    _ => {
                        return Err(Error::InvalidArgument(
                            "currency changes must use value cells".into(),
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn validate_currency_value(field: &str, value: &Value) -> Result<()> {
    let text = value
        .as_str()
        .ok_or_else(|| Error::InvalidArgument("currency setting value must be a string".into()))?;
    if field == "reference_currency" && !SUPPORTED_REFERENCE_CURRENCIES.contains(&text) {
        return Err(Error::InvalidArgument(
            "unsupported reference currency".into(),
        ));
    }
    if field == "current_month_rate_mode"
        && serde_json::from_value::<CurrentMonthRateMode>(value.clone()).is_err()
    {
        return Err(Error::InvalidArgument(
            "invalid current month rate mode".into(),
        ));
    }
    Ok(())
}

/// What happened to the row.
///
/// Expenses and recurring templates replace the whole row, because their
/// fields are edited together as one form. The net worth and income tables
/// hold a value per month in one wide row, so they merge per cell instead:
/// two devices filling in different months of the same asset must not
/// overwrite each other.
///
/// Build every number in a row or a cell with [`cell_number`]. JSON has no NaN
/// and no infinity, and `serde_json` turns both into `null`, which the other
/// device would apply as a cleared cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ChangeOp {
    /// The row exists with exactly these columns and values.
    Upsert {
        /// Every column of the row, including `row_id`.
        row: Map<String, Value>,
    },
    /// The row is gone.
    Delete,
    /// One column of the row holds this value. Other columns are untouched.
    Cell {
        /// Column name as it appears in the Parquet file.
        column: String,
        /// The new value, `null` when the cell was cleared.
        value: Value,
    },
}

/// Turn a number from a table cell into a JSON value.
///
/// # Errors
///
/// Returns [`crate::Error::InvalidArgument`] when `value` is NaN or infinite.
/// JSON cannot carry either one. The check has to happen here, on the `f64`,
/// because `serde_json` has already lost the distinction by the time the
/// number is a [`Value`]: `Value::from(f64::NAN)` is `Value::Null`, which the
/// merge engine would read as a cell the user cleared.
pub fn cell_number(value: f64) -> Result<Value> {
    if !value.is_finite() {
        return Err(Error::InvalidArgument(format!(
            "a change log value must be a finite number, got {value}"
        )));
    }
    Ok(Value::from(value))
}

/// What produced a change, when it was not a plain user action.
///
/// Stored as a string, so an unknown value from a newer version survives a
/// read and a rewrite as [`ChangeOrigin::Unknown`] instead of failing the
/// line. Do not match on `Unknown` as if it were an origin of its own: it is
/// whatever string the writer used.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ChangeOrigin {
    /// The row was generated by `/api/recurring/apply` from a recurring
    /// template, not typed by the user.
    RecurringApply,
    /// An origin this version does not know, kept as written.
    Unknown(String),
}

/// The stored form of [`ChangeOrigin::RecurringApply`].
const RECURRING_APPLY_ORIGIN: &str = "recurring_apply";

impl From<String> for ChangeOrigin {
    fn from(value: String) -> Self {
        match value.as_str() {
            RECURRING_APPLY_ORIGIN => ChangeOrigin::RecurringApply,
            _ => ChangeOrigin::Unknown(value),
        }
    }
}

impl From<ChangeOrigin> for String {
    fn from(origin: ChangeOrigin) -> Self {
        match origin {
            ChangeOrigin::RecurringApply => RECURRING_APPLY_ORIGIN.to_string(),
            ChangeOrigin::Unknown(value) => value,
        }
    }
}

/// One change, written as one line of the log.
///
/// The table and the operation are flattened into the top level object, so a
/// line reads as one flat record:
///
/// ```json
/// {"stamp":{"ms":1758067200000,"counter":0},"device_id":"6f1c…","table":"liquidity","year":2026,"row_id":"3a…","op":"cell","column":"03","value":120.5}
/// ```
///
/// Both are tagged by a field name (`table` and `op`), so a reader that meets
/// an unknown tag fails only on that line, and a field added later is ignored
/// by an older reader instead of breaking it. The example is an ordinary user
/// action, which is why it carries no `origin`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeEntry {
    /// When the change happened, in the ordering the module docs describe.
    pub stamp: Hlc,
    /// The device that made the change.
    pub device_id: String,
    /// The table and its location on disk.
    #[serde(flatten)]
    pub table: ChangeTable,
    /// The `row_id` value of the changed row.
    pub row_id: String,
    /// What produced the change. Absent, both here and on the line, for an
    /// ordinary user action, which is nearly every entry.
    ///
    /// Part 2, the merge engine, owns the rule this field exists for: an
    /// upsert with [`ChangeOrigin::RecurringApply`] never overrides a delete
    /// of the same row id, whatever the stamps say, so a generated row the
    /// user removed does not come back next month. A generated row and a hand
    /// typed one share a row id, so only this field tells them apart, and
    /// nothing can recover it after the entry is written. No code enforces
    /// the rule yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ChangeOrigin>,
    /// What happened to the row.
    #[serde(flatten)]
    pub op: ChangeOp,
}

impl ChangeEntry {
    /// Key that puts changes from every device in one total order: `ms`, then
    /// `counter`, then the device id. Two devices can issue the same stamp,
    /// but never the same key, so the order does not depend on which device
    /// merged first.
    pub fn order_key(&self) -> (Hlc, &str) {
        (self.stamp, self.device_id.as_str())
    }
}

/// Just enough of a line to move the clock.
///
/// Every other field is ignored, so a line written by a newer version, with a
/// table, an operation, or an origin this version cannot decode, still counts
/// towards [`LogRead::max_stamp`]. Without that, an older build running after
/// a newer one would reissue stamps the newer one already used.
#[derive(Deserialize)]
struct StampProbe {
    stamp: Hlc,
}

// ------------------------------------------------------------------
// The log file
// ------------------------------------------------------------------

/// Return `<data_home>/finguard/sync/changelog.jsonl`, creating its directory
/// if necessary.
pub fn changelog_path() -> Result<PathBuf> {
    Ok(get_sync_dir()?.join(CHANGELOG_FILE_NAME))
}

/// The append handle and the clock state, which one lock guards together.
#[derive(Debug)]
struct LogState {
    file: File,
    /// The newest stamp this handle has issued, starting from the largest one
    /// [`ChangeLog::open`] found in the file.
    last_stamp: Option<Hlc>,
    /// Set while the file may end in a line that was never finished, either
    /// because a write failed part way through or because the previous process
    /// died mid append. The next append writes a newline first, so its entry
    /// cannot be glued onto the remains of another one.
    needs_newline: bool,
}

/// An open change log: one append handle, one clock, one exclusive lock.
///
/// Share one value per log file. Holding two of them for the same file would
/// let each issue stamps the other does not know about; on Unix the second
/// [`ChangeLog::open`] fails instead, whether it is in this process or another
/// one.
#[derive(Debug)]
pub struct ChangeLog {
    device_id: String,
    state: Mutex<LogState>,
}

impl ChangeLog {
    /// Open the change log for append, creating the file if needed, take the
    /// single writer lock, and recover the clock from the largest stamp in the
    /// file.
    ///
    /// Prefer [`shared_log`] over a second call for the same file.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::SyncLogLocked`] when another handle, in this
    /// process or another one, already holds the log, and
    /// [`crate::Error::Io`] when the file cannot be opened or read. A damaged
    /// line is not an error here: see [`read_log`].
    pub fn open() -> Result<Self> {
        let path = changelog_path()?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        // Lock before reading, so no other process can append between the scan
        // and the first stamp this handle issues.
        lock_single_writer(&file, &path)?;
        sync_parent_dir(&path);
        let scan = scan_log(&path)?;
        Ok(ChangeLog {
            device_id: device_id()?,
            state: Mutex::new(LogState {
                file,
                last_stamp: scan.max_stamp,
                needs_newline: scan.unfinished_tail,
            }),
        })
    }

    /// This device's id, as every entry this log writes carries it.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Take a stamp seen from another device into this log's clock, so every
    /// stamp this log issues afterwards is greater than it.
    ///
    /// This is the receive half, [`Hlc::observe`], applied to the live clock.
    /// Call it for each stamp that arrives in a sync batch, before the local
    /// changes that follow it are appended. Without it, a merge cannot answer
    /// the remote device: this device would keep issuing stamps from its own
    /// wall clock, which is exactly what lets a device with a wrong clock win
    /// every conflict. [`crate::merge_apply::apply_remote_batch`] calls it
    /// once per batch, with the largest stamp in it.
    pub fn observe_remote(&self, remote: Hlc) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.last_stamp = Some(Hlc::observe(state.last_stamp, remote));
    }

    /// Stamp one user made change, append it as a line, and return the entry
    /// written. See [`ChangeLog::append_with_origin`] for a change the user
    /// did not make directly.
    ///
    /// Takes `&self`, so concurrent request handlers can share one log. The
    /// lock covers stamping and writing together: releasing it between the
    /// two would let a later stamp reach the file first.
    ///
    /// The line is flushed to disk before this returns, so a crash just after
    /// a saved change cannot lose the record of it. That costs one flush per
    /// change, which is small next to the Parquet write the change follows.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::InvalidArgument`] when `table` names a month no
    /// file can have or the entry is too long for one line (see
    /// [`MAX_LINE_BYTES`]), [`crate::Error::Json`] when the entry cannot be
    /// serialized, and [`crate::Error::Io`] when the line cannot be written or
    /// flushed.
    ///
    /// An error does **not** mean the line is absent. `write_all` can fail
    /// after writing part of the buffer, and the flush can fail after the
    /// bytes have landed. So do neither of these things with the error:
    ///
    /// - Do not retry the same change. The stamp is already spent, and a retry
    ///   can write the change twice.
    /// - Do not roll back the Parquet write this change came from. The entry
    ///   may be on disk and complete, and rolling back would leave the log
    ///   claiming a change the data does not have, which the other device
    ///   would then apply.
    ///
    /// Report the failure instead. The next append repairs the file by closing
    /// the unfinished line.
    pub fn append(
        &self,
        table: ChangeTable,
        row_id: impl Into<String>,
        op: ChangeOp,
    ) -> Result<ChangeEntry> {
        self.write_entry(table, row_id.into(), op, None)
    }

    /// [`ChangeLog::append`] for a change the user did not make directly, such
    /// as a row `/api/recurring/apply` generated. Same contract otherwise.
    pub fn append_with_origin(
        &self,
        table: ChangeTable,
        row_id: impl Into<String>,
        op: ChangeOp,
        origin: ChangeOrigin,
    ) -> Result<ChangeEntry> {
        self.write_entry(table, row_id.into(), op, Some(origin))
    }

    /// Append entries another device stamped, exactly as they arrived: their
    /// own stamps, device ids, tables, rows, origins, and operations. This is
    /// how a merge stores the remote entries
    /// [`crate::merge::MergePlan::entries_to_store`] lists, so this log can
    /// later tell a third device what it has seen.
    ///
    /// Every entry is encoded and checked before the first line is written,
    /// so an entry the log would refuse stores none of them. The clock takes
    /// each entry's stamp in through [`Hlc::observe`] before its line is
    /// written, so no stamp this log issues afterwards is at or below one the
    /// file now holds. All the lines go to the file in one write, with an
    /// unfinished earlier line closed first, as for [`ChangeLog::append`],
    /// and the file is flushed once. A merge can store a whole history at
    /// once, and a flush per line would hold the data write lock for one disk
    /// flush per entry. Returns once that flush is done. An empty slice
    /// writes nothing.
    ///
    /// # Errors
    ///
    /// The same as [`ChangeLog::append`], and with the same warning: an error
    /// does not mean the lines are absent, and after a failed write some of
    /// them may be complete in the file. So do not retry the same entries
    /// and do not undo the data write they follow. A later merge of the same
    /// batch finds whichever entries did reach the file and stores the rest.
    /// [`check_storable`] reports the errors that depend only on an entry,
    /// so a caller can run it before changing any data.
    pub fn append_remote_batch(&self, entries: &[&ChangeEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let lines = entries
            .iter()
            .map(|entry| encode_line(entry))
            .collect::<Result<Vec<_>>>()?;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // Every stamp is taken in before the one write that carries them all,
        // so the clock is past each line before that line can be in the file.
        for entry in entries {
            state.last_stamp = Some(Hlc::observe(state.last_stamp, entry.stamp));
        }
        write_lines(&mut state, &lines)
    }

    /// Empty the log file through this open handle, keeping the clock where
    /// it is. Only a phone reset from the hub calls this, holding the data
    /// write lock, right before it stores the hub's whole log.
    ///
    /// The file is truncated in place rather than replaced, because
    /// [`shared_log`] caches this handle by path: a new file renamed over
    /// the old one would leave the cache appending to an unlinked inode. The
    /// in-memory clock is untouched, so every stamp issued afterwards is
    /// still above every stamp this handle issued before. After a restart
    /// the clock recovers from the stored entries alone, which can put it
    /// below a stamp that was only on a discarded line; see
    /// [`crate::sync_exchange::reset_phone_from_hub`] for why that is safe.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] when the truncation or the flush fails. The file
    /// may then still hold some or all of its lines.
    pub(crate) fn empty_for_reset(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.file.set_len(0)?;
        state.file.sync_data()?;
        state.needs_newline = false;
        Ok(())
    }

    /// Rewrite the log to hold every line this version can read, verbatim
    /// and in the same order, and nothing else: corrupt lines, overlong
    /// lines, lines a newer version wrote, and an unfinished final line that
    /// does not parse all go. Returns how many lines were kept and dropped.
    ///
    /// Only a hub repair calls this, holding the data write lock. The new
    /// file is written beside the log, flushed, and renamed over it, so a
    /// crash leaves either the old log or the complete new one. The open
    /// handle inside this `ChangeLog` is then swapped for the new file's, so
    /// [`shared_log`] keeps appending to the file at the path, not to the
    /// unlinked old inode.
    ///
    /// The single writer lock moves with the file: the new handle takes it
    /// on the temporary file before the rename, so from the moment the new
    /// file carries the log's name it is already locked, and no other
    /// process can open and lock it in between. The old handle keeps its
    /// lock on the old inode until it is dropped at the swap, which protects
    /// nothing any more because no name leads to that inode.
    ///
    /// The clock is not moved back. Dropped lines may have held the largest
    /// stamps, and the in-memory clock stays above them, so the entries the
    /// caller appends next, a fresh baseline, carry stamps above every
    /// dropped line too, and a restart recovers the clock from those.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] when the log cannot be read or the new file
    /// cannot be written, flushed, or renamed, and
    /// [`crate::Error::SyncLogLocked`] naming the temporary file when
    /// another handle holds a lock on it. Before the rename the old log is
    /// untouched; the temporary file is removed on a failure.
    pub(crate) fn rewrite_readable(&self) -> Result<LogRewrite> {
        let path = changelog_path()?;
        let temp_path = path.with_file_name(format!(".{CHANGELOG_FILE_NAME}.tmp"));
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        let (lines, dropped) = readable_lines(&path)?;
        // A leftover from a repair that crashed before its rename. Nothing
        // else writes this name, and the old log is still whole.
        match std::fs::remove_file(&temp_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&temp_path)?;
        let written = (|| -> Result<()> {
            lock_single_writer(&file, &temp_path)?;
            let mut buffer = Vec::new();
            for line in &lines {
                buffer.extend_from_slice(line.as_bytes());
                buffer.push(b'\n');
            }
            file.write_all(&buffer)?;
            file.sync_data()?;
            std::fs::rename(&temp_path, &path)?;
            Ok(())
        })();
        if let Err(err) = written {
            // The write error is the one to report. A leftover temporary
            // file is removed by the next repair.
            let _ = std::fs::remove_file(&temp_path);
            return Err(err);
        }
        sync_parent_dir(&path);

        state.file = file;
        state.needs_newline = false;
        Ok(LogRewrite {
            kept: lines.len(),
            dropped,
        })
    }

    fn write_entry(
        &self,
        table: ChangeTable,
        row_id: String,
        op: ChangeOp,
        origin: Option<ChangeOrigin>,
    ) -> Result<ChangeEntry> {
        table.validate()?;

        // A panic in an earlier append left the state usable: the stamp it
        // took is simply never written, and the clock only moves forwards.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        let stamp = Hlc::next(state.last_stamp);
        // Consume the stamp before the write, so a failed write cannot hand
        // the same stamp to the next change.
        state.last_stamp = Some(stamp);

        let entry = ChangeEntry {
            stamp,
            device_id: self.device_id.clone(),
            table,
            row_id,
            origin,
            op,
        };

        entry.validate()?;

        // The stamp is spent even when the entry is refused, which costs
        // nothing: a gap in the sequence is fine, only a reused stamp is not.
        let line = encode_line(&entry)?;
        write_line(&mut state, &line)?;
        Ok(entry)
    }
}

/// Fail when `entry` could not be stored with
/// [`ChangeLog::append_remote_batch`]:
/// its table names a month no file can have, it cannot be serialized, or it
/// is too long for one line. Writes nothing.
///
/// A merge runs this on every entry it is about to store before it writes
/// any Parquet file, so an entry the log would refuse stops the merge while
/// nothing has changed yet.
///
/// # Errors
///
/// [`crate::Error::InvalidArgument`] or [`crate::Error::Json`], as described
/// for [`ChangeLog::append`]. The message never holds the entry's values.
pub fn check_storable(entry: &ChangeEntry) -> Result<()> {
    entry.validate()?;
    encode_line(entry).map(|_| ())
}

/// `entry` as one line of the log, without its newline, after checking that
/// its table is valid and the line fits under [`MAX_LINE_BYTES`].
fn encode_line(entry: &ChangeEntry) -> Result<Vec<u8>> {
    entry.table.validate()?;
    let line = serde_json::to_vec(entry)?;
    // The reader discards a line it cannot hold, and the clock probe cannot
    // read it either, so writing one would report success for a change that
    // is lost and takes its stamp with it. Refuse instead.
    if line.len() >= MAX_LINE_BYTES {
        return Err(Error::InvalidArgument(format!(
            "this change is {} bytes as one line, and the change log cannot carry a line of \
             {MAX_LINE_BYTES} bytes or more",
            line.len()
        )));
    }
    Ok(line)
}

/// Write `line` and its newline to the log and flush it, closing an
/// unfinished earlier line first. The caller holds the state lock.
fn write_line(state: &mut LogState, line: &[u8]) -> Result<()> {
    write_lines(state, std::slice::from_ref(&line))
}

/// Write `lines`, each followed by a newline, to the log as one write, then
/// flush it. An unfinished earlier line is closed first with one newline, and
/// only when there is one, so the file holds no empty lines of this
/// function's making. The caller holds the state lock.
fn write_lines(state: &mut LogState, lines: &[impl AsRef<[u8]>]) -> Result<()> {
    let length: usize = lines.iter().map(|line| line.as_ref().len() + 1).sum();
    let mut buffer = Vec::with_capacity(length + 1);
    if state.needs_newline {
        buffer.push(b'\n');
    }
    for line in lines {
        buffer.extend_from_slice(line.as_ref());
        buffer.push(b'\n');
    }

    // Treat the file as unfinished until both the write and the flush are
    // done. `write_all` can stop part way through, on a full disk for
    // example, and the leftover bytes must not run into the next entry.
    state.needs_newline = true;
    // One write: the file is open in append mode, so a concurrent writer
    // cannot interleave with it.
    state.file.write_all(&buffer)?;
    state.file.sync_data()?;
    state.needs_newline = false;
    Ok(())
}

/// Take the single writer lock on `file`, failing rather than waiting.
///
/// The lock belongs to this open handle, not to the process, so a second
/// [`ChangeLog::open`] in this process is refused exactly like another
/// process. The operating system releases it when the handle closes, including
/// when the process dies, so a crash leaves no stale lock to clear.
///
/// # Errors
///
/// Returns [`crate::Error::SyncLogLocked`] when another handle holds the log,
/// and [`crate::Error::Io`] when the lock could not be attempted at all. The
/// two are different problems: a held lock means another writer is running,
/// while an I/O failure says nothing about who holds what.
fn lock_single_writer(file: &File, path: &Path) -> Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::SyncLogLocked {
            path: path.to_path_buf(),
        }),
        Err(std::fs::TryLockError::Error(err)) => Err(err.into()),
    }
}

/// Return the process-wide log for the current data directory, opening it on
/// the first call.
///
/// Keyed by path, so a test that repoints `XDG_DATA_HOME` gets its own log
/// instead of the one a previous test opened.
pub fn shared_log() -> Result<Arc<ChangeLog>> {
    static LOGS: OnceLock<Mutex<HashMap<PathBuf, Arc<ChangeLog>>>> = OnceLock::new();
    let path = changelog_path()?;
    let mut logs = LOGS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(log) = logs.get(&path) {
        return Ok(Arc::clone(log));
    }
    let log = Arc::new(ChangeLog::open()?);
    logs.insert(path, Arc::clone(&log));
    Ok(log)
}

/// What one read of the log found. A missing log reads as an empty result.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LogRead {
    /// Every entry this version could read, in file order.
    pub entries: Vec<ChangeEntry>,
    /// 1-based numbers of the lines that could not be read and were not the
    /// unfinished tail. Each one is a change that is lost, so a caller that
    /// syncs must treat a non-empty list as a reason to stop rather than send
    /// an incomplete history. See the module docs.
    pub corrupt_lines: Vec<usize>,
    /// The subset of `corrupt_lines` that was skipped for being longer than
    /// [`MAX_LINE_BYTES`]. Counted apart because these lines cost a stamp as
    /// well as a change: the clock probe cannot read them either, so the
    /// ordering has a hole rather than only the history. [`ChangeLog::append`]
    /// refuses to write such a line, so a non-empty list means the file came
    /// from somewhere else or was damaged in place.
    pub oversized_lines: Vec<usize>,
    /// Set when the file ends without a newline, which means an append did not
    /// finish. If that last line still parses it is in `entries`; if it does
    /// not, it recorded nothing and is not counted as corruption.
    pub unfinished_tail: bool,
    /// The largest stamp on any line that is valid JSON carrying a `stamp`,
    /// including lines this version cannot fully read. `None` for an empty or
    /// missing log. This is what [`ChangeLog::open`] recovers the clock from.
    pub max_stamp: Option<Hlc>,
    /// How far `max_stamp` is ahead of the wall clock, in milliseconds, when
    /// that is more than [`CLOCK_AHEAD_LIMIT_MS`]. `None` when the clock is
    /// where it should be.
    ///
    /// One line with a wrong `ms`, from a broken clock on another device, from
    /// bit rot that still parses, or from a writer that put microseconds in
    /// this field, drags this device's clock to that value and keeps it there.
    /// [`Hlc::observe`] then carries it to every device that syncs. Adopting
    /// the value is still the right thing to do, because ignoring it reissues
    /// stamps, which is worse. So the read reports it instead of hiding it.
    ///
    /// Part 1b surfaces this to the user. Part 3 owns the repair. Note what
    /// repair means once entries exist: rewriting stamps across the user's
    /// real history, because every later stamp was issued from this one.
    pub clock_ahead_of_wall_by: Option<i64>,
}

/// Read the whole log: the entries, the damage, and the largest stamp.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] when the file exists but cannot be read. The
/// contents of a line never fail the read: see [`LogRead`] and the module docs.
pub fn read_log() -> Result<LogRead> {
    scan_log(&changelog_path()?)
}

fn scan_log(path: &Path) -> Result<LogRead> {
    let mut result = LogRead::default();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(err) => return Err(err.into()),
    };
    let mut reader = BufReader::new(file);

    let mut number = 0;
    while let Some(line) = read_line(&mut reader)? {
        number += 1;
        // Only the final line of a file can be unterminated, so the last
        // iteration decides this.
        result.unfinished_tail = !line.terminated;

        let text = match line.text {
            Some(text) => text,
            None => {
                let fault = if line.too_long {
                    LineFault::TooLong
                } else {
                    LineFault::NotUtf8
                };
                record_fault(&mut result, path, number, &fault);
                continue;
            }
        };
        // An empty line is what the repair in `ChangeLog::append` leaves when
        // the failed write had put down no bytes at all. It says nothing and
        // costs nothing.
        if text.trim().is_empty() {
            continue;
        }

        // The probe runs first and on every line, so a line this version
        // cannot decode still moves the clock.
        if let Ok(probe) = serde_json::from_str::<StampProbe>(&text) {
            result.max_stamp = result.max_stamp.max(Some(probe.stamp));
        }
        match parse_line(&text) {
            Ok(entry) => result.entries.push(entry),
            Err(fault) => record_fault(&mut result, path, number, &fault),
        }
    }

    result.clock_ahead_of_wall_by = clock_ahead_by(result.max_stamp, wall_clock_ms());
    if let Some(ahead) = result.clock_ahead_of_wall_by {
        eprintln!(
            "Change log: the newest stamp in {} is {} hours ahead of this machine's clock. Some \
             device wrote a wrong time, and every change stamped from here on carries it.",
            path.display(),
            ahead / (60 * 60 * 1000)
        );
    }
    Ok(result)
}

/// How far `max_stamp` runs ahead of `wall_ms`, when that is more than
/// [`CLOCK_AHEAD_LIMIT_MS`]. See [`LogRead::clock_ahead_of_wall_by`].
fn clock_ahead_by(max_stamp: Option<Hlc>, wall_ms: i64) -> Option<i64> {
    let ahead = max_stamp?.ms.saturating_sub(wall_ms);
    (ahead > CLOCK_AHEAD_LIMIT_MS).then_some(ahead)
}

/// What `ChangeLog::rewrite_readable` did, in line counts. Only a hub
/// repair rewrites the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogRewrite {
    /// Lines kept verbatim, one per readable entry.
    pub kept: usize,
    /// Lines left out: every line that was not readable as an entry, except
    /// blank ones, including an unfinished final line that does not parse.
    pub dropped: usize,
}

/// The text of every line in `path` that reads as an entry, in file order,
/// and the number of non-blank lines that do not. A missing file has none.
fn readable_lines(path: &Path) -> Result<(Vec<String>, usize)> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(err) => return Err(err.into()),
    };
    let mut reader = BufReader::new(file);
    let mut kept = Vec::new();
    let mut dropped = 0;
    while let Some(line) = read_line(&mut reader)? {
        match line.text {
            Some(text) if text.trim().is_empty() => {}
            Some(text) if parse_line(&text).is_ok() => kept.push(text),
            _ => dropped += 1,
        }
    }
    Ok((kept, dropped))
}

// ------------------------------------------------------------------
// The "log incomplete" marker
// ------------------------------------------------------------------

/// Why the log was marked incomplete. Each reason is one line of the marker
/// file, stored as a fixed word, so the file never holds row data.
///
/// Stored as a string, so a reason written by a newer version reads as
/// [`IncompleteReason::Unknown`] rather than failing. `Unknown` keeps no
/// text: the marker's contents are reported to the other device, and only
/// the words this version writes are known to carry nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum IncompleteReason {
    /// A change reached a data file and recording it in the log failed.
    AppendFailed,
    /// The startup baseline had to record the data again on a device with a
    /// sync peer, and refused, because a re-recording would outrank the
    /// peer's newer edits. See [`crate::sync_baseline::baseline_change_log`].
    BaselineRefused,
    /// This phone is due for a reset from the hub: it was just paired, or a
    /// reset started and did not finish. Such a phone sends nothing to the
    /// hub first.
    ResetPending,
    /// A hub repair started and did not finish.
    RepairUnfinished,
    /// A reason this version does not know.
    Unknown,
}

impl IncompleteReason {
    fn word(&self) -> &'static str {
        match self {
            IncompleteReason::AppendFailed => "append_failed",
            IncompleteReason::BaselineRefused => "baseline_refused",
            IncompleteReason::ResetPending => "reset_pending",
            IncompleteReason::RepairUnfinished => "repair_unfinished",
            IncompleteReason::Unknown => "unknown",
        }
    }
}

impl From<String> for IncompleteReason {
    fn from(value: String) -> Self {
        match value.trim() {
            "append_failed" => IncompleteReason::AppendFailed,
            "baseline_refused" => IncompleteReason::BaselineRefused,
            "reset_pending" => IncompleteReason::ResetPending,
            "repair_unfinished" => IncompleteReason::RepairUnfinished,
            _ => IncompleteReason::Unknown,
        }
    }
}

impl From<IncompleteReason> for String {
    fn from(reason: IncompleteReason) -> Self {
        reason.word().to_string()
    }
}

/// Return `<data_home>/finguard/sync/log_incomplete`, creating its directory
/// if necessary.
///
/// While this file exists the log is known not to describe all of this
/// device's data, and [`crate::sync_exchange::log_health`] reports the log
/// unreliable. Only a phone reset or a hub repair removes it.
pub fn log_incomplete_path() -> Result<PathBuf> {
    Ok(get_sync_dir()?.join(LOG_INCOMPLETE_FILE_NAME))
}

/// Mark the log incomplete for `reason`, adding the reason as a line unless
/// the marker already names it, then flush the marker and its folder.
///
/// A marker that exists and cannot be read is replaced, through a temporary
/// file and a rename, by one naming [`IncompleteReason::Unknown`] and
/// `reason`. `Unknown` stands for whatever the old marker said, so the
/// replacement never claims less than the old one might have. Without the
/// replacement, one unreadable marker would fail every later reset at its
/// first step, and only a reset removes the marker.
///
/// # Errors
///
/// [`crate::Error::Io`] when the marker cannot be written or flushed, or its
/// folder cannot be flushed. The folder flush is strict because a reset
/// relies on the marker surviving a crash before it deletes data. A caller
/// that must not fail, such as the data layer's record hook, warns instead of
/// returning the error.
pub(crate) fn mark_log_incomplete(reason: IncompleteReason) -> Result<()> {
    let path = log_incomplete_path()?;
    match read_incomplete_reasons(&path) {
        Ok(Some(reasons)) if reasons.contains(&reason) => return Ok(()),
        Ok(_) => {
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            file.write_all(format!("{}\n", reason.word()).as_bytes())?;
            file.sync_all()?;
        }
        Err(_) => {
            let temp_path = path.with_file_name(format!(".{LOG_INCOMPLETE_FILE_NAME}.tmp"));
            let written = (|| -> Result<()> {
                let mut file = File::create(&temp_path)?;
                file.write_all(
                    format!("{}\n{}\n", IncompleteReason::Unknown.word(), reason.word()).as_bytes(),
                )?;
                file.sync_all()?;
                std::fs::rename(&temp_path, &path)?;
                Ok(())
            })();
            if written.is_err() {
                let _ = std::fs::remove_file(&temp_path);
            }
            written?;
        }
    }
    if let Some(folder) = path.parent() {
        crate::df_operations::sync_dir(folder)?;
    }
    Ok(())
}

/// The reasons the log is marked incomplete, or `None` when it is not.
///
/// # Errors
///
/// [`crate::Error::Io`] when the marker exists and cannot be read. A caller
/// judging the log's health treats that as marked: see
/// [`crate::sync_exchange::log_health`].
pub fn read_log_incomplete() -> Result<Option<Vec<IncompleteReason>>> {
    read_incomplete_reasons(&log_incomplete_path()?)
}

fn read_incomplete_reasons(path: &Path) -> Result<Option<Vec<IncompleteReason>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            String::from_utf8_lossy(&bytes)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| IncompleteReason::from(line.to_string()))
                .collect(),
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Remove the incomplete marker and flush the folder. Only the last step of
/// a phone reset or a hub repair calls this: nothing else makes the log
/// whole again.
///
/// # Errors
///
/// [`crate::Error::Io`] when the marker exists and cannot be removed, or the
/// folder cannot be flushed. The log then still reports incomplete, which
/// is the safe side.
pub(crate) fn clear_log_incomplete() -> Result<()> {
    let path = log_incomplete_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    }
    if let Some(folder) = path.parent() {
        crate::df_operations::sync_dir(folder)?;
    }
    Ok(())
}

/// Read one line as an entry, or say why it could not be.
fn parse_line(text: &str) -> std::result::Result<ChangeEntry, LineFault> {
    let entry: ChangeEntry = serde_json::from_str(text).map_err(|err| LineFault::Json {
        category: err.classify(),
        column: err.column(),
    })?;
    entry
        .table
        .validate()
        .map_err(|err| LineFault::Unusable(err.to_string()))?;
    Ok(entry)
}

/// Why a line could not be read as a change.
///
/// Holds no part of the line. `Display` on a `serde_json` error can quote the
/// input, and a line holds row values, so only the category and the column are
/// kept.
enum LineFault {
    NotUtf8,
    TooLong,
    Json {
        category: serde_json::error::Category,
        column: usize,
    },
    /// The line decoded but describes something no file can hold. The text is
    /// from [`ChangeTable::validate`], which reports only the month.
    Unusable(String),
}

/// Record a damaged line and warn on stderr. An unterminated final line is an
/// append that did not finish, not corruption.
fn record_fault(result: &mut LogRead, path: &Path, number: usize, fault: &LineFault) {
    if !result.unfinished_tail {
        result.corrupt_lines.push(number);
        if matches!(fault, LineFault::TooLong) {
            result.oversized_lines.push(number);
        }
    }
    eprintln!(
        "{}",
        fault_warning(path, number, fault, result.unfinished_tail)
    );
}

/// Build the warning for a damaged line. Never include the line itself: the
/// warning goes to the log, and a line holds amounts and descriptions.
fn fault_warning(path: &Path, number: usize, fault: &LineFault, unfinished: bool) -> String {
    let what = match fault {
        LineFault::NotUtf8 => "is not UTF-8".to_string(),
        LineFault::TooLong => format!("is longer than {MAX_LINE_BYTES} bytes"),
        LineFault::Json { category, column } => {
            format!("is not a valid change ({category:?} error at column {column})")
        }
        LineFault::Unusable(reason) => format!("describes no real file ({reason})"),
    };
    if unfinished {
        format!(
            "Change log: the last line of {}, line {number}, {what}. An append did not finish, \
             so that line records no change. Every other line was read.",
            path.display()
        )
    } else {
        format!(
            "Change log: line {number} of {} {what}. That change is lost. Every other line was \
             read.",
            path.display()
        )
    }
}

/// One line as the reader found it.
struct RawLine {
    /// The line without its newline, or `None` when it cannot be text: either
    /// it is not UTF-8 or it is too long to hold.
    text: Option<String>,
    /// Whether the line ended with a newline. Only the last line of a file can
    /// be unterminated.
    terminated: bool,
    /// Whether `text` is `None` because the line was longer than
    /// [`MAX_LINE_BYTES`].
    too_long: bool,
}

/// Read the next line, or `None` at the end of the file.
///
/// Never holds more than [`MAX_LINE_BYTES`] in memory. A longer line is
/// reported and skipped, not read. Bytes that are not UTF-8 make the line
/// damaged like any other, rather than failing the whole read: one bad byte
/// must not make the log unreadable forever.
fn read_line(reader: &mut impl BufRead) -> std::io::Result<Option<RawLine>> {
    let mut buf = Vec::new();
    let read = reader
        .by_ref()
        .take(MAX_LINE_BYTES as u64)
        .read_until(b'\n', &mut buf)?;
    if read == 0 {
        return Ok(None);
    }
    let terminated = buf.last() == Some(&b'\n');
    if terminated {
        buf.pop();
    } else if read == MAX_LINE_BYTES {
        let terminated = discard_line(reader)?;
        return Ok(Some(RawLine {
            text: None,
            terminated,
            too_long: true,
        }));
    }
    Ok(Some(RawLine {
        text: String::from_utf8(buf).ok(),
        terminated,
        too_long: false,
    }))
}

/// Drop the rest of the current line without keeping it. Returns whether a
/// newline was found before the end of the file.
fn discard_line(reader: &mut impl BufRead) -> std::io::Result<bool> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(false);
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => {
                reader.consume(index + 1);
                return Ok(true);
            }
            None => {
                let len = available.len();
                reader.consume(len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Point `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `HOME` at three separate
    /// folders in one temp dir, so no test reads or writes the real data or
    /// config, and so a test can tell the data root from the config root.
    ///
    /// # Safety
    ///
    /// `std::env::set_var` is not thread-safe; callers hold
    /// `#[serial_test::serial]`.
    fn with_temp_env() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        for name in ["data", "config", "home"] {
            std::fs::create_dir_all(dir.path().join(name)).expect("create a root");
        }
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path().join("data"));
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
            std::env::set_var("HOME", dir.path().join("home"));
        }
        dir
    }

    fn data_root(temp: &tempfile::TempDir) -> PathBuf {
        temp.path().join("data")
    }

    fn config_root(temp: &tempfile::TempDir) -> PathBuf {
        temp.path().join("config")
    }

    fn hlc(ms: i64, counter: u32) -> Hlc {
        Hlc { ms, counter }
    }

    /// A stamp in the year 2096. Every test about clock recovery needs one:
    /// with a stamp near the Unix epoch, the real wall clock satisfies
    /// `appended > seeded` on its own and the assertion would hold even if
    /// recovery were deleted.
    fn far_future(counter: u32) -> Hlc {
        hlc(4_000_000_000_000, counter)
    }

    fn entries() -> Vec<ChangeEntry> {
        read_log().expect("read the log").entries
    }

    /// A line as a newer version might write it: valid JSON, with fields this
    /// version cannot decode.
    fn line_with(stamp: Hlc, extra: &str) -> String {
        format!(
            "{{\"stamp\":{{\"ms\":{},\"counter\":{}}},\"device_id\":\"dev-1\",{extra}}}",
            stamp.ms, stamp.counter
        )
    }

    /// A line this version reads in full.
    fn readable_line(stamp: Hlc, row_id: &str) -> String {
        line_with(
            stamp,
            &format!(
                "\"table\":\"recurring\",\"year\":2026,\"row_id\":\"{row_id}\",\"op\":\"delete\""
            ),
        )
    }

    /// A wall clock that jumps backwards, for example after an NTP
    /// correction, must not make the stamp repeat or go backwards.
    #[test]
    fn stamp_never_goes_backwards_when_the_clock_does() {
        let last = hlc(2_000, 0);

        let after_jump = Hlc::after(Some(last), 1_000);

        assert!(after_jump > last);
        assert_eq!(after_jump, hlc(2_000, 1));
        // The clock stays behind for a while: every stamp still advances.
        let mut stamp = after_jump;
        for expected in 2..=5 {
            stamp = Hlc::after(Some(stamp), 1_500);
            assert_eq!(stamp, hlc(2_000, expected));
        }
        // Once the wall clock passes the stamp again, the counter resets.
        assert_eq!(Hlc::after(Some(stamp), 2_001), hlc(2_001, 0));
    }

    /// Several changes inside one millisecond separate by counter.
    #[test]
    fn counter_increments_within_one_millisecond() {
        let first = Hlc::after(None, 7_000);
        assert_eq!(first, hlc(7_000, 0));

        let second = Hlc::after(Some(first), 7_000);
        let third = Hlc::after(Some(second), 7_000);

        assert_eq!(second, hlc(7_000, 1));
        assert_eq!(third, hlc(7_000, 2));
        assert!(first < second && second < third);
    }

    /// The counter carries into the next millisecond rather than wrapping,
    /// which would reissue a stamp.
    #[test]
    fn counter_at_its_maximum_borrows_the_next_millisecond() {
        let full = hlc(7_000, u32::MAX);

        let next = Hlc::after(Some(full), 6_000);

        assert_eq!(next, hlc(7_001, 0));
        assert!(next > full);
    }

    /// The receive half passes a stamp from a device whose clock is far ahead,
    /// so that device stops winning every later conflict.
    #[test]
    fn observing_a_remote_stamp_passes_it() {
        let last = hlc(7_000, 0);
        let far_future = hlc(4_000_000_000_000, 4);

        let observed = Hlc::observed_at(Some(last), far_future, 7_500);

        assert_eq!(observed, hlc(4_000_000_000_000, 5));
        assert!(observed > last && observed > far_future);
        // The next local stamp keeps that lead instead of dropping back to the
        // local wall clock.
        assert!(Hlc::after(Some(observed), 7_501) > far_future);
    }

    /// The other three branches of the receive half.
    #[test]
    fn observing_uses_the_larger_of_the_three_clocks() {
        let last = hlc(7_000, 3);
        // The wall clock is ahead of both, so the counter starts again.
        assert_eq!(
            Hlc::observed_at(Some(last), hlc(6_000, 1), 8_000),
            hlc(8_000, 0)
        );
        // Same millisecond on both sides: step past the larger counter.
        assert_eq!(
            Hlc::observed_at(Some(last), hlc(7_000, 9), 6_000),
            hlc(7_000, 10)
        );
        // The local stamp is the newest one, so it advances.
        assert_eq!(
            Hlc::observed_at(Some(hlc(9_000, 2)), hlc(7_000, 5), 8_000),
            hlc(9_000, 3)
        );
        // An empty log is older than anything.
        assert_eq!(Hlc::observed_at(None, hlc(7_000, 5), 1), hlc(7_000, 6));
    }

    /// Two devices that stamp a change in the same millisecond still get one
    /// definite order, decided by device id.
    #[test]
    fn order_breaks_ties_by_device_id() {
        let stamp = hlc(7_000, 3);
        let entry = |device: &str| ChangeEntry {
            stamp,
            device_id: device.to_string(),
            table: ChangeTable::Recurring { year: 2026 },
            row_id: "row-1".to_string(),
            origin: None,
            op: ChangeOp::Delete,
        };
        let alpha = entry("aaaa");
        let zulu = entry("zzzz");

        assert!(alpha.order_key() < zulu.order_key());
        // A later stamp wins whatever the device id is.
        let mut later = entry("aaaa");
        later.stamp = hlc(7_000, 4);
        assert!(later.order_key() > zulu.order_key());
    }

    /// The first call creates the id file, and every later call returns the
    /// same id, including after a fresh read from disk.
    #[test]
    #[serial_test::serial]
    fn device_id_is_created_once_and_stays_stable() {
        let temp = with_temp_env();
        let path = device_id_path().expect("device id path");
        assert!(!path.exists());

        let first = device_id().expect("create device id");
        let second = device_id().expect("read device id");

        assert_eq!(first, second);
        assert!(uuid::Uuid::parse_str(&first).is_ok(), "{first}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read the id file"),
            first
        );
        // The id belongs to the config root, not the data root.
        assert!(path.starts_with(config_root(&temp)), "{}", path.display());
        assert!(!path.starts_with(data_root(&temp)), "{}", path.display());
        assert_eq!(path.file_name().unwrap(), "device_id");
    }

    /// Losing the whole data folder, or copying somebody else's, leaves the
    /// device's identity alone. This is the reason the id is not stored with
    /// the data: two devices sharing one id would issue colliding stamps.
    #[test]
    #[serial_test::serial]
    fn device_identity_survives_losing_the_data_folder() {
        let temp = with_temp_env();
        let id = device_id().expect("create device id");
        let log = ChangeLog::open().expect("open log");
        log.append(
            ChangeTable::Liquidity { year: 2026 },
            "row-1",
            ChangeOp::Delete,
        )
        .expect("append");
        drop(log);

        std::fs::remove_dir_all(data_root(&temp)).expect("delete the data tree");

        assert_eq!(device_id().expect("device id after the data is gone"), id);
        let log = ChangeLog::open().expect("reopen log on an empty data folder");
        assert_eq!(log.device_id(), id);
        assert!(entries().is_empty());
    }

    /// A device id file left empty by an interrupted first run is filled in,
    /// not reported as a missing id.
    #[test]
    #[serial_test::serial]
    fn empty_device_id_file_is_filled_in() {
        let _temp = with_temp_env();
        let path = device_id_path().expect("device id path");
        std::fs::write(&path, "").expect("write empty file");

        let id = device_id().expect("device id recovers");

        assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");
        assert_eq!(device_id().expect("second call"), id);
    }

    /// The log lives beside `dbs/`, where the year walkers and the backup
    /// copier never look.
    #[test]
    #[serial_test::serial]
    fn log_sits_outside_the_data_folder() {
        let temp = with_temp_env();

        let path = changelog_path().expect("changelog path");

        assert_eq!(path, data_root(&temp).join("finguard/sync/changelog.jsonl"));
        assert!(!path.starts_with(data_root(&temp).join("finguard/dbs")));
    }

    fn sample_ops() -> Vec<ChangeOp> {
        let mut row = Map::new();
        row.insert("row_id".to_string(), Value::from("row-1"));
        row.insert("expense_name".to_string(), Value::from("sample"));
        row.insert(
            "expense_amount".to_string(),
            cell_number(12.5).expect("a finite amount"),
        );
        row.insert("note".to_string(), Value::Null);
        vec![
            ChangeOp::Upsert { row },
            ChangeOp::Delete,
            ChangeOp::Cell {
                column: "03".to_string(),
                value: cell_number(120.5).expect("a finite amount"),
            },
            ChangeOp::Cell {
                column: "04".to_string(),
                value: Value::Null,
            },
        ]
    }

    #[test]
    #[serial_test::serial]
    fn settings_entry_validation_rejects_invalid_shapes() {
        let _temp = with_temp_env();
        let log = ChangeLog::open().expect("open log");
        let mut mapping = Map::new();
        mapping.insert("primary_category".into(), Value::from("primary"));
        mapping.insert("secondary_category".into(), Value::from("secondary"));
        assert!(
            log.append(
                ChangeTable::CategoryMappings,
                "Uppercase",
                ChangeOp::Upsert {
                    row: mapping.clone()
                }
            )
            .is_err()
        );
        mapping.remove("secondary_category");
        assert!(
            log.append(
                ChangeTable::CategoryMappings,
                "valid",
                ChangeOp::Upsert { row: mapping }
            )
            .is_err()
        );
        assert!(
            log.append(
                ChangeTable::KnownCategories,
                "unknown:Name",
                ChangeOp::Delete
            )
            .is_err()
        );
        let mut non_empty = Map::new();
        non_empty.insert("unexpected".into(), Value::from("value"));
        assert!(
            log.append(
                ChangeTable::KnownCategories,
                "primary:Name",
                ChangeOp::Upsert { row: non_empty }
            )
            .is_err()
        );
        let mut currency = Map::new();
        currency.insert("value".into(), Value::from("CAD"));
        assert!(
            log.append(
                ChangeTable::CurrencySettings,
                "reference_currency",
                ChangeOp::Upsert { row: currency }
            )
            .is_err()
        );
        assert!(
            log.append(
                ChangeTable::CurrencySettings,
                "reference_currency",
                ChangeOp::Delete
            )
            .is_err()
        );
        let mut mode = Map::new();
        mode.insert("value".into(), Value::from("invalid"));
        assert!(
            log.append(
                ChangeTable::CurrencySettings,
                "current_month_rate_mode",
                ChangeOp::Upsert { row: mode }
            )
            .is_err()
        );
    }

    /// Every table variant, all in one year folder.
    fn sample_tables(year: i32) -> Vec<ChangeTable> {
        vec![
            ChangeTable::Expenses { year, month: 3 },
            ChangeTable::Recurring { year },
            ChangeTable::Investments { year },
            ChangeTable::InvestmentsPrices { year },
            ChangeTable::Liquidity { year },
            ChangeTable::CreditsDebts { year },
            ChangeTable::CashflowIncome { year },
            ChangeTable::CategoryMappings,
            ChangeTable::KnownCategories,
            ChangeTable::CurrencySettings,
        ]
    }

    /// Every table in every operation kind survives a write and a read, in
    /// file order, with each entry on its own line and its own year.
    #[test]
    #[serial_test::serial]
    fn entries_round_trip_through_the_file() {
        let _temp = with_temp_env();
        let log = ChangeLog::open().expect("open log");
        let years = [2025, 2026];

        let mut written = Vec::new();
        for year in years {
            for (index, table) in sample_tables(year).into_iter().enumerate() {
                let operations = match &table {
                    ChangeTable::CategoryMappings => {
                        let mut row = Map::new();
                        row.insert("primary_category".into(), Value::from("primary"));
                        row.insert("secondary_category".into(), Value::from("secondary"));
                        vec![ChangeOp::Upsert { row }, ChangeOp::Delete]
                    }
                    ChangeTable::KnownCategories => {
                        vec![ChangeOp::Upsert { row: Map::new() }, ChangeOp::Delete]
                    }
                    ChangeTable::CurrencySettings => {
                        let mut row = Map::new();
                        row.insert("value".into(), Value::from("USD"));
                        vec![
                            ChangeOp::Upsert { row },
                            ChangeOp::Cell {
                                column: "value".into(),
                                value: Value::from("GBP"),
                            },
                        ]
                    }
                    _ => sample_ops(),
                };
                for op in operations {
                    let row_id = match &table {
                        ChangeTable::CategoryMappings => format!("sample-{year}-{index}"),
                        ChangeTable::KnownCategories => format!("primary:Sample-{year}-{index}"),
                        ChangeTable::CurrencySettings => {
                            if matches!(op, ChangeOp::Cell { .. }) {
                                "reference_currency".to_string()
                            } else {
                                "reference_currency".to_string()
                            }
                        }
                        _ => format!("row-{year}-{index}"),
                    };
                    written.push(log.append(table.clone(), row_id, op).expect("append"));
                }
            }
        }

        let read = read_log().expect("read the log");
        assert_eq!(read.entries, written);
        assert!(read.corrupt_lines.is_empty());
        assert!(!read.unfinished_tail);
        assert_eq!(read.max_stamp, written.last().map(|entry| entry.stamp));
        let text = std::fs::read_to_string(changelog_path().unwrap()).unwrap();
        assert_eq!(text.lines().count(), written.len());
        assert!(text.ends_with('\n'));
        let settings_line = text
            .lines()
            .find(|line| line.contains("\"table\":\"category_mappings\""))
            .expect("a settings line");
        assert!(!settings_line.contains("\"year\""));
        for entry in &read.entries {
            assert_eq!(entry.device_id, log.device_id());
            if let Some(year) = entry.table.year() {
                assert!(years.contains(&year));
            }
        }
        let tables: HashSet<&ChangeTable> = read.entries.iter().map(|entry| &entry.table).collect();
        assert_eq!(tables.len(), 7 * years.len() + 3);
    }

    /// The line keeps the flat, readable shape the format promises, so the
    /// file can be inspected by eye and by other tools.
    #[test]
    #[serial_test::serial]
    fn a_line_is_one_flat_json_object() {
        let _temp = with_temp_env();
        let log = ChangeLog::open().expect("open log");

        let entry = log
            .append(
                ChangeTable::Expenses {
                    year: 2026,
                    month: 3,
                },
                "row-1",
                ChangeOp::Cell {
                    column: "expense_amount".to_string(),
                    value: cell_number(12.5).unwrap(),
                },
            )
            .expect("append");

        let line = std::fs::read_to_string(changelog_path().unwrap()).unwrap();
        let value: Value = serde_json::from_str(line.trim()).expect("one JSON object");
        assert_eq!(value["table"], Value::from("expenses"));
        assert_eq!(value["year"], Value::from(2026));
        assert_eq!(value["month"], Value::from(3));
        assert_eq!(value["op"], Value::from("cell"));
        assert_eq!(value["column"], Value::from("expense_amount"));
        assert_eq!(value["row_id"], Value::from("row-1"));
        assert_eq!(value["stamp"]["ms"], Value::from(entry.stamp.ms));
        assert_eq!(value["stamp"]["counter"], Value::from(entry.stamp.counter));
        // A user action writes no `origin` key at all.
        assert_eq!(entry.origin, None);
        assert_eq!(value.get("origin"), None);
    }

    /// A generated row says so on its line, and reads back as generated. The
    /// merge rule that needs this lands with the merge engine in part 2.
    #[test]
    #[serial_test::serial]
    fn origin_round_trips_and_is_absent_for_a_user_action() {
        let _temp = with_temp_env();
        let log = ChangeLog::open().expect("open log");
        let mut row = Map::new();
        row.insert("row_id".to_string(), Value::from("tpl-1:2026-03"));

        let generated = log
            .append_with_origin(
                ChangeTable::Expenses {
                    year: 2026,
                    month: 3,
                },
                "tpl-1:2026-03",
                ChangeOp::Upsert { row },
                ChangeOrigin::RecurringApply,
            )
            .expect("append the generated row");
        let by_hand = log
            .append(
                ChangeTable::Expenses {
                    year: 2026,
                    month: 3,
                },
                "tpl-1:2026-03",
                ChangeOp::Delete,
            )
            .expect("append the user's delete");

        assert_eq!(generated.origin, Some(ChangeOrigin::RecurringApply));
        assert_eq!(by_hand.origin, None);
        assert_eq!(entries(), vec![generated, by_hand]);
        let lines: Vec<Value> = std::fs::read_to_string(changelog_path().unwrap())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).expect("one JSON object"))
            .collect();
        assert_eq!(lines[0]["origin"], Value::from("recurring_apply"));
        assert_eq!(lines[1].get("origin"), None);
    }

    /// An origin value this version does not define keeps its line readable
    /// and keeps the value itself, so a later version still sees what the
    /// writer meant.
    #[test]
    #[serial_test::serial]
    fn unknown_origin_value_is_kept_as_written() {
        let _temp = with_temp_env();
        std::fs::write(
            changelog_path().unwrap(),
            format!(
                "{}\n",
                line_with(
                    hlc(7_000, 1),
                    "\"table\":\"recurring\",\"year\":2026,\"row_id\":\"row-1\",\"origin\":\
                     \"imported_backup\",\"op\":\"delete\""
                )
            ),
        )
        .expect("write line");

        let read = read_log().expect("read the log");

        assert_eq!(read.entries.len(), 1);
        assert!(read.corrupt_lines.is_empty());
        assert_eq!(
            read.entries[0].origin,
            Some(ChangeOrigin::Unknown("imported_backup".to_string()))
        );
        // Writing the entry back out preserves the value rather than dropping
        // it or turning it into a known origin.
        let line = serde_json::to_value(&read.entries[0]).expect("serialize");
        assert_eq!(line["origin"], Value::from("imported_backup"));
    }

    /// An `origin` that is not a string at all is a damaged line, not a
    /// silently ignored field.
    #[test]
    #[serial_test::serial]
    fn non_string_origin_fails_only_its_own_line() {
        let _temp = with_temp_env();
        let bad = line_with(
            hlc(7_000, 2),
            "\"table\":\"recurring\",\"year\":2026,\"row_id\":\"row-2\",\"origin\":42,\"op\":\
             \"delete\"",
        );
        std::fs::write(
            changelog_path().unwrap(),
            format!("{bad}\n{}\n", readable_line(hlc(7_000, 1), "row-1")),
        )
        .expect("write lines");

        let read = read_log().expect("a bad origin is not fatal");

        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].row_id, "row-1");
        assert_eq!(read.corrupt_lines, vec![1]);
    }

    /// An unknown field on a line, written by a newer version, does not stop
    /// an older reader from using the rest of the line.
    #[test]
    #[serial_test::serial]
    fn unknown_field_on_a_line_is_ignored() {
        let _temp = with_temp_env();
        std::fs::write(
            changelog_path().unwrap(),
            format!(
                "{}\n",
                line_with(
                    hlc(7_000, 1),
                    "\"table\":\"recurring\",\"year\":2026,\"row_id\":\"row-1\",\"op\":\
                     \"delete\",\"future_field\":42"
                )
            ),
        )
        .expect("write line");

        let read = read_log().expect("read the log");

        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].stamp, hlc(7_000, 1));
        assert_eq!(read.entries[0].table, ChangeTable::Recurring { year: 2026 });
        assert_eq!(read.entries[0].op, ChangeOp::Delete);
        assert!(read.corrupt_lines.is_empty());
    }

    /// A complete line this version cannot decode, such as one a newer version
    /// wrote, still moves the clock. Reusing its stamp would give two
    /// different changes the same name forever.
    #[test]
    #[serial_test::serial]
    fn a_line_this_version_cannot_read_still_moves_the_clock() {
        let _temp = with_temp_env();
        let from_the_future = far_future(9);
        std::fs::write(
            changelog_path().unwrap(),
            format!(
                "{}\n",
                line_with(
                    from_the_future,
                    "\"table\":\"a_table_from_a_later_version\",\"year\":2026,\"row_id\":\
                     \"row-1\",\"op\":\"delete\""
                )
            ),
        )
        .expect("write line");

        let before = read_log().expect("read the log");
        let log = ChangeLog::open().expect("open log");
        let appended = log
            .append(
                ChangeTable::Recurring { year: 2026 },
                "row-2",
                ChangeOp::Delete,
            )
            .expect("append");

        assert!(before.entries.is_empty());
        assert_eq!(before.corrupt_lines, vec![1]);
        assert_eq!(before.max_stamp, Some(from_the_future));
        assert!(
            appended.stamp > from_the_future,
            "the stamp must pass the line it could not read"
        );
    }

    /// A crash during an append leaves a half written final line. The read
    /// keeps every complete entry, reports the tail as unfinished rather than
    /// as corruption, and drops the half line.
    #[test]
    #[serial_test::serial]
    fn unfinished_final_line_is_not_corruption() {
        let _temp = with_temp_env();
        let path = changelog_path().expect("changelog path");
        let kept = readable_line(hlc(7_000, 1), "row-1");
        let cut = readable_line(hlc(7_000, 2), "row-2");
        std::fs::write(&path, format!("{kept}\n{}", &cut[..cut.len() / 2]))
            .expect("write an unfinished tail");

        let read = read_log().expect("an unfinished tail is not fatal");

        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].row_id, "row-1");
        assert!(read.unfinished_tail);
        assert!(
            read.corrupt_lines.is_empty(),
            "an interrupted append is not lost data"
        );
    }

    /// What a partial write leaves behind: the next append closes the
    /// unfinished line instead of continuing it, so its own entry is readable
    /// and the entries around it survive.
    #[test]
    #[serial_test::serial]
    fn an_append_after_a_partial_write_is_readable() {
        let _temp = with_temp_env();
        let path = changelog_path().expect("changelog path");
        let kept = readable_line(far_future(1), "row-1");
        let cut = readable_line(far_future(2), "row-2");
        std::fs::write(&path, format!("{kept}\n{}", &cut[..cut.len() / 2]))
            .expect("write an unfinished tail");

        let log = ChangeLog::open().expect("open log");
        let appended = log
            .append(
                ChangeTable::Recurring { year: 2026 },
                "row-3",
                ChangeOp::Delete,
            )
            .expect("append after the partial write");

        let read = read_log().expect("read the log");
        assert_eq!(read.entries.len(), 2);
        assert_eq!(read.entries[0].row_id, "row-1");
        assert_eq!(read.entries[1], appended);
        assert!(!read.unfinished_tail);
        // The remains of the partial write are now a line of their own, and
        // they are genuinely corrupt: that change never reached the file.
        assert_eq!(read.corrupt_lines, vec![2]);
        // The kept line is dated 2096, so only recovery can satisfy this.
        assert!(appended.stamp > far_future(1));
    }

    /// A line that is not JSON at all, in the middle of the file, costs only
    /// that line, and the read says which line it was.
    #[test]
    #[serial_test::serial]
    fn damaged_line_in_the_middle_costs_only_that_line() {
        let _temp = with_temp_env();
        std::fs::write(
            changelog_path().unwrap(),
            format!(
                "{}\nnot json\n{}\n",
                readable_line(hlc(7_000, 1), "row-1"),
                readable_line(hlc(7_000, 2), "row-2")
            ),
        )
        .expect("write a damaged file");

        let read = read_log().expect("a damaged line is not fatal");

        assert_eq!(read.entries.len(), 2);
        assert_eq!(read.corrupt_lines, vec![2]);
        assert!(
            read.oversized_lines.is_empty(),
            "this line is short, just not JSON"
        );
        assert!(!read.unfinished_tail);
        assert_eq!(read.max_stamp, Some(hlc(7_000, 2)));
    }

    /// One byte that is not UTF-8 costs its own line and nothing else. Making
    /// it fatal would leave the app unable to record any change at all, with
    /// no way out from inside the app.
    #[test]
    #[serial_test::serial]
    fn a_line_that_is_not_utf8_costs_only_that_line() {
        let _temp = with_temp_env();
        let path = changelog_path().expect("changelog path");
        let mut bytes = b"\xff\xfe not text\n".to_vec();
        bytes.extend_from_slice(readable_line(hlc(7_000, 1), "row-1").as_bytes());
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).expect("write a file with a bad byte");

        let read = read_log().expect("a byte that is not UTF-8 is not fatal");

        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].row_id, "row-1");
        assert_eq!(read.corrupt_lines, vec![1]);
        // The log still accepts new changes.
        let log = ChangeLog::open().expect("open log");
        log.append(
            ChangeTable::Recurring { year: 2026 },
            "row-2",
            ChangeOp::Delete,
        )
        .expect("append");
    }

    /// A run of bytes with no newline in it is skipped without being read into
    /// memory, and the lines after it are still read.
    #[test]
    #[serial_test::serial]
    fn an_overlong_line_is_skipped() {
        let _temp = with_temp_env();
        let path = changelog_path().expect("changelog path");
        let mut bytes = vec![b'x'; MAX_LINE_BYTES + 5_000];
        bytes.push(b'\n');
        bytes.extend_from_slice(readable_line(hlc(7_000, 1), "row-1").as_bytes());
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).expect("write an overlong line");

        let read = read_log().expect("an overlong line is not fatal");

        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].row_id, "row-1");
        assert_eq!(read.corrupt_lines, vec![1]);
        // Counted apart, because this is the one kind of damage that loses the
        // stamp as well as the change.
        assert_eq!(read.oversized_lines, vec![1]);
    }

    /// A change too long to be read back is refused when it is written, rather
    /// than written and then skipped. The reader drops an oversized line, and
    /// the clock probe cannot read it either, so writing one would report
    /// success for a change that is lost and takes its stamp with it.
    #[test]
    #[serial_test::serial]
    fn a_change_too_long_for_one_line_is_refused() {
        let _temp = with_temp_env();
        let log = ChangeLog::open().expect("open log");
        let mut row = Map::new();
        row.insert("row_id".to_string(), Value::from("row-1"));
        row.insert("note".to_string(), Value::from("x".repeat(MAX_LINE_BYTES)));

        let err = log
            .append(
                ChangeTable::Recurring { year: 2026 },
                "row-1",
                ChangeOp::Upsert { row },
            )
            .expect_err("a line that cannot be read back must not be written");

        assert!(err.to_string().contains("cannot carry a line"), "{err}");
        let read = read_log().expect("read the log");
        assert_eq!(read, LogRead::default(), "nothing may reach the file");
        // The log still works, and the spent stamp costs nothing: a gap in the
        // sequence is fine, a reused stamp is not.
        let after = log
            .append(
                ChangeTable::Recurring { year: 2026 },
                "row-2",
                ChangeOp::Delete,
            )
            .expect("append after the refusal");
        assert_eq!(read_log().unwrap().entries, vec![after]);
    }

    /// Remote entries are stored exactly as they arrived, each on its own
    /// line even after an unfinished one, and the clock passes the largest
    /// stamp. A batch holding an entry too long for one line is refused by
    /// the pre-write check and by the append alike, and none of it reaches
    /// the file.
    #[test]
    #[serial_test::serial]
    fn remote_entries_are_stored_verbatim_and_move_the_clock() {
        let _temp = with_temp_env();
        let path = changelog_path().unwrap();
        std::fs::write(&path, "{\"stamp\":{\"ms\":1,").unwrap();
        let log = ChangeLog::open().expect("open log");
        let remote = ChangeEntry {
            stamp: far_future(3),
            device_id: "other-device".to_string(),
            table: ChangeTable::Liquidity { year: 2026 },
            row_id: "row-1".to_string(),
            origin: Some(ChangeOrigin::RecurringApply),
            op: ChangeOp::Delete,
        };

        let second = ChangeEntry {
            stamp: far_future(1),
            device_id: "third-device".to_string(),
            table: ChangeTable::Expenses {
                year: 2026,
                month: 4,
            },
            row_id: "row-9".to_string(),
            origin: None,
            op: ChangeOp::Delete,
        };

        check_storable(&remote).expect("a small entry is storable");
        log.append_remote_batch(&[&remote, &second])
            .expect("store the remote entries");
        let local = log
            .append(
                ChangeTable::Recurring { year: 2026 },
                "row-2",
                ChangeOp::Delete,
            )
            .expect("append a local change");

        let bytes = std::fs::read(&path).unwrap();
        let cut_off = b"{\"stamp\":{\"ms\":1,";
        assert!(
            bytes.starts_with(cut_off) && bytes[cut_off.len()] == b'\n',
            "one repair newline closes the cut-off line"
        );
        assert!(
            !bytes.windows(2).any(|pair| pair == b"\n\n"),
            "no empty line anywhere"
        );
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 4);
        let read = read_log().unwrap();
        assert_eq!(read.entries, vec![remote.clone(), second, local.clone()]);
        // The cut-off first line is closed by the repair newline, so it now
        // reads as one damaged line, and the new lines stand apart from it.
        assert_eq!(read.corrupt_lines, vec![1]);
        assert!(!read.unfinished_tail);
        assert!(local.stamp > far_future(3));

        let mut row = Map::new();
        row.insert("note".to_string(), Value::from("x".repeat(MAX_LINE_BYTES)));
        let oversized = ChangeEntry {
            stamp: far_future(9),
            device_id: "other-device".to_string(),
            table: ChangeTable::Recurring { year: 2026 },
            row_id: "row-3".to_string(),
            origin: None,
            op: ChangeOp::Upsert { row },
        };
        let before = std::fs::read(&path).unwrap();
        assert!(check_storable(&oversized).is_err());
        assert!(log.append_remote_batch(&[&remote, &oversized]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// A stamp far ahead of this machine's clock is adopted, because ignoring
    /// it would reissue stamps, and reported, because it pins the clock there
    /// for good and spreads to the other device.
    #[test]
    #[serial_test::serial]
    fn a_stamp_far_ahead_of_the_wall_clock_is_reported() {
        let _temp = with_temp_env();
        let wrong_clock = hlc(wall_clock_ms() + 3 * CLOCK_AHEAD_LIMIT_MS, 0);
        std::fs::write(
            changelog_path().unwrap(),
            format!("{}\n", readable_line(wrong_clock, "row-1")),
        )
        .expect("write line");

        let read = read_log().expect("read the log");

        assert_eq!(read.max_stamp, Some(wrong_clock), "it is still adopted");
        let ahead = read
            .clock_ahead_of_wall_by
            .expect("a clock this far ahead must be reported");
        assert!(ahead > CLOCK_AHEAD_LIMIT_MS, "{ahead}");
        assert!(ahead <= 3 * CLOCK_AHEAD_LIMIT_MS, "{ahead}");
    }

    /// A log written by working clocks reports nothing. The threshold is a day,
    /// so ordinary skew and a slow sync stay quiet.
    #[test]
    #[serial_test::serial]
    fn an_ordinary_log_reports_no_clock_problem() {
        let _temp = with_temp_env();
        let log = ChangeLog::open().expect("open log");
        log.append(
            ChangeTable::Recurring { year: 2026 },
            "row-1",
            ChangeOp::Delete,
        )
        .expect("append");

        assert_eq!(read_log().unwrap().clock_ahead_of_wall_by, None);
        // An hour of skew is not worth a warning either.
        let wall = wall_clock_ms();
        assert_eq!(clock_ahead_by(Some(hlc(wall + 3_600_000, 0)), wall), None);
        assert_eq!(clock_ahead_by(None, wall), None);
    }

    /// The receive half reaches the live clock, so a merge can answer a device
    /// whose stamps are ahead of this one.
    #[test]
    #[serial_test::serial]
    fn observing_a_remote_stamp_moves_the_live_clock() {
        let _temp = with_temp_env();
        let log = ChangeLog::open().expect("open log");
        let remote = far_future(4);

        log.observe_remote(remote);
        let appended = log
            .append(
                ChangeTable::Recurring { year: 2026 },
                "row-1",
                ChangeOp::Delete,
            )
            .expect("append");

        assert!(
            appended.stamp > remote,
            "the next local stamp must pass the remote one"
        );
    }

    /// The warning about a damaged line says where it is, never what it said.
    /// A line holds amounts, descriptions, and category names, and these
    /// warnings end up in logs.
    #[test]
    fn the_warning_never_repeats_the_line() {
        let secret = "Dentist for Jane 1234.56";
        // Valid up to the trailing comma, so the fault comes from a real parse
        // of a line that holds row values.
        let line = format!(
            "{{\"stamp\":{{\"ms\":7000,\"counter\":1}},\"table\":\"recurring\",\"year\":2026,\
             \"op\":\"upsert\",\"row\":{{\"expense_name\":\"{secret}\"}},}}"
        );
        let fault = parse_line(&line).expect_err("the line does not parse");

        let message = fault_warning(Path::new("/data/changelog.jsonl"), 7, &fault, false);

        for text in [secret, "Dentist", "Jane", "1234.56", "expense_name"] {
            assert!(!message.contains(text), "{message}");
        }
        assert!(message.contains("line 7"), "{message}");
        assert!(message.contains("/data/changelog.jsonl"), "{message}");
    }

    /// A month no file can have is refused when it is written and when it is
    /// read, rather than reaching the data layer and failing there.
    #[test]
    #[serial_test::serial]
    fn an_impossible_month_is_refused() {
        let _temp = with_temp_env();
        let log = ChangeLog::open().expect("open log");

        let err = log
            .append(
                ChangeTable::Expenses {
                    year: 2026,
                    month: 13,
                },
                "row-1",
                ChangeOp::Delete,
            )
            .expect_err("month 13 has no file");

        assert!(err.to_string().contains("month must be between 1 and 12"));
        assert!(entries().is_empty(), "the line must not be written");

        drop(log);
        std::fs::write(
            changelog_path().unwrap(),
            format!(
                "{}\n",
                line_with(
                    hlc(7_000, 1),
                    "\"table\":\"expenses\",\"year\":2026,\"month\":13,\"row_id\":\"row-1\",\
                     \"op\":\"delete\""
                )
            ),
        )
        .expect("write line");
        let read = read_log().expect("read the log");
        assert!(read.entries.is_empty());
        assert_eq!(read.corrupt_lines, vec![1]);
        // The clock still moves past it: the stamp was issued.
        assert_eq!(read.max_stamp, Some(hlc(7_000, 1)));
    }

    /// A value JSON cannot carry is refused where the number is still a float.
    /// Once it is a `Value` the information is gone, which is why the check
    /// cannot live in `append`.
    #[test]
    fn a_non_finite_number_is_refused() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = cell_number(bad).expect_err("JSON has no such number");
            assert!(err.to_string().contains("finite"), "{err}");
            // The evidence for the doc comment: by the time it is a `Value`,
            // a non-finite number is indistinguishable from a cleared cell.
            assert_eq!(Value::from(bad), Value::Null);
        }
        assert_eq!(cell_number(12.5).unwrap(), Value::from(12.5));
    }

    /// An empty or missing log reads as nothing at all.
    #[test]
    #[serial_test::serial]
    fn missing_log_reads_as_empty() {
        let _temp = with_temp_env();

        let read = read_log().expect("read the log");

        assert_eq!(read, LogRead::default());
        assert!(!changelog_path().unwrap().exists());
    }

    /// A reopened log carries on from the largest stamp in the file, not from
    /// the wall clock. The seeded line is dated 2096, so an `open` that
    /// ignored the file would fail this.
    #[test]
    #[serial_test::serial]
    fn a_reopened_log_continues_from_the_largest_stamp() {
        let _temp = with_temp_env();
        let seeded = far_future(3);
        std::fs::write(
            changelog_path().unwrap(),
            format!("{}\n", readable_line(seeded, "row-0")),
        )
        .expect("seed the log");
        let log = ChangeLog::open().expect("open log");
        let first = log
            .append(
                ChangeTable::CreditsDebts { year: 2026 },
                "row-1",
                ChangeOp::Delete,
            )
            .expect("append");
        drop(log);

        let log = ChangeLog::open().expect("reopen log");
        let second = log
            .append(
                ChangeTable::CreditsDebts { year: 2026 },
                "row-1",
                ChangeOp::Delete,
            )
            .expect("append");

        assert!(first.stamp > seeded, "the first append must pass the file");
        assert!(second.stamp > first.stamp, "the reopen must pass the file");
        assert_eq!(second.device_id, first.device_id);
        assert_eq!(entries().len(), 3);
    }

    /// Only one handle at a time may append. Two would recover the same stamp
    /// and then issue it twice, which is exactly what the format must never
    /// allow. The lock belongs to the open handle, so a second handle in this
    /// process is refused the same way another process is.
    #[test]
    #[serial_test::serial]
    fn a_second_writer_is_refused_while_the_first_holds_the_log() {
        let _temp = with_temp_env();
        let path = changelog_path().expect("changelog path");
        let first = ChangeLog::open().expect("open log");

        let err = ChangeLog::open().expect_err("a second writer must be refused");

        assert!(
            matches!(&err, Error::SyncLogLocked { path: held } if held == &path),
            "{err}"
        );
        assert!(err.to_string().contains("already open"), "{err}");
        assert!(err.to_string().contains("the same stamps"), "{err}");
        drop(first);
        ChangeLog::open().expect("the lock goes with the handle");
    }

    /// Appends from several threads through one shared log write every line
    /// and hand out stamps that only increase.
    #[test]
    #[serial_test::serial]
    fn concurrent_appends_keep_every_line_and_increase_the_stamp() {
        let _temp = with_temp_env();
        let log = shared_log().expect("shared log");
        let threads = 8;
        let per_thread = 50;

        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    for index in 0..per_thread {
                        log.append(
                            ChangeTable::Expenses {
                                year: 2026,
                                month: 3,
                            },
                            format!("row-{thread}-{index}"),
                            ChangeOp::Delete,
                        )
                        .expect("append");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread finished");
        }

        let read = read_log().expect("read the log");
        assert_eq!(read.entries.len(), threads * per_thread);
        assert!(read.corrupt_lines.is_empty());
        for pair in read.entries.windows(2) {
            assert!(
                pair[0].stamp < pair[1].stamp,
                "stamps must increase in file order"
            );
        }
        let rows: HashSet<&str> = read.entries.iter().map(|e| e.row_id.as_str()).collect();
        assert_eq!(rows.len(), threads * per_thread, "a line was lost");
        // The same shared value serves every caller, so no second handle can
        // stamp behind this one's back.
        assert!(Arc::ptr_eq(&log, &shared_log().expect("shared log again")));
    }
}
