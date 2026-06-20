//! Chart **data preparation**, ported from the original Python `plots.py`.
//!
//! The Python functions returned Apache ECharts option dictionaries that mixed
//! the underlying chart data with a lot of presentation config (colors, axes,
//! tooltips, legends, titles). Our UI is native ([iced]), not ECharts, so this
//! module keeps **only the data** and discards every styling concern.
//!
//! Each function returns `Result<Option<...>>`, returning `Ok(None)` in exactly
//! the same situations where the Python original returned `None`.
//!
//! # Rounding fidelity
//!
//! Python's built-in `round` uses *round half to even* (banker's rounding), so
//! these ports replicate that with [`round_half_even`] / [`round2_half_even`]
//! rather than Rust's `f64::round` (which rounds half away from zero). This
//! matters for values landing exactly on a `.5` boundary.
//!
//! [iced]: https://github.com/iced-rs/iced

use polars::prelude::*;

use crate::df_operations::{
    Cashflow, CreditsDebts, DetailedExpenses, InvestmentHoldings, Liquidity,
};
use crate::error::Result;
use crate::paths::{PRIMARIES_FILENAME, SECONDARIES_FILENAME, get_year_summary_path};

// ======================================================================
// Constants / helpers
// ======================================================================

/// Income categories (mirrors `df_operations::INCOME_CATEGORIES`, which is
/// private). Used by [`income_pie_chart`].
const INCOME_CATEGORIES: &[&str] = &[
    "Salary",
    "Interests Bank account",
    "Dividendi e Cedole",
    "Other",
];

/// Investment categories (mirrors `df_operations::INVESTMENT_CATEGORIES`, which
/// is private). Used by the net-worth charts.
const INVESTMENT_CATEGORIES: &[&str] = &["Stocks/ETF", "Commodities", "Bonds"];

/// Abbreviated English month names, 1-indexed to match Python's
/// `calendar.month_abbr` (index 0 is the empty string).
const MONTH_ABBR: [&str; 13] = [
    "", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Return the abbreviated month name for `month` (1–12), matching
/// `calendar.month_abbr[month]`. Out-of-range months yield an empty string.
fn month_abbr(month: usize) -> String {
    MONTH_ABBR.get(month).copied().unwrap_or("").to_string()
}

/// Round `value` to the nearest integer using **round half to even** (banker's
/// rounding), matching Python's `round(value)`.
///
/// The result is an integer-valued `f64` so it can be compared exactly in tests.
fn round_half_even(value: f64) -> f64 {
    let floor = value.floor();
    let diff = value - floor;
    if diff < 0.5 {
        floor
    } else if diff > 0.5 {
        floor + 1.0
    } else {
        // Exactly halfway: round to the even neighbour.
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    }
}

/// Round `value` to two decimal places using **round half to even**, matching
/// Python's `round(value, 2)`.
///
/// Scaling by 100 before rounding (`round_half_even(value * 100.0) / 100.0`)
/// changes which side of a halfway point the binary float lands on, diverging
/// from CPython's `round(x, 2)` (e.g. `2.675` → Rust `2.68` vs Python `2.67`,
/// because `2.675` is really `2.67499…`). Rust's `{:.2}` formatting rounds the
/// *true* double value to nearest with ties-to-even, matching CPython; we format
/// then parse back. Formatting a finite `f64` with `{:.2}` always yields a string
/// that parses back to an `f64`, so the parse is infallible.
fn round2_half_even(value: f64) -> f64 {
    format!("{value:.2}").parse::<f64>().unwrap()
}

// ======================================================================
// Result structs (no colors, no styling)
// ======================================================================

/// A single slice of a pie chart.
#[derive(Debug, Clone, PartialEq)]
pub struct PieSlice {
    /// Slice label.
    pub name: String,
    /// Slice value.
    pub value: f64,
}

/// A pie chart: an ordered list of slices.
#[derive(Debug, Clone, PartialEq)]
pub struct PieChart {
    /// The slices, in insertion order.
    pub slices: Vec<PieSlice>,
}

/// A named data series (one line / one bar group).
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    /// Series label.
    pub name: String,
    /// Series values, aligned with the chart's category / x-axis labels.
    pub values: Vec<f64>,
}

