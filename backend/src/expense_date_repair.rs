//! Repair tool for `expense_date` values that do not belong to their own
//! monthly file.
//!
//! [`DetailedExpenses::add_row`](crate::df_operations::DetailedExpenses::add_row)
//! and [`edit_row`](crate::df_operations::DetailedExpenses::edit_row) both
//! build `expense_date` from the table's own `year` and `month`, so the
//! current write path can never produce a row whose date falls outside its
//! own file's month. A row that does was not written by that path, and its
//! date is wrong. [`repair_expense_dates`] finds and, only when asked to,
//! fixes those rows.
//!
//! # The three classes, and which two are defects
//!
//! Every row of every monthly file is classified against its own file's
//! `(year, month)`, taken from the file name, never against today's date:
//!
//! - **Class A**, `expense_date` is null. A real expense that today's
//!   readers already skip, so it is silently uncounted.
//! - **Class B**, `expense_date` is non-null but its `(year, month)` differs
//!   from the file's own. This includes the Unix epoch day, which is what a
//!   stored zero or a bad cast reads back as.
//! - **Class C**, `expense_date` is later than today but still inside the
//!   file's own month. **Not a defect.** A row in `2026/10_…` dated later in
//!   October 2026 is exactly what that file is for; the user entered it on
//!   purpose. This tool never selects it, because it never compares against
//!   today at all: the classification above only looks at the file's own
//!   month, so a class C row simply never matches class A or B.
//!
//! # The day cannot be recovered
//!
//! The year and month of a class A or class B row are recoverable from the
//! file name. The day of month is not recoverable from any column, so a
//! repair needs a policy for it, given explicitly on every `--apply` run as
//! a [`DayPolicy`]. There is no default: a run cannot invent a day by
//! accident. See the binary at `src/bin/repair_expense_dates.rs` for the
//! flag.
//!
//! # Safety
//!
//! Running with `apply = false` (the binary's default, with no `--apply`
//! flag) only classifies and reports; it writes no file and takes no
//! backup, and doubles as the audit the caller reads before ever passing
//! `apply = true`. An actual write additionally takes the whole `dbs` tree
//! through [`crate::dbs_backup::backup_dbs`] before the first file changes,
//! exactly as [`crate::row_id_migration::migrate_row_ids`] does; if the
//! backup fails, nothing is written. A file with no class A or class B row
//! is never opened for writing.
//!
//! # Sync change log
//!
//! [`crate::sync_baseline::baseline_change_log`] only re-records a folder
//! when its marker is missing, unreadable, or names a file that was
//! previously unreadable (see that module's docs); an ordinary content
//! change to an already-baselined file is invisible to it. A repaired row
//! therefore needs its own log entry, or a paired device could later merge
//! the old, wrong date back over it. Each repaired row gets one
//! [`crate::sync::ChangeOp::Upsert`] entry, appended right after the file
//! that holds it is saved, unconditionally, the same as an ordinary
//! [`edit_row`](crate::df_operations::DetailedExpenses::edit_row) call
//! makes regardless of whether this device has ever paired: recording is
//! cheap when there is no peer yet, and is exactly what protects a device
//! that pairs later. A failed append never rolls back the save or retries;
//! it warns and marks the log incomplete, matching
//! [`crate::df_operations`]'s own change log hooks.
//!
//! # Output discipline
//!
//! [`ExpenseDateRepairReport`] and every warning this module prints hold
//! only counts, file paths, and schema-level facts. No row content, amount,
//! description, category, date, or row ID reaches a report or stderr.

use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{Datelike, NaiveDate};
use polars::prelude::*;

use crate::dbs_backup;
use crate::df_operations::{DetailedExpenses, ROW_ID_COLUMN, row_json, write_parquet_atomic};
use crate::error::{Error, Result};
use crate::paths::{PARQUET_SUFFIX, get_dbs_root};
use crate::sync::{self, ChangeOp, ChangeTable, IncompleteReason};

/// Suffix of the backup folder name, after the UTC timestamp. See
/// [`dbs_backup::backup_dbs`].
const BACKUP_SUFFIX: &str = "before-expense-date-repair";

/// The day assigned to a repaired row's month. Required and explicit on
/// every `--apply` run, since the day is not recoverable from any column
/// (see the module docs) and no run may invent one silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayPolicy {
    /// The first day of the file's own month.
    FirstOfMonth,
}

