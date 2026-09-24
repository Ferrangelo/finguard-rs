//! Startup migration that gives every row of the synced Parquet tables a
//! stable [`ROW_ID_COLUMN`] value.
//!
//! [`migrate_row_ids`] runs before the backend serves requests. It reads
//! every listed file in each year folder under [`get_dbs_root`] and plans all
//! changes in memory first. A file that cannot be read or planned is skipped:
//! it gets a warning on stderr naming it, is counted in
//! [`RowIdMigrationReport::skipped_paths`], and is never written, so its
//! loader keeps failing on it the way it did before. When no file needs IDs
//! the run writes nothing, not even a backup. Otherwise it copies the whole
//! `dbs` tree to `<data_home>/finguard/backups/<UTC timestamp>-before-row-ids/`
//! and flushes that copy to disk, and only then rewrites each file that needs
//! IDs, through a temporary file in the same folder and a rename
//! ([`crate::df_operations::write_parquet_atomic`]). Only the
//! `row_id` column changes: every other column, its type, and the row order
//! stay as they were. A second run finds nothing to do.
//!
//! The migrated files are the monthly detailed expenses,
//! `recurring_expenses.parquet`, `investments.parquet`,
//! `investments_prices.parquet`, `liquidity.parquet`, and
//! `credits_debts.parquet`. The loaders in [`crate::df_operations`] refuse
//! a file that still lacks IDs instead of assigning unsaved ones.
//!
//! Symbolic links are followed everywhere: a linked year folder is migrated,
//! and the backup copies a linked file or folder as its contents. A folder
//! link that leads back into a folder the backup is already copying is
//! skipped with a warning, so a link loop cannot make the copy endless.
//!
//! Warnings and the report name files and folders and give counts. They
//! never contain row contents.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use polars::prelude::*;

use crate::dbs_backup;
use crate::df_operations::{
    ROW_ID_COLUMN, has_column, needs_row_ids, new_row_id, read_parquet, write_parquet_atomic,
};
use crate::error::{Error, Result};
use crate::paths::{
    CREDITS_DEBTS_FILENAME, INVESTMENTS_FILENAME, INVESTMENTS_PRICES_FILENAME, LIQUIDITY_FILENAME,
    PARQUET_SUFFIX, RECURRING_EXPENSES_FILENAME, get_dbs_root,
};

/// Table names used as keys of [`RowIdMigrationReport::files_migrated`].
const DETAILED_EXPENSES_TABLE: &str = "detailed_expenses";
const RECURRING_EXPENSES_TABLE: &str = "recurring_expenses";
const INVESTMENTS_TABLE: &str = "investments";
const INVESTMENTS_PRICES_TABLE: &str = "investments_prices";
const LIQUIDITY_TABLE: &str = "liquidity";
const CREDITS_DEBTS_TABLE: &str = "credits_debts";

/// Suffix of the backup folder name, after the UTC timestamp.
const BACKUP_SUFFIX: &str = "before-row-ids";

/// Counts from one [`migrate_row_ids`] run. It holds no row contents, so it
/// is safe to log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RowIdMigrationReport {
    /// Table name -> number of files of that table that gained row IDs.
    /// Monthly files count under `detailed_expenses`.
    pub files_migrated: BTreeMap<&'static str, usize>,
    /// Number of rows that received a new row ID, across every file.
    pub rows_given_ids: usize,
    /// Copy of the `dbs` tree taken before the first change, or `None` when
    /// nothing needed migrating and nothing was written.
    pub backup_dir: Option<PathBuf>,
    /// Files and year folders that could not be read or planned. Each got a
    /// warning on stderr and was left unchanged.
    pub skipped_paths: Vec<PathBuf>,
}

impl fmt::Display for RowIdMigrationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.backup_dir {
            None => write!(
                f,
                "Row ID migration: every readable file already has row IDs."
            )?,
            Some(backup_dir) => {
                let files: usize = self.files_migrated.values().sum();
                let per_table: Vec<String> = self
                    .files_migrated
                    .iter()
                    .map(|(table, count)| format!("{table}: {count}"))
                    .collect();
                write!(
                    f,
                    "Row ID migration: gave IDs to {} rows in {files} files ({}). Backup: {}",
                    self.rows_given_ids,
                    per_table.join(", "),
                    backup_dir.display()
                )?;
            }
        }
        if !self.skipped_paths.is_empty() {
            write!(
                f,
                " Skipped {} unreadable paths, named in the warnings above.",
                self.skipped_paths.len()
            )?;
        }
        Ok(())
    }
}

/// A file rewrite planned by [`migrate_row_ids`], applied only after the
/// backup exists.
struct PlannedWrite {
    table: &'static str,
    path: PathBuf,
    df: DataFrame,
    rows_given_ids: usize,
}

/// What the planning pass of one [`migrate_row_ids`] run decided.
#[derive(Default)]
struct Plan {
    writes: Vec<PlannedWrite>,
    skipped: Vec<PathBuf>,
}