/// A grouped bar chart.
#[derive(Debug, Clone, PartialEq)]
pub struct BarChart {
    /// X-axis category labels.
    pub categories: Vec<String>,
    /// One series per group; each series' values align with `categories`.
    pub series: Vec<Series>,
}

/// A line chart.
#[derive(Debug, Clone, PartialEq)]
pub struct LineChart {
    /// X-axis labels.
    pub x_labels: Vec<String>,
    /// One series per line; each series' values align with `x_labels`.
    pub series: Vec<Series>,
}

/// Net-worth evolution: stacked components plus a bold total line.
#[derive(Debug, Clone, PartialEq)]
pub struct NetworthEvolution {
    /// Month labels (abbreviated names), one per month.
    pub months: Vec<String>,
    /// Stacked component series (investment categories, Liquidity,
    /// Credits/Debts); each series' values align with `months`.
    pub components: Vec<Series>,
    /// Total net worth per month: the sum of all components at each index.
    pub net_worth: Vec<f64>,
}

// ======================================================================
// Internal parquet helper
// ======================================================================

/// Read a parquet file eagerly. (`df_operations`'s reader is private, so plots
/// reads summary files directly, exactly as the Python code did.)
fn read_parquet(path: &std::path::Path) -> Result<DataFrame> {
    let file = std::fs::File::open(path)?;
    Ok(ParquetReader::new(file).finish()?)
}

/// Return whether `df` has a column named `name`.
fn has_column(df: &DataFrame, name: &str) -> bool {
    df.get_column_names().iter().any(|c| c.as_str() == name)
}

/// Read a string column as owned `String`s (nulls become empty).
fn str_col(df: &DataFrame, name: &str) -> Result<Vec<String>> {
    Ok(df
        .column(name)?
        .str()?
        .iter()
        .map(|o| o.unwrap_or("").to_string())
        .collect())
}

/// Read a float column as `f64`s (nulls become `0.0`).
fn f64_col(df: &DataFrame, name: &str) -> Result<Vec<f64>> {
    Ok(df
        .column(name)?
        .f64()?
        .iter()
        .map(|o| o.unwrap_or(0.0))
        .collect())
}

/// Sum a numeric month column, returning `0.0` for a null/empty sum.
fn sum_col(df: &DataFrame, name: &str) -> Result<f64> {
    Ok(df.column(name)?.f64()?.sum().unwrap_or(0.0))
}

/// Resolve the summary filename and category column for a `kind`.
///
/// Matches the Python convention: any `kind` other than `"primary"` is treated
/// as `"secondary"`.
fn kind_parts(kind: &str) -> (&'static str, String) {
    if kind == "primary" {
        (PRIMARIES_FILENAME, "primary_category".to_string())
    } else {
        (SECONDARIES_FILENAME, "secondary_category".to_string())
    }
}

/// Return the month columns (`YYYY-MM`) of a summary dataframe, in dataframe
/// (insertion) order. A column qualifies when it contains `-` and the part
/// after the first `-` is all digits, matching Python's detection logic.
fn month_columns(df: &DataFrame) -> Vec<String> {
    df.get_column_names()
        .iter()
        .map(|c| c.to_string())
        .filter(|c| {
            if let Some((_, rest)) = c.split_once('-') {
                !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit())
            } else {
                false
            }
        })
        .collect()
}

// ======================================================================
// Expense charts
// ======================================================================

