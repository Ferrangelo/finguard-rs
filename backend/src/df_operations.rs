//! Core dataframe operations, ported from the original Python `df_operations.py`
//! plus the expense edit/delete/filter logic that lived in `ui_expenses.py`.
//!
//! All tables are stored as parquet files under the standard XDG data path
//! (see [`crate::paths`]). Schemas are kept byte-compatible with the Python
//! application at <https://github.com/Ferrangelo/finguard> so the two can read each
//! other's parquet files: renaming, retyping, or reordering a column here is a
//! cross-project data-contract change, not a local refactor, and must be
//! checked against that project first.
//!
//! Exception: the synced tables also carry a [`ROW_ID_COLUMN`] column that
//! the Python application never wrote. See that constant for which tables
//! have it and how it is maintained.
//!
//! Every method here that persists a change also records it in the sync
//! change log ([`crate::sync`]), right after the write that saved it. See
//! "Change log hooks" below for the rules those hooks follow, and
//! [`crate::sync_baseline`] for the startup pass that records the rows a
//! data folder already holds.

use std::collections::HashSet;

use chrono::NaiveDate;
use polars::functions::concat_df_diagonal;
use polars::prelude::*;
use serde_json::{Map, Value};

use crate::config;
use crate::error::{Error, Result};
use crate::paths::{
    CASHFLOW_FILENAME, CREDITS_DEBTS_FILENAME, INVESTMENTS_FILENAME, INVESTMENTS_PRICES_FILENAME,
    LIQUIDITY_FILENAME, PRIMARIES_FILENAME, RECURRING_EXPENSES_FILENAME, SECONDARIES_FILENAME,
    get_dbs_root, get_monthly_parquet_path, get_year_summary_path, year_month_from_parquet_path,
};
use crate::sync::{self, ChangeOp, ChangeOrigin, ChangeTable, IncompleteReason};

// ======================================================================
// Constants
// ======================================================================

/// Special-case category-name mappings (lower-cased key → canonical display
/// value). Mirrors the Python `_SPECIAL_CASES` dict in
/// `src/finguard/df_operations.py` in <https://github.com/Ferrangelo/finguard>, entry for
/// entry, including its irregular internal capitalization (e.g. `"MrStuff"`,
/// `"TechDonations"`). These spellings already exist in user data and in the
/// Python UI, so this list must stay a literal copy rather than a
/// "corrected" or alphabetized version.
fn special_cases() -> &'static [(&'static str, &'static str)] {
    &[
        ("tv", "TV"),
        ("otherexpenses", "OtherExpenses"),
        ("mrstuff", "MrStuff"),
        ("techdonations", "TechDonations"),
        ("othergroceries", "OtherGroceries"),
        ("condofee", "CondoFee"),
        ("takeaway", "TakeAway"),
        ("mrclothing", "MrClothing"),
        ("mrbooks", "MrBooks"),
        ("mrleisure", "MrLeisure"),
        ("mrlearning", "MrLearning"),
        ("otherleisure", "OtherLeisure"),
        ("otherfees", "OtherFees"),
        ("unatantum", "Unatantum"),
        ("charityenv", "CharityEnv"),
        ("charityhum", "CharityHum"),
        ("patreon-like", "Patreon-Like"),
    ]
}

/// Row labels for the income categories (user-editable values).
pub(crate) const INCOME_CATEGORIES: &[&str] = &[
    "Salary",
    "Interests Bank account",
    "Dividendi e Cedole",
    "Other",
];

/// Row labels for the derived cashflow categories. Only `Income` is still
/// recomputed, from the income categories above. `Spending`, `Saving`, and
/// `Saving %` stay in this list only so a freshly created table keeps the
/// same row set as an older file that persisted them from
/// `primaries.parquet`; see [`Cashflow::recompute`].
const DERIVED_CATEGORIES: &[&str] = &["Income", "Spending", "Saving", "Saving %"];

/// Valid investment categories.
const INVESTMENT_CATEGORIES: &[&str] = &["Stocks/ETF", "Commodities", "Bonds"];

/// Valid liquidity categories.
const LIQUIDITY_CATEGORIES: &[&str] = &["Bank/Broker account", "Cash", "Other"];

/// Month column labels (`"01"`..`"12"`) used in wide yearly tables.
pub(crate) fn month_labels() -> Vec<String> {
    (1..=12).map(|m| format!("{m:02}")).collect()
}

/// Stable row ID column of the synced tables: the monthly detailed
/// expenses, recurring expenses, investments and investment prices,
/// liquidity, and credits/debts. `cashflow.parquet` has none (its category
/// name is the key), nor do the legacy `primaries.parquet` and
/// `secondaries.parquet` summaries.
///
/// Each value is a random UUID v4 string from [`new_row_id`], except a row
/// `POST /api/recurring/apply` generates: that ID is derived from the
/// recurring template and the target month (see
/// [`RecurringExpenses::pending_for_month`]), so the desktop and the phone
/// produce the same ID for the same template and month. Edits, renames, and
/// cell updates keep whichever ID a row already has. An investment asset has
/// one ID, shared by its `investments.parquet` and `investments_prices.parquet`
/// rows. Only [`crate::row_id_migration`] assigns IDs to rows that lack one;
/// every loader here rejects such a file with [`Error::RowIdsMissing`].
pub const ROW_ID_COLUMN: &str = "row_id";

/// Return a fresh random row ID for [`ROW_ID_COLUMN`].
pub fn new_row_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Whether `df` still needs [`crate::row_id_migration`]: it has no
/// [`ROW_ID_COLUMN`], the column is not a string column, or a row's value
/// is null or empty.
pub(crate) fn needs_row_ids(df: &DataFrame) -> Result<bool> {
    if !has_column(df, ROW_ID_COLUMN) {
        return Ok(true);
    }
    let ids = df.column(ROW_ID_COLUMN)?;
    if ids.dtype() != &DataType::String {
        return Ok(true);
    }
    Ok(ids.str()?.iter().any(|id| id.is_none_or(str::is_empty)))
}

/// Fail with [`Error::RowIdsMissing`] when `df`, just read from `path`,
/// still needs row IDs (see [`needs_row_ids`]).
fn require_row_ids(df: &DataFrame, path: &std::path::Path) -> Result<()> {
    if needs_row_ids(df)? {
        return Err(Error::RowIdsMissing(format!(
            "{} has rows without a row ID. Restart the finguard backend: at startup it backs \
             up the data folder and assigns the missing row IDs.",
            path.display()
        )));
    }
    Ok(())
}

/// Succeed when exactly one row of `df` has `row_id` as its
/// [`ROW_ID_COLUMN`] value, so an edit or delete by that ID touches one row.
///
/// Returns [`Error::NotFound`] when no row matches. Returns
/// [`Error::AlreadyExists`], the uniqueness violation, when several rows
/// share the ID: the caller must then change nothing rather than edit or
/// delete every copy. `kind` names the row kind (for example `"expense"`)
/// and `place` the table (for example `"2026-09"`) in the message.
fn require_single_row(df: &DataFrame, row_id: &str, kind: &str, place: &str) -> Result<()> {
    let matches = df
        .column(ROW_ID_COLUMN)?
        .str()?
        .iter()
        .filter(|id| *id == Some(row_id))
        .count();
    match matches {
        1 => Ok(()),
        0 => Err(Error::NotFound(format!(
            "No {kind} with id '{row_id}' in {place}."
        ))),
        n => Err(Error::AlreadyExists(format!(
            "{n} {kind} rows in {place} share the id '{row_id}', so nothing was changed. \
             Each row ID must be unique; the data file needs repair."
        ))),
    }
}

/// Whether any row of `df` already has `row_id` as its [`ROW_ID_COLUMN`]
/// value.
fn has_row_id(df: &DataFrame, row_id: &str) -> Result<bool> {
    Ok(df
        .column(ROW_ID_COLUMN)?
        .str()?
        .iter()
        .any(|id| id == Some(row_id)))
}

// ======================================================================
// Free functions
// ======================================================================

/// Return the canonical casing for a category string.
///
/// Special-cased names are mapped to their canonical spelling; otherwise the
/// value is lower-cased and its first character upper-cased.
pub fn normalize_category_value(value: &str) -> String {
    let lower = value.to_lowercase();
    for (k, v) in special_cases() {
        if *k == lower {
            return (*v).to_string();
        }
    }
    if lower.is_empty() {
        return lower;
    }
    let mut chars = lower.chars();
    // first char upper-cased, rest unchanged (already lower-cased)
    let first = chars.next().unwrap();
    let upper: String = first.to_uppercase().collect();
    format!("{upper}{}", chars.as_str())
}

/// Match `value` case-insensitively against the `existing` category set.
///
/// If a match is found the existing spelling is returned; otherwise this falls
/// back to [`normalize_category_value`].
pub fn resolve_category(value: &str, existing: &HashSet<String>) -> String {
    if value.is_empty() {
        return value.to_string();
    }
    let lower = value.to_lowercase();
    for c in existing {
        if c.to_lowercase() == lower {
            return c.clone();
        }
    }
    normalize_category_value(value)
}

/// Rewrite every legacy `"E"` value in `df`'s `currency` column to `"EUR"`.
///
/// Older parquet files (and older seed data) stored the euro as the
/// single-letter code `"E"`. New rows always write `"EUR"`, but a table
/// loaded from disk can still carry the legacy value, so every load path maps
/// it forward here instead of leaking it to the API. A no-op when the column
/// is absent or the table has no rows, so callers can apply it unconditionally.
fn normalize_currency_column(df: DataFrame) -> Result<DataFrame> {
    if !has_column(&df, "currency") || df.height() == 0 {
        return Ok(df);
    }
    Ok(df
        .lazy()
        .with_column(
            when(col("currency").eq(lit("E")))
                .then(lit("EUR"))
                .otherwise(col("currency"))
                .alias("currency"),
        )
        .collect()?)
}

/// Delete the row for `name` from every year-summary parquet file of `kind`.
///
/// This is a permanent write operation. Call only after confirming the
/// category total is `0.0`. Per-file errors are swallowed (mirroring Python).
///
/// `kind` must be `"primary"` or `"secondary"`.
pub fn remove_category_from_all_summaries(name: &str, kind: &str) -> Result<()> {
    if kind != "primary" && kind != "secondary" {
        return Err(Error::InvalidArgument(format!(
            "kind must be 'primary' or 'secondary', got '{kind}'"
        )));
    }
    let filename = if kind == "primary" {
        PRIMARIES_FILENAME
    } else {
        SECONDARIES_FILENAME
    };
    let category_col = format!("{kind}_category");

    let _ = (|| -> Result<()> {
        let root = get_dbs_root()?;
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path().join(filename);
            if !path.exists() {
                continue;
            }
            let df = match read_parquet(&path) {
                Ok(df) => df,
                Err(_) => continue,
            };
            if !df
                .get_column_names()
                .iter()
                .any(|c| c.as_str() == category_col)
            {
                continue;
            }
            let original_height = df.height();
            let filtered = df
                .lazy()
                .filter(col(category_col.as_str()).neq(lit(name)))
                .collect()?;
            if filtered.height() != original_height {
                write_parquet(&filtered, &path)?;
            }
        }
        Ok(())
    })();

    Ok(())
}

// ======================================================================
// Parquet I/O helpers
// ======================================================================

/// Read a parquet file eagerly from `path`.
pub(crate) fn read_parquet(path: &std::path::Path) -> Result<DataFrame> {
    let file = std::fs::File::open(path)?;
    Ok(ParquetReader::new(file).finish()?)
}

/// Write `df` to `path` as a parquet file.
fn write_parquet(df: &DataFrame, path: &std::path::Path) -> Result<()> {
    let mut df = df.clone();
    let file = std::fs::File::create(path)?;
    ParquetWriter::new(file).finish(&mut df)?;
    Ok(())
}

/// Write `df` to `path` through a temporary file in the same folder and a
/// rename, then flush the folder, so a crash leaves either the old file or
/// the complete new one.
///
/// Used by the startup row ID migration and by a merge from another device,
/// the two writers that change files the user did not just touch. The
/// handlers still save through [`write_parquet`].
///
/// # Errors
///
/// [`Error::Io`] or [`Error::Polars`] from creating, writing, flushing, or
/// renaming the file, or from flushing the folder. After a failure the old
/// file is still in place, unless the failure was the folder flush, which
/// comes after the rename.
pub(crate) fn write_parquet_atomic(df: &DataFrame, path: &std::path::Path) -> Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp_path = path.with_file_name(format!(".{file_name}.tmp"));

    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&temp_path)?;
        let mut df = df.clone();
        ParquetWriter::new(&mut file).finish(&mut df)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if result.is_err() {
        // The write error is the one to report. A leftover temporary file
        // is harmless: no loader reads a name starting with a dot.
        let _ = std::fs::remove_file(&temp_path);
    }
    result?;
    match path.parent() {
        Some(folder) => Ok(sync_dir(folder)?),
        None => Ok(()),
    }
}