impl DayPolicy {
    /// Parse a `--day` flag value, or `None` for one this version does not
    /// know.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "first-of-month" => Some(Self::FirstOfMonth),
            _ => None,
        }
    }

    /// The day of month this policy assigns. Always valid for any
    /// `(year, month)`: every month has a first day.
    fn day(self) -> u32 {
        match self {
            Self::FirstOfMonth => 1,
        }
    }
}

/// Total row count and class A/B counts for one file that has at least one
/// row needing repair. A file with neither class never appears here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRepairCounts {
    /// The monthly parquet file this count describes.
    pub path: PathBuf,
    /// Total rows in the file, repaired or not.
    pub total_rows: usize,
    /// Rows with a null `expense_date` (class A).
    pub class_a: usize,
    /// Rows with a non-null `expense_date` outside the file's own month
    /// (class B).
    pub class_b: usize,
}

/// Counts from one [`repair_expense_dates`] run. Holds no row content, so it
/// is safe to print; see the module docs' "Output discipline".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExpenseDateRepairReport {
    /// One entry per file that has a class A or class B row, in the order
    /// the scan visited them (year, then month).
    pub files: Vec<FileRepairCounts>,
    /// Rows whose `expense_date` was actually rewritten. Always 0 on a dry
    /// run.
    pub rows_repaired: usize,
    /// Copy of the `dbs` tree taken before the first write, or `None` on a
    /// dry run, or when nothing needed repair.
    pub backup_dir: Option<PathBuf>,
    /// Files that could not be read or classified. Each got a warning on
    /// stderr naming it and was left unchanged.
    pub skipped_paths: Vec<PathBuf>,
    /// Whether this run was `--apply` (`true`) or a dry run (`false`).
    pub applied: bool,
}

impl fmt::Display for ExpenseDateRepairReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.files.is_empty() {
            writeln!(
                f,
                "Expense date repair: no file has a class A or class B row."
            )?;
        } else {
            let class_a: usize = self.files.iter().map(|file| file.class_a).sum();
            let class_b: usize = self.files.iter().map(|file| file.class_b).sum();
            writeln!(
                f,
                "Expense date repair: {} file{} affected, {class_a} null-date row{} (class A), \
                 {class_b} out-of-month row{} (class B).",
                self.files.len(),
                plural(self.files.len()),
                plural(class_a),
                plural(class_b),
            )?;
            for file in &self.files {
                writeln!(
                    f,
                    "  {}: {} row{} total, {} class A, {} class B",
                    file.path.display(),
                    file.total_rows,
                    plural(file.total_rows),
                    file.class_a,
                    file.class_b
                )?;
            }
        }
        match (self.applied, &self.backup_dir) {
            (false, _) => write!(f, "Dry run: nothing was written.")?,
            (true, None) => write!(f, "Nothing needed writing, so no backup was taken.")?,
            (true, Some(backup_dir)) => write!(
                f,
                "Repaired {} row{} across {} file{}. Backup: {}",
                self.rows_repaired,
                plural(self.rows_repaired),
                self.files.len(),
                plural(self.files.len()),
                backup_dir.display()
            )?,
        }
        if !self.skipped_paths.is_empty() {
            write!(
                f,
                " Skipped {} unreadable path{}, named in the warnings above.",
                self.skipped_paths.len(),
                plural(self.skipped_paths.len())
            )?;
        }
        Ok(())
    }
}