/// Bar chart comparing per-category expenses across up to a few months.
///
/// X-axis = the category column (the synthetic `"Total"` row dropped). One
/// [`Series`] per `YYYY-MM` month — formed from `year` and each entry of
/// `months` — that exists as a column, named by its abbreviated month name.
/// Values use integer (round-half-even) rounding.
///
/// Returns `Ok(None)` when the summary file is absent, has no non-`Total` rows,
/// or none of the requested months are present as columns.
pub fn monthly_expenses_comparison(
    year: i32,
    months: &[u32],
    kind: &str,
) -> Result<Option<BarChart>> {
    let (filename, cat_col) = kind_parts(kind);
    let path = get_year_summary_path(year, filename)?;
    if !path.exists() {
        return Ok(None);
    }

    let df = read_parquet(&path)?;
    let df = df
        .lazy()
        .filter(col(cat_col.as_str()).neq(lit("Total")))
        .collect()?;
    if df.height() == 0 {
        return Ok(None);
    }

    // Requested month columns, keeping only those that exist (in `months` order).
    let available: Vec<(u32, String)> = months
        .iter()
        .map(|m| (*m, format!("{year}-{m:02}")))
        .filter(|(_, label)| has_column(&df, label))
        .collect();
    if available.is_empty() {
        return Ok(None);
    }

    let categories = str_col(&df, &cat_col)?;

    let mut series = Vec::with_capacity(available.len());
    for (m, label) in &available {
        let values: Vec<f64> = f64_col(&df, label)?
            .into_iter()
            .map(round_half_even)
            .collect();
        series.push(Series {
            name: month_abbr(*m as usize),
            values,
        });
    }

    Ok(Some(BarChart { categories, series }))
}

/// Line chart of up to three categories' expenses over the year's months.
///
/// X-axis = abbreviated names of the sorted `YYYY-MM` columns present. One
/// [`Series`] per category among the first three of `selected_categories`; a
/// category that is absent from the table contributes a series of zeros. Values
/// use integer (round-half-even) rounding.
///
/// Returns `Ok(None)` when the summary file is absent, has no non-`Total` rows,
/// or contains no month columns.
pub fn category_expenses_over_months(
    year: i32,
    selected_categories: &[String],
    kind: &str,
) -> Result<Option<LineChart>> {
    let (filename, cat_col) = kind_parts(kind);
    let path = get_year_summary_path(year, filename)?;
    if !path.exists() {
        return Ok(None);
    }

    let df = read_parquet(&path)?;
    let df = df
        .lazy()
        .filter(col(cat_col.as_str()).neq(lit("Total")))
        .collect()?;
    if df.height() == 0 {
        return Ok(None);
    }

    let mut month_cols = month_columns(&df);
    month_cols.sort();
    if month_cols.is_empty() {
        return Ok(None);
    }

    let x_labels: Vec<String> = month_cols
        .iter()
        .map(|c| {
            let m: usize = c.split_once('-').unwrap().1.parse().unwrap_or(0);
            month_abbr(m)
        })
        .collect();

    let categories = str_col(&df, &cat_col)?;

    let mut series = Vec::new();
    for cat in selected_categories.iter().take(3) {
        let row_idx = categories.iter().position(|c| c == cat);
        let values: Vec<f64> = match row_idx {
            None => vec![0.0; month_cols.len()],
            Some(i) => month_cols
                .iter()
                .map(|c| -> Result<f64> { Ok(round_half_even(f64_col(&df, c)?[i])) })
                .collect::<Result<Vec<f64>>>()?,
        };
        series.push(Series {
            name: cat.clone(),
            values,
        });
    }

    Ok(Some(LineChart { x_labels, series }))
}