impl Plan {
    /// Record that `path` could not be read or planned, and warn on stderr.
    ///
    /// The warning holds the path and the error text. Neither can contain a
    /// row value: the errors here come from reading the file's structure or
    /// from [`text_column`], which reports a column's type instead of
    /// casting its values.
    fn skip(&mut self, path: &Path, err: impl Into<Error>) {
        eprintln!(
            "Row ID migration: skipped {}, which could not be read ({}). It was left unchanged.",
            path.display(),
            err.into()
        );
        self.skipped.push(path.to_path_buf());
    }
}

/// Give every row of the synced tables a row ID, backing up the `dbs` tree
/// first. See the module docs for the full contract. Needs no network
/// listener, so any program that serves this data can call it at startup.
///
/// A file that cannot be read is skipped, not an error: see
/// [`RowIdMigrationReport::skipped_paths`].
///
/// # Errors
///
/// Returns [`Error::RowIdMigration`] naming the path when the `dbs` folder
/// cannot be listed, the backup cannot be written or flushed, or a migrated
/// file cannot be written. The files written before the failure keep their
/// new IDs and the backup keeps the originals; the next run migrates the
/// rest. The caller must not serve requests after an error.
pub fn migrate_row_ids() -> Result<RowIdMigrationReport> {
    let dbs_root = get_dbs_root()?;

    let mut plan = Plan::default();
    for year_dir in year_dirs(&dbs_root, &mut plan)? {
        plan_year(&year_dir, &mut plan);
    }

    let mut report = RowIdMigrationReport {
        skipped_paths: plan.skipped,
        ..RowIdMigrationReport::default()
    };
    if plan.writes.is_empty() {
        return Ok(report);
    }

    report.backup_dir = Some(
        dbs_backup::backup_dbs(&dbs_root, BACKUP_SUFFIX, "Row ID migration")
            .map_err(|failure| fail_at(&failure.path, failure.source))?,
    );
    for write in plan.writes {
        write_parquet_atomic(&write.df, &write.path).map_err(|e| fail_at(&write.path, e))?;
        *report.files_migrated.entry(write.table).or_default() += 1;
        report.rows_given_ids += write.rows_given_ids;
    }
    Ok(report)
}

/// Wrap `err` as a migration failure on `path`.
fn fail_at(path: &Path, err: impl Into<Error>) -> Error {
    Error::RowIdMigration {
        path: path.to_path_buf(),
        source: Box::new(err.into()),
    }
}