/// The plural ending for `count`, so a report reads as English.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Detect, and when `apply` is set and `day` is given, repair rows whose
/// `expense_date` does not belong to their own monthly file. See the module
/// docs for the full contract.
///
/// A dry run (`apply = false`) only classifies and reports: it writes no
/// file, takes no backup, and ignores `day`, so the file-list audit it
/// doubles as never needs a day policy decided first.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] when `apply` is set without a `day`
/// policy, so no run can invent a day by accident. Returns the underlying
/// I/O or Polars error when the `dbs` folder cannot be listed, the backup
/// cannot be written or flushed, or a repaired file cannot be written. Files
/// written before a mid-run failure keep their repaired dates and the
/// backup keeps the originals; a second dry run reports only what is still
/// wrong, and a second `--apply` run finishes the rest.
pub fn repair_expense_dates(
    apply: bool,
    day: Option<DayPolicy>,
) -> Result<ExpenseDateRepairReport> {
    if apply && day.is_none() {
        return Err(Error::InvalidArgument(
            "--apply requires an explicit --day policy, for example --day=first-of-month. \
             Refusing to run so no day is invented by accident."
                .to_string(),
        ));
    }

    let dbs_root = get_dbs_root()?;
    let mut report = ExpenseDateRepairReport {
        applied: apply,
        ..ExpenseDateRepairReport::default()
    };
    // Planned writes, built while scanning, applied only after the backup
    // exists (or never, on a dry run).
    let mut planned: Vec<PlannedWrite> = Vec::new();

    for path in month_files(&dbs_root, &mut report.skipped_paths)? {
        let detailed = match DetailedExpenses::from_path(&path) {
            Ok(detailed) => detailed,
            Err(err) => {
                eprintln!(
                    "Expense date repair: skipped {}, which could not be read ({err}). It was \
                     left unchanged.",
                    path.display()
                );
                report.skipped_paths.push(path);
                continue;
            }
        };
        let class = classify(&detailed.expense_df, detailed.year, detailed.month)?;
        if class.class_a == 0 && class.class_b == 0 {
            continue;
        }
        report.files.push(FileRepairCounts {
            path: path.clone(),
            total_rows: detailed.expense_df.height(),
            class_a: class.class_a,
            class_b: class.class_b,
        });
        if let Some(day) = day {
            let (repaired_df, changed) = repair_dates(
                &detailed.expense_df,
                detailed.year,
                detailed.month,
                day.day(),
                &class.needs_repair,
            )?;
            let table = ChangeTable::Expenses {
                year: detailed.year,
                month: detailed.month,
            };
            planned.push(PlannedWrite {
                path,
                table,
                df: repaired_df,
                changed,
            });
        }
    }

    if !apply || planned.is_empty() {
        return Ok(report);
    }

    // No dedicated error variant exists for a backup failure outside
    // `crate::row_id_migration`, and `error.rs` is not this module's to
    // change; `InvalidArgument` is the closest generic, message-carrying
    // variant.
    let backup_dir = dbs_backup::backup_dbs(&dbs_root, BACKUP_SUFFIX, "Expense date repair")
        .map_err(|failure| {
            Error::InvalidArgument(format!(
                "the backup before expense date repair failed for {}: {}",
                failure.path.display(),
                failure.source
            ))
        })?;
    // Printed immediately, before any write is attempted: if a later write
    // fails, the returned `Err` drops `report` and this is otherwise the
    // only place the backup path would have appeared.
    eprintln!(
        "Expense date repair: backup written to {}",
        backup_dir.display()
    );
    report.backup_dir = Some(backup_dir);

    for write in planned {
        write_parquet_atomic(&write.df, &write.path)?;
        report.rows_repaired += write.changed.len();
        for (index, row_id) in write.changed {
            record_repair(&write.table, &write.df, index, &row_id);
        }
    }
    Ok(report)
}

/// One file's repaired dataframe, planned during the scan and applied only
/// after the backup exists.
struct PlannedWrite {
    path: PathBuf,
    table: ChangeTable,
    df: DataFrame,
    /// `(row index, row_id)` of every row this write changed, in row order.
    changed: Vec<(usize, String)>,
}