/// Pie chart of cumulative yearly expenses per category.
///
/// One slice per category whose name is non-empty and whose total across all
/// month columns is `> 0`. Values use integer (round-half-even) rounding.
///
/// Returns `Ok(None)` when the summary file is absent, has no non-`Total` rows,
/// has no month columns, or yields no qualifying slices.
pub fn cumulative_expenses_pie(year: i32, kind: &str) -> Result<Option<PieChart>> {
    let (filename, cat_col) = kind_parts(kind);
    let path = get_year_summary_path(year, filename)?;
    if !path.exists() {
        return Ok(None);
    }

    let df = read_parquet(&path)?;
    let df = df
        .lazy()
        .filter(col(cat_col.as_str()).neq(lit("Total")))
        .collect()?;
    if df.height() == 0 {
        return Ok(None);
    }

    let month_cols = month_columns(&df);
    if month_cols.is_empty() {
        return Ok(None);
    }

    let categories = str_col(&df, &cat_col)?;
    let month_data: Vec<Vec<f64>> = month_cols
        .iter()
        .map(|c| f64_col(&df, c))
        .collect::<Result<_>>()?;

    let mut slices = Vec::new();
    for (i, cat) in categories.iter().enumerate() {
        let total: f64 = month_data.iter().map(|col_vals| col_vals[i]).sum();
        if !cat.is_empty() && total > 0.0 {
            slices.push(PieSlice {
                name: cat.clone(),
                value: round_half_even(total),
            });
        }
    }
    if slices.is_empty() {
        return Ok(None);
    }

    Ok(Some(PieChart { slices }))
}

// ======================================================================
// Cashflow charts
// ======================================================================

/// Grouped bar chart of Income / Spending / Saving per month.
///
/// Recomputes the [`Cashflow`] for `year`, then builds 12 month categories with
/// three series. Values use two-decimal (round-half-even) rounding.
///
/// Returns `Ok(None)` when every income and spending value is zero.
pub fn cashflow_bar_chart(year: i32) -> Result<Option<BarChart>> {
    let mut cf = Cashflow::new(year)?;
    cf.recompute()?;

    let categories: Vec<String> = (1..=12).map(|m| month_abbr(m as usize)).collect();

    let collect_row = |row: &str| -> Result<Vec<f64>> {
        (1..=12)
            .map(|m| -> Result<f64> {
                Ok(round2_half_even(cf.get_value(row, &format!("{m:02}"))?))
            })
            .collect()
    };

    let income = collect_row("Income")?;
    let spending = collect_row("Spending")?;
    let saving = collect_row("Saving")?;

    if income.iter().chain(spending.iter()).all(|&v| v == 0.0) {
        return Ok(None);
    }

    Ok(Some(BarChart {
        categories,
        series: vec![
            Series {
                name: "Income".to_string(),
                values: income,
            },
            Series {
                name: "Spending".to_string(),
                values: spending,
            },
            Series {
                name: "Saving".to_string(),
                values: saving,
            },
        ],
    }))
}

/// Pie chart of yearly income by category.
///
/// Recomputes the [`Cashflow`] for `year`, then emits one slice per income
/// category whose yearly total is `> 0`. Values use two-decimal
/// (round-half-even) rounding.
///
/// Returns `Ok(None)` when no income category has a positive total.
pub fn income_pie_chart(year: i32) -> Result<Option<PieChart>> {
    let mut cf = Cashflow::new(year)?;
    cf.recompute()?;

    let mut slices = Vec::new();
    for cat in INCOME_CATEGORIES {
        let mut total = 0.0;
        for m in 1..=12 {
            total += cf.get_value(cat, &format!("{m:02}"))?;
        }
        if total > 0.0 {
            slices.push(PieSlice {
                name: (*cat).to_string(),
                value: round2_half_even(total),
            });
        }
    }
    if slices.is_empty() {
        return Ok(None);
    }

    Ok(Some(PieChart { slices }))
}

// ======================================================================
// Net-worth charts
// ======================================================================