/// Flush the entries of the folder `path` to disk, so a file created or
/// renamed in it survives a crash. Only Unix can open a folder to flush it,
/// so elsewhere this does nothing.
pub(crate) fn sync_dir(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Return whether `df` contains a column named `name`.
pub(crate) fn has_column(df: &DataFrame, name: &str) -> bool {
    df.get_column_names().iter().any(|c| c.as_str() == name)
}

/// Read a string column into an owned `Vec<String>` (nulls become empty).
pub(crate) fn str_col_to_vec(df: &DataFrame, name: &str) -> Result<Vec<String>> {
    Ok(df
        .column(name)?
        .str()?
        .iter()
        .map(|o| o.unwrap_or("").to_string())
        .collect())
}

/// Read a `Date` column into an owned `Vec<NaiveDate>`.
/// Read a date column as calendar dates, preserving nulls.
///
/// Null stays null: a missing date is "no date", never 1970-01-01. Callers
/// decide what a dateless row means; this layer must not invent a date.
fn date_col_to_vec(df: &DataFrame, name: &str) -> Result<Vec<Option<NaiveDate>>> {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let days = df.column(name)?.cast(&DataType::Int32)?;
    Ok(days
        .i32()?
        .iter()
        .map(|o| o.map(|days| epoch + chrono::Duration::days(days as i64)))
        .collect())
}

// ======================================================================
// Change log hooks
// ======================================================================
//
// Every mutator in this module records what it changed in the sync change
// log ([`crate::sync`]), immediately after the write that saved it. The hooks
// sit here rather than in the API handlers because a handler can be added
// later and forget to log, while nothing reaches a Parquet file without
// passing through this module.
//
// Which shape a change takes is a decision from the step 7 design, not a
// local choice: an expense and a recurring template are recorded as whole
// rows ([`ChangeOp::Upsert`], [`ChangeOp::Delete`]) because their fields are
// edited together as one form, while the net worth tables and cashflow
// income are recorded per cell ([`ChangeOp::Cell`]) because two devices
// filling in different months of the same asset is ordinary use, not a
// conflict. A net worth row that is created or removed still needs a whole
// row or a delete: those two are the only entries on those tables that are
// not cells.

/// Record `op` on `row_id` of `table`, after the save that put the change on
/// disk. See [`record_with_origin`] for the failure contract.
fn record(table: ChangeTable, row_id: &str, op: Result<ChangeOp>) {
    record_with_origin(table, row_id, op, None);
}

/// Record `op` on `row_id` of `table`, marked with `origin` when the user did
/// not make the change directly. See [`ChangeOrigin`].
///
/// This never fails the caller and never undoes the save. The change is
/// already on disk, so returning the error would invite a retry that writes
/// the change a second time, and rolling the save back would leave the log
/// describing data no file holds: a failed append can still have put a
/// complete line on disk (see [`crate::sync::ChangeLog::append`]). A failure
/// is warned about on stderr instead, and marks the log incomplete (see
/// [`crate::sync::log_incomplete_path`]), so sync refuses to trust it until a
/// repair. Writing the marker is best effort too: when it fails, the warning
/// says so.
///
/// The warning names the table, its year, and the reason the append failed.
/// That reason is an error message from this crate, from Polars, or from
/// `serde_json`: this crate's can name the row ID, and the lower two can
/// quote the value they could not handle. So the warning is diagnostic output
/// for the user's own machine and goes nowhere else. It never carries the
/// entry itself.
fn record_with_origin(
    table: ChangeTable,
    row_id: &str,
    op: Result<ChangeOp>,
    origin: Option<ChangeOrigin>,
) {
    // The table is moved into the append, and a warning needs it afterwards.
    // It holds only a year and a month, so this copy costs nothing.
    let described = table.clone();
    let appended = op.and_then(|op| {
        let log = sync::shared_log()?;
        match origin {
            Some(origin) => log.append_with_origin(table, row_id, op, origin),
            None => log.append(table, row_id, op),
        }
    });
    if let Err(err) = appended {
        // The marker is what lets sync find out later: without it nothing
        // records that the log and the data now disagree. Best effort, for
        // the same reason the append failure itself is only a warning.
        let marked = match sync::mark_log_incomplete(IncompleteReason::AppendFailed) {
            Ok(()) => {
                "The log is marked incomplete, so sync stops until it is repaired.".to_string()
            }
            Err(mark_err) => format!(
                "Marking the log incomplete failed too ({mark_err}), so sync cannot detect this \
                 gap by itself."
            ),
        };
        eprintln!(
            "Change log: the change to {described:?} is saved, but recording it failed ({err}). \
             This device's log no longer describes all of its own data, so a later sync can miss \
             that change. {marked}"
        );
    }
}

/// A [`ChangeOp::Cell`] holding a text value, such as a renamed asset.
fn text_cell(column: &str, value: &str) -> ChangeOp {
    ChangeOp::Cell {
        column: column.to_string(),
        value: Value::from(value),
    }
}

/// A [`ChangeOp::Cell`] holding a number, built through
/// [`crate::sync::cell_number`] so a NaN or an infinity is refused here
/// instead of reaching the log as a cleared cell.
fn number_cell(column: &str, value: f64) -> Result<ChangeOp> {
    Ok(ChangeOp::Cell {
        column: column.to_string(),
        value: sync::cell_number(value)?,
    })
}

/// A [`ChangeOp::Upsert`] holding every column of the row of `df` whose
/// [`ROW_ID_COLUMN`] is `row_id`.
fn upsert_row(df: &DataFrame, row_id: &str) -> Result<ChangeOp> {
    let index = df
        .column(ROW_ID_COLUMN)?
        .str()?
        .iter()
        .position(|id| id == Some(row_id))
        .ok_or_else(|| {
            Error::NotFound(format!(
                "the row with id '{row_id}' is not in the table it was just saved to, so the \
                 change log has no row to record"
            ))
        })?;
    Ok(ChangeOp::Upsert {
        row: row_json(df, index)?,
    })
}

/// Every column of row `index` of `df` as one JSON object.
///
/// The keys are the Parquet file's own column names and the values keep the
/// file's own meaning, because the other device applies the object to the
/// same schema: text stays a string, a number stays a JSON number, a date
/// becomes the ISO 8601 day it names (`"2026-03-04"`), and an empty cell
/// becomes `null`. A column of any other type is an error naming the column
/// and its type, never its contents.
pub(crate) fn row_json(df: &DataFrame, index: usize) -> Result<Map<String, Value>> {
    let mut row = Map::new();
    for column in df.columns() {
        let name = column.name().as_str();
        row.insert(name.to_string(), json_cell(name, column.get(index)?)?);
    }
    Ok(row)
}

/// One cell of `column` as JSON. See [`row_json`] for the encoding.
fn json_cell(column: &str, value: AnyValue<'_>) -> Result<Value> {
    Ok(match value {
        AnyValue::Null => Value::Null,
        AnyValue::String(text) => Value::from(text),
        AnyValue::StringOwned(text) => Value::from(text.as_str()),
        AnyValue::Boolean(flag) => Value::from(flag),
        AnyValue::Float32(number) => sync::cell_number(number as f64)?,
        AnyValue::Float64(number) => sync::cell_number(number)?,
        AnyValue::Int32(number) => Value::from(number),
        AnyValue::Int64(number) => Value::from(number),
        AnyValue::Date(days) => Value::from(date_from_days(days).to_string()),
        other => {
            return Err(Error::InvalidArgument(format!(
                "the '{column}' column holds {} values, which the change log cannot carry",
                other.dtype()
            )));
        }
    })
}

/// The date `days` after the Unix epoch, the meaning of a Polars `Date` cell.
fn date_from_days(days: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 is a real date")
        + chrono::Duration::days(days as i64)
}

/// The [`ROW_ID_COLUMN`] value of every row of `df` where `key_col` equals
/// `key`.
///
/// The wide tables are keyed by name in the API, and [`set_str_where`] and
/// [`set_f64_where`] update every row that matches, so a change to a name
/// held by two rows is two changes and gets two entries. Call this before the
/// change, so a delete still has the IDs of the rows it is about to remove.
fn row_ids_where(df: &DataFrame, key_col: &str, key: &str) -> Result<Vec<String>> {
    let keys = df.column(key_col)?.str()?;
    let ids = df.column(ROW_ID_COLUMN)?.str()?;
    Ok(keys
        .iter()
        .zip(ids.iter())
        .filter(|(name, _)| *name == Some(key))
        .filter_map(|(_, id)| id.map(str::to_string))
        .collect())
}

/// Build a `Date`-typed column from a list of [`NaiveDate`] values.
fn date_series(name: &str, dates: &[NaiveDate]) -> Series {
    let days: Vec<i32> = dates
        .iter()
        .map(|d| {
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            (*d - epoch).num_days() as i32
        })
        .collect();
    Int32Chunked::from_vec(name.into(), days)
        .into_series()
        .cast(&DataType::Date)
        .expect("i32 -> Date cast is always valid")
}

// ======================================================================
// DetailedExpenses
// ======================================================================

/// Build an empty detailed-expenses dataframe with the canonical schema.
///
/// The schema stores only the facts of an expense: its own amount and
/// currency. It deliberately holds no reference-currency amount or FX rate;
/// those are derived at read time from `expense_amount`/`currency` (see
/// [`ExpenseFact`] and [`resolve_fact`]) so that changing the reference
/// currency never mislabels a stored figure.
fn empty_expenses_df() -> DataFrame {
    DataFrame::empty_with_schema(&Schema::from_iter([
        Field::new("expense_name".into(), DataType::String),
        Field::new("expense_date".into(), DataType::Date),
        Field::new("expense_amount".into(), DataType::Float64),
        Field::new("currency".into(), DataType::String),
        Field::new("primary_category".into(), DataType::String),
        Field::new("secondary_category".into(), DataType::String),
        Field::new(ROW_ID_COLUMN.into(), DataType::String),
    ]))
}

/// Manage a monthly detailed-expenses parquet file.
///
/// The file lives at
/// `$XDG_DATA_HOME/finguard/dbs/<year>/MM_detailed_expenses.parquet`.
pub struct DetailedExpenses {
    /// Calendar year of this table.
    pub year: i32,
    /// Month number (1–12) of this table.
    pub month: u32,
    /// Filesystem path of the backing parquet file.
    pub expense_df_path: std::path::PathBuf,
    /// In-memory dataframe of expense rows.
    pub expense_df: DataFrame,
}

impl DetailedExpenses {
    /// Construct from a `year` and `month`, loading the parquet file if it
    /// exists or initialising an empty table otherwise. Returns
    /// [`Error::RowIdsMissing`] for a file with a row that has no
    /// [`ROW_ID_COLUMN`] value.
    pub fn new(year: i32, month: u32) -> Result<Self> {
        let path = get_monthly_parquet_path(year, month)?;
        Self::load(year, month, path)
    }

    /// Construct from an explicit parquet path, inferring `year`/`month` from it.
    pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let (year, month) = year_month_from_parquet_path(&path)?;
        Self::load(year, month, path)
    }

    fn load(year: i32, month: u32, path: std::path::PathBuf) -> Result<Self> {
        let expense_df = if path.exists() {
            let mut df = read_parquet(&path)?;
            require_row_ids(&df, &path)?;
            // Ensure expense_date is Date (older files may store it as Int64).
            if has_column(&df, "expense_date")
                && df.column("expense_date")?.dtype() != &DataType::Date
            {
                df = df
                    .lazy()
                    .with_column(col("expense_date").cast(DataType::Date))
                    .collect()?;
            }
            // A file written before this schema simplification may still
            // carry `expense_in_ref_currency`/`fx_rate`/`rate_date`. Drop
            // them rather than erroring: they are derived at read time now
            // (see `ExpenseFact`/`resolve_fact`), and the file loses them for
            // good the next time it is written.
            for stale_column in ["expense_in_ref_currency", "fx_rate", "rate_date"] {
                if has_column(&df, stale_column) {
                    df = df.drop(stale_column)?;
                }
            }
            normalize_currency_column(df)?
        } else {
            empty_expenses_df()
        };
        Ok(Self {
            year,
            month,
            expense_df_path: path,
            expense_df,
        })
    }

    /// Append an expense row with a fresh [`ROW_ID_COLUMN`] value, save the
    /// updated dataframe, and return the new row's ID.
    ///
    /// If `primary_category` or `secondary_category` are `None`, they are
    /// resolved from the category-mappings config. Returns
    /// [`Error::InvalidArgument`] when `primary_category` cannot be resolved.
    pub fn add_row(
        &mut self,
        expense_name: &str,
        expense_day: u32,
        expense_amount: f64,
        primary_category: Option<&str>,
        currency: &str,
        secondary_category: Option<&str>,
    ) -> Result<String> {
        let row_id = new_row_id();
        self.add_row_with_id(
            &row_id,
            expense_name,
            expense_day,
            expense_amount,
            primary_category,
            currency,
            secondary_category,
            None,
        )?;
        Ok(row_id)
    }

    /// Append an expense row under the caller-supplied `row_id` instead of a
    /// fresh one, save the updated dataframe, and record it in the change log
    /// under `origin`.
    ///
    /// [`Self::add_row`] is the ordinary entry point: it generates its own ID
    /// and passes `None` as the origin, because the user typed the row. This
    /// exists so [`RecurringExpenses::insert_resolved`] can give a generated
    /// row the ID derived from its template and target month, and mark it
    /// [`ChangeOrigin::RecurringApply`]. That mark is the only thing telling
    /// a generated row from a hand typed one, since both carry the same
    /// derived ID, and it cannot be recovered after the entry is written.
    ///
    /// The caller owns `row_id` uniqueness: this method does not check
    /// whether `row_id` already appears in `self.expense_df`, it only
    /// appends. [`require_single_row`] depends on every row's ID being
    /// unique within the month, so a caller that inserts a duplicate ID
    /// leaves that row (and the one it collides with) permanently rejected
    /// by every future edit or delete.
    #[allow(clippy::too_many_arguments)]
    fn add_row_with_id(
        &mut self,
        row_id: &str,
        expense_name: &str,
        expense_day: u32,
        expense_amount: f64,
        primary_category: Option<&str>,
        currency: &str,
        secondary_category: Option<&str>,
        origin: Option<ChangeOrigin>,
    ) -> Result<()> {
        let mut primary = primary_category.map(|s| s.to_string());
        let mut secondary = secondary_category.map(|s| s.to_string());

        if primary.is_none() || secondary.is_none() {
            match config::get_mapping(expense_name)? {
                Some(mapping) => {
                    if primary.is_none() {
                        primary = Some(mapping.primary_category);
                    }
                    if secondary.is_none() {
                        secondary = Some(mapping.secondary_category);
                    }
                }
                None => {
                    if primary.is_none() {
                        return Err(Error::InvalidArgument(format!(
                            "No category mapping found for '{expense_name}' and no \
                             primary_category was provided. Either add a mapping via \
                             config.add_mapping() or pass primary_category explicitly."
                        )));
                    }
                    if secondary.is_none() {
                        secondary = Some(String::new());
                    }
                }
            }
        }

        let primary = primary.unwrap();
        let secondary = secondary.unwrap();

        let date =
            NaiveDate::from_ymd_opt(self.year, self.month, expense_day).ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "invalid date {}-{:02}-{:02}",
                    self.year, self.month, expense_day
                ))
            })?;

        let new_row = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &[expense_name]),
            date_series("expense_date", &[date]).into(),
            Column::new("expense_amount".into(), &[expense_amount]),
            Column::new("currency".into(), &[currency]),
            Column::new(
                "primary_category".into(),
                &[normalize_category_value(&primary)],
            ),
            Column::new(
                "secondary_category".into(),
                &[normalize_category_value(&secondary)],
            ),
            Column::new(ROW_ID_COLUMN.into(), &[row_id]),
        ])?;

        self.expense_df = concat_df_diagonal(&[self.expense_df.clone(), new_row])?;
        write_parquet(&self.expense_df, &self.expense_df_path)?;
        record_with_origin(
            self.change_table(),
            row_id,
            upsert_row(&self.expense_df, row_id),
            origin,
        );
        Ok(())
    }

    /// Where this month's rows live, for the change log.
    fn change_table(&self) -> ChangeTable {
        ChangeTable::Expenses {
            year: self.year,
            month: self.month,
        }
    }

    /// Succeed when exactly one row of this month has `row_id`; see
    /// [`require_single_row`] for the errors.
    fn require_row(&self, row_id: &str) -> Result<()> {
        require_single_row(
            &self.expense_df,
            row_id,
            "expense",
            &format!("{}-{:02}", self.year, self.month),
        )
    }

    /// Remove the row whose [`ROW_ID_COLUMN`] is `row_id`, then save.
    /// Returns [`Error::NotFound`] when this month has no such row, and
    /// [`Error::AlreadyExists`] without changing anything when several rows
    /// share that ID.
    pub fn delete_row(&mut self, row_id: &str) -> Result<()> {
        self.require_row(row_id)?;
        self.expense_df = self
            .expense_df
            .clone()
            .lazy()
            .filter(col(ROW_ID_COLUMN).neq(lit(row_id)))
            .collect()?;
        write_parquet(&self.expense_df, &self.expense_df_path)?;
        record(self.change_table(), row_id, Ok(ChangeOp::Delete));
        Ok(())
    }

    /// Build the set of existing categories for `category_col`
    /// (`"primary_category"` or `"secondary_category"`), used to resolve edited
    /// category values against current spellings.
    ///
    /// Mirrors the Python edit dialog's `existing_pri` / `existing_sec`
    /// construction: the non-null, non-empty values from the current month's
    /// `expense_df`, augmented with the known categories registered in config
    /// (the `"primary"` known set for the primary column, `"secondary"` for the
    /// secondary column). A missing/unreadable config is treated as no known
    /// categories rather than an error.
    fn existing_category_set(&self, category_col: &str) -> HashSet<String> {
        let mut set: HashSet<String> = HashSet::new();

        if let Ok(s) = self.expense_df.column(category_col).and_then(|c| c.str()) {
            for opt in s.iter().flatten() {
                if !opt.is_empty() {
                    set.insert(opt.to_string());
                }
            }
        }

        if let Ok(known) = config::get_known_categories() {
            let list = if category_col == "primary_category" {
                &known.primary
            } else {
                &known.secondary
            };
            for c in list {
                if !c.is_empty() {
                    set.insert(c.clone());
                }
            }
        }

        set
    }

    /// Apply field edits to the row whose [`ROW_ID_COLUMN`] is `row_id`,
    /// then save. The row keeps its ID. Returns [`Error::NotFound`] when this
    /// month has no such row, and [`Error::AlreadyExists`] without changing
    /// anything when several rows share that ID.
    ///
    /// This ports the `save_edit` logic from `ui_expenses.py`: any field left
    /// `None` is unchanged; `expense_day` rebuilds `expense_date` from the
    /// table's year/month.
    ///
    /// Categories are resolved via [`resolve_category`] (not
    /// [`normalize_category_value`]): the edited value is first matched
    /// case-insensitively against the **existing** category set (the current
    /// month's `primary_category` / `secondary_category` values plus the known
    /// categories from [`config::get_known_categories`]), preserving the existing
    /// spelling, and only normalized on no match. This mirrors the Python edit
    /// dialog, which builds `existing_pri` / `existing_sec` and calls
    /// `resolve_category`.
    #[allow(clippy::too_many_arguments)]
    pub fn edit_row(
        &mut self,
        row_id: &str,
        expense_name: Option<&str>,
        expense_day: Option<u32>,
        expense_amount: Option<f64>,
        currency: Option<&str>,
        primary_category: Option<&str>,
        secondary_category: Option<&str>,
    ) -> Result<()> {
        self.require_row(row_id)?;
        let mut df = self.expense_df.clone().lazy();

        let pred = col(ROW_ID_COLUMN).eq(lit(row_id));

        if let Some(name) = expense_name {
            df = df.with_column(
                when(pred.clone())
                    .then(lit(name))
                    .otherwise(col("expense_name"))
                    .alias("expense_name"),
            );
        }
        if let Some(day) = expense_day {
            let date = NaiveDate::from_ymd_opt(self.year, self.month, day).ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "invalid date {}-{:02}-{:02}",
                    self.year, self.month, day
                ))
            })?;
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let days = (date - epoch).num_days() as i32;
            // Cast explicitly to Date to avoid dtype corruption (see Python note).
            df = df.with_column(
                when(pred.clone())
                    .then(lit(days).cast(DataType::Date))
                    .otherwise(col("expense_date"))
                    .alias("expense_date"),
            );
        }
        if let Some(amount) = expense_amount {
            df = df.with_column(
                when(pred.clone())
                    .then(lit(amount))
                    .otherwise(col("expense_amount"))
                    .alias("expense_amount"),
            );
        }
        if let Some(cur) = currency {
            df = df.with_column(
                when(pred.clone())
                    .then(lit(cur))
                    .otherwise(col("currency"))
                    .alias("currency"),
            );
        }

        if let Some(pri) = primary_category {
            let primary_set = self.existing_category_set("primary_category");
            df = df.with_column(
                when(pred.clone())
                    .then(lit(resolve_category(pri, &primary_set)))
                    .otherwise(col("primary_category"))
                    .alias("primary_category"),
            );
        }
        if let Some(sec) = secondary_category {
            let secondary_set = self.existing_category_set("secondary_category");
            df = df.with_column(
                when(pred.clone())
                    .then(lit(resolve_category(sec, &secondary_set)))
                    .otherwise(col("secondary_category"))
                    .alias("secondary_category"),
            );
        }

        self.expense_df = df.collect()?;
        write_parquet(&self.expense_df, &self.expense_df_path)?;
        record(
            self.change_table(),
            row_id,
            upsert_row(&self.expense_df, row_id),
        );
        Ok(())
    }

    /// Return a filtered copy of the expense dataframe. Each returned row
    /// keeps its [`ROW_ID_COLUMN`] value, which
    /// [`edit_row`](Self::edit_row) and [`delete_row`](Self::delete_row)
    /// take.
    ///
    /// `name_substr` matches `expense_name` case-insensitively; `category_substr`
    /// matches either category column case-insensitively; `amount_min`/`amount_max`
    /// bound `expense_amount`. Empty / `None` filters are ignored. Ports the
    /// filter logic from `ui_expenses.build_table`.
    pub fn filter_expenses(
        &self,
        name_substr: Option<&str>,
        category_substr: Option<&str>,
        amount_min: Option<f64>,
        amount_max: Option<f64>,
    ) -> Result<DataFrame> {
        let lf = Self::apply_expense_filters(
            self.expense_df.clone().lazy(),
            name_substr,
            category_substr,
            amount_min,
            amount_max,
        );
        Ok(lf.collect()?)
    }

    /// Apply the expense filter predicates of
    /// [`filter_expenses`](Self::filter_expenses) to a lazy frame. Empty /
    /// `None` filters are ignored.
    fn apply_expense_filters(
        mut lf: LazyFrame,
        name_substr: Option<&str>,
        category_substr: Option<&str>,
        amount_min: Option<f64>,
        amount_max: Option<f64>,
    ) -> LazyFrame {
        if let Some(name) = name_substr.filter(|s| !s.is_empty()) {
            let pat = format!("(?i){}", regex_escape(name));
            lf = lf.filter(col("expense_name").str().contains(lit(pat), false));
        }
        if let Some(cat) = category_substr.filter(|s| !s.is_empty()) {
            let pat = format!("(?i){}", regex_escape(cat));
            lf = lf.filter(
                col("primary_category")
                    .str()
                    .contains(lit(pat.clone()), false)
                    .or(col("secondary_category").str().contains(lit(pat), false)),
            );
        }
        if let Some(min) = amount_min {
            lf = lf.filter(col("expense_amount").gt_eq(lit(min)));
        }
        if let Some(max) = amount_max {
            lf = lf.filter(col("expense_amount").lt_eq(lit(max)));
        }
        lf
    }

    /// Return every row's [`ExpenseFact`]s, for computing reference-currency
    /// totals at read time (see the module docs on why this dataframe layer
    /// no longer stores a derived amount).
    pub fn expense_facts(&self) -> Result<Vec<ExpenseFact>> {
        facts_from_df(&self.expense_df)
    }

    /// Write `expense_df` atomically and record nothing in the change log.
    /// Only for [`crate::merge_apply`], which stores the other device's
    /// entries verbatim instead of recording its writes as local changes.
    pub(crate) fn save_merged(&self) -> Result<()> {
        write_parquet_atomic(&self.expense_df, &self.expense_df_path)
    }
}

// ======================================================================
// Read-time reference-currency conversion
// ======================================================================
//
// `expense_amount`/`currency` are the only facts a row stores; everything
// else about a reference-currency total is derived here, from an already
// (asynchronously) resolved rate table the caller builds via
// `distinct_rate_keys` and `crate::fx::rate_on`. Keeping the resolution step
// out of this module is what keeps `df_operations` synchronous and
// network-free (see the module docs).

