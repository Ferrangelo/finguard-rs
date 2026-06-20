//! Core dataframe operations, ported from the original Python `df_operations.py`
//! plus the expense edit/delete/filter logic that lived in `ui_expenses.py`.
//!
//! All tables are stored as parquet files under the standard XDG data path
//! (see [`crate::paths`]). Schemas are kept byte-compatible with the Python
//! application so the two can read each other's files.

use std::collections::HashSet;

use chrono::NaiveDate;
use indexmap::IndexMap;
use polars::functions::concat_df_diagonal;
use polars::prelude::*;

use crate::config;
use crate::error::{Error, Result};
use crate::paths::{
    CASHFLOW_FILENAME, CREDITS_DEBTS_FILENAME, INVESTMENTS_FILENAME, INVESTMENTS_PRICES_FILENAME,
    LIQUIDITY_FILENAME, PRIMARIES_FILENAME, RECURRING_EXPENSES_FILENAME, SECONDARIES_FILENAME,
    get_dbs_root, get_monthly_parquet_path, get_year_summary_path, year_month_from_parquet_path,
};

// ======================================================================
// Constants
// ======================================================================

/// Special-case category-name mappings (lower-cased key → canonical display
/// value). Mirrors the Python `_SPECIAL_CASES` dict exactly.
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

/// Canonical display order for primary categories.
const PRIMARY_CATEGORY_ORDER: &[&str] = &[
    "Housing",
    "Health",
    "Groceries",
    "Transport",
    "Lunchbreak",
    "Out",
    "Travel",
    "Baby",
    "Clothing",
    "Leisure",
    "Gifts",
    "Fees",
    "OtherExpenses",
    "Missioni",
];

/// Row labels for the income categories (user-editable values).
const INCOME_CATEGORIES: &[&str] = &[
    "Salary",
    "Interests Bank account",
    "Dividendi e Cedole",
    "Other",
];

/// Row labels for the derived (computed) cashflow categories.
const DERIVED_CATEGORIES: &[&str] = &["Income", "Spending", "Saving", "Saving %"];

/// Valid investment categories.
const INVESTMENT_CATEGORIES: &[&str] = &["Stocks/ETF", "Commodities", "Bonds"];

/// Valid liquidity categories.
const LIQUIDITY_CATEGORIES: &[&str] = &["Bank/Broker account", "Cash", "Other"];

/// Month column labels (`"01"`..`"12"`) used in wide yearly tables.
fn month_labels() -> Vec<String> {
    (1..=12).map(|m| format!("{m:02}")).collect()
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

/// Return the total `expense_in_ref_currency` for every category of `kind`,
/// summed across **all** year summary parquet files.
///
/// Categories whose rows only contain zeros still appear with a value of `0.0`.
/// The synthetic `"Total"` row is excluded. Per-file errors are swallowed
/// (mirroring the Python `try/except: pass`).
///
/// `kind` must be `"primary"` or `"secondary"`.
pub fn get_category_totals_across_all_years(kind: &str) -> Result<IndexMap<String, f64>> {
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
    let mut totals: IndexMap<String, f64> = IndexMap::new();

    // Mirror Python's broad try/except: swallow any error encountered while
    // iterating directories or reading files.
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
            let month_cols: Vec<String> = df
                .get_column_names()
                .iter()
                .map(|s| s.to_string())
                .filter(|c| *c != category_col)
                .collect();
            let cat_series = df.column(&category_col)?.str()?.clone();
            for (i, cat_opt) in cat_series.iter().enumerate() {
                let Some(cat) = cat_opt else { continue };
                if cat.is_empty() || cat == "Total" {
                    continue;
                }
                let mut row_total = 0.0;
                for mc in &month_cols {
                    let v = df
                        .column(mc)?
                        .f64()
                        .ok()
                        .and_then(|s| s.get(i))
                        .unwrap_or(0.0);
                    row_total += v;
                }
                *totals.entry(cat.to_string()).or_insert(0.0) += row_total;
            }
        }
        Ok(())
    })();

    Ok(totals)
}