/// Pie chart of net-worth allocation for a single month.
///
/// Slices, in order: each investment category total (from
/// [`InvestmentHoldings::df_value`]), then `"Liquidity"`, then `"Credits"` or
/// `"Debts"` (a positive credits/debts month sum yields `"Credits"`; a negative
/// sum yields `"Debts"` with the absolute value). Only `> 0` slices are kept.
/// Values use two-decimal (round-half-even) rounding.
///
/// Returns `Ok(None)` when no slice qualifies.
pub fn networth_allocation_pie(year: i32, month: u32) -> Result<Option<PieChart>> {
    let inv = InvestmentHoldings::new(year)?;
    let liq = Liquidity::new(year)?;
    let cd = CreditsDebts::new(year)?;
    let col_name = format!("{month:02}");

    let df_value = inv.df_value()?;

    let mut slices = Vec::new();
    for cat in INVESTMENT_CATEGORIES {
        let cat_df = df_value
            .clone()
            .lazy()
            .filter(col("category").eq(lit(*cat)))
            .collect()?;
        let val = if cat_df.height() > 0 {
            sum_col(&cat_df, &col_name)?
        } else {
            0.0
        };
        if val > 0.0 {
            slices.push(PieSlice {
                name: (*cat).to_string(),
                value: round2_half_even(val),
            });
        }
    }

    let liq_val = if liq.df.height() > 0 {
        sum_col(&liq.df, &col_name)?
    } else {
        0.0
    };
    if liq_val > 0.0 {
        slices.push(PieSlice {
            name: "Liquidity".to_string(),
            value: round2_half_even(liq_val),
        });
    }

    let cd_val = if cd.df.height() > 0 {
        sum_col(&cd.df, &col_name)?
    } else {
        0.0
    };
    if cd_val > 0.0 {
        slices.push(PieSlice {
            name: "Credits".to_string(),
            value: round2_half_even(cd_val),
        });
    } else if cd_val < 0.0 {
        slices.push(PieSlice {
            name: "Debts".to_string(),
            value: round2_half_even(cd_val.abs()),
        });
    }

    if slices.is_empty() {
        return Ok(None);
    }

    Ok(Some(PieChart { slices }))
}

/// Net-worth evolution over the year: stacked components plus a total line.
///
/// Components, in order: each investment category, then `"Liquidity"`, then
/// `"Credits/Debts"`. Each component series holds the monthly column sums (two
/// decimals, round-half-even). `net_worth[i]` is the sum of all components at
/// index `i`.
///
/// Returns `Ok(None)` when every net-worth value is zero.
pub fn networth_evolution_line(year: i32) -> Result<Option<NetworthEvolution>> {
    let inv = InvestmentHoldings::new(year)?;
    let liq = Liquidity::new(year)?;
    let cd = CreditsDebts::new(year)?;

    let mcols: Vec<String> = (1..=12).map(|m| format!("{m:02}")).collect();
    let months: Vec<String> = (1..=12).map(|m| month_abbr(m as usize)).collect();

    let df_value = inv.df_value()?;

    let mut components: Vec<Series> = Vec::new();

    for cat in INVESTMENT_CATEGORIES {
        let cat_df = df_value
            .clone()
            .lazy()
            .filter(col("category").eq(lit(*cat)))
            .collect()?;
        let has_rows = cat_df.height() > 0;
        let vals: Vec<f64> = mcols
            .iter()
            .map(|c| -> Result<f64> {
                Ok(if has_rows {
                    round2_half_even(sum_col(&cat_df, c)?)
                } else {
                    0.0
                })
            })
            .collect::<Result<_>>()?;
        components.push(Series {
            name: (*cat).to_string(),
            values: vals,
        });
    }

    let liq_has = liq.df.height() > 0;
    let liq_vals: Vec<f64> = mcols
        .iter()
        .map(|c| -> Result<f64> {
            Ok(if liq_has {
                round2_half_even(sum_col(&liq.df, c)?)
            } else {
                0.0
            })
        })
        .collect::<Result<_>>()?;
    components.push(Series {
        name: "Liquidity".to_string(),
        values: liq_vals,
    });

    let cd_has = cd.df.height() > 0;
    let cd_vals: Vec<f64> = mcols
        .iter()
        .map(|c| -> Result<f64> {
            Ok(if cd_has {
                round2_half_even(sum_col(&cd.df, c)?)
            } else {
                0.0
            })
        })
        .collect::<Result<_>>()?;
    components.push(Series {
        name: "Credits/Debts".to_string(),
        values: cd_vals,
    });

    let net_worth: Vec<f64> = (0..12)
        .map(|i| components.iter().map(|c| c.values[i]).sum())
        .collect();

    if net_worth.iter().all(|&v| v == 0.0) {
        return Ok(None);
    }

    Ok(Some(NetworthEvolution {
        months,
        components,
        net_worth,
    }))
}