/// One stored expense row's facts: its own amount and currency, with no
/// reference-currency amount or rate attached.
#[derive(Debug, Clone)]
pub struct ExpenseFact {
    /// The row's `expense_date`.
    pub expense_date: NaiveDate,
    /// The row's `currency`, as stored (not yet upper-cased/trimmed).
    pub currency: String,
    /// The row's `expense_amount`, in `currency`.
    pub expense_amount: f64,
    /// The row's `primary_category`.
    pub primary_category: String,
    /// The row's `secondary_category`.
    pub secondary_category: String,
}

/// Build [`ExpenseFact`]s from any dataframe carrying the detailed-expenses
/// columns (shared by [`DetailedExpenses::expense_facts`]).
///
/// Rows with no date are skipped: a dateless row has no rate key and no
/// display day, so it belongs to neither totals nor lists. This matches the
/// Expenses list, which already skips rows whose day is missing, and keeps
/// a dateless row from implicating its currency as unavailable.
fn facts_from_df(df: &DataFrame) -> Result<Vec<ExpenseFact>> {
    let dates = date_col_to_vec(df, "expense_date")?;
    let currencies = str_col_to_vec(df, "currency")?;
    let amounts: Vec<f64> = df
        .column("expense_amount")?
        .f64()?
        .iter()
        .map(|o| o.unwrap_or(0.0))
        .collect();
    let primaries = str_col_to_vec(df, "primary_category")?;
    let secondaries = str_col_to_vec(df, "secondary_category")?;
    Ok((0..dates.len())
        .filter_map(|i| {
            Some(ExpenseFact {
                expense_date: dates[i]?,
                currency: currencies[i].clone(),
                expense_amount: amounts[i],
                primary_category: primaries[i].clone(),
                secondary_category: secondaries[i].clone(),
            })
        })
        .collect())
}

/// Return every [`ExpenseFact`] for `year`'s existing monthly files. A month
/// with no parquet file yet is skipped rather than treated as an error,
/// matching [`DetailedExpenses::new`]'s "empty table" convention for it.
pub fn expense_facts_for_year(year: i32) -> Result<Vec<ExpenseFact>> {
    let mut facts = Vec::new();
    for month in 1..=12u32 {
        let path = get_monthly_parquet_path(year, month)?;
        if !path.exists() {
            continue;
        }
        facts.extend(DetailedExpenses::new(year, month)?.expense_facts()?);
    }
    Ok(facts)
}

/// Return every [`ExpenseFact`] across every year found under the data root.
///
/// Scans every year directory under [`get_dbs_root`], the same directory
/// walk [`remove_category_from_all_summaries`] uses; per-year read errors
/// are swallowed the same way (a year directory that cannot be read
/// contributes no facts rather than failing the whole scan).
///
/// The exception is [`Error::RowIdsMissing`], which is returned: skipping a
/// year that only waits for the row ID migration would silently understate
/// the totals that guard a category delete.
pub fn all_expense_facts() -> Result<Vec<ExpenseFact>> {
    let mut facts = Vec::new();
    let scan = (|| -> Result<()> {
        let root = get_dbs_root()?;
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(year) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
            else {
                continue;
            };
            match expense_facts_for_year(year) {
                Ok(year_facts) => facts.extend(year_facts),
                Err(err @ Error::RowIdsMissing(_)) => return Err(err),
                Err(_) => {}
            }
        }
        Ok(())
    })();
    match scan {
        Err(err @ Error::RowIdsMissing(_)) => Err(err),
        _ => Ok(facts),
    }
}

