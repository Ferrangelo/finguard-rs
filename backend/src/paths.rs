//! Path functions for finguard data files.
//!
//! Data files are stored in `$XDG_DATA_HOME/finguard/dbs/` (defaulting to
//! `$HOME/.local/share/finguard/dbs/` when `XDG_DATA_HOME` is not set).
//!
//! # Directory layout
//!
//! ```text
//! $XDG_DATA_HOME/
//! └── finguard/
//!     └── dbs/
//!         ├── 2025/
//!         │   ├── 01_detailed_expenses.parquet
//!         │   ├── 02_detailed_expenses.parquet
//!         │   ├── ...
//!         │   ├── primaries.parquet
//!         │   └── secondaries.parquet
//!         └── ...
//! ```
//!
//! Naming convention: monthly parquet files are named
//! `MM_detailed_expenses.parquet` where `MM` is the zero-padded month number
//! (01–12).
//!
//! Path resolution reads the environment at call time (it is *not* cached) so
//! that tests can override `XDG_DATA_HOME` / `HOME` between invocations.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const APP_DIR_NAME: &str = "finguard";
const DBS_DIR_NAME: &str = "dbs";
const PARQUET_SUFFIX: &str = "_detailed_expenses.parquet";

/// Filename for the per-year primary-category summary.
pub const PRIMARIES_FILENAME: &str = "primaries.parquet";
/// Filename for the per-year secondary-category summary.
pub const SECONDARIES_FILENAME: &str = "secondaries.parquet";
/// Filename for the cashflow table.
pub const CASHFLOW_FILENAME: &str = "cashflow.parquet";
/// Filename for the investments table.
pub const INVESTMENTS_FILENAME: &str = "investments.parquet";
/// Filename for the investment-prices table.
pub const INVESTMENTS_PRICES_FILENAME: &str = "investments_prices.parquet";
/// Filename for the liquidity table.
pub const LIQUIDITY_FILENAME: &str = "liquidity.parquet";
/// Filename for the credits/debts table.
pub const CREDITS_DEBTS_FILENAME: &str = "credits_debts.parquet";
/// Filename for the recurring-expenses table.
pub const RECURRING_EXPENSES_FILENAME: &str = "recurring_expenses.parquet";

/// Return the XDG data home directory (`$XDG_DATA_HOME` or
/// `$HOME/.local/share`).
fn get_data_home() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg));
    }
    let home = dirs::home_dir().ok_or(Error::NoHomeDir)?;
    Ok(home.join(".local").join("share"))
}

/// Return `<XDG_DATA_HOME>/finguard/dbs`, creating it if necessary.
pub fn get_dbs_root() -> Result<PathBuf> {
    let dbs_root = get_data_home()?.join(APP_DIR_NAME).join(DBS_DIR_NAME);
    std::fs::create_dir_all(&dbs_root)?;
    Ok(dbs_root)
}

/// Return `<dbs_root>/<year>/`, creating it if necessary.
pub fn get_year_dir(year: i32) -> Result<PathBuf> {
    let year_dir = get_dbs_root()?.join(year.to_string());
    std::fs::create_dir_all(&year_dir)?;
    Ok(year_dir)
}

/// Return the path for a summary parquet file inside the given year directory.
///
/// For example, `get_year_summary_path(2026, PRIMARIES_FILENAME)` yields
/// `~/.local/share/finguard/dbs/2026/primaries.parquet`.
pub fn get_year_summary_path(year: i32, filename: &str) -> Result<PathBuf> {
    Ok(get_year_dir(year)?.join(filename))
}

/// Return the full path for a monthly detailed-expenses parquet file.
///
/// Parent directories are created if they do not already exist. For example,
/// `get_monthly_parquet_path(2026, 3)` yields
/// `~/.local/share/finguard/dbs/2026/03_detailed_expenses.parquet`.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] if `month` is not in the range 1–12.
pub fn get_monthly_parquet_path(year: i32, month: u32) -> Result<PathBuf> {
    if !(1..=12).contains(&month) {
        return Err(Error::InvalidArgument(format!(
            "month must be between 1 and 12, got {month}"
        )));
    }
    let filename = format!("{month:02}{PARQUET_SUFFIX}");
    Ok(get_year_dir(year)?.join(filename))
}

/// Extract the month number (1–12) from a parquet file path.
///
/// The filename must follow the `MM_detailed_expenses.parquet` convention.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] if the filename does not match the
/// expected pattern, cannot be parsed, or yields an out-of-range month.
pub fn month_from_parquet_path(path: impl AsRef<Path>) -> Result<u32> {
    let path = path.as_ref();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::InvalidArgument(format!("Invalid path: {}", path.display())))?;

    let Some(month_str) = name.strip_suffix(PARQUET_SUFFIX) else {
        return Err(Error::InvalidArgument(format!(
            "Filename '{name}' does not match the expected pattern 'MM{PARQUET_SUFFIX}'."
        )));
    };

    let month: u32 = month_str.parse().map_err(|_| {
        Error::InvalidArgument(format!(
            "Could not parse month from filename '{name}'. Expected the first two characters \
             to be a zero-padded month number."
        ))
    })?;

    if !(1..=12).contains(&month) {
        return Err(Error::InvalidArgument(format!(
            "Parsed month {month} from '{name}' is out of range (1–12)."
        )));
    }

    Ok(month)
}

/// Extract the year from a parquet file path.
///
/// Assumes the file sits directly inside a directory named after the year,
/// e.g. `…/2026/03_detailed_expenses.parquet`.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] if the parent directory name is not a
/// valid year.
pub fn year_from_parquet_path(path: impl AsRef<Path>) -> Result<i32> {
    let path = path.as_ref();
    let parent_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");

    parent_name.parse().map_err(|_| {
        Error::InvalidArgument(format!(
            "Could not parse year from parent directory '{parent_name}'. Expected the parquet \
             file to sit inside a directory named after the year (e.g. '2026/')."
        ))
    })
}

/// Extract both year and month from a parquet file path.
///
/// The path must follow the `<year>/MM_detailed_expenses.parquet` convention.
/// Returns `(year, month)`.
pub fn year_month_from_parquet_path(path: impl AsRef<Path>) -> Result<(i32, u32)> {
    let path = path.as_ref();
    Ok((
        year_from_parquet_path(path)?,
        month_from_parquet_path(path)?,
    ))
}