/// Return the folders directly under `dbs_root` whose name parses as a
/// year, sorted by name, following symbolic links. Other folders, such as a
/// nested `dbs/` copy, are not migrated. An entry whose type cannot be read
/// (for example, a broken link) is skipped through `plan`.
fn year_dirs(dbs_root: &Path, plan: &mut Plan) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(dbs_root).map_err(|e| fail_at(dbs_root, e))? {
        let entry = entry.map_err(|e| fail_at(dbs_root, e))?;
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.parse::<i32>().is_ok())
        {
            continue;
        }
        let path = entry.path();
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => dirs.push(path),
            Ok(_) => {}
            Err(err) => plan.skip(&path, err),
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Plan every listed file in `year_dir`.
fn plan_year(year_dir: &Path, plan: &mut Plan) {
    let mut single_tables: Vec<(&'static str, PathBuf)> = (1..=12u32)
        .map(|month| {
            (
                DETAILED_EXPENSES_TABLE,
                year_dir.join(format!("{month:02}{PARQUET_SUFFIX}")),
            )
        })
        .collect();
    single_tables.push((
        RECURRING_EXPENSES_TABLE,
        year_dir.join(RECURRING_EXPENSES_FILENAME),
    ));
    single_tables.push((LIQUIDITY_TABLE, year_dir.join(LIQUIDITY_FILENAME)));
    single_tables.push((CREDITS_DEBTS_TABLE, year_dir.join(CREDITS_DEBTS_FILENAME)));

    for (table, path) in single_tables {
        if let Some(df) = read_or_skip(&path, plan) {
            match fill_if_needed(df, &HashMap::new()) {
                Ok(Some((df, rows_given_ids))) => plan.writes.push(PlannedWrite {
                    table,
                    path,
                    df,
                    rows_given_ids,
                }),
                Ok(None) => {}
                Err(err) => plan.skip(&path, err),
            }
        }
    }

    plan_investments(year_dir, plan);
}

/// Plan both investment files of `year_dir` so each asset ends up with one
/// ID shared by its holdings row and its prices row.
///
/// A holdings row missing an ID reuses the prices row's ID for the same
/// `asset_name` when there is one. A prices row missing an ID takes the
/// holdings row's ID for its `asset_name`. A row whose asset has no
/// counterpart in the other file gets a fresh ID. Because the reuse works in
/// both directions, a file skipped in this run still ends up sharing IDs
/// with the other one when a later run migrates it.
fn plan_investments(year_dir: &Path, plan: &mut Plan) {
    let holdings_path = year_dir.join(INVESTMENTS_FILENAME);
    let prices_path = year_dir.join(INVESTMENTS_PRICES_FILENAME);
    let holdings = read_or_skip(&holdings_path, plan);
    let prices = read_or_skip(&prices_path, plan);

    // An error here means the prices file cannot be planned either, and it
    // is recorded once, when the prices file itself is planned below.
    let prices_ids = prices
        .as_ref()
        .and_then(|df| ids_by_asset(df).ok())
        .unwrap_or_default();
    let holdings_ids = match holdings {
        Some(df) => plan_investment_file(INVESTMENTS_TABLE, holdings_path, df, &prices_ids, plan),
        None => HashMap::new(),
    };
    if let Some(df) = prices {
        plan_investment_file(
            INVESTMENTS_PRICES_TABLE,
            prices_path,
            df,
            &holdings_ids,
            plan,
        );
    }
}

/// Plan one investment file, reusing `other_ids` (the other file's IDs by
/// asset name) for rows missing an ID. Returns this file's IDs by asset name
/// as they will be after the run, or an empty map when the file is skipped.
fn plan_investment_file(
    table: &'static str,
    path: PathBuf,
    df: DataFrame,
    other_ids: &HashMap<String, String>,
    plan: &mut Plan,
) -> HashMap<String, String> {
    let planned = fill_if_needed(df.clone(), other_ids).and_then(|filled| {
        let ids = ids_by_asset(filled.as_ref().map_or(&df, |(filled_df, _)| filled_df))?;
        Ok((filled, ids))
    });
    match planned {
        Ok((filled, ids)) => {
            if let Some((df, rows_given_ids)) = filled {
                plan.writes.push(PlannedWrite {
                    table,
                    path,
                    df,
                    rows_given_ids,
                });
            }
            ids
        }
        Err(err) => {
            plan.skip(&path, err);
            HashMap::new()
        }
    }
}

/// Read `path`. Returns `None` when it does not exist, and also when it
/// cannot be read, after recording the skip in `plan`.
fn read_or_skip(path: &Path, plan: &mut Plan) -> Option<DataFrame> {
    if !path.exists() {
        return None;
    }
    match read_parquet(path) {
        Ok(df) => Some(df),
        Err(err) => {
            plan.skip(path, err);
            None
        }
    }
}

/// Return `df` with its missing row IDs filled and the number filled, or
/// `None` when no ID is missing. A row missing an ID reuses
/// `ids_for_asset[asset_name]` when present (see [`fill_row_ids`]).
fn fill_if_needed(
    df: DataFrame,
    ids_for_asset: &HashMap<String, String>,
) -> Result<Option<(DataFrame, usize)>> {
    if !needs_row_ids(&df)? {
        return Ok(None);
    }
    let names = asset_names(&df)?;
    let filled = fill_row_ids(df, |row| {
        names
            .get(row)
            .and_then(Option::as_ref)
            .and_then(|name| ids_for_asset.get(name).cloned())
    })?;
    Ok(Some(filled))
}

/// Return the values of the text column `name`, or `None` for each row
/// when `df` has no such column or it holds only nulls.
///
/// Any other column type is an error that names the type. The values are
/// never cast, because a failed cast reports sample values, and this error
/// can end up in a warning.
fn text_column(df: &DataFrame, name: &str) -> Result<Vec<Option<String>>> {
    if !has_column(df, name) {
        return Ok(vec![None; df.height()]);
    }
    let column = df.column(name)?;
    match column.dtype() {
        DataType::String => Ok(column
            .str()?
            .iter()
            .map(|value| value.map(str::to_string))
            .collect()),
        DataType::Null => Ok(vec![None; df.height()]),
        other => Err(Error::InvalidArgument(format!(
            "the '{name}' column has type {other}, not text"
        ))),
    }
}

/// Return each row's `asset_name` (see [`text_column`]).
fn asset_names(df: &DataFrame) -> Result<Vec<Option<String>>> {
    text_column(df, "asset_name")
}

/// Return each row's row ID, with null and empty values as `None` (see
/// [`text_column`]).
fn existing_row_ids(df: &DataFrame) -> Result<Vec<Option<String>>> {
    Ok(text_column(df, ROW_ID_COLUMN)?
        .into_iter()
        .map(|id| id.filter(|id| !id.is_empty()))
        .collect())
}

/// Return the existing non-empty row IDs of `df`, keyed by `asset_name`. The
/// first row wins when an asset name repeats.
fn ids_by_asset(df: &DataFrame) -> Result<HashMap<String, String>> {
    let mut ids = HashMap::new();
    let names = asset_names(df)?;
    for (name, id) in names.into_iter().zip(existing_row_ids(df)?) {
        if let (Some(name), Some(id)) = (name, id) {
            ids.entry(name).or_insert(id);
        }
    }
    Ok(ids)
}

/// Return `df` with every missing row ID filled, and the number filled.
///
/// An existing ID is kept. A missing one takes `preferred(row_index)` when
/// that ID is not already used in `df`, and a fresh random ID otherwise, so
/// the IDs this assigns are unique within the file. The row ID column keeps
/// its position, or is appended as the last column when absent, and is
/// always a string column afterwards. No other column changes.
fn fill_row_ids(
    mut df: DataFrame,
    preferred: impl Fn(usize) -> Option<String>,
) -> Result<(DataFrame, usize)> {
    let existing = existing_row_ids(&df)?;
    let mut used: HashSet<String> = existing.iter().flatten().cloned().collect();
    let mut filled = 0;
    let ids: Vec<String> = existing
        .into_iter()
        .enumerate()
        .map(|(row, id)| {
            if let Some(id) = id {
                return id;
            }
            filled += 1;
            let id = preferred(row)
                .filter(|id| !used.contains(id))
                .unwrap_or_else(new_row_id);
            used.insert(id.clone());
            id
        })
        .collect();
    df.with_column(Column::new(ROW_ID_COLUMN.into(), ids))?;
    Ok((df, filled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::df_operations::{
        Cashflow, CreditsDebts, DetailedExpenses, InvestmentHoldings, Liquidity, RecurringExpenses,
    };
    use crate::paths::{CASHFLOW_FILENAME, PRIMARIES_FILENAME, get_backups_dir, get_year_dir};
    use chrono::NaiveDate;

    /// Point `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `HOME` at a fresh temp
    /// folder and force offline FX mode, so no test touches real data.
    ///
    /// # Safety
    ///
    /// `std::env::set_var` is not thread-safe; callers hold
    /// `#[serial_test::serial]`.
    fn with_temp_env_offline() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            std::env::set_var("HOME", dir.path());
            std::env::set_var("FINGUARD_FX_OFFLINE", "1");
        }
        dir
    }

    fn write(df: &DataFrame, path: &Path) {
        let mut df = df.clone();
        let file = std::fs::File::create(path).expect("create parquet file");
        ParquetWriter::new(file)
            .finish(&mut df)
            .expect("write parquet file");
    }

    fn read(path: &Path) -> DataFrame {
        read_parquet(path).expect("read parquet file")
    }

    fn date(day: u32) -> i32 {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        (NaiveDate::from_ymd_opt(2026, 3, day).unwrap() - epoch).num_days() as i32
    }

    fn month_columns(rows: usize, base: f64) -> Vec<Column> {
        (1..=12)
            .map(|m| {
                let values: Vec<f64> = (0..rows).map(|r| base + (r * 100 + m) as f64).collect();
                Column::new(format!("{m:02}").into(), values)
            })
            .collect()
    }

    fn wide(meta: Vec<Column>, rows: usize, base: f64) -> DataFrame {
        let mut cols = meta;
        cols.extend(month_columns(rows, base));
        DataFrame::new_infer_height(cols).expect("build wide frame")
    }

    /// Frames in the format written before row IDs existed, one per migrated
    /// table plus a second monthly file, including legacy quirks the
    /// migration must leave alone: nulls in a Date and a Float64 column, a
    /// monthly file storing `expense_date` as Int32, a stale FX column, a
    /// legacy `"E"` currency, an investments file shaped like the real one
    /// (no `currency`, `link` after the month columns), a prices file
    /// without `link`, a liquidity file without `currency`, and a `type`
    /// column.
    struct OldTree {
        expenses: DataFrame,
        expenses_int_dates: DataFrame,
        recurring: DataFrame,
        investments: DataFrame,
        prices: DataFrame,
        liquidity: DataFrame,
        credits_debts: DataFrame,
    }

    fn old_tree() -> OldTree {
        let expenses = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Rent", "Tea", "Bus"]),
            Int32Chunked::from_iter([Some(date(1)), None, Some(date(9))])
                .into_series()
                .cast(&DataType::Date)
                .unwrap()
                .with_name("expense_date".into())
                .into(),
            Column::new("expense_amount".into(), &[Some(900.0), Some(3.5), None]),
            Column::new("currency".into(), &["E", "GBP", "EUR"]),
            Column::new("expense_in_ref_currency".into(), &[900.0, 4.1, 2.0]),
            Column::new("primary_category".into(), &["Housing", "Out", "Transport"]),
            Column::new("secondary_category".into(), &["Rent", "Cafe", "Bus"]),
        ])
        .unwrap();
        let expenses_int_dates = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Rent", "Gym"]),
            Column::new("expense_date".into(), &[date(1), date(5)]),
            Column::new("expense_amount".into(), &[900.0, 40.0]),
            Column::new("currency".into(), &["EUR", "EUR"]),
            Column::new("primary_category".into(), &["Housing", "Leisure"]),
            Column::new("secondary_category".into(), &["Rent", "Gym"]),
        ])
        .unwrap();
        let recurring = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Rent", "Gym"]),
            Column::new("expense_day".into(), &[1_i64, 5]),
            Column::new("expense_amount".into(), &[900.0, 40.0]),
            Column::new("currency".into(), &["EUR", "E"]),
            Column::new("primary_category".into(), &["Housing", "Leisure"]),
            Column::new("secondary_category".into(), &["Rent", "Gym"]),
        ])
        .unwrap();
        let mut investments = wide(
            vec![
                Column::new("asset_name".into(), &["Fund A", "Gold", "Fund B"]),
                Column::new("category".into(), &["Stocks/ETF", "Commodities", "Bonds"]),
            ],
            3,
            0.0,
        );
        investments
            .with_column(Column::new("link".into(), &["", "", "https://example.com"]))
            .unwrap();
        // Deliberately in another order than the holdings, with "Old Fund"
        // as an orphan that has no holdings row, and without `link`.
        let prices = wide(
            vec![
                Column::new(
                    "asset_name".into(),
                    &["Fund B", "Old Fund", "Fund A", "Gold"],
                ),
                Column::new(
                    "category".into(),
                    &["Bonds", "Stocks/ETF", "Stocks/ETF", "Commodities"],
                ),
            ],
            4,
            1000.0,
        );
        let liquidity = wide(
            vec![
                Column::new("asset_name".into(), &["Bank", "Wallet"]),
                Column::new("category".into(), &["Bank/Broker account", "Cash"]),
            ],
            2,
            50.0,
        );
        let credits_debts = wide(
            vec![
                Column::new("name".into(), &["Loan"]),
                Column::new("currency".into(), &["E"]),
                Column::new("type".into(), &["debt"]),
            ],
            1,
            -500.0,
        );
        OldTree {
            expenses,
            expenses_int_dates,
            recurring,
            investments,
            prices,
            liquidity,
            credits_debts,
        }
    }

    /// Write `old_tree()` to year 2026 and return each frame with the path
    /// it was written to. The monthly file is the first entry.
    fn write_old_tree() -> Vec<(PathBuf, DataFrame)> {
        let tree = old_tree();
        let dir = get_year_dir(2026).unwrap();
        let files = vec![
            (dir.join("03_detailed_expenses.parquet"), tree.expenses),
            (
                dir.join("04_detailed_expenses.parquet"),
                tree.expenses_int_dates,
            ),
            (dir.join(RECURRING_EXPENSES_FILENAME), tree.recurring),
            (dir.join(INVESTMENTS_FILENAME), tree.investments),
            (dir.join(INVESTMENTS_PRICES_FILENAME), tree.prices),
            (dir.join(LIQUIDITY_FILENAME), tree.liquidity),
            (dir.join(CREDITS_DEBTS_FILENAME), tree.credits_debts),
        ];
        for (path, df) in &files {
            write(df, path);
        }
        files
    }

    fn row_ids(df: &DataFrame) -> Vec<String> {
        df.column(ROW_ID_COLUMN)
            .expect("row_id column")
            .str()
            .expect("row_id is a string column")
            .iter()
            .map(|id| id.expect("row_id is not null").to_string())
            .collect()
    }

    fn ids_by_name(df: &DataFrame) -> HashMap<String, String> {
        ids_by_asset(df).unwrap()
    }

    fn backup_folders() -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(get_backups_dir().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        dirs.sort();
        dirs
    }

    /// Every listed table gains unique, non-null, non-empty IDs as its last
    /// column, and nothing else in the file changes: same columns in the
    /// same order, same types, same values including nulls, same row order.
    #[test]
    #[serial_test::serial]
    fn migrates_every_table_and_changes_nothing_else() {
        let _temp = with_temp_env_offline();
        let files = write_old_tree();

        let report = migrate_row_ids().expect("migration succeeds");

        let mut all_ids = HashSet::new();
        for (path, original) in &files {
            let migrated = read(path);
            let mut expected_names: Vec<String> = original
                .get_column_names()
                .iter()
                .map(|c| c.to_string())
                .collect();
            expected_names.push(ROW_ID_COLUMN.to_string());
            let names: Vec<String> = migrated
                .get_column_names()
                .iter()
                .map(|c| c.to_string())
                .collect();
            assert_eq!(names, expected_names, "{}", path.display());
            assert_eq!(
                migrated.drop(ROW_ID_COLUMN).unwrap().schema(),
                original.schema(),
                "{} changed a column type",
                path.display()
            );
            assert!(
                migrated
                    .drop(ROW_ID_COLUMN)
                    .unwrap()
                    .equals_missing(original),
                "{} changed outside row_id",
                path.display()
            );

            let ids = row_ids(&migrated);
            assert_eq!(ids.len(), original.height());
            assert!(ids.iter().all(|id| uuid::Uuid::parse_str(id).is_ok()));
            let unique: HashSet<&String> = ids.iter().collect();
            assert_eq!(
                unique.len(),
                ids.len(),
                "{} has duplicate IDs",
                path.display()
            );
            if !path.ends_with(INVESTMENTS_PRICES_FILENAME) {
                all_ids.extend(ids);
            }
        }
        // Across files, only an investment and its prices row share an ID.
        assert_eq!(all_ids.len(), 3 + 2 + 2 + 3 + 2 + 1);

        assert_eq!(report.rows_given_ids, 3 + 2 + 2 + 3 + 4 + 2 + 1);
        assert!(report.skipped_paths.is_empty());
        assert_eq!(
            report.files_migrated,
            BTreeMap::from([
                (CREDITS_DEBTS_TABLE, 1),
                (DETAILED_EXPENSES_TABLE, 2),
                (INVESTMENTS_TABLE, 1),
                (INVESTMENTS_PRICES_TABLE, 1),
                (LIQUIDITY_TABLE, 1),
                (RECURRING_EXPENSES_TABLE, 1),
            ])
        );

        // Every loader accepts the migrated files.
        DetailedExpenses::new(2026, 3).expect("expenses load");
        DetailedExpenses::new(2026, 4).expect("expenses with Int32 dates load");
        RecurringExpenses::new(2026).expect("recurring loads");
        InvestmentHoldings::new(2026).expect("investments load");
        Liquidity::new(2026).expect("liquidity loads");
        CreditsDebts::new(2026).expect("credits/debts load");
    }

    /// Each asset's holdings row and prices row get the same ID, whatever
    /// the row order of the two files, and an orphan prices row gets its
    /// own ID.
    #[test]
    #[serial_test::serial]
    fn investments_and_prices_share_one_id_per_asset() {
        let _temp = with_temp_env_offline();
        write_old_tree();

        migrate_row_ids().expect("migration succeeds");

        let dir = get_year_dir(2026).unwrap();
        let holdings = ids_by_name(&read(&dir.join(INVESTMENTS_FILENAME)));
        let prices = ids_by_name(&read(&dir.join(INVESTMENTS_PRICES_FILENAME)));
        for asset in ["Fund A", "Gold", "Fund B"] {
            assert_eq!(holdings[asset], prices[asset], "{asset}");
        }
        let orphan = &prices["Old Fund"];
        assert!(!holdings.values().any(|id| id == orphan));
    }

    /// A holdings file that lacks IDs reuses the IDs its already migrated
    /// prices file holds, so the two still agree afterwards.
    #[test]
    #[serial_test::serial]
    fn holdings_missing_ids_reuse_the_prices_ids() {
        let _temp = with_temp_env_offline();
        let tree = old_tree();
        let dir = get_year_dir(2026).unwrap();
        let (prices, _) = fill_row_ids(tree.prices, |_| None).unwrap();
        write(&prices, &dir.join(INVESTMENTS_PRICES_FILENAME));
        write(&tree.investments, &dir.join(INVESTMENTS_FILENAME));

        let report = migrate_row_ids().expect("migration succeeds");

        assert_eq!(
            report.files_migrated,
            BTreeMap::from([(INVESTMENTS_TABLE, 1)])
        );
        let holdings = ids_by_name(&read(&dir.join(INVESTMENTS_FILENAME)));
        let prices = ids_by_name(&read(&dir.join(INVESTMENTS_PRICES_FILENAME)));
        for asset in ["Fund A", "Gold", "Fund B"] {
            assert_eq!(holdings[asset], prices[asset], "{asset}");
        }
    }

    /// The backup holds a byte-for-byte copy of the whole `dbs` tree as it
    /// was before the run, including files the migration does not touch and
    /// folders that are not years.
    #[test]
    #[serial_test::serial]
    fn backup_holds_the_original_tree() {
        let _temp = with_temp_env_offline();
        let files = write_old_tree();
        let dbs_root = get_dbs_root().unwrap();
        Cashflow::new(2026)
            .expect("load cashflow")
            .save()
            .expect("write cashflow");
        let cashflow_path = get_year_dir(2026).unwrap().join(CASHFLOW_FILENAME);
        let cashflow_before = std::fs::read(&cashflow_path).unwrap();
        let nested = dbs_root.join("dbs").join("2026");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::copy(&files[0].0, nested.join("03_detailed_expenses.parquet")).unwrap();

        let mut originals: Vec<(PathBuf, Vec<u8>)> = files
            .iter()
            .map(|(path, _)| path.clone())
            .chain([
                cashflow_path.clone(),
                nested.join("03_detailed_expenses.parquet"),
            ])
            .map(|path| {
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        originals.sort();

        let report = migrate_row_ids().expect("migration succeeds");

        let backup_dir = report.backup_dir.expect("a backup was written");
        assert_eq!(backup_folders(), vec![backup_dir.clone()]);
        let name = backup_dir.file_name().unwrap().to_str().unwrap();
        assert!(name.ends_with("-before-row-ids"), "{name}");
        for (path, bytes) in &originals {
            let relative = path.strip_prefix(&dbs_root).unwrap();
            assert_eq!(
                &std::fs::read(backup_dir.join(relative)).unwrap(),
                bytes,
                "backup of {}",
                relative.display()
            );
        }
        // Files outside the listed tables and outside year folders stay
        // exactly as they were.
        assert_eq!(std::fs::read(&cashflow_path).unwrap(), cashflow_before);
        let nested_file = nested.join("03_detailed_expenses.parquet");
        assert!(!has_column(&read(&nested_file), ROW_ID_COLUMN));
    }

    /// A second run finds every ID in place: it writes no file and no second
    /// backup, and reports nothing to do.
    #[test]
    #[serial_test::serial]
    fn second_run_writes_nothing() {
        let _temp = with_temp_env_offline();
        let files = write_old_tree();
        migrate_row_ids().expect("first run succeeds");
        let after_first: Vec<Vec<u8>> = files
            .iter()
            .map(|(path, _)| std::fs::read(path).unwrap())
            .collect();
        let backups_after_first = backup_folders();

        let report = migrate_row_ids().expect("second run succeeds");

        assert_eq!(report, RowIdMigrationReport::default());
        assert_eq!(backup_folders(), backups_after_first);
        for ((path, _), bytes) in files.iter().zip(after_first) {
            assert_eq!(std::fs::read(path).unwrap(), bytes, "{}", path.display());
        }
    }

    /// A tree written entirely by the current code, which always sets row
    /// IDs, needs no migration: no backup, no write.
    #[test]
    #[serial_test::serial]
    fn up_to_date_tree_makes_no_backup() {
        let _temp = with_temp_env_offline();
        let mut de = DetailedExpenses::new(2026, 3).unwrap();
        de.add_row("Rent", 1, 900.0, Some("Housing"), "EUR", Some("Rent"))
            .unwrap();
        RecurringExpenses::new(2026)
            .unwrap()
            .add("Gym", 40.0, "EUR", "Leisure", "Gym")
            .unwrap();
        InvestmentHoldings::new(2026)
            .unwrap()
            .add_asset("Fund A", "Stocks/ETF", "", "EUR")
            .unwrap();
        Liquidity::new(2026)
            .unwrap()
            .add_asset("Bank", "Cash", "EUR")
            .unwrap();
        CreditsDebts::new(2026)
            .unwrap()
            .add_entry("Loan", "EUR")
            .unwrap();
        Cashflow::new(2026).unwrap().save().unwrap();

        let report = migrate_row_ids().expect("migration succeeds");

        assert_eq!(report, RowIdMigrationReport::default());
        assert!(backup_folders().is_empty());
    }

    /// A file that already has a `row_id` column keeps every existing ID and
    /// only fills the null and empty ones.
    #[test]
    #[serial_test::serial]
    fn fills_only_null_and_empty_ids() {
        let _temp = with_temp_env_offline();
        let path = get_year_dir(2026).unwrap().join(LIQUIDITY_FILENAME);
        let df = wide(
            vec![
                Column::new("asset_name".into(), &["Bank", "Wallet", "Box"]),
                Column::new("category".into(), &["Cash", "Cash", "Cash"]),
                Column::new("currency".into(), &["EUR", "EUR", "EUR"]),
                Column::new(ROW_ID_COLUMN.into(), &[Some("kept-id"), None, Some("")]),
            ],
            3,
            0.0,
        );
        write(&df, &path);

        let report = migrate_row_ids().expect("migration succeeds");

        assert_eq!(report.rows_given_ids, 2);
        let ids = row_ids(&read(&path));
        assert_eq!(ids[0], "kept-id");
        assert!(ids[1..].iter().all(|id| uuid::Uuid::parse_str(id).is_ok()));
        assert_ne!(ids[1], ids[2]);
    }

    /// An empty table saved before row IDs existed still gains the column,
    /// since a loader rejects a file without it.
    #[test]
    #[serial_test::serial]
    fn empty_file_without_the_column_gains_it() {
        let _temp = with_temp_env_offline();
        let path = get_year_dir(2026).unwrap().join(CREDITS_DEBTS_FILENAME);
        let df = wide(
            vec![
                Column::new("name".into(), Vec::<String>::new()),
                Column::new("currency".into(), Vec::<String>::new()),
            ],
            0,
            0.0,
        );
        write(&df, &path);

        let report = migrate_row_ids().expect("migration succeeds");

        assert_eq!(report.rows_given_ids, 0);
        assert_eq!(report.files_migrated[CREDITS_DEBTS_TABLE], 1);
        CreditsDebts::new(2026).expect("the migrated empty file loads");
    }

    /// A file that cannot be read is skipped and reported, not fatal: every
    /// other file still migrates, the backup still holds the unreadable
    /// file, and the file itself stays byte-for-byte as it was, in this run
    /// and the next.
    #[test]
    #[serial_test::serial]
    fn unreadable_file_is_skipped_and_the_rest_migrates() {
        let _temp = with_temp_env_offline();
        let files = write_old_tree();
        let broken = get_year_dir(2026)
            .unwrap()
            .join("07_detailed_expenses.parquet");
        std::fs::write(&broken, b"not a parquet file").unwrap();

        let report = migrate_row_ids().expect("an unreadable file does not fail the run");

        assert_eq!(report.skipped_paths, vec![broken.clone()]);
        assert_eq!(report.files_migrated.values().sum::<usize>(), files.len());
        for (path, _) in &files {
            assert!(!needs_row_ids(&read(path)).unwrap(), "{}", path.display());
        }
        assert_eq!(std::fs::read(&broken).unwrap(), b"not a parquet file");
        let backup_dir = report
            .backup_dir
            .as_ref()
            .expect("the other files needed a backup");
        assert_eq!(
            std::fs::read(backup_dir.join("2026").join("07_detailed_expenses.parquet")).unwrap(),
            b"not a parquet file"
        );
        assert!(report.to_string().contains("Skipped 1 unreadable paths"));

        let again = migrate_row_ids().expect("second run succeeds");
        assert_eq!(again.skipped_paths, vec![broken.clone()]);
        assert_eq!(again.backup_dir, None);
        assert_eq!(backup_folders().len(), 1);
        assert_eq!(std::fs::read(&broken).unwrap(), b"not a parquet file");
    }

    /// A file that cannot be written still aborts the run with an error
    /// naming it. The backup was already complete, and no data file changed.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn failed_write_aborts_and_names_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let _temp = with_temp_env_offline();
        let files = write_old_tree();
        let year_dir = get_year_dir(2026).unwrap();
        std::fs::set_permissions(&year_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let probe = year_dir.join(".write-probe");
        if std::fs::File::create(&probe).is_ok() {
            // Running as root: a read-only folder cannot simulate the failure.
            std::fs::remove_file(&probe).unwrap();
            std::fs::set_permissions(&year_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipped: the folder stayed writable, probably running as root");
            return;
        }

        let result = migrate_row_ids();
        std::fs::set_permissions(&year_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.expect_err("a failed write must abort the run");
        let Error::RowIdMigration { path, .. } = &err else {
            panic!("expected a migration error, got {err}");
        };
        assert_eq!(path, &files[0].0, "the first planned write is named");
        assert!(err.to_string().contains("03_detailed_expenses.parquet"));
        let backups = backup_folders();
        assert_eq!(backups.len(), 1);
        assert!(backups[0].join("2026").join(LIQUIDITY_FILENAME).exists());
        for (path, _) in &files {
            assert!(
                !has_column(&read(path), ROW_ID_COLUMN),
                "{}",
                path.display()
            );
        }
    }

    /// The migration keeps the shapes found in real files: a monthly file
    /// storing `expense_date` as Int32 keeps it Int32, and an investments
    /// file with no `currency` and with `link` after the month columns gains
    /// neither `currency` nor a moved `link`. Only `row_id` is appended.
    #[test]
    #[serial_test::serial]
    fn real_file_shapes_only_gain_row_id() {
        let _temp = with_temp_env_offline();
        write_old_tree();
        let dir = get_year_dir(2026).unwrap();

        migrate_row_ids().expect("migration succeeds");

        let expenses = read(&dir.join("04_detailed_expenses.parquet"));
        assert_eq!(
            expenses.column("expense_date").unwrap().dtype(),
            &DataType::Int32
        );

        let investments = read(&dir.join(INVESTMENTS_FILENAME));
        let mut expected: Vec<String> = vec!["asset_name".into(), "category".into()];
        expected.extend((1..=12).map(|m| format!("{m:02}")));
        expected.push("link".into());
        expected.push(ROW_ID_COLUMN.into());
        let names: Vec<String> = investments
            .get_column_names()
            .iter()
            .map(|c| c.to_string())
            .collect();
        assert_eq!(names, expected);
        assert!(!has_column(&investments, "currency"));
    }

    /// A year folder reached through a symbolic link is migrated like any
    /// other, and the backup holds its files as regular copies.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn linked_year_folder_is_migrated_and_backed_up() {
        let temp = with_temp_env_offline();
        let outside = temp.path().join("elsewhere").join("2027");
        std::fs::create_dir_all(&outside).unwrap();
        let liquidity = outside.join(LIQUIDITY_FILENAME);
        write(&old_tree().liquidity, &liquidity);
        let original = std::fs::read(&liquidity).unwrap();
        std::os::unix::fs::symlink(&outside, get_dbs_root().unwrap().join("2027")).unwrap();

        let report = migrate_row_ids().expect("migration succeeds");

        assert_eq!(report.files_migrated[LIQUIDITY_TABLE], 1);
        assert!(!needs_row_ids(&read(&liquidity)).unwrap());
        let copy = report
            .backup_dir
            .expect("a backup was written")
            .join("2027")
            .join(LIQUIDITY_FILENAME);
        assert!(!std::fs::symlink_metadata(&copy).unwrap().is_symlink());
        assert_eq!(std::fs::read(&copy).unwrap(), original);
    }

    /// A link loop inside the data folder, and a broken link, cannot stall
    /// or fail the backup: the loop and the broken link are skipped and
    /// everything else is copied.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn backup_survives_a_link_loop_and_a_broken_link() {
        let temp = with_temp_env_offline();
        write_old_tree();
        let dbs_root = get_dbs_root().unwrap();
        let year_dir = get_year_dir(2026).unwrap();
        std::os::unix::fs::symlink(&dbs_root, year_dir.join("loop")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("missing"), year_dir.join("broken")).unwrap();
        std::os::unix::fs::symlink(get_backups_dir().unwrap(), dbs_root.join("backups-link"))
            .unwrap();

        let report = migrate_row_ids().expect("migration succeeds");

        let backup_dir = report.backup_dir.expect("a backup was written");
        assert!(backup_dir.join("2026").join(LIQUIDITY_FILENAME).exists());
        assert!(!backup_dir.join("2026").join("loop").exists());
        assert!(!backup_dir.join("2026").join("broken").exists());
        // The link to `backups/` is copied, but not the backup being written.
        let copied_backups = backup_dir.join("backups-link");
        assert!(copied_backups.is_dir());
        assert!(
            !copied_backups
                .join(backup_dir.file_name().unwrap())
                .exists()
        );
    }

    /// `primaries.parquet` and the other untracked files are never touched,
    /// even in a year that is otherwise migrated.
    #[test]
    #[serial_test::serial]
    fn untracked_files_are_left_alone() {
        let _temp = with_temp_env_offline();
        write_old_tree();
        let primaries = get_year_dir(2026).unwrap().join(PRIMARIES_FILENAME);
        let df =
            DataFrame::new_infer_height(vec![Column::new("primary_category".into(), &["Housing"])])
                .unwrap();
        write(&df, &primaries);
        let before = std::fs::read(&primaries).unwrap();

        migrate_row_ids().expect("migration succeeds");

        assert_eq!(std::fs::read(&primaries).unwrap(), before);
    }
}