/// Every distinct `(expense_date, currency)` pair in `facts` whose currency
/// differs from `reference_currency` (which always converts at `1.0`, with
/// no lookup needed). The caller resolves each pair once via
/// [`crate::fx::rate_on`] and reuses it for every row that shares it,
/// instead of one lookup per row.
pub fn distinct_rate_keys(
    facts: &[ExpenseFact],
    reference_currency: &str,
) -> Vec<(NaiveDate, String)> {
    let reference_currency = reference_currency.trim().to_uppercase();
    let mut keys: Vec<(NaiveDate, String)> = facts
        .iter()
        .map(|f| (f.expense_date, f.currency.trim().to_uppercase()))
        .filter(|(_, currency)| *currency != reference_currency)
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Resolve `fact`'s reference-currency rate and the date that rate was
/// published for, using `rates` (built by the caller from
/// [`distinct_rate_keys`] and [`crate::fx::rate_on`]).
///
/// The reference-currency case needs no entry in `rates`: it always reports
/// rate `1.0` and `fact.expense_date`, mirroring
/// [`crate::fx::ResolvedRate`]'s `rate_date: None` convention for that case.
/// Multiply `fact.expense_amount` by the returned rate to get the
/// reference-currency amount.
///
/// # Errors
///
/// Returns [`Error::NotFound`] if `fact`'s `(expense_date, currency)` pair is
/// missing from `rates`, instead of silently substituting `1.0`.
pub fn resolve_fact(
    fact: &ExpenseFact,
    reference_currency: &str,
    rates: &std::collections::HashMap<(NaiveDate, String), crate::fx::ResolvedRate>,
) -> Result<(f64, NaiveDate)> {
    let currency = fact.currency.trim().to_uppercase();
    if currency == reference_currency.trim().to_uppercase() {
        return Ok((1.0, fact.expense_date));
    }
    let resolved = rates
        .get(&(fact.expense_date, currency.clone()))
        .ok_or_else(|| {
            Error::NotFound(format!(
                "no resolved FX rate for '{currency}' on {}; it was not included in the rates \
                 passed to resolve_fact",
                fact.expense_date
            ))
        })?;
    Ok((
        resolved.rate,
        resolved.rate_date.unwrap_or(fact.expense_date),
    ))
}

/// Escape regex metacharacters in `s` (equivalent to Python's `re.escape`).
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !c.is_alphanumeric() && c != '_' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ======================================================================
// Cashflow
// ======================================================================

/// Yearly cashflow table (wide format), with income rows set manually and the
/// `Income` row recomputed from them. `Spending`, `Saving`, and `Saving %`
/// rows may still be present from an older build that derived them from
/// `primaries.parquet`, a summary file this branch no longer writes; this
/// type no longer reads that file or updates those rows, and leaves whatever
/// is already on disk exactly as it is.
pub struct Cashflow {
    /// Calendar year of this table.
    pub year: i32,
    path: std::path::PathBuf,
    /// In-memory cashflow dataframe.
    pub df: DataFrame,
}

impl Cashflow {
    /// Construct for `year`, loading from disk or initialising a zeroed table.
    pub fn new(year: i32) -> Result<Self> {
        let path = get_year_summary_path(year, CASHFLOW_FILENAME)?;
        let df = if path.exists() {
            read_parquet(&path)?
        } else {
            let all_cats: Vec<&str> = INCOME_CATEGORIES
                .iter()
                .chain(DERIVED_CATEGORIES.iter())
                .copied()
                .collect();
            let n = all_cats.len();
            let mut cols: Vec<Column> = vec![Column::new("category".into(), &all_cats)];
            for m in month_labels() {
                cols.push(Column::new(m.as_str().into(), vec![0.0_f64; n]));
            }
            DataFrame::new_infer_height(cols)?
        };
        Ok(Self { year, path, df })
    }

    /// Set an income-category value for a given month, then recompute and save.
    ///
    /// `category` must be one of the income categories and `month` in 1..=12.
    ///
    /// This is the only edit the cashflow table records in the change log,
    /// and it records one cell: the income category and the month column.
    /// [`Self::recompute`] derives the `Income` row from these values on
    /// every device, so recording it too would send a figure the receiver
    /// recomputes anyway.
    pub fn set_income(&mut self, month: u32, category: &str, value: f64) -> Result<()> {
        if !INCOME_CATEGORIES.contains(&category) {
            return Err(Error::InvalidArgument(format!(
                "'{category}' is not a valid income category. Choose from: {INCOME_CATEGORIES:?}"
            )));
        }
        if !(1..=12).contains(&month) {
            return Err(Error::InvalidArgument(format!(
                "month must be between 1 and 12, got {month}"
            )));
        }
        let col_name = format!("{month:02}");
        self.set_value(category, &col_name, value)?;
        self.recompute()?;
        // `recompute` is what saves, so the entry follows it. The row ID of a
        // cashflow change is the category name: that table has no
        // `row_id` column, and the category is what identifies a row in it.
        record(
            ChangeTable::CashflowIncome { year: self.year },
            category,
            number_cell(&col_name, value),
        );
        Ok(())
    }

    /// Recompute the `Income` row from income values, then save.
    ///
    /// This used to also derive `Spending` from the `Total` row of
    /// `primaries.parquet` and `Saving`/`Saving %` from `Income` and
    /// `Spending`. This branch no longer writes `primaries.parquet` and
    /// stopped storing a reference-currency figure at write time in favor of
    /// converting each expense at its own date's rate when it is requested
    /// (see `get_monthly_spending_handler` in `api.rs`), so recomputing
    /// `Spending` here would persist a figure denominated in whichever
    /// reference currency happened to be configured at edit time. Any
    /// `Spending`/`Saving`/`Saving %` values an older build already wrote are
    /// left untouched on disk; this method neither reads nor rewrites them.
    pub fn recompute(&mut self) -> Result<()> {
        self.recompute_income()?;
        self.save()?;
        Ok(())
    }

    /// Recompute the `Income` row from the income values, in memory only.
    /// [`Self::recompute`] is this followed by a save.
    pub(crate) fn recompute_income(&mut self) -> Result<()> {
        for month in 1..=12u32 {
            let col_name = format!("{month:02}");

            // Income = sum of income categories
            let mut income = 0.0;
            for cat in INCOME_CATEGORIES {
                income += self.get_value(cat, &col_name)?;
            }
            self.set_value("Income", &col_name, income)?;
        }
        Ok(())
    }

    /// Write the cashflow dataframe to disk.
    ///
    /// Saving records nothing in the change log. [`Self::set_income`] records
    /// the one cell it changed, after calling [`Self::recompute`], which
    /// calls this. Code that edits `df` directly and then saves leaves that
    /// change out of the log and out of any later sync.
    pub fn save(&self) -> Result<()> {
        write_parquet(&self.df, &self.path)
    }

    /// Write `df` atomically and record nothing in the change log. See
    /// [`DetailedExpenses::save_merged`].
    pub(crate) fn save_merged(&self) -> Result<()> {
        write_parquet_atomic(&self.df, &self.path)
    }

    /// Return the value at `category`/`col`, or `0.0` if the row is absent.
    pub fn get_value(&self, category: &str, col_name: &str) -> Result<f64> {
        let row = self
            .df
            .clone()
            .lazy()
            .filter(col("category").eq(lit(category)))
            .collect()?;
        if row.height() == 0 {
            return Ok(0.0);
        }
        Ok(row.column(col_name)?.f64()?.get(0).unwrap_or(0.0))
    }

    /// Set the value at `category`/`col` (no save).
    pub fn set_value(&mut self, category: &str, col_name: &str, value: f64) -> Result<()> {
        self.df = self
            .df
            .clone()
            .lazy()
            .with_column(
                when(col("category").eq(lit(category)))
                    .then(lit(value))
                    .otherwise(col(col_name))
                    .alias(col_name),
            )
            .collect()?;
        Ok(())
    }
}

// ======================================================================
// Wide-table helpers (shared by InvestmentHoldings / Liquidity / CreditsDebts)
// ======================================================================

/// Build an empty wide dataframe with the given leading `String` meta columns
/// followed by month columns `"01".."12"` of `Float64`, then
/// [`ROW_ID_COLUMN`].
fn empty_wide_df(meta_cols: &[&str]) -> DataFrame {
    let mut fields: Vec<Field> = meta_cols
        .iter()
        .map(|c| Field::new((*c).into(), DataType::String))
        .collect();
    for m in month_labels() {
        fields.push(Field::new(m.as_str().into(), DataType::Float64));
    }
    fields.push(Field::new(ROW_ID_COLUMN.into(), DataType::String));
    DataFrame::empty_with_schema(&Schema::from_iter(fields))
}

/// Build a single-row wide dataframe from `(meta column, value)` pairs plus
/// zeroed month columns, then `row_id` as its [`ROW_ID_COLUMN`].
fn wide_row(meta: &[(&str, &str)], row_id: &str) -> Result<DataFrame> {
    let mut cols: Vec<Column> = meta
        .iter()
        .map(|(name, val)| Column::new((*name).into(), &[*val]))
        .collect();
    for m in month_labels() {
        cols.push(Column::new(m.as_str().into(), &[0.0_f64]));
    }
    cols.push(Column::new(ROW_ID_COLUMN.into(), &[row_id]));
    Ok(DataFrame::new_infer_height(cols)?)
}

/// Set a `when key == value` string column update on a lazy frame.
fn set_str_where(
    df: DataFrame,
    key_col: &str,
    key: &str,
    target: &str,
    value: &str,
) -> Result<DataFrame> {
    Ok(df
        .lazy()
        .with_column(
            when(col(key_col).eq(lit(key)))
                .then(lit(value))
                .otherwise(col(target))
                .alias(target),
        )
        .collect()?)
}

/// Set a `when key == value` float column update on a lazy frame.
fn set_f64_where(
    df: DataFrame,
    key_col: &str,
    key: &str,
    target: &str,
    value: f64,
) -> Result<DataFrame> {
    Ok(df
        .lazy()
        .with_column(
            when(col(key_col).eq(lit(key)))
                .then(lit(value))
                .otherwise(col(target))
                .alias(target),
        )
        .collect()?)
}

// ======================================================================
// InvestmentHoldings
// ======================================================================

/// Yearly investment holdings table. `df` holds quantities; `df_prices` holds
/// per-month unit prices; [`Self::df_value`] multiplies them.
///
/// `currency` lives on `df` only, not on `df_prices`. A holding has exactly
/// one currency, `df` is the frame the API reads asset metadata from (see
/// `get_investments_handler` in `api.rs`), and `df_value`'s join already
/// selects only the price columns out of `df_prices`, so a copy on
/// `df_prices` would never reach a caller and would just be a second value to
/// keep in sync.
///
/// An asset's row in `df` and its row in `df_prices` share one
/// [`ROW_ID_COLUMN`] value.
pub struct InvestmentHoldings {
    /// Calendar year of this table.
    pub year: i32,
    path: std::path::PathBuf,
    path_prices: std::path::PathBuf,
    /// Quantities dataframe (`asset_name, category, link, currency, 01..12,
    /// row_id`).
    pub df: DataFrame,
    /// Prices dataframe (`asset_name, category, link, 01..12, row_id`; no
    /// `currency`).
    pub df_prices: DataFrame,
}

impl InvestmentHoldings {
    /// Construct for `year`, loading holdings and prices from disk (or
    /// initialising empty), migrating a missing `link` column on both frames
    /// and a missing or legacy `currency` column on `df`. Returns
    /// [`Error::RowIdsMissing`] when either file has a row without a
    /// [`ROW_ID_COLUMN`] value.
    pub fn new(year: i32) -> Result<Self> {
        let path = get_year_summary_path(year, INVESTMENTS_FILENAME)?;
        let path_prices = get_year_summary_path(year, INVESTMENTS_PRICES_FILENAME)?;

        let load = |p: &std::path::Path, empty_meta_cols: &[&str]| -> Result<DataFrame> {
            if p.exists() {
                let mut df = read_parquet(p)?;
                require_row_ids(&df, p)?;
                if !has_column(&df, "link") {
                    df = df.lazy().with_column(lit("").alias("link")).collect()?;
                }
                Ok(df)
            } else {
                Ok(empty_wide_df(empty_meta_cols))
            }
        };

        let mut df = load(&path, &["asset_name", "category", "link", "currency"])?;
        if !has_column(&df, "currency") {
            // Holdings saved before this column existed are already priced in
            // the reference currency, so backfill it from settings rather
            // than a fixed value the user may since have changed.
            let reference_currency = config::get_currency_settings()?.reference_currency;
            df = df
                .lazy()
                .with_column(lit(reference_currency).alias("currency"))
                .collect()?;
        }
        let df = normalize_currency_column(df)?;

        Ok(Self {
            year,
            df,
            df_prices: load(&path_prices, &["asset_name", "category", "link"])?,
            path,
            path_prices,
        })
    }

    fn asset_names(&self) -> Result<Vec<String>> {
        str_col_to_vec(&self.df, "asset_name")
    }

    /// Where the holdings rows live, for the change log.
    fn holdings_table(&self) -> ChangeTable {
        ChangeTable::Investments { year: self.year }
    }

    /// Where the price rows live, for the change log. The two files are
    /// separate tables there, because a receiver writes each one on its own.
    fn prices_table(&self) -> ChangeTable {
        ChangeTable::InvestmentsPrices { year: self.year }
    }

    /// The row IDs an asset has in the holdings frame and in the prices
    /// frame. Take them before a change, so a delete still has the IDs of the
    /// rows it removes and a rename still matches on the old name.
    fn asset_ids(&self, asset_name: &str) -> Result<(Vec<String>, Vec<String>)> {
        Ok((
            row_ids_where(&self.df, "asset_name", asset_name)?,
            row_ids_where(&self.df_prices, "asset_name", asset_name)?,
        ))
    }

    /// Record the same change on both frames' rows for one asset, after the
    /// save. `ids` comes from [`Self::asset_ids`], taken before the change.
    fn record_asset(&self, ids: &(Vec<String>, Vec<String>), op: &ChangeOp) {
        for id in &ids.0 {
            record(self.holdings_table(), id, Ok(op.clone()));
        }
        for id in &ids.1 {
            record(self.prices_table(), id, Ok(op.clone()));
        }
    }

    /// Add a new asset row (monthly quantities initialised to 0) and save.
    /// The rows appended to `df` and `df_prices` get the same fresh
    /// [`ROW_ID_COLUMN`] value.
    ///
    /// `currency` is stored on `df` only; the row appended to `df_prices`
    /// keeps that frame's existing `asset_name, category, link, 01..12,
    /// row_id` schema.
    pub fn add_asset(
        &mut self,
        asset_name: &str,
        category: &str,
        link: &str,
        currency: &str,
    ) -> Result<()> {
        if !INVESTMENT_CATEGORIES.contains(&category) {
            return Err(Error::InvalidArgument(format!(
                "'{category}' is not a valid category. Choose from: {INVESTMENT_CATEGORIES:?}"
            )));
        }
        if self.asset_names()?.iter().any(|n| n == asset_name) {
            return Err(Error::AlreadyExists(format!(
                "Asset '{asset_name}' already exists."
            )));
        }
        let row_id = new_row_id();
        let new_row = wide_row(
            &[
                ("asset_name", asset_name),
                ("category", category),
                ("link", link),
                ("currency", currency),
            ],
            &row_id,
        )?;
        let price_row = wide_row(
            &[
                ("asset_name", asset_name),
                ("category", category),
                ("link", link),
            ],
            &row_id,
        )?;
        self.df = concat_df_diagonal(&[self.df.clone(), new_row])?;
        self.df_prices = concat_df_diagonal(&[self.df_prices.clone(), price_row])?;
        self.save()?;
        record(
            self.holdings_table(),
            &row_id,
            upsert_row(&self.df, &row_id),
        );
        record(
            self.prices_table(),
            &row_id,
            upsert_row(&self.df_prices, &row_id),
        );
        Ok(())
    }

    /// Remove an asset row by name from both holdings and prices, then save.
    pub fn remove_asset(&mut self, asset_name: &str) -> Result<()> {
        let ids = self.asset_ids(asset_name)?;
        self.df = self
            .df
            .clone()
            .lazy()
            .filter(col("asset_name").neq(lit(asset_name)))
            .collect()?;
        self.df_prices = self
            .df_prices
            .clone()
            .lazy()
            .filter(col("asset_name").neq(lit(asset_name)))
            .collect()?;
        self.save()?;
        self.record_asset(&ids, &ChangeOp::Delete);
        Ok(())
    }

    /// Rename an asset row in both holdings and prices, then save.
    pub fn rename_asset(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        let names = self.asset_names()?;
        if !names.iter().any(|n| n == old_name) {
            return Err(Error::NotFound(format!("Asset '{old_name}' not found.")));
        }
        if new_name != old_name && names.iter().any(|n| n == new_name) {
            return Err(Error::AlreadyExists(format!(
                "Asset '{new_name}' already exists."
            )));
        }
        let ids = self.asset_ids(old_name)?;
        self.df = set_str_where(
            self.df.clone(),
            "asset_name",
            old_name,
            "asset_name",
            new_name,
        )?;
        self.df_prices = set_str_where(
            self.df_prices.clone(),
            "asset_name",
            old_name,
            "asset_name",
            new_name,
        )?;
        self.save()?;
        self.record_asset(&ids, &text_cell("asset_name", new_name));
        Ok(())
    }

    /// Update the category for an asset in both frames, then save.
    pub fn set_category(&mut self, asset_name: &str, category: &str) -> Result<()> {
        if !self.asset_names()?.iter().any(|n| n == asset_name) {
            return Err(Error::NotFound(format!("Asset '{asset_name}' not found.")));
        }
        if !INVESTMENT_CATEGORIES.contains(&category) {
            return Err(Error::InvalidArgument(format!(
                "'{category}' is not a valid category. Choose from: {INVESTMENT_CATEGORIES:?}"
            )));
        }
        let ids = self.asset_ids(asset_name)?;
        self.df = set_str_where(
            self.df.clone(),
            "asset_name",
            asset_name,
            "category",
            category,
        )?;
        self.df_prices = set_str_where(
            self.df_prices.clone(),
            "asset_name",
            asset_name,
            "category",
            category,
        )?;
        self.save()?;
        self.record_asset(&ids, &text_cell("category", category));
        Ok(())
    }

    /// Update the link URL for an asset in both frames, then save.
    pub fn set_link(&mut self, asset_name: &str, link: &str) -> Result<()> {
        if !self.asset_names()?.iter().any(|n| n == asset_name) {
            return Err(Error::NotFound(format!("Asset '{asset_name}' not found.")));
        }
        let ids = self.asset_ids(asset_name)?;
        self.df = set_str_where(self.df.clone(), "asset_name", asset_name, "link", link)?;
        self.df_prices = set_str_where(
            self.df_prices.clone(),
            "asset_name",
            asset_name,
            "link",
            link,
        )?;
        self.save()?;
        self.record_asset(&ids, &text_cell("link", link));
        Ok(())
    }

    /// Update the currency for an asset, then save the holdings frame.
    ///
    /// `currency` lives on `df` only, so this writes and records only
    /// `investments.parquet`; see the type's own docs for why `df_prices`
    /// carries no currency.
    pub fn set_currency(&mut self, asset_name: &str, currency: &str) -> Result<()> {
        if !self.asset_names()?.iter().any(|n| n == asset_name) {
            return Err(Error::NotFound(format!("Asset '{asset_name}' not found.")));
        }
        let ids = row_ids_where(&self.df, "asset_name", asset_name)?;
        self.df = set_str_where(
            self.df.clone(),
            "asset_name",
            asset_name,
            "currency",
            currency,
        )?;
        self.save_df()?;
        for id in &ids {
            record(
                self.holdings_table(),
                id,
                Ok(text_cell("currency", currency)),
            );
        }
        Ok(())
    }

    /// Set the quantity (`"quantity"`) or unit price (`"price"`) for an asset in
    /// a given month, then save the affected frame.
    pub fn set_quantity_or_price(
        &mut self,
        asset_name: &str,
        month: u32,
        value: f64,
        quant_or_price: &str,
    ) -> Result<()> {
        if !(1..=12).contains(&month) {
            return Err(Error::InvalidArgument(format!(
                "month must be between 1 and 12, got {month}"
            )));
        }
        if !self.asset_names()?.iter().any(|n| n == asset_name) {
            return Err(Error::NotFound(format!("Asset '{asset_name}' not found.")));
        }
        let col_name = format!("{month:02}");
        match quant_or_price {
            "quantity" => {
                let ids = row_ids_where(&self.df, "asset_name", asset_name)?;
                self.df =
                    set_f64_where(self.df.clone(), "asset_name", asset_name, &col_name, value)?;
                self.save_df()?;
                for id in &ids {
                    record(self.holdings_table(), id, number_cell(&col_name, value));
                }
                Ok(())
            }
            "price" => {
                let ids = row_ids_where(&self.df_prices, "asset_name", asset_name)?;
                self.df_prices = set_f64_where(
                    self.df_prices.clone(),
                    "asset_name",
                    asset_name,
                    &col_name,
                    value,
                )?;
                self.save_df_prices()?;
                for id in &ids {
                    record(self.prices_table(), id, number_cell(&col_name, value));
                }
                Ok(())
            }
            other => Err(Error::InvalidArgument(format!(
                "quant_or_price must be 'quantity' or 'price', got '{other}'"
            ))),
        }
    }

    /// Set the quantity for an asset in a given month.
    pub fn set_quantity(&mut self, asset_name: &str, month: u32, quantity: f64) -> Result<()> {
        self.set_quantity_or_price(asset_name, month, quantity, "quantity")
    }

    /// Set the price for an asset in a given month.
    pub fn set_price(&mut self, asset_name: &str, month: u32, price: f64) -> Result<()> {
        self.set_quantity_or_price(asset_name, month, price, "price")
    }

    /// Return a dataframe of quantity × price for each asset and month.
    ///
    /// Same shape as `df` (`asset_name, category, link, currency, 01..12,
    /// row_id`) but each monthly cell contains `quantity * price`. The join
    /// only pulls the price columns out of `df_prices`, so `df`'s other
    /// columns, including `currency` and `row_id`, pass through unchanged.
    pub fn df_value(&self) -> Result<DataFrame> {
        let mcols = month_labels();
        let price_select: Vec<Expr> = std::iter::once(col("asset_name"))
            .chain(
                mcols
                    .iter()
                    .map(|c| col(c.as_str()).alias(format!("{c}_price"))),
            )
            .collect();
        let prices = self.df_prices.clone().lazy().select(price_select);

        let mut value = self.df.clone().lazy().join(
            prices,
            [col("asset_name")],
            [col("asset_name")],
            JoinArgs::new(JoinType::Left),
        );
        for c in &mcols {
            value = value
                .with_column((col(c.as_str()) * col(format!("{c}_price"))).alias(c.as_str()))
                .drop(cols([format!("{c}_price")]));
        }
        Ok(value.collect()?)
    }

    /// Write the holdings dataframe to disk.
    ///
    /// Saving records nothing in the change log; the methods above record the
    /// change they made after calling this. Code that edits `df` directly and
    /// then saves leaves that change out of the log and out of any later
    /// sync, so add a method here instead.
    pub fn save_df(&self) -> Result<()> {
        write_parquet(&self.df, &self.path)
    }

    /// Write the prices dataframe to disk. Records nothing: see
    /// [`Self::save_df`].
    pub fn save_df_prices(&self) -> Result<()> {
        write_parquet(&self.df_prices, &self.path_prices)
    }

    /// Write both holdings and prices dataframes to disk. Records nothing:
    /// see [`Self::save_df`].
    pub fn save(&self) -> Result<()> {
        self.save_df()?;
        self.save_df_prices()
    }

    /// Write the holdings frame atomically and record nothing in the change
    /// log. See [`DetailedExpenses::save_merged`].
    pub(crate) fn save_merged_holdings(&self) -> Result<()> {
        write_parquet_atomic(&self.df, &self.path)
    }

    /// Write the prices frame atomically and record nothing in the change
    /// log. See [`DetailedExpenses::save_merged`].
    pub(crate) fn save_merged_prices(&self) -> Result<()> {
        write_parquet_atomic(&self.df_prices, &self.path_prices)
    }
}

// ======================================================================
// Liquidity
// ======================================================================

/// Yearly liquidity table (`asset_name, category, currency, 01..12, row_id`).
pub struct Liquidity {
    /// Calendar year of this table.
    pub year: i32,
    path: std::path::PathBuf,
    /// In-memory liquidity dataframe.
    pub df: DataFrame,
}

impl Liquidity {
    /// Construct for `year`, loading from disk (migrating a missing `currency`
    /// column to `"EUR"` and a legacy `"E"` value in an existing one to
    /// `"EUR"`) or initialising empty. Returns [`Error::RowIdsMissing`] for
    /// a file with a row that has no [`ROW_ID_COLUMN`] value.
    pub fn new(year: i32) -> Result<Self> {
        let path = get_year_summary_path(year, LIQUIDITY_FILENAME)?;
        let df = if path.exists() {
            let mut df = read_parquet(&path)?;
            require_row_ids(&df, &path)?;
            if !has_column(&df, "currency") {
                df = df
                    .lazy()
                    .with_column(lit("EUR").alias("currency"))
                    .collect()?;
            }
            normalize_currency_column(df)?
        } else {
            empty_wide_df(&["asset_name", "category", "currency"])
        };
        Ok(Self { year, path, df })
    }

    fn asset_names(&self) -> Result<Vec<String>> {
        str_col_to_vec(&self.df, "asset_name")
    }

    /// Where these rows live, for the change log.
    fn change_table(&self) -> ChangeTable {
        ChangeTable::Liquidity { year: self.year }
    }

    /// Record `op` on every row named `asset_name`, after the save. `ids`
    /// comes from [`row_ids_where`], taken before the change.
    fn record_rows(&self, ids: &[String], op: &ChangeOp) {
        for id in ids {
            record(self.change_table(), id, Ok(op.clone()));
        }
    }

    /// Add a new liquidity asset row (monthly values initialised to 0) with a
    /// fresh [`ROW_ID_COLUMN`] value, then save.
    pub fn add_asset(&mut self, asset_name: &str, category: &str, currency: &str) -> Result<()> {
        if !LIQUIDITY_CATEGORIES.contains(&category) {
            return Err(Error::InvalidArgument(format!(
                "'{category}' is not a valid category. Choose from: {LIQUIDITY_CATEGORIES:?}"
            )));
        }
        if self.asset_names()?.iter().any(|n| n == asset_name) {
            return Err(Error::AlreadyExists(format!(
                "Asset '{asset_name}' already exists."
            )));
        }
        let row_id = new_row_id();
        let new_row = wide_row(
            &[
                ("asset_name", asset_name),
                ("category", category),
                ("currency", currency),
            ],
            &row_id,
        )?;
        self.df = concat_df_diagonal(&[self.df.clone(), new_row])?;
        self.save()?;
        record(self.change_table(), &row_id, upsert_row(&self.df, &row_id));
        Ok(())
    }

    /// Remove a liquidity asset row by name, then save.
    pub fn remove_asset(&mut self, asset_name: &str) -> Result<()> {
        let ids = row_ids_where(&self.df, "asset_name", asset_name)?;
        self.df = self
            .df
            .clone()
            .lazy()
            .filter(col("asset_name").neq(lit(asset_name)))
            .collect()?;
        self.save()?;
        self.record_rows(&ids, &ChangeOp::Delete);
        Ok(())
    }

    /// Rename a liquidity asset row, then save.
    pub fn rename_asset(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        let names = self.asset_names()?;
        if !names.iter().any(|n| n == old_name) {
            return Err(Error::NotFound(format!("Asset '{old_name}' not found.")));
        }
        if new_name != old_name && names.iter().any(|n| n == new_name) {
            return Err(Error::AlreadyExists(format!(
                "Asset '{new_name}' already exists."
            )));
        }
        let ids = row_ids_where(&self.df, "asset_name", old_name)?;
        self.df = set_str_where(
            self.df.clone(),
            "asset_name",
            old_name,
            "asset_name",
            new_name,
        )?;
        self.save()?;
        self.record_rows(&ids, &text_cell("asset_name", new_name));
        Ok(())
    }

    /// Update the category for a liquidity asset, then save.
    pub fn set_category(&mut self, asset_name: &str, category: &str) -> Result<()> {
        if !self.asset_names()?.iter().any(|n| n == asset_name) {
            return Err(Error::NotFound(format!("Asset '{asset_name}' not found.")));
        }
        if !LIQUIDITY_CATEGORIES.contains(&category) {
            return Err(Error::InvalidArgument(format!(
                "'{category}' is not a valid category. Choose from: {LIQUIDITY_CATEGORIES:?}"
            )));
        }
        let ids = row_ids_where(&self.df, "asset_name", asset_name)?;
        self.df = set_str_where(
            self.df.clone(),
            "asset_name",
            asset_name,
            "category",
            category,
        )?;
        self.save()?;
        self.record_rows(&ids, &text_cell("category", category));
        Ok(())
    }

    /// Update the currency for a liquidity asset, then save.
    pub fn set_currency(&mut self, asset_name: &str, currency: &str) -> Result<()> {
        if !self.asset_names()?.iter().any(|n| n == asset_name) {
            return Err(Error::NotFound(format!("Asset '{asset_name}' not found.")));
        }
        let ids = row_ids_where(&self.df, "asset_name", asset_name)?;
        self.df = set_str_where(
            self.df.clone(),
            "asset_name",
            asset_name,
            "currency",
            currency,
        )?;
        self.save()?;
        self.record_rows(&ids, &text_cell("currency", currency));
        Ok(())
    }

    /// Set the value for an asset in a given month (1..=12), then save.
    pub fn set_value(&mut self, asset_name: &str, month: u32, value: f64) -> Result<()> {
        if !(1..=12).contains(&month) {
            return Err(Error::InvalidArgument(format!(
                "month must be between 1 and 12, got {month}"
            )));
        }
        if !self.asset_names()?.iter().any(|n| n == asset_name) {
            return Err(Error::NotFound(format!("Asset '{asset_name}' not found.")));
        }
        let col_name = format!("{month:02}");
        let ids = row_ids_where(&self.df, "asset_name", asset_name)?;
        self.df = set_f64_where(self.df.clone(), "asset_name", asset_name, &col_name, value)?;
        self.save()?;
        for id in &ids {
            record(self.change_table(), id, number_cell(&col_name, value));
        }
        Ok(())
    }

    /// Write the liquidity dataframe to disk.
    ///
    /// Saving records nothing in the change log; the methods above record the
    /// change they made after calling this. Code that edits `df` directly and
    /// then saves leaves that change out of the log and out of any later
    /// sync, so add a method here instead.
    pub fn save(&self) -> Result<()> {
        write_parquet(&self.df, &self.path)
    }

    /// Write `df` atomically and record nothing in the change log. See
    /// [`DetailedExpenses::save_merged`].
    pub(crate) fn save_merged(&self) -> Result<()> {
        write_parquet_atomic(&self.df, &self.path)
    }
}

// ======================================================================
// CreditsDebts
// ======================================================================

/// Yearly credits & debts table (`name, currency, 01..12, row_id`).
///
/// Positive monthly values represent credits, negative values represent debts.
pub struct CreditsDebts {
    /// Calendar year of this table.
    pub year: i32,
    path: std::path::PathBuf,
    /// In-memory credits/debts dataframe.
    pub df: DataFrame,
}

impl CreditsDebts {
    /// Construct for `year`, loading from disk (migrating a missing `currency`
    /// column to `"EUR"`, a legacy `"E"` value in an existing one to `"EUR"`,
    /// and dropping a legacy `type` column) or initialising empty. Returns
    /// [`Error::RowIdsMissing`] for a file with a row that has no
    /// [`ROW_ID_COLUMN`] value.
    pub fn new(year: i32) -> Result<Self> {
        let path = get_year_summary_path(year, CREDITS_DEBTS_FILENAME)?;
        let df = if path.exists() {
            let mut df = read_parquet(&path)?;
            require_row_ids(&df, &path)?;
            if !has_column(&df, "currency") {
                df = df
                    .lazy()
                    .with_column(lit("EUR").alias("currency"))
                    .collect()?;
            }
            let mut df = normalize_currency_column(df)?;
            if has_column(&df, "type") {
                df = df.drop("type")?;
            }
            df
        } else {
            empty_wide_df(&["name", "currency"])
        };
        Ok(Self { year, path, df })
    }

    fn entry_names(&self) -> Result<Vec<String>> {
        str_col_to_vec(&self.df, "name")
    }

    /// Where these rows live, for the change log.
    fn change_table(&self) -> ChangeTable {
        ChangeTable::CreditsDebts { year: self.year }
    }

    /// Record `op` on every row named `name`, after the save. `ids` comes
    /// from [`row_ids_where`], taken before the change.
    fn record_rows(&self, ids: &[String], op: &ChangeOp) {
        for id in ids {
            record(self.change_table(), id, Ok(op.clone()));
        }
    }

    /// Add a new credit/debt row (monthly values initialised to 0) with a
    /// fresh [`ROW_ID_COLUMN`] value, then save.
    pub fn add_entry(&mut self, name: &str, currency: &str) -> Result<()> {
        if self.entry_names()?.iter().any(|n| n == name) {
            return Err(Error::AlreadyExists(format!(
                "Entry '{name}' already exists."
            )));
        }
        let row_id = new_row_id();
        let new_row = wide_row(&[("name", name), ("currency", currency)], &row_id)?;
        self.df = concat_df_diagonal(&[self.df.clone(), new_row])?;
        self.save()?;
        record(self.change_table(), &row_id, upsert_row(&self.df, &row_id));
        Ok(())
    }

    /// Remove a credit/debt row by name, then save.
    pub fn remove_entry(&mut self, name: &str) -> Result<()> {
        let ids = row_ids_where(&self.df, "name", name)?;
        self.df = self
            .df
            .clone()
            .lazy()
            .filter(col("name").neq(lit(name)))
            .collect()?;
        self.save()?;
        self.record_rows(&ids, &ChangeOp::Delete);
        Ok(())
    }

    /// Rename a credit/debt row, then save.
    pub fn rename_entry(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        let names = self.entry_names()?;
        if !names.iter().any(|n| n == old_name) {
            return Err(Error::NotFound(format!("Entry '{old_name}' not found.")));
        }
        if new_name != old_name && names.iter().any(|n| n == new_name) {
            return Err(Error::AlreadyExists(format!(
                "Entry '{new_name}' already exists."
            )));
        }
        let ids = row_ids_where(&self.df, "name", old_name)?;
        self.df = set_str_where(self.df.clone(), "name", old_name, "name", new_name)?;
        self.save()?;
        self.record_rows(&ids, &text_cell("name", new_name));
        Ok(())
    }

    /// Update the currency for a credit/debt entry, then save.
    pub fn set_currency(&mut self, name: &str, currency: &str) -> Result<()> {
        if !self.entry_names()?.iter().any(|n| n == name) {
            return Err(Error::NotFound(format!("Entry '{name}' not found.")));
        }
        let ids = row_ids_where(&self.df, "name", name)?;
        self.df = set_str_where(self.df.clone(), "name", name, "currency", currency)?;
        self.save()?;
        self.record_rows(&ids, &text_cell("currency", currency));
        Ok(())
    }

    /// Set the outstanding amount for an entry in a given month (1..=12), save.
    pub fn set_value(&mut self, name: &str, month: u32, value: f64) -> Result<()> {
        if !(1..=12).contains(&month) {
            return Err(Error::InvalidArgument(format!(
                "month must be between 1 and 12, got {month}"
            )));
        }
        if !self.entry_names()?.iter().any(|n| n == name) {
            return Err(Error::NotFound(format!("Entry '{name}' not found.")));
        }
        let col_name = format!("{month:02}");
        let ids = row_ids_where(&self.df, "name", name)?;
        self.df = set_f64_where(self.df.clone(), "name", name, &col_name, value)?;
        self.save()?;
        for id in &ids {
            record(self.change_table(), id, number_cell(&col_name, value));
        }
        Ok(())
    }

    /// Write the credits/debts dataframe to disk.
    ///
    /// Saving records nothing in the change log; the methods above record the
    /// change they made after calling this. Code that edits `df` directly and
    /// then saves leaves that change out of the log and out of any later
    /// sync, so add a method here instead.
    pub fn save(&self) -> Result<()> {
        write_parquet(&self.df, &self.path)
    }

    /// Write `df` atomically and record nothing in the change log. See
    /// [`DetailedExpenses::save_merged`].
    pub(crate) fn save_merged(&self) -> Result<()> {
        write_parquet_atomic(&self.df, &self.path)
    }
}

// ======================================================================
// RecurringExpenses
// ======================================================================

/// Build an empty recurring-expenses dataframe with the canonical schema.
fn empty_recurring_df() -> DataFrame {
    DataFrame::empty_with_schema(&Schema::from_iter([
        Field::new("expense_name".into(), DataType::String),
        Field::new("expense_day".into(), DataType::Int64),
        Field::new("expense_amount".into(), DataType::Float64),
        Field::new("currency".into(), DataType::String),
        Field::new("primary_category".into(), DataType::String),
        Field::new("secondary_category".into(), DataType::String),
        Field::new(ROW_ID_COLUMN.into(), DataType::String),
    ]))
}

/// A recurring template not yet present in a target month, along with the
/// data needed to insert it.
///
/// Returned by [`RecurringExpenses::plan_for_month`] in both halves of a
/// [`RecurringApplyPlan`]. Pass the pending half straight to
/// [`RecurringExpenses::insert_resolved`]; the skipped half is for showing
/// the user what apply left out, so it carries the same fields.
#[derive(Debug, Clone)]
pub struct PendingRecurringRow {
    /// The [`ROW_ID_COLUMN`] value the inserted row must get: the template's
    /// own `row_id` and the target month, from [`derived_recurring_row_id`].
    /// Both devices derive the same value for the same template and month,
    /// so a later sync merges the rows instead of duplicating them.
    pub row_id: String,
    /// The [`ROW_ID_COLUMN`] value of the template this row comes from.
    /// [`RecurringExpenses::reinstate_for_month`] takes it to name a skipped
    /// row the user asks back, since `row_id` alone cannot be split reliably:
    /// a template id is a UUID today, but nothing stops an older one from
    /// holding a `:`.
    pub template_row_id: String,
    /// The expense name to insert.
    pub expense_name: String,
    /// Day of month (1..=28) the generated row lands on.
    pub expense_day: u32,
    /// The expense amount, in `currency`.
    pub expense_amount: f64,
    /// The currency `expense_amount` is denominated in.
    pub currency: String,
    /// Primary category to assign to the new row.
    pub primary_category: String,
    /// Secondary category to assign to the new row.
    pub secondary_category: String,
}

/// What one `/api/recurring/apply` call will and will not generate for a
/// month. Returned by [`RecurringExpenses::plan_for_month`].
///
/// Neither list holds a template whose row the month already has, by derived
/// ID or by name and day: those are not due at all and are reported nowhere.
#[derive(Debug, Clone, Default)]
pub struct RecurringApplyPlan {
    /// Templates due for the month. Hand this to
    /// [`RecurringExpenses::insert_resolved`].
    pub pending: Vec<PendingRecurringRow>,
    /// Templates due for the month whose generated row the user deleted, and
    /// whose deletion still stands in the change log. Apply must not generate
    /// these: that is the whole point of the rule. Show them to the user, who
    /// can ask for one back through
    /// [`RecurringExpenses::reinstate_for_month`].
    pub skipped: Vec<PendingRecurringRow>,
}

/// What [`RecurringExpenses::reinstate_for_month`] did.
#[derive(Debug, Clone)]
pub struct ReinstatedRow {
    /// The [`ROW_ID_COLUMN`] value of the row in the month, whether this call
    /// created it or found it already there.
    pub row_id: String,
    /// False when the month already held that row and the call wrote nothing.
    pub created: bool,
}

/// Derive the [`ROW_ID_COLUMN`] value for a row that `/api/recurring/apply`
/// generates from `template_row_id` for `year`/`month`. Both the desktop and
/// the phone hold the same template row and compute the same target month,
/// so they derive the same ID and a later sync merges the rows instead of
/// duplicating them.
fn derived_recurring_row_id(template_row_id: &str, year: i32, month: u32) -> String {
    format!("{template_row_id}:{year:04}-{month:02}")
}

/// Manage recurring monthly expense definitions, stored per year in
/// `<dbs_root>/<year>/recurring_expenses.parquet`.
pub struct RecurringExpenses {
    /// Calendar year of this table.
    pub year: i32,
    path: std::path::PathBuf,
    /// In-memory recurring-expenses dataframe.
    pub df: DataFrame,
}

impl RecurringExpenses {
    /// Construct for `year`, loading from disk (normalizing a legacy `"E"`
    /// currency value to `"EUR"`) or initialising empty. Returns
    /// [`Error::RowIdsMissing`] for a file with a row that has no
    /// [`ROW_ID_COLUMN`] value.
    pub fn new(year: i32) -> Result<Self> {
        let path = get_year_summary_path(year, RECURRING_EXPENSES_FILENAME)?;
        let df = if path.exists() {
            let df = read_parquet(&path)?;
            require_row_ids(&df, &path)?;
            normalize_currency_column(df)?
        } else {
            empty_recurring_df()
        };
        Ok(Self { year, path, df })
    }

    /// Add a recurring expense definition (day must be 1..=28) with a fresh
    /// [`ROW_ID_COLUMN`] value, save, and return the new row's ID.
    pub fn add(
        &mut self,
        expense_name: &str,
        expense_day: i64,
        expense_amount: f64,
        currency: &str,
        primary_category: &str,
        secondary_category: &str,
    ) -> Result<String> {
        if !(1..=28).contains(&expense_day) {
            return Err(Error::InvalidArgument(format!(
                "expense_day must be between 1 and 28, got {expense_day}"
            )));
        }
        let row_id = new_row_id();
        let new_row = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &[expense_name]),
            Column::new("expense_day".into(), &[expense_day]),
            Column::new("expense_amount".into(), &[expense_amount]),
            Column::new("currency".into(), &[currency]),
            Column::new(
                "primary_category".into(),
                &[normalize_category_value(primary_category)],
            ),
            Column::new(
                "secondary_category".into(),
                &[normalize_category_value(secondary_category)],
            ),
            Column::new(ROW_ID_COLUMN.into(), &[row_id.as_str()]),
        ])?;
        self.df = concat_df_diagonal(&[self.df.clone(), new_row])?;
        self.save()?;
        record(self.change_table(), &row_id, upsert_row(&self.df, &row_id));
        Ok(row_id)
    }

    /// Where these rows live, for the change log.
    fn change_table(&self) -> ChangeTable {
        ChangeTable::Recurring { year: self.year }
    }

    /// Remove the recurring expense whose [`ROW_ID_COLUMN`] is `row_id`, then
    /// save. Returns [`Error::NotFound`] when this year has no such row, and
    /// [`Error::AlreadyExists`] without changing anything when several rows
    /// share that ID.
    pub fn remove(&mut self, row_id: &str) -> Result<()> {
        require_single_row(
            &self.df,
            row_id,
            "recurring expense",
            &self.year.to_string(),
        )?;
        self.df = self
            .df
            .clone()
            .lazy()
            .filter(col(ROW_ID_COLUMN).neq(lit(row_id)))
            .collect()?;
        self.save()?;
        record(self.change_table(), row_id, Ok(ChangeOp::Delete));
        Ok(())
    }

    /// Every template of this year as the row it would generate in
    /// `year`/`month`, in table order and unfiltered: this consults neither
    /// the target month nor the change log. [`Self::plan_for_month`] does
    /// that filtering, and [`Self::reinstate_for_month`] picks one row out.
    fn rows_for_month(&self, year: i32, month: u32) -> Result<Vec<PendingRecurringRow>> {
        let names = str_col_to_vec(&self.df, "expense_name")?;
        let days: Vec<i64> = self
            .df
            .column("expense_day")?
            .i64()?
            .iter()
            .map(|o| o.unwrap_or(0))
            .collect();
        let amounts: Vec<f64> = self
            .df
            .column("expense_amount")?
            .f64()?
            .iter()
            .map(|o| o.unwrap_or(0.0))
            .collect();
        let currencies = str_col_to_vec(&self.df, "currency")?;
        let primaries = str_col_to_vec(&self.df, "primary_category")?;
        let secondaries = str_col_to_vec(&self.df, "secondary_category")?;
        let template_ids = str_col_to_vec(&self.df, ROW_ID_COLUMN)?;

        Ok((0..names.len())
            .map(|i| PendingRecurringRow {
                row_id: derived_recurring_row_id(&template_ids[i], year, month),
                template_row_id: template_ids[i].clone(),
                expense_name: names[i].clone(),
                expense_day: days[i] as u32,
                expense_amount: amounts[i],
                currency: currencies[i].clone(),
                primary_category: primaries[i].clone(),
                secondary_category: secondaries[i].clone(),
            })
            .collect())
    }

    /// Split this year's recurring definitions into the rows `de`'s month
    /// still needs and the rows apply must not generate.
    ///
    /// A template is due when the target month holds neither a row whose
    /// [`ROW_ID_COLUMN`] equals [`derived_recurring_row_id`] for that
    /// template (a row a previous apply generated, on either device) nor a
    /// row with the same `expense_name` and the day component of
    /// `expense_date` (a row from before derived IDs existed, so it carries
    /// a random UUID that the first check cannot match). A template that is
    /// not due appears in neither half of the plan.
    ///
    /// A due template goes to [`RecurringApplyPlan::skipped`] instead of
    /// [`RecurringApplyPlan::pending`] when
    /// [`crate::merge::generated_row_stays_deleted`] says the user's deletion
    /// of that row still stands. The question has to be put to the change
    /// log, because a deleted row is absent from the month's dataframe, which
    /// is the very state that makes it look due. Without this, deleting
    /// March's rent and pressing apply again brings it straight back on a
    /// device that never syncs.
    ///
    /// The log is read once here and the same slice is reused for every
    /// template.
    ///
    /// # Which rows this covers
    ///
    /// The question is asked about the derived ID, so it covers every row a
    /// recurring apply generated once row IDs existed. It does not cover a
    /// row that was already in a month's file before then:
    /// [`crate::row_id_migration`] gave that row a fresh random UUID and
    /// [`crate::sync_baseline`] recorded it as an ordinary row, so deleting
    /// it records a delete against that UUID, and this function, asking about
    /// the derived ID, finds nothing and puts the template back in `pending`.
    /// The row returns once.
    ///
    /// That gap closes itself, one cycle per row: the regenerated row carries
    /// the derived ID, so deleting it a second time sticks for good. The user
    /// chose on 2026-09-18 to accept the single repeat rather than the
    /// alternative, widening the identity to `expense_name` and day. Widening
    /// would silently withhold a hand typed expense that happens to share a
    /// name and day with a template, and that failure would be permanent and
    /// invisible, which is worse than one repeat that fixes itself. Do not
    /// read this function as a complete guarantee for rows older than the
    /// change log.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the change log exists but cannot be read.
    /// Failing is deliberate: a plan built without the log would regenerate
    /// every row the user has ever deleted.
    ///
    /// A log that reads but drops lines is reported, not refused. Each lost
    /// line is a lost change, and a delete on one of them is a row this plan
    /// generates again, so the counts go to stderr (counts only, never the
    /// contents) and the plan goes ahead. Refusing would stop apply working
    /// at all on a damaged log, which costs the user more than the deletes
    /// those lines held. Part 3 owns the user-facing answer, because it owns
    /// whether this device is fit to sync at all.
    pub fn plan_for_month(&self, de: &DetailedExpenses) -> Result<RecurringApplyPlan> {
        let candidates = self.rows_for_month(de.year, de.month)?;
        let log = sync::read_log()?;
        if !log.corrupt_lines.is_empty() {
            eprintln!(
                "Change log: {} line(s) could not be read, {} of them too long to read at all. \
                 Recurring apply is going ahead without them, so a row whose deletion was \
                 recorded on one of those lines can come back.",
                log.corrupt_lines.len(),
                log.oversized_lines.len()
            );
        }
        let table = de.change_table();

        let mut plan = RecurringApplyPlan::default();
        for row in candidates {
            let existing = de
                .expense_df
                .clone()
                .lazy()
                .filter(
                    (col("expense_name").eq(lit(row.expense_name.as_str())).and(
                        col("expense_date")
                            .dt()
                            .day()
                            .eq(lit(row.expense_day as i8)),
                    ))
                    .or(col(ROW_ID_COLUMN).eq(lit(row.row_id.as_str()))),
                )
                .collect()?;
            if existing.height() > 0 {
                continue;
            }

            if crate::merge::generated_row_stays_deleted(&log.entries, &table, &row.row_id) {
                plan.skipped.push(row);
            } else {
                plan.pending.push(row);
            }
        }
        Ok(plan)
    }

    /// The pending half of [`Self::plan_for_month`], for a caller that has
    /// nowhere to report what apply skipped.
    pub fn pending_for_month(&self, de: &DetailedExpenses) -> Result<Vec<PendingRecurringRow>> {
        Ok(self.plan_for_month(de)?.pending)
    }

    /// Create the row `template_row_id` would generate in `de`'s month, after
    /// the user confirmed they want back a row [`Self::plan_for_month`]
    /// skipped.
    ///
    /// The change is recorded as an ordinary upsert, with no
    /// [`ChangeOrigin`], and that is the point of this method rather than an
    /// oversight. [`crate::merge`] holds a generated upsert back from
    /// overriding a delete of the same row, so that a mechanical
    /// regeneration cannot resurrect a deleted row, while an ordinary upsert
    /// wins on stamp like any other deliberate edit. A user confirming this
    /// row back is a deliberate edit. Marking it generated would make the
    /// reinstate lose to the old delete at the next sync, and the row would
    /// vanish again on both devices.
    ///
    /// When the month already holds the derived row ID this writes nothing
    /// and reports [`ReinstatedRow::created`] as false. Two confirmations of
    /// the same row, from a double click or a second open tab, then leave one
    /// row rather than two: a duplicate ID in one month would be far worse
    /// than a redundant call, because the uniqueness check then rejects every
    /// later edit or delete of either copy.
    ///
    /// The derived ID is the only thing checked. A row the user retyped by
    /// hand under a fresh ID does not block a reinstate, so a caller should
    /// only offer this for rows [`Self::plan_for_month`] actually reported as
    /// skipped, which have already passed the name-and-day check.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when this year has no template with that
    /// [`ROW_ID_COLUMN`], and [`Error::AlreadyExists`], without writing
    /// anything, when several templates share it: they derive the same row ID
    /// for the month, so which one is meant cannot be answered.
    pub fn reinstate_for_month(
        &self,
        de: &mut DetailedExpenses,
        template_row_id: &str,
    ) -> Result<ReinstatedRow> {
        require_single_row(
            &self.df,
            template_row_id,
            "recurring expense",
            &self.year.to_string(),
        )?;
        let row = self
            .rows_for_month(de.year, de.month)?
            .into_iter()
            .find(|row| row.template_row_id == template_row_id)
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "No recurring expense with id '{template_row_id}' in {}.",
                    self.year
                ))
            })?;

        if has_row_id(&de.expense_df, &row.row_id)? {
            return Ok(ReinstatedRow {
                row_id: row.row_id,
                created: false,
            });
        }

        de.add_row_with_id(
            &row.row_id,
            &row.expense_name,
            row.expense_day,
            row.expense_amount,
            Some(&row.primary_category),
            &row.currency,
            Some(&row.secondary_category),
            None,
        )?;
        Ok(ReinstatedRow {
            row_id: row.row_id,
            created: true,
        })
    }

    /// Insert `rows` (the [`RecurringApplyPlan::pending`] half of
    /// [`Self::plan_for_month`]) into `de` under each row's derived ID, and
    /// return the inserted names.
    ///
    /// This is the one place that records a change as
    /// [`ChangeOrigin::RecurringApply`]. The mark is what lets the merge
    /// engine keep a generated row the user deleted from coming back next
    /// month, and a generated row carries the same derived ID as a hand typed
    /// one, so nothing can tell them apart afterwards if the entry does not
    /// say so. [`Self::reinstate_for_month`] deliberately does not use it.
    ///
    /// Skips a row whose derived ID is already present in `de.expense_df`
    /// and does not count it in the returned names. This guards against two
    /// recurring templates sharing a `row_id` (a hand-copied or restored
    /// `recurring_expenses.parquet`): without the check, both would derive
    /// the same ID for the month and both would be inserted, leaving the
    /// month with a duplicate `row_id` that [`require_single_row`] then
    /// rejects for every future edit or delete of either row.
    pub fn insert_resolved(
        &self,
        de: &mut DetailedExpenses,
        rows: &[PendingRecurringRow],
    ) -> Result<Vec<String>> {
        let mut added = Vec::new();
        for row in rows {
            if has_row_id(&de.expense_df, &row.row_id)? {
                continue;
            }
            de.add_row_with_id(
                &row.row_id,
                &row.expense_name,
                row.expense_day,
                row.expense_amount,
                Some(&row.primary_category),
                &row.currency,
                Some(&row.secondary_category),
                Some(ChangeOrigin::RecurringApply),
            )?;
            added.push(row.expense_name.clone());
        }
        Ok(added)
    }

    /// Write the recurring expenses dataframe to disk.
    ///
    /// Saving records nothing in the change log; the methods above record the
    /// change they made after calling this. Code that edits `df` directly and
    /// then saves leaves that change out of the log and out of any later
    /// sync, so add a method here instead.
    pub fn save(&self) -> Result<()> {
        write_parquet(&self.df, &self.path)
    }

    /// Write `df` atomically and record nothing in the change log. See
    /// [`DetailedExpenses::save_merged`].
    pub(crate) fn save_merged(&self) -> Result<()> {
        write_parquet_atomic(&self.df, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `HOME` (as a guard) at a
    /// fresh temp directory so a test never touches the user's real data
    /// under `~/.local/share/finguard/` or config under `~/.config/finguard/`.
    ///
    /// `XDG_CONFIG_HOME` must be set explicitly rather than left to `HOME`'s
    /// `$HOME/.config` fallback: a caller's environment can already export
    /// `XDG_CONFIG_HOME`, which would then win over `HOME` and point
    /// `config::get_config_dir` at the real `$HOME/.config/finguard`.
    ///
    /// # Safety
    ///
    /// `std::env::set_var` is unsafe because it is not thread-safe; callers
    /// must hold `#[serial_test::serial]` so no other test reads or writes
    /// these variables concurrently.
    fn with_temp_data_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            std::env::set_var("HOME", dir.path());
        }
        dir
    }

    /// Every change recorded so far, in the order it was written.
    fn log_entries() -> Vec<sync::ChangeEntry> {
        sync::read_log().expect("read the change log").entries
    }

    /// The [`ROW_ID_COLUMN`] value of `df`'s only row.
    fn only_row_id(df: &DataFrame) -> String {
        let ids = str_col_to_vec(df, ROW_ID_COLUMN).expect("read the row ID column");
        assert_eq!(ids.len(), 1, "this helper expects a single row");
        ids[0].clone()
    }

    /// The row an [`ChangeOp::Upsert`] carries, or a failure naming what the
    /// entry held instead.
    fn upserted_row(op: &ChangeOp) -> &Map<String, Value> {
        match op {
            ChangeOp::Upsert { row } => row,
            other => panic!("expected an upsert, got {other:?}"),
        }
    }

    /// Pins the wire values `set_quantity_or_price` accepts. This is a
    /// regression test for the bug where the frontend sent `"qty"` and the
    /// backend rejected it with `Error::InvalidArgument`.
    #[test]
    #[serial_test::serial]
    fn set_quantity_or_price_accepts_only_quantity_and_price() {
        let _temp = with_temp_data_home();

        let mut holdings = InvestmentHoldings::new(2026).expect("load holdings");
        holdings
            .add_asset("Test Asset", "Stocks/ETF", "", "EUR")
            .expect("add asset");

        holdings
            .set_quantity_or_price("Test Asset", 1, 10.0, "quantity")
            .expect("'quantity' must be accepted");
        let qty = holdings
            .df
            .column("01")
            .expect("holdings frame has month column")
            .f64()
            .expect("month column is f64")
            .get(0);
        assert_eq!(qty, Some(10.0));

        holdings
            .set_quantity_or_price("Test Asset", 1, 25.5, "price")
            .expect("'price' must be accepted");
        let price = holdings
            .df_prices
            .column("01")
            .expect("prices frame has month column")
            .f64()
            .expect("month column is f64")
            .get(0);
        assert_eq!(price, Some(25.5));

        let qty_err = holdings
            .set_quantity_or_price("Test Asset", 1, 1.0, "qty")
            .expect_err("'qty' must be rejected, not silently accepted");
        assert!(matches!(qty_err, Error::InvalidArgument(_)));

        let other_err = holdings
            .set_quantity_or_price("Test Asset", 1, 1.0, "bogus")
            .expect_err("unrecognized field must be rejected");
        assert!(matches!(other_err, Error::InvalidArgument(_)));
    }

    /// A pre-existing investments file with no `currency` column must
    /// backfill it from the configured reference currency, not a fixed
    /// value: the user can change the reference currency, and existing
    /// holdings were priced in whatever it was at the time.
    #[test]
    #[serial_test::serial]
    fn investment_holdings_backfill_uses_configured_reference_currency() {
        let _temp = with_temp_data_home();
        config::set_currency_settings(&config::CurrencySettings {
            reference_currency: "GBP".to_string(),
            current_month_rate_mode: Default::default(),
        })
        .expect("set currency settings");

        let path = get_year_summary_path(2026, INVESTMENTS_FILENAME).expect("investments path");
        let legacy_row = wide_row(
            &[
                ("asset_name", "Legacy Holding"),
                ("category", "Stocks/ETF"),
                ("link", ""),
            ],
            &new_row_id(),
        )
        .expect("build legacy row without a currency column");
        write_parquet(&legacy_row, &path).expect("write legacy investments file");

        let holdings = InvestmentHoldings::new(2026).expect("load holdings");
        assert_eq!(
            str_col_to_vec(&holdings.df, "currency").expect("read currency column"),
            vec!["GBP"]
        );
    }

    /// Reloading an investments table must migrate a legacy `"E"` currency
    /// value to `"EUR"`, matching every other table with a currency column.
    #[test]
    #[serial_test::serial]
    fn investment_holdings_load_normalizes_legacy_currency() {
        let _temp = with_temp_data_home();

        let mut holdings = InvestmentHoldings::new(2026).expect("load holdings");
        holdings
            .add_asset("Legacy Asset", "Stocks/ETF", "", "E")
            .expect("add legacy asset");
        holdings
            .add_asset("Modern Asset", "Stocks/ETF", "", "USD")
            .expect("add modern asset");

        let reloaded = InvestmentHoldings::new(2026).expect("reload holdings");
        assert_eq!(
            str_col_to_vec(&reloaded.df, "currency").expect("read currency column"),
            vec!["EUR", "USD"]
        );
    }

    /// `set_income` (via `recompute`) must still sum the income categories
    /// into `Income`, and must never write `Spending`, `Saving`, or
    /// `Saving %`: those used to come from `primaries.parquet`, a file this
    /// branch no longer writes.
    #[test]
    #[serial_test::serial]
    fn cashflow_recompute_updates_income_only() {
        let _temp = with_temp_data_home();

        let mut cf = Cashflow::new(2026).expect("load cashflow");
        cf.set_income(3, "Salary", 1000.0).expect("set salary");
        cf.set_income(3, "Other", 200.0).expect("set other income");

        assert_eq!(cf.get_value("Income", "03").expect("read income"), 1200.0);
        assert_eq!(cf.get_value("Spending", "03").expect("read spending"), 0.0);
        assert_eq!(cf.get_value("Saving", "03").expect("read saving"), 0.0);
        assert_eq!(
            cf.get_value("Saving %", "03").expect("read saving pct"),
            0.0
        );
    }

    /// A `cashflow.parquet` written by an older build that persisted
    /// `Spending`/`Saving`/`Saving %` must keep those exact values after a
    /// later edit and recompute: this branch stops writing them, but must
    /// not touch what is already on disk.
    #[test]
    #[serial_test::serial]
    fn cashflow_recompute_preserves_legacy_derived_rows() {
        let _temp = with_temp_data_home();

        // Build a legacy-shaped table by hand: income rows plus derived rows
        // already carrying nonzero Spending/Saving/Saving% values, as an
        // older build would have written them from primaries.parquet.
        let all_cats: Vec<&str> = INCOME_CATEGORIES
            .iter()
            .chain(DERIVED_CATEGORIES.iter())
            .copied()
            .collect();
        let mut cols: Vec<Column> = vec![Column::new("category".into(), &all_cats)];
        for m in month_labels() {
            let values: Vec<f64> = all_cats
                .iter()
                .map(|cat| match *cat {
                    "Salary" if m == "03" => 1000.0,
                    "Spending" if m == "03" => 400.0,
                    "Saving" if m == "03" => 600.0,
                    "Saving %" if m == "03" => 60.0,
                    "Income" if m == "03" => 1000.0,
                    _ => 0.0,
                })
                .collect();
            cols.push(Column::new(m.as_str().into(), values));
        }
        let legacy_df = DataFrame::new_infer_height(cols).expect("build legacy cashflow frame");
        let path = get_year_summary_path(2026, CASHFLOW_FILENAME).expect("cashflow path");
        write_parquet(&legacy_df, &path).expect("write legacy cashflow file");

        // Editing an unrelated month must not disturb the legacy row.
        let mut cf = Cashflow::new(2026).expect("load cashflow");
        cf.set_income(4, "Salary", 50.0).expect("set salary");

        assert_eq!(
            cf.get_value("Spending", "03")
                .expect("read legacy spending"),
            400.0
        );
        assert_eq!(
            cf.get_value("Saving", "03").expect("read legacy saving"),
            600.0
        );
        assert_eq!(
            cf.get_value("Saving %", "03")
                .expect("read legacy saving pct"),
            60.0
        );

        // `set_income` calls `recompute`, which calls `save`. Reload from
        // disk instead of trusting the in-memory frame, so the assertion
        // proves the legacy values actually survived the write, not just
        // that this process's copy was left alone.
        let reloaded = Cashflow::new(2026).expect("reload cashflow from disk");
        assert_eq!(
            reloaded
                .get_value("Spending", "03")
                .expect("read reloaded legacy spending"),
            400.0
        );
        assert_eq!(
            reloaded
                .get_value("Saving", "03")
                .expect("read reloaded legacy saving"),
            600.0
        );
        assert_eq!(
            reloaded
                .get_value("Saving %", "03")
                .expect("read reloaded legacy saving pct"),
            60.0
        );
        assert_eq!(
            reloaded
                .get_value("Income", "04")
                .expect("read reloaded updated income"),
            50.0
        );
    }

    /// `add_asset` must store the given currency on `df`, the frame the API
    /// reads asset metadata from, and must not add a `currency` column to
    /// `df_prices`, which owns none (see the `InvestmentHoldings` struct
    /// docs for why).
    #[test]
    #[serial_test::serial]
    fn investment_holdings_add_asset_stores_currency_on_df_only() {
        let _temp = with_temp_data_home();

        let mut holdings = InvestmentHoldings::new(2026).expect("load holdings");
        holdings
            .add_asset("Test Asset", "Stocks/ETF", "", "USD")
            .expect("add asset");

        assert_eq!(
            str_col_to_vec(&holdings.df, "currency").expect("read currency column"),
            vec!["USD"]
        );
        assert!(
            !has_column(&holdings.df_prices, "currency"),
            "currency belongs on df only, not df_prices"
        );
    }

    /// `df_value` must still multiply quantity by price correctly with a
    /// `currency` column present on `df`.
    #[test]
    #[serial_test::serial]
    fn investment_holdings_df_value_unaffected_by_currency_column() {
        let _temp = with_temp_data_home();

        let mut holdings = InvestmentHoldings::new(2026).expect("load holdings");
        holdings
            .add_asset("Test Asset", "Stocks/ETF", "", "USD")
            .expect("add asset");
        holdings
            .set_quantity("Test Asset", 1, 10.0)
            .expect("set quantity");
        holdings
            .set_price("Test Asset", 1, 25.0)
            .expect("set price");

        let value = holdings.df_value().expect("compute value");
        assert_eq!(
            value
                .column("01")
                .expect("value frame has month column")
                .f64()
                .expect("month column is f64")
                .get(0),
            Some(250.0)
        );
    }

    /// `normalize_currency_column` must rewrite the legacy `"E"` code to
    /// `"EUR"` and leave every other code, such as `"USD"`, unchanged.
    #[test]
    fn normalize_currency_column_maps_legacy_e_to_eur() {
        let df =
            DataFrame::new_infer_height(vec![Column::new("currency".into(), &["E", "USD", "E"])])
                .expect("build test frame");

        let normalized = normalize_currency_column(df).expect("normalize currency column");

        assert_eq!(
            str_col_to_vec(&normalized, "currency").expect("read currency column"),
            vec!["EUR", "USD", "EUR"]
        );
    }

    /// A table without a `currency` column must pass through unchanged
    /// instead of erroring, since not every table always has one at every
    /// stage of the load path.
    #[test]
    fn normalize_currency_column_is_noop_without_currency_column() {
        let df = DataFrame::new_infer_height(vec![Column::new("asset_name".into(), &["Test"])])
            .expect("build test frame");

        let normalized = normalize_currency_column(df.clone()).expect("normalize currency column");

        assert_eq!(normalized.get_column_names(), df.get_column_names());
    }

    /// Reloading a liquidity table must migrate a legacy `"E"` currency value
    /// to `"EUR"`, matching what the `/liquidity` handler in `api.rs` reads.
    #[test]
    #[serial_test::serial]
    fn liquidity_load_normalizes_legacy_currency() {
        let _temp = with_temp_data_home();

        let mut liq = Liquidity::new(2026).expect("load liquidity");
        liq.add_asset("Legacy Account", "Cash", "E")
            .expect("add legacy asset");
        liq.add_asset("Modern Account", "Cash", "USD")
            .expect("add modern asset");

        let reloaded = Liquidity::new(2026).expect("reload liquidity");
        assert_eq!(
            str_col_to_vec(&reloaded.df, "currency").expect("read currency column"),
            vec!["EUR", "USD"]
        );
    }

    /// Reloading a credits/debts table must migrate a legacy `"E"` currency
    /// value to `"EUR"`, matching what the `/credits-debts` handler in
    /// `api.rs` reads.
    #[test]
    #[serial_test::serial]
    fn credits_debts_load_normalizes_legacy_currency() {
        let _temp = with_temp_data_home();

        let mut cd = CreditsDebts::new(2026).expect("load credits/debts");
        cd.add_entry("Legacy Loan", "E").expect("add legacy entry");
        cd.add_entry("Modern Loan", "USD")
            .expect("add modern entry");

        let reloaded = CreditsDebts::new(2026).expect("reload credits/debts");
        assert_eq!(
            str_col_to_vec(&reloaded.df, "currency").expect("read currency column"),
            vec!["EUR", "USD"]
        );
    }

    /// Reloading a recurring-expenses table must migrate a legacy `"E"`
    /// currency value to `"EUR"`, matching what the recurring-expenses
    /// handler in `api.rs` reads.
    #[test]
    #[serial_test::serial]
    fn recurring_expenses_load_normalizes_legacy_currency() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        recurring
            .add("Legacy Rent", 1, 1_000.0, "E", "Housing", "Rent")
            .expect("add legacy recurring expense");
        recurring
            .add("Modern Rent", 1, 1_000.0, "USD", "Housing", "Rent")
            .expect("add modern recurring expense");

        let reloaded = RecurringExpenses::new(2026).expect("reload recurring expenses");
        assert_eq!(
            str_col_to_vec(&reloaded.df, "currency").expect("read currency column"),
            vec!["EUR", "USD"]
        );
    }

    /// Applying a month must give every generated row the ID
    /// `<template row_id>:<year>-<month>`, the ID that lets a later sync
    /// merge the same generated row from two devices.
    #[test]
    #[serial_test::serial]
    fn apply_recurring_gives_generated_rows_derived_ids() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        let template_id = recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add recurring template");

        let mut de = DetailedExpenses::new(2026, 3).expect("load march expenses");
        let pending = recurring
            .pending_for_month(&de)
            .expect("compute pending rows");
        recurring
            .insert_resolved(&mut de, &pending)
            .expect("insert pending rows");

        let row_ids = str_col_to_vec(&de.expense_df, ROW_ID_COLUMN).expect("read row_id column");
        assert_eq!(row_ids, vec![format!("{template_id}:2026-03")]);
    }

    /// Applying the same month a second time must add nothing: the row
    /// generated the first time already carries the derived ID, so
    /// `pending_for_month` skips the template and the row count is unchanged.
    #[test]
    #[serial_test::serial]
    fn apply_recurring_twice_adds_nothing_second_time() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add recurring template");

        let mut de = DetailedExpenses::new(2026, 3).expect("load march expenses");
        let first_pending = recurring
            .pending_for_month(&de)
            .expect("compute pending rows");
        let first_added = recurring
            .insert_resolved(&mut de, &first_pending)
            .expect("insert pending rows");
        assert_eq!(first_added.len(), 1);

        let second_pending = recurring
            .pending_for_month(&de)
            .expect("compute pending rows again");
        assert!(
            second_pending.is_empty(),
            "a second apply must find nothing pending"
        );
        assert_eq!(de.expense_df.height(), 1, "the row count must not change");
    }

    /// A month already holding a row with the template's name and day, but a
    /// random UUID `row_id` from before derived IDs existed, must still be
    /// skipped: `pending_for_month` keeps the name-and-day check for exactly
    /// this case.
    #[test]
    #[serial_test::serial]
    fn apply_recurring_skips_legacy_row_with_random_id() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add recurring template");

        let mut de = DetailedExpenses::new(2026, 3).expect("load march expenses");
        de.add_row("Rent", 1, 1_000.0, Some("Housing"), "EUR", Some("Rent"))
            .expect("add a legacy row with a random row_id");

        let pending = recurring
            .pending_for_month(&de)
            .expect("compute pending rows");
        assert!(
            pending.is_empty(),
            "the name-and-day match must still skip the template"
        );
    }

    /// Two `DetailedExpenses` loads of the same month must derive the same
    /// ID for the same template, independently of each other: this is the
    /// property sync depends on to merge, rather than duplicate, the rows
    /// each device generates from the same recurring template.
    #[test]
    #[serial_test::serial]
    fn pending_for_month_derives_same_id_across_instances() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        let template_id = recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add recurring template");

        let de_a = DetailedExpenses::new(2026, 3).expect("load march expenses (device a)");
        let de_b = DetailedExpenses::new(2026, 3).expect("load march expenses (device b)");

        let pending_a = recurring
            .pending_for_month(&de_a)
            .expect("compute pending rows (a)");
        let pending_b = recurring
            .pending_for_month(&de_b)
            .expect("compute pending rows (b)");

        assert_eq!(pending_a.len(), 1);
        assert_eq!(pending_b.len(), 1);
        assert_eq!(pending_a[0].row_id, pending_b[0].row_id);
        assert_eq!(pending_a[0].row_id, format!("{template_id}:2026-03"));
    }

    /// After a generated row is renamed (still keeping its `row_id`, as
    /// [`DetailedExpenses::edit_row`] guarantees), the name-and-day check can
    /// no longer see it, so `pending_for_month` must still skip the template
    /// through the derived-ID check alone. Deleting that check's `.or(...)`
    /// branch was confirmed to make this test fail (see the change report).
    #[test]
    #[serial_test::serial]
    fn pending_for_month_skips_generated_row_after_it_is_renamed() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add recurring template");

        let mut de = DetailedExpenses::new(2026, 3).expect("load march expenses");
        let pending = recurring
            .pending_for_month(&de)
            .expect("compute pending rows");
        let row_id = pending[0].row_id.clone();
        recurring
            .insert_resolved(&mut de, &pending)
            .expect("insert pending rows");

        de.edit_row(
            &row_id,
            Some("No Longer Rent"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("rename the generated row, keeping its row_id");

        let second_pending = recurring
            .pending_for_month(&de)
            .expect("compute pending rows after the rename");
        assert!(
            second_pending.is_empty(),
            "the derived-ID check must still skip the renamed row"
        );
    }

    /// Two recurring templates sharing a `row_id` (for example a
    /// hand-restored `recurring_expenses.parquet`) must not both land in the
    /// same month: both derive the same ID for the month, and inserting both
    /// would leave the month with a duplicate `row_id` that
    /// `require_single_row` then rejects for every future edit or delete.
    #[test]
    #[serial_test::serial]
    fn insert_resolved_skips_a_second_template_sharing_a_row_id() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        let template_id = recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add the first recurring template");

        // Simulate a corrupted or hand-restored file: a second template that
        // shares the first template's row_id instead of getting its own.
        let duplicate_template = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Internet"]),
            Column::new("expense_day".into(), &[5i64]),
            Column::new("expense_amount".into(), &[40.0]),
            Column::new("currency".into(), &["EUR"]),
            Column::new("primary_category".into(), &["Housing"]),
            Column::new("secondary_category".into(), &["Internet"]),
            Column::new(ROW_ID_COLUMN.into(), &[template_id.as_str()]),
        ])
        .expect("build a duplicate-id template row");
        recurring.df = concat_df_diagonal(&[recurring.df.clone(), duplicate_template])
            .expect("append the duplicate-id template");

        let mut de = DetailedExpenses::new(2026, 3).expect("load march expenses");
        let pending = recurring
            .pending_for_month(&de)
            .expect("compute pending rows");
        assert_eq!(
            pending.len(),
            2,
            "both templates derive the same ID and are both pending"
        );

        let added = recurring
            .insert_resolved(&mut de, &pending)
            .expect("insert pending rows");
        assert_eq!(
            added,
            vec!["Rent".to_string()],
            "the second template must be skipped, and not counted as added"
        );
        assert_eq!(
            de.expense_df.height(),
            1,
            "only one row may land in the month under the shared id"
        );
    }

    /// The state the reinstate tests start from: a 2026 "Rent" template, its
    /// row generated into March, and that row then deleted by the user.
    /// Returns the loaded tables and the template's `row_id`; the deleted
    /// row's id is `<template id>:2026-03`.
    fn march_rent_generated_then_deleted() -> (RecurringExpenses, DetailedExpenses, String) {
        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        let template_id = recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add recurring template");

        let mut de = DetailedExpenses::new(2026, 3).expect("load march expenses");
        let plan = recurring.plan_for_month(&de).expect("plan march");
        recurring
            .insert_resolved(&mut de, &plan.pending)
            .expect("insert pending rows");
        de.delete_row(&format!("{template_id}:2026-03"))
            .expect("delete the generated row");

        (recurring, de, template_id)
    }

    /// Applying a month again after the user deleted a row it generated must
    /// not bring the row back, and must report the template as skipped. The
    /// deleted row is absent from the month's dataframe, which is exactly
    /// what used to make it look due, so the answer comes from the change
    /// log.
    #[test]
    #[serial_test::serial]
    fn plan_for_month_skips_a_generated_row_the_user_deleted() {
        let _temp = with_temp_data_home();
        let (recurring, de, template_id) = march_rent_generated_then_deleted();

        let plan = recurring.plan_for_month(&de).expect("plan march again");
        assert!(
            plan.pending.is_empty(),
            "a deleted generated row must not be pending again"
        );
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].row_id, format!("{template_id}:2026-03"));
        assert_eq!(plan.skipped[0].template_row_id, template_id);
        assert_eq!(plan.skipped[0].expense_name, "Rent");
        assert_eq!(plan.skipped[0].expense_day, 1);
        assert_eq!(de.expense_df.height(), 0, "the month must stay empty");
    }

    /// One plan must carry both answers at once. Each template is decided on
    /// its own, so a standing deletion on one must not withhold another
    /// template that is legitimately due, and the row that is generated must
    /// be the due one.
    #[test]
    #[serial_test::serial]
    fn plan_for_month_reports_a_deleted_and_a_due_template_together() {
        let _temp = with_temp_data_home();
        let (mut recurring, mut de, deleted_template) = march_rent_generated_then_deleted();

        let due_template = recurring
            .add("Internet", 5, 40.0, "EUR", "Housing", "Internet")
            .expect("add a second template");

        let plan = recurring.plan_for_month(&de).expect("plan march again");
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].template_row_id, due_template);
        assert_eq!(plan.pending[0].row_id, format!("{due_template}:2026-03"));
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].template_row_id, deleted_template);
        assert_eq!(
            plan.skipped[0].row_id,
            format!("{deleted_template}:2026-03")
        );

        let added = recurring
            .insert_resolved(&mut de, &plan.pending)
            .expect("insert pending rows");
        assert_eq!(added, vec!["Internet".to_string()]);
        assert_eq!(
            str_col_to_vec(&de.expense_df, ROW_ID_COLUMN).expect("read row_id column"),
            vec![format!("{due_template}:2026-03")],
            "only the due template's row may land in the month"
        );
    }

    /// The other half of a reinstate is decided by the merge engine, not by
    /// this file: the entry it wrote must beat the older delete when the two
    /// devices meet. The same entry marked as generated must lose to that
    /// delete instead, which is why [`RecurringExpenses::reinstate_for_month`]
    /// passes no origin.
    #[test]
    #[serial_test::serial]
    fn a_reinstated_row_beats_the_older_delete_at_the_next_merge() {
        use crate::merge::{MergeOutcome, RowAction, SkipReason, plan_merge};

        let _temp = with_temp_data_home();
        let (recurring, mut de, template_id) = march_rent_generated_then_deleted();
        let row_id = format!("{template_id}:2026-03");
        recurring
            .reinstate_for_month(&mut de, &template_id)
            .expect("reinstate the deleted row");

        // The other device's log stops at the delete; the reinstate is the
        // one entry this device has to send it.
        let entries = log_entries();
        let (local, remote) = entries.split_at(entries.len() - 1);
        assert_eq!(remote[0].row_id, row_id);
        assert_eq!(remote[0].origin, None);
        assert!(
            local
                .iter()
                .any(|entry| entry.row_id == row_id && entry.op == ChangeOp::Delete),
            "the other device must still hold the delete this reinstate has to beat"
        );

        let plan = plan_merge(local, remote);
        assert_eq!(plan.decisions.len(), 1);
        assert_eq!(plan.decisions[0].outcome, MergeOutcome::Applied);
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].row_id, row_id);
        assert!(
            matches!(plan.actions[0].action, RowAction::Upsert { .. }),
            "the reinstated row must be written back"
        );

        let mut as_generated = remote[0].clone();
        as_generated.origin = Some(ChangeOrigin::RecurringApply);
        let held_back = plan_merge(local, &[as_generated]);
        assert!(
            matches!(
                held_back.decisions[0].outcome,
                MergeOutcome::Skipped(SkipReason::GeneratedRowStaysDeleted(_))
            ),
            "the same entry marked generated must lose to the delete: {:?}",
            held_back.decisions[0].outcome
        );
        assert!(
            !held_back
                .actions
                .iter()
                .any(|action| matches!(action.action, RowAction::Upsert { .. })),
            "a generated upsert must not write the row back"
        );
    }

    /// A template whose row was never deleted is still due, and nothing is
    /// reported as skipped.
    #[test]
    #[serial_test::serial]
    fn plan_for_month_reports_an_untouched_template_as_pending() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        let template_id = recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add recurring template");

        let de = DetailedExpenses::new(2026, 3).expect("load march expenses");
        let plan = recurring.plan_for_month(&de).expect("plan march");
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].row_id, format!("{template_id}:2026-03"));
        assert!(plan.skipped.is_empty());
    }

    /// Deleting a generated row must only bind the month it was in. The
    /// derived ID carries the month, so another month's row is another row
    /// and stays due.
    #[test]
    #[serial_test::serial]
    fn deleting_a_generated_row_leaves_another_month_due() {
        let _temp = with_temp_data_home();
        let (recurring, _march, template_id) = march_rent_generated_then_deleted();

        let april = DetailedExpenses::new(2026, 4).expect("load april expenses");
        let plan = recurring.plan_for_month(&april).expect("plan april");
        assert_eq!(plan.pending.len(), 1, "april is untouched and still due");
        assert_eq!(plan.pending[0].row_id, format!("{template_id}:2026-04"));
        assert!(plan.skipped.is_empty());
    }

    /// Reinstating a skipped row creates it under the derived ID and records
    /// an ordinary upsert with no origin. The missing origin is what makes
    /// the reinstate survive the next sync: the merge engine holds a
    /// generated upsert back from overriding the old delete, so marking this
    /// row generated would make it vanish again on both devices.
    #[test]
    #[serial_test::serial]
    fn reinstating_a_skipped_row_records_an_ordinary_upsert() {
        let _temp = with_temp_data_home();
        let (recurring, mut de, template_id) = march_rent_generated_then_deleted();
        let row_id = format!("{template_id}:2026-03");

        let reinstated = recurring
            .reinstate_for_month(&mut de, &template_id)
            .expect("reinstate the deleted row");
        assert!(reinstated.created);
        assert_eq!(reinstated.row_id, row_id);
        assert_eq!(de.expense_df.height(), 1);
        assert_eq!(
            str_col_to_vec(&de.expense_df, ROW_ID_COLUMN).expect("read row_id column"),
            vec![row_id.clone()]
        );

        let entries = log_entries();
        let last = entries
            .iter()
            .rev()
            .find(|entry| entry.row_id == row_id)
            .expect("the reinstated row is recorded");
        assert_eq!(last.origin, None, "a confirmed reinstate is a user action");
        assert!(
            matches!(last.op, ChangeOp::Upsert { .. }),
            "the reinstate must record the whole row"
        );
    }

    /// Applying the month again after a reinstate must neither add a second
    /// row nor report the template as skipped: the row is back in the
    /// dataframe, so the template is not due at all.
    #[test]
    #[serial_test::serial]
    fn applying_after_a_reinstate_adds_nothing_and_skips_nothing() {
        let _temp = with_temp_data_home();
        let (recurring, mut de, template_id) = march_rent_generated_then_deleted();
        recurring
            .reinstate_for_month(&mut de, &template_id)
            .expect("reinstate the deleted row");

        let plan = recurring.plan_for_month(&de).expect("plan march again");
        assert!(plan.pending.is_empty());
        assert!(plan.skipped.is_empty());

        let added = recurring
            .insert_resolved(&mut de, &plan.pending)
            .expect("insert pending rows");
        assert!(added.is_empty());
        assert_eq!(de.expense_df.height(), 1, "the row must not be duplicated");
    }

    /// Confirming the same reinstate twice, from a double click or a second
    /// open tab, must leave one row. A duplicate `row_id` in one month would
    /// leave both copies rejected by every later edit or delete.
    #[test]
    #[serial_test::serial]
    fn reinstating_twice_creates_one_row() {
        let _temp = with_temp_data_home();
        let (recurring, mut de, template_id) = march_rent_generated_then_deleted();

        recurring
            .reinstate_for_month(&mut de, &template_id)
            .expect("reinstate the deleted row");
        let entries_after_first = log_entries().len();

        let second = recurring
            .reinstate_for_month(&mut de, &template_id)
            .expect("a second reinstate succeeds");
        assert!(!second.created, "the second call must write nothing");
        assert_eq!(de.expense_df.height(), 1);
        assert_eq!(
            log_entries().len(),
            entries_after_first,
            "a call that writes nothing records nothing"
        );
    }

    /// Reinstating an ID no template holds is [`Error::NotFound`], and two
    /// templates sharing an ID make the request ambiguous, so it is refused
    /// with [`Error::AlreadyExists`] and nothing is written.
    #[test]
    #[serial_test::serial]
    fn reinstate_refuses_an_unknown_or_duplicated_template() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        let template_id = recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add recurring template");
        let mut de = DetailedExpenses::new(2026, 3).expect("load march expenses");

        let unknown = recurring
            .reinstate_for_month(&mut de, "no-such-template")
            .expect_err("an unknown template must be refused");
        assert!(matches!(unknown, Error::NotFound(_)), "{unknown}");

        let duplicate_template = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Internet"]),
            Column::new("expense_day".into(), &[5i64]),
            Column::new("expense_amount".into(), &[40.0]),
            Column::new("currency".into(), &["EUR"]),
            Column::new("primary_category".into(), &["Housing"]),
            Column::new("secondary_category".into(), &["Internet"]),
            Column::new(ROW_ID_COLUMN.into(), &[template_id.as_str()]),
        ])
        .expect("build a duplicate-id template row");
        recurring.df = concat_df_diagonal(&[recurring.df.clone(), duplicate_template])
            .expect("append the duplicate-id template");

        let ambiguous = recurring
            .reinstate_for_month(&mut de, &template_id)
            .expect_err("a shared template id must be refused");
        assert!(matches!(ambiguous, Error::AlreadyExists(_)), "{ambiguous}");
        assert_eq!(de.expense_df.height(), 0, "nothing may be written");
    }

    /// Reloading a detailed-expenses table must migrate a legacy `"E"`
    /// currency value to `"EUR"`, matching what the expenses handler in
    /// `api.rs` reads.
    #[test]
    #[serial_test::serial]
    fn detailed_expenses_load_normalizes_legacy_currency() {
        let _temp = with_temp_data_home();

        let mut de = DetailedExpenses::new(2026, 1).expect("load detailed expenses");
        de.add_row(
            "Legacy Purchase",
            1,
            10.0,
            Some("Housing"),
            "E",
            Some("Rent"),
        )
        .expect("add legacy expense row");
        de.add_row(
            "Modern Purchase",
            1,
            10.0,
            Some("Housing"),
            "USD",
            Some("Rent"),
        )
        .expect("add modern expense row");

        let reloaded = DetailedExpenses::new(2026, 1).expect("reload detailed expenses");
        assert_eq!(
            str_col_to_vec(&reloaded.expense_df, "currency").expect("read currency column"),
            vec!["EUR", "USD"]
        );
    }

    /// A parquet file carrying the removed `expense_in_ref_currency`,
    /// `fx_rate`, and `rate_date` columns must still load: those columns are
    /// dropped rather than erroring, and the row's facts
    /// (`expense_amount`/`currency`) survive untouched.
    #[test]
    #[serial_test::serial]
    fn load_drops_stale_fx_columns_but_keeps_facts() {
        let _temp = with_temp_data_home();
        let path = get_monthly_parquet_path(2026, 9).expect("monthly parquet path");
        std::fs::create_dir_all(path.parent().unwrap()).expect("create year dir");

        let date = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        let pre_migration = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Old Rent"]),
            date_series("expense_date", &[date]).into(),
            Column::new("expense_amount".into(), &[1_200.0]),
            Column::new("currency".into(), &["EUR"]),
            Column::new("expense_in_ref_currency".into(), &[1_200.0]),
            Column::new("fx_rate".into(), &[1.0]),
            date_series("rate_date", &[date]).into(),
            Column::new("primary_category".into(), &["Housing"]),
            Column::new("secondary_category".into(), &["Rent"]),
            Column::new(ROW_ID_COLUMN.into(), &["old-rent-id"]),
        ])
        .expect("build a file written under the old schema");
        write_parquet(&pre_migration, &path).expect("write pre-migration file");

        let de = DetailedExpenses::new(2026, 9).expect("load and drop stale columns");

        for stale_column in ["expense_in_ref_currency", "fx_rate", "rate_date"] {
            assert!(
                !has_column(&de.expense_df, stale_column),
                "'{stale_column}' must be dropped on load"
            );
        }
        assert_eq!(
            de.expense_df
                .column("expense_amount")
                .unwrap()
                .f64()
                .unwrap()
                .get(0),
            Some(1_200.0),
            "the stored amount must survive dropping the stale columns"
        );
        assert_eq!(
            str_col_to_vec(&de.expense_df, "currency").unwrap(),
            vec!["EUR"]
        );
    }

    /// `resolve_fact` must report rate `1.0` for a reference-currency row
    /// without ever looking `rates` up, so an empty rate table is enough.
    #[test]
    fn resolve_fact_identity_for_reference_currency() {
        let expense_date = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        let fact = ExpenseFact {
            expense_date,
            currency: "EUR".to_string(),
            expense_amount: 100.0,
            primary_category: "Housing".to_string(),
            secondary_category: "Rent".to_string(),
        };

        let (rate, rate_date) =
            resolve_fact(&fact, "EUR", &std::collections::HashMap::new()).unwrap();
        assert_eq!(rate, 1.0);
        assert_eq!(rate_date, expense_date);
    }

    /// `resolve_fact` must report exactly the rate and date resolved for the
    /// fact's own `expense_date`, regardless of what "today" is: the caller
    /// (an async handler) already did the resolution, so this is a pure map
    /// lookup with no room for a different date to leak in.
    #[test]
    fn resolve_fact_uses_the_pre_resolved_rate_for_its_own_date() {
        let expense_date = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        let published_date = NaiveDate::from_ymd_opt(2026, 9, 3).unwrap();
        let fact = ExpenseFact {
            expense_date,
            currency: "usd".to_string(),
            expense_amount: 100.0,
            primary_category: "Groceries".to_string(),
            secondary_category: "OtherGroceries".to_string(),
        };
        let mut rates = std::collections::HashMap::new();
        rates.insert(
            (expense_date, "USD".to_string()),
            crate::fx::ResolvedRate {
                rate: 0.86,
                rate_date: Some(published_date),
            },
        );

        let (rate, rate_date) = resolve_fact(&fact, "EUR", &rates).unwrap();
        assert_eq!(rate, 0.86);
        assert_eq!(rate_date, published_date);
    }

    /// A currency/date pair missing from `rates` is an error, not a silent
    /// `1.0`: that silent fallback is exactly the bug this feature removes.
    #[test]
    fn resolve_fact_errors_when_the_pair_was_not_resolved() {
        let fact = ExpenseFact {
            expense_date: NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(),
            currency: "USD".to_string(),
            expense_amount: 100.0,
            primary_category: "Groceries".to_string(),
            secondary_category: "OtherGroceries".to_string(),
        };

        let err = resolve_fact(&fact, "EUR", &std::collections::HashMap::new()).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    /// A null date stays null through `date_col_to_vec`: it is "no date",
    /// never 1970-01-01.
    #[test]
    fn date_col_to_vec_preserves_nulls_instead_of_inventing_epoch() {
        let days = Series::new("expense_date".into(), &[Some(20700i32), None])
            .cast(&DataType::Date)
            .expect("i32 with null casts to Date");
        let df = DataFrame::new(2, vec![days.into()]).expect("frame");

        let dates = date_col_to_vec(&df, "expense_date").expect("read dates");

        assert_eq!(dates.len(), 2);
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        assert_eq!(dates[0], Some(epoch + chrono::Duration::days(20700)));
        assert_eq!(dates[1], None);
    }

    /// `facts_from_df` skips dateless rows: with no date a row has no rate
    /// key and no display day, so it belongs to neither totals nor lists.
    /// This matches the Expenses list, which already skips rows whose day
    /// is missing, and keeps a dateless row from implicating its currency
    /// as unavailable.
    #[test]
    fn facts_from_df_skips_rows_without_a_date() {
        let df = DataFrame::new(
            2,
            vec![
                Series::new("expense_date".into(), &[Some(20700i32), None])
                    .cast(&DataType::Date)
                    .expect("cast dates")
                    .into(),
                Series::new("currency".into(), &["EUR", "USD"]).into(),
                Series::new("expense_amount".into(), &[10.0, 20.0]).into(),
                Series::new("primary_category".into(), &["A", "B"]).into(),
                Series::new("secondary_category".into(), &["a", "b"]).into(),
            ],
        )
        .expect("frame");

        let facts = facts_from_df(&df).expect("read facts");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].currency, "EUR");
        assert_eq!(facts[0].expense_amount, 10.0);
    }

    /// `distinct_rate_keys` must dedupe repeated `(date, currency)` pairs and
    /// exclude the reference currency entirely, since that case never needs
    /// a lookup.
    #[test]
    fn distinct_rate_keys_dedupes_and_excludes_the_reference_currency() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        let facts = vec![
            ExpenseFact {
                expense_date: date,
                currency: "usd".to_string(),
                expense_amount: 10.0,
                primary_category: String::new(),
                secondary_category: String::new(),
            },
            ExpenseFact {
                expense_date: date,
                currency: "USD".to_string(),
                expense_amount: 20.0,
                primary_category: String::new(),
                secondary_category: String::new(),
            },
            ExpenseFact {
                expense_date: date,
                currency: "EUR".to_string(),
                expense_amount: 30.0,
                primary_category: String::new(),
                secondary_category: String::new(),
            },
        ];

        assert_eq!(
            distinct_rate_keys(&facts, "eur"),
            vec![(date, "USD".to_string())]
        );
    }

    /// A `row_id` shared by two rows (only possible through a damaged or
    /// hand-edited file) must make edit and delete refuse with
    /// `AlreadyExists` and leave the file byte-for-byte unchanged, instead of
    /// changing both rows.
    #[test]
    #[serial_test::serial]
    fn duplicate_row_id_changes_nothing() {
        let _temp = with_temp_data_home();

        let date = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        let expenses = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Rent", "Tea", "Bus"]),
            date_series("expense_date", &[date, date, date]).into(),
            Column::new("expense_amount".into(), &[900.0, 3.0, 2.0]),
            Column::new("currency".into(), &["EUR", "EUR", "EUR"]),
            Column::new("primary_category".into(), &["Housing", "Out", "Transport"]),
            Column::new("secondary_category".into(), &["Rent", "Cafe", "Bus"]),
            Column::new(ROW_ID_COLUMN.into(), &["twin", "twin", "single"]),
        ])
        .unwrap();
        let expenses_path = get_monthly_parquet_path(2026, 9).unwrap();
        write_parquet(&expenses, &expenses_path).unwrap();
        let expenses_before = std::fs::read(&expenses_path).unwrap();

        let mut de = DetailedExpenses::new(2026, 9).unwrap();
        let err = de.delete_row("twin").expect_err("delete must refuse");
        assert!(matches!(err, Error::AlreadyExists(_)), "{err}");
        assert!(err.to_string().contains("nothing was changed"), "{err}");
        let err = de
            .edit_row("twin", Some("Changed"), None, None, None, None, None)
            .expect_err("edit must refuse");
        assert!(matches!(err, Error::AlreadyExists(_)), "{err}");
        assert_eq!(std::fs::read(&expenses_path).unwrap(), expenses_before);
        assert_eq!(de.expense_df.height(), 3);

        // The unique row is still editable and deletable.
        de.delete_row("single")
            .expect("a unique row_id still deletes");
        assert_eq!(de.expense_df.height(), 2);

        let recurring = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Gym", "Gym"]),
            Column::new("expense_day".into(), &[5_i64, 5]),
            Column::new("expense_amount".into(), &[40.0, 40.0]),
            Column::new("currency".into(), &["EUR", "EUR"]),
            Column::new("primary_category".into(), &["Leisure", "Leisure"]),
            Column::new("secondary_category".into(), &["Gym", "Gym"]),
            Column::new(ROW_ID_COLUMN.into(), &["twin", "twin"]),
        ])
        .unwrap();
        let recurring_path = get_year_summary_path(2026, RECURRING_EXPENSES_FILENAME).unwrap();
        write_parquet(&recurring, &recurring_path).unwrap();
        let recurring_before = std::fs::read(&recurring_path).unwrap();

        let mut rec = RecurringExpenses::new(2026).unwrap();
        let err = rec.remove("twin").expect_err("remove must refuse");
        assert!(matches!(err, Error::AlreadyExists(_)), "{err}");
        assert_eq!(std::fs::read(&recurring_path).unwrap(), recurring_before);
        assert_eq!(rec.df.height(), 2);
    }

    /// Creating, editing, and deleting one expense records three changes to
    /// the same row, in that order, each stamped after the one before it.
    /// This is the shape the merge engine needs: the newest whole row wins,
    /// and a delete after an edit keeps the row gone.
    #[test]
    #[serial_test::serial]
    fn an_expense_records_its_create_edit_and_delete() {
        let _temp = with_temp_data_home();

        let mut de = DetailedExpenses::new(2026, 3).expect("load march expenses");
        let row_id = de
            .add_row("Rent", 4, 1_000.0, Some("Housing"), "EUR", Some("Rent"))
            .expect("add the row");
        de.edit_row(
            &row_id,
            Some("Rent March"),
            None,
            Some(1_100.0),
            None,
            None,
            None,
        )
        .expect("edit the row");
        de.delete_row(&row_id).expect("delete the row");

        let entries = log_entries();
        assert_eq!(entries.len(), 3, "one entry per change");
        for entry in &entries {
            assert_eq!(entry.row_id, row_id);
            assert_eq!(
                entry.table,
                ChangeTable::Expenses {
                    year: 2026,
                    month: 3
                }
            );
            assert_eq!(entry.origin, None, "a typed row carries no origin");
        }
        assert!(
            entries[0].stamp < entries[1].stamp && entries[1].stamp < entries[2].stamp,
            "stamps must increase: {:?}",
            entries.iter().map(|e| e.stamp).collect::<Vec<_>>()
        );

        let created = upserted_row(&entries[0].op);
        assert_eq!(created["expense_name"], Value::from("Rent"));
        assert_eq!(created["expense_amount"], Value::from(1_000.0));
        assert_eq!(created["currency"], Value::from("EUR"));
        assert_eq!(created["row_id"], Value::from(row_id.as_str()));
        // A date is carried as the day it names, not as a day count.
        assert_eq!(created["expense_date"], Value::from("2026-03-04"));

        let edited = upserted_row(&entries[1].op);
        assert_eq!(edited["expense_name"], Value::from("Rent March"));
        assert_eq!(edited["expense_amount"], Value::from(1_100.0));
        assert_eq!(edited["row_id"], Value::from(row_id.as_str()));

        assert_eq!(entries[2].op, ChangeOp::Delete);
    }

    /// A row `/api/recurring/apply` generates is marked
    /// `origin: recurring_apply`, and nothing else is. The merge rule that
    /// keeps a deleted generated row deleted has no other way to tell a
    /// generated row from a hand typed one: both carry the derived ID.
    #[test]
    #[serial_test::serial]
    fn only_an_applied_recurring_row_is_marked_generated() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        let template_id = recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add the template");
        let mut de = DetailedExpenses::new(2026, 3).expect("load march expenses");
        let pending = recurring
            .pending_for_month(&de)
            .expect("compute pending rows");
        recurring
            .insert_resolved(&mut de, &pending)
            .expect("insert the generated row");
        de.add_row("Tea", 2, 3.0, Some("Out"), "EUR", Some("Cafe"))
            .expect("add a typed row");

        let entries = log_entries();
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].table, ChangeTable::Recurring { year: 2026 });
        assert_eq!(entries[0].row_id, template_id);
        assert_eq!(
            entries[0].origin, None,
            "adding a template is a user action"
        );

        assert_eq!(
            entries[1].table,
            ChangeTable::Expenses {
                year: 2026,
                month: 3
            }
        );
        assert_eq!(entries[1].row_id, format!("{template_id}:2026-03"));
        assert_eq!(entries[1].origin, Some(ChangeOrigin::RecurringApply));

        assert_eq!(entries[2].origin, None, "a typed row carries no origin");
    }

    /// Deleting a recurring template records the delete against the
    /// template's own table, and leaves the rows it already generated alone.
    #[test]
    #[serial_test::serial]
    fn removing_a_recurring_template_records_a_delete() {
        let _temp = with_temp_data_home();

        let mut recurring = RecurringExpenses::new(2026).expect("load recurring expenses");
        let template_id = recurring
            .add("Rent", 1, 1_000.0, "EUR", "Housing", "Rent")
            .expect("add the template");
        recurring.remove(&template_id).expect("remove the template");

        let entries = log_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].table, ChangeTable::Recurring { year: 2026 });
        assert_eq!(entries[1].row_id, template_id);
        assert_eq!(entries[1].op, ChangeOp::Delete);
    }

    /// A net worth row is created and deleted as a whole row, and every edit
    /// in between is one cell, under the column name the Parquet file uses.
    /// A month column is the zero padded number, so the December cell is
    /// `"12"` and never `"December"` or `12`.
    #[test]
    #[serial_test::serial]
    fn a_liquidity_row_records_a_cell_per_edit() {
        let _temp = with_temp_data_home();

        let mut liquidity = Liquidity::new(2026).expect("load liquidity");
        liquidity
            .add_asset("Main", "Cash", "EUR")
            .expect("add the account");
        let row_id = only_row_id(&liquidity.df);
        liquidity
            .set_value("Main", 3, 120.5)
            .expect("set the march value");
        liquidity
            .set_category("Main", "Other")
            .expect("set the category");
        liquidity
            .set_currency("Main", "USD")
            .expect("set the currency");
        liquidity
            .rename_asset("Main", "Wallet")
            .expect("rename the account");
        liquidity
            .remove_asset("Wallet")
            .expect("remove the account");

        let entries = log_entries();
        assert_eq!(entries.len(), 6, "one entry per change");
        for entry in &entries {
            assert_eq!(entry.table, ChangeTable::Liquidity { year: 2026 });
            assert_eq!(entry.row_id, row_id);
            assert_eq!(entry.origin, None);
        }

        let created = upserted_row(&entries[0].op);
        assert_eq!(created["asset_name"], Value::from("Main"));
        assert_eq!(created["currency"], Value::from("EUR"));
        assert_eq!(created["01"], Value::from(0.0));
        assert_eq!(created["row_id"], Value::from(row_id.as_str()));

        assert_eq!(
            entries[1].op,
            ChangeOp::Cell {
                column: "03".to_string(),
                value: Value::from(120.5),
            }
        );
        assert_eq!(
            entries[2].op,
            ChangeOp::Cell {
                column: "category".to_string(),
                value: Value::from("Other"),
            }
        );
        assert_eq!(
            entries[3].op,
            ChangeOp::Cell {
                column: "currency".to_string(),
                value: Value::from("USD"),
            }
        );
        assert_eq!(
            entries[4].op,
            ChangeOp::Cell {
                column: "asset_name".to_string(),
                value: Value::from("Wallet"),
            }
        );
        assert_eq!(entries[5].op, ChangeOp::Delete);
    }

    /// A credit or debt row records the same way a liquidity row does, under
    /// its own key column, which is `name` rather than `asset_name`.
    #[test]
    #[serial_test::serial]
    fn a_credit_or_debt_row_records_a_cell_per_edit() {
        let _temp = with_temp_data_home();

        let mut credits_debts = CreditsDebts::new(2026).expect("load credits and debts");
        credits_debts
            .add_entry("Mortgage", "EUR")
            .expect("add the entry");
        let row_id = only_row_id(&credits_debts.df);
        credits_debts
            .set_value("Mortgage", 12, -40_600.0)
            .expect("set the december value");
        credits_debts
            .rename_entry("Mortgage", "House loan")
            .expect("rename the entry");
        credits_debts
            .set_currency("House loan", "USD")
            .expect("set the currency");
        credits_debts
            .remove_entry("House loan")
            .expect("remove the entry");

        let entries = log_entries();
        assert_eq!(entries.len(), 5);
        for entry in &entries {
            assert_eq!(entry.table, ChangeTable::CreditsDebts { year: 2026 });
            assert_eq!(entry.row_id, row_id);
        }
        assert_eq!(
            entries[1].op,
            ChangeOp::Cell {
                column: "12".to_string(),
                value: Value::from(-40_600.0),
            }
        );
        assert_eq!(
            entries[2].op,
            ChangeOp::Cell {
                column: "name".to_string(),
                value: Value::from("House loan"),
            }
        );
        assert_eq!(entries[4].op, ChangeOp::Delete);
    }

    /// An investment change is recorded against the file it changed. The
    /// holdings and the prices are two files and two tables, so a quantity
    /// and a price never overwrite each other, while a shared field such as
    /// the link is recorded once per file under the asset's one row ID.
    #[test]
    #[serial_test::serial]
    fn an_investment_change_names_the_file_it_changed() {
        let _temp = with_temp_data_home();
        let holdings_table = ChangeTable::Investments { year: 2026 };
        let prices_table = ChangeTable::InvestmentsPrices { year: 2026 };

        let mut holdings = InvestmentHoldings::new(2026).expect("load holdings");
        holdings
            .add_asset("VWCE", "Stocks/ETF", "https://example.invalid/vwce", "EUR")
            .expect("add the asset");
        let row_id = only_row_id(&holdings.df);
        assert_eq!(
            only_row_id(&holdings.df_prices),
            row_id,
            "one asset, one row ID in both files"
        );
        holdings
            .set_quantity("VWCE", 1, 10.0)
            .expect("set the quantity");
        holdings.set_price("VWCE", 1, 25.5).expect("set the price");
        holdings
            .set_currency("VWCE", "USD")
            .expect("set the currency");
        holdings
            .set_link("VWCE", "https://example.invalid/new")
            .expect("set the link");
        holdings.remove_asset("VWCE").expect("remove the asset");

        let entries = log_entries();
        assert!(
            entries.iter().all(|entry| entry.row_id == row_id),
            "every entry belongs to the one asset"
        );
        let tables: Vec<&ChangeTable> = entries.iter().map(|entry| &entry.table).collect();
        let ops: Vec<&ChangeOp> = entries.iter().map(|entry| &entry.op).collect();

        assert_eq!(
            tables,
            vec![
                &holdings_table, // the new holdings row
                &prices_table,   // the new prices row
                &holdings_table, // the quantity
                &prices_table,   // the price
                &holdings_table, // the currency, which lives on holdings only
                &holdings_table, // the link, on both files
                &prices_table,
                &holdings_table, // the delete, on both files
                &prices_table,
            ]
        );
        assert_eq!(
            ops[2],
            &ChangeOp::Cell {
                column: "01".to_string(),
                value: Value::from(10.0),
            }
        );
        assert_eq!(
            ops[3],
            &ChangeOp::Cell {
                column: "01".to_string(),
                value: Value::from(25.5),
            }
        );
        assert_eq!(
            ops[4],
            &ChangeOp::Cell {
                column: "currency".to_string(),
                value: Value::from("USD"),
            }
        );
        assert_eq!(ops[7], &ChangeOp::Delete);
        assert_eq!(ops[8], &ChangeOp::Delete);
    }

    /// An income edit records one cell, keyed by the category name, because
    /// the cashflow table has no row ID column. The `Income` row that
    /// `recompute` derives from it records nothing: every device recomputes
    /// it from the same cells.
    #[test]
    #[serial_test::serial]
    fn an_income_edit_records_one_cell_and_nothing_derived() {
        let _temp = with_temp_data_home();

        let mut cashflow = Cashflow::new(2026).expect("load cashflow");
        cashflow
            .set_income(3, "Salary", 1_000.0)
            .expect("set the salary");
        cashflow
            .set_income(3, "Other", 200.0)
            .expect("set the other income");
        assert_eq!(
            cashflow.get_value("Income", "03").expect("read income"),
            1_200.0,
            "the derived row is still computed, it is just not recorded"
        );

        let entries = log_entries();
        assert_eq!(entries.len(), 2);
        for entry in &entries {
            assert_eq!(entry.table, ChangeTable::CashflowIncome { year: 2026 });
        }
        assert_eq!(entries[0].row_id, "Salary");
        assert_eq!(
            entries[0].op,
            ChangeOp::Cell {
                column: "03".to_string(),
                value: Value::from(1_000.0),
            }
        );
        assert_eq!(entries[1].row_id, "Other");
        assert!(
            entries.iter().all(|entry| entry.row_id != "Income"),
            "the derived row must record nothing"
        );
    }

    /// A change the log cannot record still succeeds and is still saved.
    ///
    /// This is the rule the whole hook design rests on. Returning the error
    /// would tell the user their save failed when it did not, and the obvious
    /// answer, pressing save again, would write the row twice. The cost is
    /// that the log can fall behind the data, which the warning on stderr
    /// reports and part 2 has to allow for.
    ///
    /// The log is made unopenable by putting a folder where its file goes,
    /// which is the closest a test can get to a broken sync folder without
    /// root.
    #[test]
    #[serial_test::serial]
    fn a_change_the_log_cannot_record_is_still_saved() {
        let _temp = with_temp_data_home();
        let log_path = sync::changelog_path().expect("changelog path");
        std::fs::create_dir(&log_path).expect("put a folder where the log file goes");
        assert!(
            sync::read_log().is_err(),
            "the log has to be unusable, or this test proves nothing"
        );

        let mut de = DetailedExpenses::new(2026, 3).expect("load march expenses");
        let row_id = de
            .add_row("Rent", 4, 1_000.0, Some("Housing"), "EUR", Some("Rent"))
            .expect("the row is saved even though it cannot be recorded");

        let reloaded = DetailedExpenses::new(2026, 3).expect("reload march expenses");
        assert_eq!(reloaded.expense_df.height(), 1, "the row reached the file");
        assert_eq!(
            str_col_to_vec(&reloaded.expense_df, ROW_ID_COLUMN).expect("read the row IDs"),
            vec![row_id.clone()]
        );
        // Editing and deleting stay usable too, so the app keeps working
        // with a broken sync folder rather than failing every save.
        de.edit_row(&row_id, Some("Rent March"), None, None, None, None, None)
            .expect("an edit still succeeds");
        de.delete_row(&row_id).expect("a delete still succeeds");
        assert_eq!(
            DetailedExpenses::new(2026, 3)
                .expect("reload after the delete")
                .expense_df
                .height(),
            0
        );
    }
}