/// Every existing monthly detailed-expenses file under `dbs_root`, sorted by
/// year then month. A year folder whose entry type cannot be read is
/// recorded in `skipped_paths`, matching
/// [`crate::row_id_migration`]'s walk. A year folder holding no month
/// files, such as an otherwise-empty year, contributes nothing.
fn month_files(dbs_root: &Path, skipped_paths: &mut Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut years: Vec<(i32, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dbs_root)? {
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
            Ok(metadata) if metadata.is_dir() => years.push((year, path)),
            Ok(_) => {}
            Err(_) => skipped_paths.push(path),
        }
    }
    years.sort();

    let mut files = Vec::new();
    for (_, year_dir) in years {
        for month in 1..=12u32 {
            let path = year_dir.join(format!("{month:02}{PARQUET_SUFFIX}"));
            if path.exists() {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// Per-row classification of one file's `expense_df` against its own
/// `(year, month)`.
struct RowClass {
    /// One flag per row, in row order: `true` for class A or class B.
    needs_repair: Vec<bool>,
    class_a: usize,
    class_b: usize,
}

/// Classify every row of `df` (a loaded `expense_df`) against the file's own
/// `year` and `month`. Never compares against today: a row later than today
/// but inside `(year, month)` is class C, not a defect, and is never
/// flagged.
fn classify(df: &DataFrame, year: i32, month: u32) -> Result<RowClass> {
    let dates = expense_dates(df)?;
    let mut needs_repair = Vec::with_capacity(dates.len());
    let mut class_a = 0;
    let mut class_b = 0;
    for date in dates {
        match date {
            None => {
                class_a += 1;
                needs_repair.push(true);
            }
            Some(date) if date.year() != year || date.month() != month => {
                class_b += 1;
                needs_repair.push(true);
            }
            Some(_) => needs_repair.push(false),
        }
    }
    Ok(RowClass {
        needs_repair,
        class_a,
        class_b,
    })
}

/// Read the `expense_date` column as calendar dates, preserving null.
/// Mirrors `df_operations::date_col_to_vec`, which is private to that
/// module: this module is not allowed to change its visibility, and the
/// logic is a few lines of epoch arithmetic already duplicated at several
/// call sites in that file.
fn expense_dates(df: &DataFrame) -> Result<Vec<Option<NaiveDate>>> {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let days = df.column("expense_date")?.cast(&DataType::Int32)?;
    Ok(days
        .i32()?
        .iter()
        .map(|day| day.map(|day| epoch + chrono::Duration::days(day as i64)))
        .collect())
}

/// Build a repaired `expense_date` column for `df`: every row `needs_repair`
/// flags gets `year`-`month`-`day`, every other row keeps its stored value.
/// Returns the repaired dataframe and the `(row index, row_id)` of every row
/// that changed, in row order.
fn repair_dates(
    df: &DataFrame,
    year: i32,
    month: u32,
    day: u32,
    needs_repair: &[bool],
) -> Result<(DataFrame, Vec<(usize, String)>)> {
    let repaired_date = NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| {
        Error::InvalidArgument(format!("invalid repaired date {year}-{month:02}-{day:02}"))
    })?;
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let repaired_days = (repaired_date - epoch).num_days() as i32;

    let original_days = df.column("expense_date")?.cast(&DataType::Int32)?;
    let original_days = original_days.i32()?;
    let row_ids: Vec<Option<&str>> = df.column(ROW_ID_COLUMN)?.str()?.iter().collect();

    let mut new_days: Vec<i32> = Vec::with_capacity(df.height());
    let mut changed = Vec::new();
    for (index, flag) in needs_repair.iter().enumerate() {
        if *flag {
            new_days.push(repaired_days);
            match row_ids.get(index).copied().flatten() {
                Some(id) => changed.push((index, id.to_string())),
                None => eprintln!(
                    "Expense date repair: row {index} of the {year}-{month:02} file has no \
                     row_id, although the loader requires one; its date on disk was repaired \
                     but no change log entry was recorded for it."
                ),
            }
        } else {
            // Only class A rows are null, and those are always flagged
            // above, so every unflagged row has a stored value.
            new_days.push(original_days.get(index).unwrap_or(repaired_days));
        }
    }

    let new_column = Int32Chunked::from_vec("expense_date".into(), new_days)
        .into_series()
        .cast(&DataType::Date)?;
    let mut df = df.clone();
    df.with_column(new_column.into())?;
    Ok((df, changed))
}

/// Append this device's change log entry for one repaired row, mirroring
/// the warn-only failure handling every save in `crate::df_operations`
/// uses: the row is already on disk, so an append failure here must not
/// roll it back or retry it, only warn and mark the log incomplete.
fn record_repair(table: &ChangeTable, df: &DataFrame, index: usize, row_id: &str) {
    let appended = row_json(df, index).and_then(|row| {
        let log = sync::shared_log()?;
        log.append(table.clone(), row_id, ChangeOp::Upsert { row })
    });
    if let Err(err) = appended {
        let marked = match sync::mark_log_incomplete(IncompleteReason::AppendFailed) {
            Ok(()) => {
                "The log is marked incomplete, so sync stops until it is repaired.".to_string()
            }
            Err(mark_err) => format!(
                "Marking the log incomplete failed too ({mark_err}), so sync cannot detect this \
                 gap by itself."
            ),
        };
        let ChangeTable::Expenses { year, month } = table else {
            unreachable!("record_repair is only called for ChangeTable::Expenses")
        };
        eprintln!(
            "Expense date repair: the date fix for row {index} of the {year}-{month:02} file is \
             saved, but recording it in the change log failed ({err}). {marked}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::df_operations::read_parquet;
    use crate::paths::get_year_dir;

    /// Point `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `HOME` at a fresh temp
    /// folder, so no test touches real data.
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

    fn write(df: &DataFrame, path: &Path) {
        let mut df = df.clone();
        let file = std::fs::File::create(path).expect("create parquet file");
        ParquetWriter::new(file)
            .finish(&mut df)
            .expect("write parquet file");
    }

    fn day_series(name: &str, days: &[Option<i32>]) -> Column {
        Int32Chunked::from_iter(days.iter().copied())
            .into_series()
            .cast(&DataType::Date)
            .unwrap()
            .with_name(name.into())
            .into()
    }

    fn epoch_days(date: NaiveDate) -> i32 {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        (date - epoch).num_days() as i32
    }

    /// A 4-row March 2026 file: a null date (class A), the Unix epoch day
    /// (class B), an ordinary in-month date, and an in-month date near the
    /// end of the month (class C, standing in for a "future" entered on
    /// purpose; this module never compares against a clock, so nothing
    /// about that row needs to be "in the future" for the test to be
    /// meaningful).
    fn seed_march_2026() -> (PathBuf, Vec<String>) {
        let dir = get_year_dir(2026).unwrap();
        let path = dir.join("03_detailed_expenses.parquet");
        let row_ids: Vec<String> = (0..4).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        let df = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["A", "B", "C", "D"]),
            day_series(
                "expense_date",
                &[
                    None,
                    Some(epoch_days(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())),
                    Some(epoch_days(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap())),
                    Some(epoch_days(NaiveDate::from_ymd_opt(2026, 3, 31).unwrap())),
                ],
            ),
            Column::new("expense_amount".into(), &[10.0, 20.0, 30.0, 40.0]),
            Column::new("currency".into(), &["EUR", "EUR", "EUR", "EUR"]),
            Column::new(
                "primary_category".into(),
                &["Housing", "Housing", "Housing", "Housing"],
            ),
            Column::new(
                "secondary_category".into(),
                &["Rent", "Rent", "Rent", "Rent"],
            ),
            Column::new(ROW_ID_COLUMN.into(), row_ids.as_slice()),
        ])
        .unwrap();
        write(&df, &path);
        (path, row_ids)
    }

    /// A second, unrelated file with no bad dates, used to prove a clean
    /// file is left alone.
    fn seed_april_2026_clean() -> PathBuf {
        let dir = get_year_dir(2026).unwrap();
        let path = dir.join("04_detailed_expenses.parquet");
        let df = DataFrame::new_infer_height(vec![
            Column::new("expense_name".into(), &["Gym"]),
            day_series(
                "expense_date",
                &[Some(epoch_days(
                    NaiveDate::from_ymd_opt(2026, 4, 5).unwrap(),
                ))],
            ),
            Column::new("expense_amount".into(), &[40.0]),
            Column::new("currency".into(), &["EUR"]),
            Column::new("primary_category".into(), &["Leisure"]),
            Column::new("secondary_category".into(), &["Gym"]),
            Column::new(ROW_ID_COLUMN.into(), &[uuid::Uuid::new_v4().to_string()]),
        ])
        .unwrap();
        write(&df, &path);
        path
    }

    /// The null-date row and the epoch-date row are selected as class A and
    /// class B; the ordinary and the in-month "future" rows are not
    /// selected at all.
    #[test]
    #[serial_test::serial]
    fn selects_only_null_and_out_of_month_rows() {
        let _temp = with_temp_data_home();
        let (path, _) = seed_march_2026();

        let report = repair_expense_dates(false, None).expect("dry run succeeds");

        assert_eq!(report.files.len(), 1);
        let file = &report.files[0];
        assert_eq!(file.path, path);
        assert_eq!(file.total_rows, 4);
        assert_eq!(file.class_a, 1);
        assert_eq!(file.class_b, 1);
        assert!(report.skipped_paths.is_empty());
    }

    /// A dry run writes nothing: the file's bytes are unchanged and no
    /// backup folder is created.
    #[test]
    #[serial_test::serial]
    fn dry_run_writes_nothing() {
        let _temp = with_temp_data_home();
        let (path, _) = seed_march_2026();
        let before = std::fs::read(&path).unwrap();

        let report = repair_expense_dates(false, None).expect("dry run succeeds");

        assert!(!report.applied);
        assert_eq!(report.backup_dir, None);
        assert_eq!(report.rows_repaired, 0);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            crate::paths::get_backups_dir()
                .unwrap()
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );
    }

    /// `--apply` without a day policy refuses to run and changes nothing.
    #[test]
    #[serial_test::serial]
    fn apply_without_day_policy_refuses() {
        let _temp = with_temp_data_home();
        let (path, _) = seed_march_2026();
        let before = std::fs::read(&path).unwrap();

        let err = repair_expense_dates(true, None).expect_err("must refuse");

        assert!(matches!(err, Error::InvalidArgument(_)));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            crate::paths::get_backups_dir()
                .unwrap()
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );
    }

    /// After `--apply`, the class A and class B rows carry the file's own
    /// year and month plus the policy day; every other column, every
    /// `row_id`, and the untouched rows are unchanged. A second file with
    /// nothing to repair is not rewritten. A backup holds the pre-change
    /// copy of the repaired file, and the change log carries one Upsert per
    /// repaired row.
    #[test]
    #[serial_test::serial]
    fn apply_repairs_only_the_bad_rows_and_logs_them() {
        let _temp = with_temp_data_home();
        let (path, row_ids) = seed_march_2026();
        let clean_path = seed_april_2026_clean();
        let before_repair = read_parquet(&path).unwrap();
        let before_repair_bytes = std::fs::read(&path).unwrap();
        let clean_before = std::fs::read(&clean_path).unwrap();

        let report =
            repair_expense_dates(true, Some(DayPolicy::FirstOfMonth)).expect("apply succeeds");

        assert!(report.applied);
        assert_eq!(report.rows_repaired, 2);
        let backup_dir = report.backup_dir.expect("a backup was taken");
        assert_eq!(
            std::fs::read(backup_dir.join("2026").join("03_detailed_expenses.parquet")).unwrap(),
            before_repair_bytes,
            "the backup holds the pre-repair copy of the file"
        );

        let after = read_parquet(&path).unwrap();
        let repaired_date = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        let dates = expense_dates(&after).unwrap();
        assert_eq!(dates[0], Some(repaired_date), "class A row repaired");
        assert_eq!(dates[1], Some(repaired_date), "class B row repaired");
        assert_eq!(
            dates[2],
            Some(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()),
            "ordinary row untouched"
        );
        assert_eq!(
            dates[3],
            Some(NaiveDate::from_ymd_opt(2026, 3, 31).unwrap()),
            "class C row untouched"
        );
        assert_eq!(
            after.drop("expense_date").unwrap(),
            before_repair.drop("expense_date").unwrap(),
            "no column other than expense_date changed"
        );
        let ids_after: Vec<String> = after
            .column(ROW_ID_COLUMN)
            .unwrap()
            .str()
            .unwrap()
            .iter()
            .map(|id| id.unwrap().to_string())
            .collect();
        assert_eq!(ids_after, row_ids, "row_id values and order are unchanged");

        // The unrelated clean file is byte-for-byte untouched.
        assert_eq!(std::fs::read(&clean_path).unwrap(), clean_before);

        let entries = sync::read_log().unwrap().entries;
        let expenses_upserts: Vec<_> = entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.table,
                    ChangeTable::Expenses {
                        year: 2026,
                        month: 3
                    }
                )
            })
            .collect();
        assert_eq!(expenses_upserts.len(), 2);
        for row_id in [&row_ids[0], &row_ids[1]] {
            assert!(
                expenses_upserts
                    .iter()
                    .any(|entry| &entry.row_id == row_id
                        && matches!(entry.op, ChangeOp::Upsert { .. })),
                "expected an Upsert entry for {row_id}"
            );
        }

        // A second dry run finds nothing left to repair.
        let second = repair_expense_dates(false, None).expect("second dry run succeeds");
        assert!(second.files.is_empty());
    }

    /// An unrecognized `--day` value is rejected by the parser, not
    /// silently accepted as a policy.
    #[test]
    fn day_policy_parse_rejects_unknown_values() {
        assert_eq!(
            DayPolicy::parse("first-of-month"),
            Some(DayPolicy::FirstOfMonth)
        );
        assert_eq!(DayPolicy::parse("last-of-month"), None);
        assert_eq!(DayPolicy::parse(""), None);
    }
}