/// Delete the row for `name` from every year-summary parquet file of `kind`.
///
/// This is a permanent write operation — call only after confirming the
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
fn read_parquet(path: &std::path::Path) -> Result<DataFrame> {
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

/// Return whether `df` contains a column named `name`.
fn has_column(df: &DataFrame, name: &str) -> bool {
    df.get_column_names().iter().any(|c| c.as_str() == name)
}

/// Read a string column into an owned `Vec<String>` (nulls become empty).
fn str_col_to_vec(df: &DataFrame, name: &str) -> Result<Vec<String>> {
    Ok(df
        .column(name)?
        .str()?
        .iter()
        .map(|o| o.unwrap_or("").to_string())
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
fn empty_expenses_df() -> DataFrame {
    DataFrame::empty_with_schema(&Schema::from_iter([
        Field::new("expense_name".into(), DataType::String),
        Field::new("expense_date".into(), DataType::Date),
        Field::new("expense_amount".into(), DataType::Float64),
        Field::new("currency".into(), DataType::String),
        Field::new("expense_in_ref_currency".into(), DataType::Float64),
        Field::new("primary_category".into(), DataType::String),
        Field::new("secondary_category".into(), DataType::String),
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
    /// exists or initialising an empty table otherwise.
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
            // Ensure expense_date is Date (older files may store it as Int64).
            if has_column(&df, "expense_date")
                && df.column("expense_date")?.dtype() != &DataType::Date
            {
                df = df
                    .lazy()
                    .with_column(col("expense_date").cast(DataType::Date))
                    .collect()?;
            }
            df
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

    /// Convert `amount` into the reference currency (currently a 1.0 stub).
    fn convert_in_ref_currency(amount: f64) -> f64 {
        let change = 1.0;
        amount * change
    }

    /// Append an expense row and save the updated dataframe.
    ///
    /// If `primary_category` or `secondary_category` are `None`, they are
    /// resolved from the category-mappings config. Returns
    /// [`Error::InvalidArgument`] when `primary_category` cannot be resolved.
    #[allow(clippy::too_many_arguments)]
    pub fn add_row(
        &mut self,
        expense_name: &str,
        expense_day: u32,
        expense_amount: f64,
        primary_category: Option<&str>,
        currency: &str,
        secondary_category: Option<&str>,
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

        let expense_in_ref_currency = Self::convert_in_ref_currency(expense_amount);

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
            Column::new("expense_in_ref_currency".into(), &[expense_in_ref_currency]),
            Column::new(
                "primary_category".into(),
                &[normalize_category_value(&primary)],
            ),
            Column::new(
                "secondary_category".into(),
                &[normalize_category_value(&secondary)],
            ),
        ])?;

        self.expense_df = concat_df_diagonal(&[self.expense_df.clone(), new_row])?;
        write_parquet(&self.expense_df, &self.expense_df_path)?;
        self.update_all_summary_tables()?;
        Ok(())
    }

    /// Remove the `row_index`-th row, save, and recompute the summary tables.
    pub fn delete_row(&mut self, row_index: u32) -> Result<()> {
        self.expense_df = self
            .expense_df
            .clone()
            .lazy()
            .with_row_index("_idx", None)
            .filter(col("_idx").neq(lit(row_index)))
            .drop(cols(["_idx"]))
            .collect()?;
        write_parquet(&self.expense_df, &self.expense_df_path)?;
        self.update_all_summary_tables()?;
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

    /// Apply field edits to the `row_index`-th row, save, and recompute summaries.
    ///
    /// This ports the `save_edit` logic from `ui_expenses.py`: any field left
    /// `None` is unchanged; `expense_day` rebuilds `expense_date` from the
    /// table's year/month; `expense_amount` also drives `expense_in_ref_currency`.
    ///
    /// Categories are resolved via [`resolve_category`] (not
    /// [`normalize_category_value`]): the edited value is first matched
    /// case-insensitively against the **existing** category set (the current
    /// month's `primary_category` / `secondary_category` values plus the known
    /// categories from [`config::get_known_categories`]), preserving the existing
    /// spelling, and only normalized on no match. This mirrors the Python edit
    /// dialog, which builds `existing_pri` / `existing_sec` and calls
    /// `resolve_category`.
    ///
    /// Divergence from Python: `save_edit` did not recompute the on-disk summary
    /// tables. This port calls [`Self::update_all_summary_tables`] after editing
    /// (intentional improvement) so the summaries stay consistent with the edit.
    #[allow(clippy::too_many_arguments)]
    pub fn edit_row(
        &mut self,
        row_index: u32,
        expense_name: Option<&str>,
        expense_day: Option<u32>,
        expense_amount: Option<f64>,
        currency: Option<&str>,
        primary_category: Option<&str>,
        secondary_category: Option<&str>,
    ) -> Result<()> {
        let mut df = self.expense_df.clone().lazy().with_row_index("_idx", None);

        let pred = col("_idx").eq(lit(row_index));

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
            let ref_amount = Self::convert_in_ref_currency(amount);
            df = df
                .with_column(
                    when(pred.clone())
                        .then(lit(amount))
                        .otherwise(col("expense_amount"))
                        .alias("expense_amount"),
                )
                .with_column(
                    when(pred.clone())
                        .then(lit(ref_amount))
                        .otherwise(col("expense_in_ref_currency"))
                        .alias("expense_in_ref_currency"),
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

        self.expense_df = df.drop(cols(["_idx"])).collect()?;
        write_parquet(&self.expense_df, &self.expense_df_path)?;
        self.update_all_summary_tables()?;
        Ok(())
    }

    /// Return a filtered copy of the expense dataframe.
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
        let mut lf = self.expense_df.clone().lazy();

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

        Ok(lf.collect()?)
    }

    /// Return a summary grouped by `category_col` (`"primary_category"` or
    /// `"secondary_category"`), summing `expense_in_ref_currency`.
    pub fn create_expenses_summary_table(&self, category_col: &str) -> Result<DataFrame> {
        if category_col != "primary_category" && category_col != "secondary_category" {
            return Err(Error::InvalidArgument(format!(
                "category_col must be 'primary_category' or 'secondary_category', got \
                 '{category_col}'"
            )));
        }
        Ok(self
            .expense_df
            .clone()
            .lazy()
            .group_by([col(category_col)])
            .agg([col("expense_in_ref_currency")
                .sum()
                .alias("total_expense_in_ref_currency")])
            .collect()?)
    }

    /// Return the current month as a `YYYY-MM` string (e.g. `"2026-03"`).
    fn month_label(&self) -> String {
        format!("{}-{:02}", self.year, self.month)
    }

    /// Core logic for updating a cumulative wide summary table. See module-level
    /// docs; this faithfully reproduces the Python `_update_summary_table`.
    fn update_summary_table(&self, kind: &str) -> Result<DataFrame> {
        let category_col = format!("{kind}_category");
        let filename = if kind == "primary" {
            PRIMARIES_FILENAME
        } else {
            SECONDARIES_FILENAME
        };
        let summary_path = get_year_summary_path(self.year, filename)?;
        let month_label = self.month_label();

        // current month totals (long format)
        let monthly = self
            .expense_df
            .clone()
            .lazy()
            .group_by([col(category_col.as_str())])
            .agg([col("expense_in_ref_currency")
                .sum()
                .alias(month_label.as_str())])
            .collect()?;

        // load or initialise the summary table
        let mut summary = if summary_path.exists() {
            let mut existing = read_parquet(&summary_path)?;
            if has_column(&existing, &month_label) {
                existing = existing.drop(&month_label)?;
            }
            // Outer/full join with coalesce on the category key.
            existing
                .lazy()
                .join(
                    monthly.lazy(),
                    [col(category_col.as_str())],
                    [col(category_col.as_str())],
                    JoinArgs::new(JoinType::Full).with_coalesce(JoinCoalesce::CoalesceColumns),
                )
                .collect()?
        } else {
            monthly
        };

        // fill all month columns with 0.0 where null
        let month_cols: Vec<String> = summary
            .get_column_names()
            .iter()
            .map(|s| s.to_string())
            .filter(|c| *c != category_col)
            .collect();
        summary = summary
            .lazy()
            .with_columns(
                month_cols
                    .iter()
                    .map(|c| col(c.as_str()).fill_null(lit(0.0)))
                    .collect::<Vec<_>>(),
            )
            .collect()?;

        // sort columns: category first, then months chronologically (YYYY-MM)
        let mut sorted_month_cols = month_cols.clone();
        sorted_month_cols.sort();
        let mut ordered_cols = vec![category_col.clone()];
        ordered_cols.extend(sorted_month_cols.iter().cloned());
        summary = summary.select(&ordered_cols)?;

        // Add totals row (strip any pre-existing Total rows first)
        summary = summary
            .lazy()
            .filter(col(category_col.as_str()).neq(lit("Total")))
            .collect()?;

        // Compute per-month column sums for the Total row.
        let mut total_cols: Vec<Column> =
            vec![Column::new(category_col.as_str().into(), &["Total"])];
        for mc in &sorted_month_cols {
            let sum = summary.column(mc)?.f64()?.sum().unwrap_or(0.0);
            total_cols.push(Column::new(mc.as_str().into(), &[sum]));
        }
        let total_row = DataFrame::new_infer_height(total_cols)?;
        summary = concat_df_diagonal(&[summary, total_row])?;

        // sort rows by canonical category order (primary only)
        if kind == "primary" {
            let mut order_names: Vec<&str> = PRIMARY_CATEGORY_ORDER.to_vec();
            order_names.push("Total");
            let order_idx: Vec<i64> = (0..order_names.len() as i64).collect();
            let order_df = DataFrame::new_infer_height(vec![
                Column::new(category_col.as_str().into(), &order_names),
                Column::new("_order".into(), order_idx),
            ])?;
            summary = summary
                .lazy()
                .join(
                    order_df.lazy(),
                    [col(category_col.as_str())],
                    [col(category_col.as_str())],
                    JoinArgs::new(JoinType::Left),
                )
                .sort(
                    ["_order"],
                    SortMultipleOptions::default().with_nulls_last(true),
                )
                .drop(cols(["_order"]))
                .collect()?;
        }

        write_parquet(&summary, &summary_path)?;
        Ok(summary)
    }

    /// Update and save the cumulative primaries summary table.
    pub fn update_primaries_summary_table(&self) -> Result<DataFrame> {
        self.update_summary_table("primary")
    }

    /// Update and save the cumulative secondaries summary table.
    pub fn update_secondaries_summary_table(&self) -> Result<DataFrame> {
        self.update_summary_table("secondary")
    }

    /// Update both summary tables, returning `(primaries, secondaries)`.
    pub fn update_all_summary_tables(&self) -> Result<(DataFrame, DataFrame)> {
        Ok((
            self.update_primaries_summary_table()?,
            self.update_secondaries_summary_table()?,
        ))
    }
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

/// Yearly cashflow table (wide format), with income rows set manually and
/// derived rows recomputed from income totals and `primaries.parquet`.
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
        Ok(())
    }

    /// Recompute all derived rows from income values and `primaries.parquet`,
    /// then save.
    pub fn recompute(&mut self) -> Result<()> {
        let primaries_path = get_year_summary_path(self.year, PRIMARIES_FILENAME)?;
        let primaries = if primaries_path.exists() {
            Some(read_parquet(&primaries_path)?)
        } else {
            None
        };

        for month in 1..=12u32 {
            let col_name = format!("{month:02}");
            let month_label = format!("{}-{:02}", self.year, month);

            // Income = sum of income categories
            let mut income = 0.0;
            for cat in INCOME_CATEGORIES {
                income += self.get_value(cat, &col_name)?;
            }
            self.set_value("Income", &col_name, income)?;

            // Spending = "Total" row from primaries.parquet for this month
            let mut spending = 0.0;
            if let Some(p) = &primaries
                && has_column(p, &month_label)
            {
                let total_rows = p
                    .clone()
                    .lazy()
                    .filter(col("primary_category").eq(lit("Total")))
                    .collect()?;
                if total_rows.height() > 0 {
                    spending = total_rows
                        .column(&month_label)?
                        .f64()?
                        .get(0)
                        .unwrap_or(0.0);
                }
            }
            self.set_value("Spending", &col_name, spending)?;

            let saving = income - spending;
            self.set_value("Saving", &col_name, saving)?;

            let saving_pct = if income != 0.0 {
                100.0 * saving / income
            } else {
                0.0
            };
            self.set_value("Saving %", &col_name, saving_pct)?;
        }

        self.save()?;
        Ok(())
    }

    /// Write the cashflow dataframe to disk.
    pub fn save(&self) -> Result<()> {
        write_parquet(&self.df, &self.path)
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
/// followed by month columns `"01".."12"` of `Float64`.
fn empty_wide_df(meta_cols: &[&str]) -> DataFrame {
    let mut fields: Vec<Field> = meta_cols
        .iter()
        .map(|c| Field::new((*c).into(), DataType::String))
        .collect();
    for m in month_labels() {
        fields.push(Field::new(m.as_str().into(), DataType::Float64));
    }
    DataFrame::empty_with_schema(&Schema::from_iter(fields))
}

/// Build a single-row wide dataframe from `(meta column, value)` pairs plus
/// zeroed month columns.
fn wide_row(meta: &[(&str, &str)]) -> Result<DataFrame> {
    let mut cols: Vec<Column> = meta
        .iter()
        .map(|(name, val)| Column::new((*name).into(), &[*val]))
        .collect();
    for m in month_labels() {
        cols.push(Column::new(m.as_str().into(), &[0.0_f64]));
    }
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
pub struct InvestmentHoldings {
    /// Calendar year of this table.
    pub year: i32,
    path: std::path::PathBuf,
    path_prices: std::path::PathBuf,
    /// Quantities dataframe (`asset_name, category, link, 01..12`).
    pub df: DataFrame,
    /// Prices dataframe (same schema as `df`).
    pub df_prices: DataFrame,
}

impl InvestmentHoldings {
    /// Construct for `year`, loading holdings and prices from disk (or
    /// initialising empty), migrating a missing `link` column.
    pub fn new(year: i32) -> Result<Self> {
        let path = get_year_summary_path(year, INVESTMENTS_FILENAME)?;
        let path_prices = get_year_summary_path(year, INVESTMENTS_PRICES_FILENAME)?;

        let load = |p: &std::path::Path| -> Result<DataFrame> {
            if p.exists() {
                let mut df = read_parquet(p)?;
                if !has_column(&df, "link") {
                    df = df.lazy().with_column(lit("").alias("link")).collect()?;
                }
                Ok(df)
            } else {
                Ok(empty_wide_df(&["asset_name", "category", "link"]))
            }
        };

        Ok(Self {
            year,
            df: load(&path)?,
            df_prices: load(&path_prices)?,
            path,
            path_prices,
        })
    }

    fn asset_names(&self) -> Result<Vec<String>> {
        str_col_to_vec(&self.df, "asset_name")
    }

    /// Add a new asset row (monthly quantities initialised to 0) and save.
    pub fn add_asset(&mut self, asset_name: &str, category: &str, link: &str) -> Result<()> {
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
        let new_row = wide_row(&[
            ("asset_name", asset_name),
            ("category", category),
            ("link", link),
        ])?;
        self.df = concat_df_diagonal(&[self.df.clone(), new_row.clone()])?;
        self.df_prices = concat_df_diagonal(&[self.df_prices.clone(), new_row])?;
        self.save()
    }

    /// Remove an asset row by name from both holdings and prices, then save.
    pub fn remove_asset(&mut self, asset_name: &str) -> Result<()> {
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
        self.save()
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
        self.save()
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
        self.save()
    }

    /// Update the link URL for an asset in both frames, then save.
    pub fn set_link(&mut self, asset_name: &str, link: &str) -> Result<()> {
        if !self.asset_names()?.iter().any(|n| n == asset_name) {
            return Err(Error::NotFound(format!("Asset '{asset_name}' not found.")));
        }
        self.df = set_str_where(self.df.clone(), "asset_name", asset_name, "link", link)?;
        self.df_prices = set_str_where(
            self.df_prices.clone(),
            "asset_name",
            asset_name,
            "link",
            link,
        )?;
        self.save()
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
                self.df =
                    set_f64_where(self.df.clone(), "asset_name", asset_name, &col_name, value)?;
                self.save_df()
            }
            "price" => {
                self.df_prices = set_f64_where(
                    self.df_prices.clone(),
                    "asset_name",
                    asset_name,
                    &col_name,
                    value,
                )?;
                self.save_df_prices()
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
    /// Same shape as `df` (`asset_name, category, link, 01..12`) but each
    /// monthly cell contains `quantity * price`.
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
    pub fn save_df(&self) -> Result<()> {
        write_parquet(&self.df, &self.path)
    }

    /// Write the prices dataframe to disk.
    pub fn save_df_prices(&self) -> Result<()> {
        write_parquet(&self.df_prices, &self.path_prices)
    }

    /// Write both holdings and prices dataframes to disk.
    pub fn save(&self) -> Result<()> {
        self.save_df()?;
        self.save_df_prices()
    }
}

// ======================================================================
// Liquidity
// ======================================================================

/// Yearly liquidity table (`asset_name, category, currency, 01..12`).
pub struct Liquidity {
    /// Calendar year of this table.
    pub year: i32,
    path: std::path::PathBuf,
    /// In-memory liquidity dataframe.
    pub df: DataFrame,
}

impl Liquidity {
    /// Construct for `year`, loading from disk (migrating a missing `currency`
    /// column to `"E"`) or initialising empty.
    pub fn new(year: i32) -> Result<Self> {
        let path = get_year_summary_path(year, LIQUIDITY_FILENAME)?;
        let df = if path.exists() {
            let mut df = read_parquet(&path)?;
            if !has_column(&df, "currency") {
                df = df
                    .lazy()
                    .with_column(lit("E").alias("currency"))
                    .collect()?;
            }
            df
        } else {
            empty_wide_df(&["asset_name", "category", "currency"])
        };
        Ok(Self { year, path, df })
    }

    fn asset_names(&self) -> Result<Vec<String>> {
        str_col_to_vec(&self.df, "asset_name")
    }

    /// Add a new liquidity asset row (monthly values initialised to 0), save.
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
        let new_row = wide_row(&[
            ("asset_name", asset_name),
            ("category", category),
            ("currency", currency),
        ])?;
        self.df = concat_df_diagonal(&[self.df.clone(), new_row])?;
        self.save()
    }

    /// Remove a liquidity asset row by name, then save.
    pub fn remove_asset(&mut self, asset_name: &str) -> Result<()> {
        self.df = self
            .df
            .clone()
            .lazy()
            .filter(col("asset_name").neq(lit(asset_name)))
            .collect()?;
        self.save()
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
        self.df = set_str_where(
            self.df.clone(),
            "asset_name",
            old_name,
            "asset_name",
            new_name,
        )?;
        self.save()
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
        self.df = set_str_where(
            self.df.clone(),
            "asset_name",
            asset_name,
            "category",
            category,
        )?;
        self.save()
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
        self.df = set_f64_where(self.df.clone(), "asset_name", asset_name, &col_name, value)?;
        self.save()
    }

    /// Write the liquidity dataframe to disk.
    pub fn save(&self) -> Result<()> {
        write_parquet(&self.df, &self.path)
    }
}

// ======================================================================
// CreditsDebts
// ======================================================================

/// Yearly credits & debts table (`name, currency, 01..12`).
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
    /// to `"E"` and dropping a legacy `type` column) or initialising empty.
    pub fn new(year: i32) -> Result<Self> {
        let path = get_year_summary_path(year, CREDITS_DEBTS_FILENAME)?;
        let df = if path.exists() {
            let mut df = read_parquet(&path)?;
            if !has_column(&df, "currency") {
                df = df
                    .lazy()
                    .with_column(lit("E").alias("currency"))
                    .collect()?;
            }
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

    /// Add a new credit/debt row (monthly values initialised to 0), then save.
    pub fn add_entry(&mut self, name: &str, currency: &str) -> Result<()> {
        if self.entry_names()?.iter().any(|n| n == name) {
            return Err(Error::AlreadyExists(format!(
                "Entry '{name}' already exists."
            )));
        }
        let new_row = wide_row(&[("name", name), ("currency", currency)])?;
        self.df = concat_df_diagonal(&[self.df.clone(), new_row])?;
        self.save()
    }

    /// Remove a credit/debt row by name, then save.
    pub fn remove_entry(&mut self, name: &str) -> Result<()> {
        self.df = self
            .df
            .clone()
            .lazy()
            .filter(col("name").neq(lit(name)))
            .collect()?;
        self.save()
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
        self.df = set_str_where(self.df.clone(), "name", old_name, "name", new_name)?;
        self.save()
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
        self.df = set_f64_where(self.df.clone(), "name", name, &col_name, value)?;
        self.save()
    }

    /// Write the credits/debts dataframe to disk.
    pub fn save(&self) -> Result<()> {
        write_parquet(&self.df, &self.path)
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
    ]))
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
    /// Construct for `year`, loading from disk or initialising empty.
    pub fn new(year: i32) -> Result<Self> {
        let path = get_year_summary_path(year, RECURRING_EXPENSES_FILENAME)?;
        let df = if path.exists() {
            read_parquet(&path)?
        } else {
            empty_recurring_df()
        };
        Ok(Self { year, path, df })
    }

    /// Add a recurring expense definition (day must be 1..=28), then save.
    pub fn add(
        &mut self,
        expense_name: &str,
        expense_day: i64,
        expense_amount: f64,
        currency: &str,
        primary_category: &str,
        secondary_category: &str,
    ) -> Result<()> {
        if !(1..=28).contains(&expense_day) {
            return Err(Error::InvalidArgument(format!(
                "expense_day must be between 1 and 28, got {expense_day}"
            )));
        }
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
        ])?;
        self.df = concat_df_diagonal(&[self.df.clone(), new_row])?;
        self.save()
    }

    /// Remove a recurring expense by row index, then save.
    pub fn remove(&mut self, index: u32) -> Result<()> {
        self.df = self
            .df
            .clone()
            .lazy()
            .with_row_index("_idx", None)
            .filter(col("_idx").neq(lit(index)))
            .drop(cols(["_idx"]))
            .collect()?;
        self.save()
    }

    /// Insert all recurring definitions into `de`'s month.
    ///
    /// Skips entries that already exist (matched by `expense_name` and the day
    /// component of `expense_date`). Returns the names that were added.
    pub fn apply_to_month(&self, de: &mut DetailedExpenses) -> Result<Vec<String>> {
        let mut added: Vec<String> = Vec::new();

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

        for i in 0..names.len() {
            let name = &names[i];
            let day = days[i];

            // Check for duplicate: same name and same day-of-month already present.
            let existing = de
                .expense_df
                .clone()
                .lazy()
                .filter(
                    col("expense_name")
                        .eq(lit(name.as_str()))
                        .and(col("expense_date").dt().day().eq(lit(day as i8))),
                )
                .collect()?;
            if existing.height() > 0 {
                continue;
            }

            de.add_row(
                name,
                day as u32,
                amounts[i],
                Some(&primaries[i]),
                &currencies[i],
                Some(&secondaries[i]),
            )?;
            added.push(name.clone());
        }
        Ok(added)
    }

    /// Write the recurring expenses dataframe to disk.
    pub fn save(&self) -> Result<()> {
        write_parquet(&self.df, &self.path)
    }
}