// ======================================================================
// Monthly expenses pie (from a DetailedExpenses month)
// ======================================================================

/// Pie chart of a single month's expenses by category.
///
/// Uses [`DetailedExpenses::create_expenses_summary_table`] for the relevant
/// category column. One slice per row whose label is non-empty and whose total
/// is `> 0`, with integer (round-half-even) rounding.
///
/// Note: matching the Python original, an empty summary table returns
/// `Ok(None)`, but a non-empty table with no qualifying slices returns
/// `Ok(Some(PieChart { slices: vec![] }))`.
pub fn monthly_expenses_pie(de: &DetailedExpenses, kind: &str) -> Result<Option<PieChart>> {
    let cat_col = if kind == "primary" {
        "primary_category"
    } else {
        "secondary_category"
    };
    let df = de.create_expenses_summary_table(cat_col)?;
    if df.height() == 0 {
        return Ok(None);
    }

    let labels = str_col(&df, cat_col)?;
    let values = f64_col(&df, "total_expense_in_ref_currency")?;

    let mut slices = Vec::new();
    for (label, val) in labels.iter().zip(values.iter()) {
        if !label.is_empty() && *val > 0.0 {
            slices.push(PieSlice {
                name: label.clone(),
                value: round_half_even(*val),
            });
        }
    }

    Ok(Some(PieChart { slices }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `round2_half_even` must match CPython's `round(x, 2)` (round-half-to-even
    /// on the *true* binary value). Each expected value below was verified
    /// against `python3 -c "print(round(v, 2))"`.
    #[test]
    fn round2_half_even_matches_python() {
        // The headline regressions: scaling by 100 used to push these the wrong
        // way (2.68 / -2.68); Python rounds the true value (2.6749…) down.
        assert_eq!(round2_half_even(2.675), 2.67);
        assert_eq!(round2_half_even(-2.675), -2.67);
        // Other ties / boundary cases.
        assert_eq!(round2_half_even(0.125), 0.12);
        assert_eq!(round2_half_even(2.555), 2.56);
        // Values already at <=2 decimals are unchanged.
        assert_eq!(round2_half_even(2.5), 2.5);
        assert_eq!(round2_half_even(0.5), 0.5);
        assert_eq!(round2_half_even(-0.5), -0.5);
        assert_eq!(round2_half_even(3.5), 3.5);
        // Ordinary rounding.
        assert_eq!(round2_half_even(1.005), 1.0);
        assert_eq!(round2_half_even(12.344), 12.34);
        assert_eq!(round2_half_even(12.346), 12.35);
        assert_eq!(round2_half_even(0.0), 0.0);
    }

    /// The integer `round_half_even` must match CPython's `round(x)` (no ndigits),
    /// including the ties-to-even behaviour for negatives. Verified against
    /// `python3 -c "print(round(v))"`.
    #[test]
    fn round_half_even_matches_python() {
        assert_eq!(round_half_even(2.5), 2.0);
        assert_eq!(round_half_even(0.5), 0.0);
        assert_eq!(round_half_even(-2.5), -2.0);
        assert_eq!(round_half_even(3.5), 4.0);
        assert_eq!(round_half_even(-0.5), 0.0);
        assert_eq!(round_half_even(1.5), 2.0);
    }
}
