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
//! these ports replicate that behavior rather than Rust's `f64::round`
//! (which rounds half away from zero). This matters for values landing exactly
//! on a `.5` boundary.
//!
//! This is a deliberate reproduction of `round(...)` calls in the original
//! Python `plots.py` at `/home/anferrar/Projects/finguard`, not an
//! independent design choice: charts rendered from this module must show the
//! same numbers a user would see in the Python app for the same data.
//!
//! [iced]: https://github.com/iced-rs/iced

use polars::prelude::*;

use crate::df_operations::{CreditsDebts, InvestmentHoldings, Liquidity};
use crate::error::Result;
use crate::fx::MonthlyRates;

// ======================================================================
// Constants / helpers
// ======================================================================

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

/// Round `value` to two decimal places using **round half to even**, matching
/// Python's `round(value, 2)`.
///
/// Scaling by 100 before rounding changes which side of a halfway point the
/// binary float lands on, diverging from CPython's `round(x, 2)` (e.g. `2.675`
/// → Rust `2.68` vs Python `2.67`, because `2.675` is really `2.67499…`).
/// This implementation uses Rust's `{:.2}` formatting, which rounds the *true*
/// double value to nearest with ties-to-even, matching CPython; format then
/// parse back. Formatting a finite `f64` with `{:.2}` always yields a string
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

/// Sum a numeric month column, converting each row from its own `currency`
/// column into the reference currency via `rates` before adding it in.
///
/// Net-worth rows are not all in the same currency (a holding, a liquidity
/// asset, or a credit/debt can each carry its own), so summing raw values
/// directly would add unlike units together. Returns `0.0` for an empty `df`.
///
/// A row whose currency is listed in `unavailable_currencies` is dropped from
/// the sum instead of being looked up in `rates`: that list names currencies
/// the caller already knows it could not resolve, so this is an intentional
/// exclusion, not the "missing from `rates`" bug case, which still fails via
/// `rates.rate`'s own error below.
fn sum_col_converted(
    df: &DataFrame,
    col_name: &str,
    month: u32,
    rates: &MonthlyRates,
    unavailable_currencies: &[String],
) -> Result<f64> {
    if df.height() == 0 {
        return Ok(0.0);
    }
    let currencies = str_col(df, "currency")?;
    let values = f64_col(df, col_name)?;
    let mut total = 0.0;
    for (currency, value) in currencies.iter().zip(values.iter()) {
        if unavailable_currencies
            .iter()
            .any(|c| c.eq_ignore_ascii_case(currency.trim()))
        {
            continue;
        }
        total += value * rates.rate(month, currency)?;
    }
    Ok(total)
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
/// Every row is converted into the reference currency via `rates` (see
/// [`crate::fx::monthly_rates`]) before being summed, using its own
/// `currency` column. A row whose currency is listed in
/// `unavailable_currencies` is excluded from every total instead of being
/// summed at all; a currency present in the data but missing from `rates`
/// for any other reason is still an error rather than a silent skip.
///
/// Returns `Ok(None)` only when no slice qualifies *and* `unavailable_currencies`
/// is empty. A non-empty `unavailable_currencies` with no qualifying slice
/// still returns `Ok(Some(PieChart { slices: vec![] }))`, so a caller can
/// tell "nothing to show because these currencies could not be converted"
/// apart from "this month genuinely has no data".
pub fn networth_allocation_pie(
    year: i32,
    month: u32,
    rates: &MonthlyRates,
    unavailable_currencies: &[String],
) -> Result<Option<PieChart>> {
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
            sum_col_converted(&cat_df, &col_name, month, rates, unavailable_currencies)?
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
        sum_col_converted(&liq.df, &col_name, month, rates, unavailable_currencies)?
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
        sum_col_converted(&cd.df, &col_name, month, rates, unavailable_currencies)?
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

    if slices.is_empty() && unavailable_currencies.is_empty() {
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
/// Every row is converted into the reference currency via `rates` (see
/// [`crate::fx::monthly_rates`]) before being summed, using its own
/// `currency` column. A row whose currency is listed in
/// `unavailable_currencies` is excluded from every component series and the
/// total instead of being summed at all; a currency present in the data but
/// missing from `rates` for any other reason is still an error rather than a
/// silent skip.
///
/// Returns `Ok(None)` only when every net-worth value is zero *and*
/// `unavailable_currencies` is empty. A non-empty `unavailable_currencies`
/// with an all-zero total still returns `Ok(Some(...))`, with `months` fully
/// populated (so a chart has an axis to render) and every component zero, so
/// a caller can tell "nothing to show because these currencies could not be
/// converted" apart from "this year genuinely has no data".
pub fn networth_evolution_line(
    year: i32,
    rates: &MonthlyRates,
    unavailable_currencies: &[String],
) -> Result<Option<NetworthEvolution>> {
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
            .enumerate()
            .map(|(i, c)| -> Result<f64> {
                Ok(if has_rows {
                    round2_half_even(sum_col_converted(
                        &cat_df,
                        c,
                        (i + 1) as u32,
                        rates,
                        unavailable_currencies,
                    )?)
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
        .enumerate()
        .map(|(i, c)| -> Result<f64> {
            Ok(if liq_has {
                round2_half_even(sum_col_converted(
                    &liq.df,
                    c,
                    (i + 1) as u32,
                    rates,
                    unavailable_currencies,
                )?)
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
        .enumerate()
        .map(|(i, c)| -> Result<f64> {
            Ok(if cd_has {
                round2_half_even(sum_col_converted(
                    &cd.df,
                    c,
                    (i + 1) as u32,
                    rates,
                    unavailable_currencies,
                )?)
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

    if net_worth.iter().all(|&v| v == 0.0) && unavailable_currencies.is_empty() {
        return Ok(None);
    }

    Ok(Some(NetworthEvolution {
        months,
        components,
        net_worth,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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

    /// Points `XDG_DATA_HOME`, `XDG_CONFIG_HOME` and `HOME` at a fresh temp
    /// dir, so a test's parquet writes and config reads never touch real user
    /// data. Matches `df_operations.rs`'s `with_temp_data_home`.
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

    /// Build a one-asset, one-month net worth: an investment, a liquidity
    /// balance, and a credit/debt entry, each in `currency`, for `year`/month
    /// `01`. Shared by the regression test and the mixed-currency test below,
    /// which each seed it with a different `currency` per asset.
    fn seed_single_month_networth(year: i32, currency: &str, suffix: &str) {
        let mut inv = InvestmentHoldings::new(year).expect("load investments");
        let asset = format!("Asset{suffix}");
        inv.add_asset(&asset, "Stocks/ETF", "", currency)
            .expect("add investment asset");
        inv.set_quantity(&asset, 1, 10.0).expect("set quantity");
        inv.set_price(&asset, 1, 50.0).expect("set price");

        let mut liq = Liquidity::new(year).expect("load liquidity");
        let cash = format!("Cash{suffix}");
        liq.add_asset(&cash, "Bank/Broker account", currency)
            .expect("add liquidity asset");
        liq.set_value(&cash, 1, 1000.0)
            .expect("set liquidity value");

        let mut cd = CreditsDebts::new(year).expect("load credits/debts");
        let loan = format!("Loan{suffix}");
        cd.add_entry(&loan, currency)
            .expect("add credit/debt entry");
        cd.set_value(&loan, 1, -200.0)
            .expect("set credit/debt value");
    }

    /// A euro-only user (every row's `currency` is the reference currency)
    /// must see exactly the totals the pre-conversion code produced: `500.0`
    /// invested (`10 * 50`), `1000.0` liquid, `-200.0` credits/debts, net
    /// worth `1300.0`. This is the regression this feature could most easily
    /// introduce without a test to catch it.
    #[test]
    #[serial_test::serial]
    fn euro_only_networth_is_unchanged_by_currency_conversion() {
        let _temp = with_temp_data_home();
        let year = 2026;
        seed_single_month_networth(year, "EUR", "");
        // No currency needs resolving, so an empty rate table is enough.
        let rates = MonthlyRates::for_test("EUR", BTreeMap::new());

        let evolution = networth_evolution_line(year, &rates, &[])
            .expect("compute evolution")
            .expect("non-zero net worth");
        let component = |name: &str| {
            evolution
                .components
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("no '{name}' component"))
        };
        assert_eq!(component("Stocks/ETF").values[0], 500.0);
        assert_eq!(component("Liquidity").values[0], 1000.0);
        assert_eq!(component("Credits/Debts").values[0], -200.0);
        assert_eq!(evolution.net_worth[0], 1300.0);

        let pie = networth_allocation_pie(year, 1, &rates, &[])
            .expect("compute allocation")
            .expect("non-empty allocation");
        assert_eq!(
            pie.slices,
            vec![
                PieSlice {
                    name: "Stocks/ETF".to_string(),
                    value: 500.0
                },
                PieSlice {
                    name: "Liquidity".to_string(),
                    value: 1000.0
                },
                PieSlice {
                    name: "Debts".to_string(),
                    value: 200.0
                },
            ]
        );
    }

    /// A liquidity balance split across two currencies must be converted
    /// through each row's own rate before summing, not added as if both were
    /// the reference currency. `1000` EUR plus `100` USD at `0.90` is `1090`,
    /// not `1100`; the wrong total is exactly the bug this feature removes.
    #[test]
    #[serial_test::serial]
    fn mixed_currency_liquidity_is_converted_before_summing() {
        let _temp = with_temp_data_home();
        let year = 2026;

        let mut liq = Liquidity::new(year).expect("load liquidity");
        liq.add_asset("EuroCash", "Bank/Broker account", "EUR")
            .expect("add EUR asset");
        liq.set_value("EuroCash", 1, 1000.0).expect("set EUR value");
        liq.add_asset("DollarCash", "Bank/Broker account", "USD")
            .expect("add USD asset");
        liq.set_value("DollarCash", 1, 100.0)
            .expect("set USD value");

        // A real `monthly_rates` table always covers every month of the
        // year for a currency it resolves at all (see `fx.rs`), since the
        // USD row's other, zero-valued months still need a rate to convert.
        let rates_by_month: BTreeMap<u32, BTreeMap<String, f64>> = (1..=12u32)
            .map(|month| {
                let mut month_rates = BTreeMap::new();
                month_rates.insert("USD".to_string(), 0.90);
                (month, month_rates)
            })
            .collect();
        let rates = MonthlyRates::for_test("EUR", rates_by_month);

        let evolution = networth_evolution_line(year, &rates, &[])
            .expect("compute evolution")
            .expect("non-zero net worth");
        let liquidity = evolution
            .components
            .iter()
            .find(|c| c.name == "Liquidity")
            .expect("liquidity component");
        assert_eq!(liquidity.values[0], 1090.0);

        let pie = networth_allocation_pie(year, 1, &rates, &[])
            .expect("compute allocation")
            .expect("non-empty allocation");
        let liquidity_slice = pie
            .slices
            .iter()
            .find(|s| s.name == "Liquidity")
            .expect("liquidity slice");
        assert_eq!(liquidity_slice.value, 1090.0);
    }

    /// A currency present in the data but absent from the rate table, and not
    /// named in `unavailable_currencies`, is an error, not a silent `1.0`:
    /// that silent fallback is the exact bug this feature exists to remove.
    #[test]
    #[serial_test::serial]
    fn unresolved_currency_is_an_error_not_a_silent_identity() {
        let _temp = with_temp_data_home();
        let year = 2026;
        seed_single_month_networth(year, "USD", "");
        // No USD rate was ever added to this table.
        let rates = MonthlyRates::for_test("EUR", BTreeMap::new());

        let err = networth_evolution_line(year, &rates, &[]).expect_err("USD was never resolved");
        assert!(matches!(err, crate::error::Error::NotFound(_)));
    }

    /// A currency named in `unavailable_currencies` is dropped from every
    /// total instead of erroring: the USD holding and USD liquidity balance
    /// disappear from the sums, leaving only the EUR credit/debt entry.
    #[test]
    #[serial_test::serial]
    fn unavailable_currency_rows_are_excluded_from_every_total() {
        let _temp = with_temp_data_home();
        let year = 2026;
        seed_single_month_networth(year, "USD", "Usd");
        seed_single_month_networth(year, "EUR", "Eur");
        // No USD rate is resolved; the caller reports it as unavailable
        // instead of leaving it out of the table by mistake.
        let rates = MonthlyRates::for_test("EUR", BTreeMap::new());
        let unavailable = vec!["USD".to_string()];

        let evolution = networth_evolution_line(year, &rates, &unavailable)
            .expect("USD rows are excluded, not looked up")
            .expect("the EUR rows still produce a non-zero net worth");
        let component = |name: &str| {
            evolution
                .components
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("no '{name}' component"))
        };
        assert_eq!(component("Stocks/ETF").values[0], 500.0);
        assert_eq!(component("Liquidity").values[0], 1000.0);
        assert_eq!(component("Credits/Debts").values[0], -200.0);
        assert_eq!(evolution.net_worth[0], 1300.0);

        let pie = networth_allocation_pie(year, 1, &rates, &unavailable)
            .expect("USD rows are excluded, not looked up")
            .expect("non-empty allocation");
        assert_eq!(
            pie.slices,
            vec![
                PieSlice {
                    name: "Stocks/ETF".to_string(),
                    value: 500.0
                },
                PieSlice {
                    name: "Liquidity".to_string(),
                    value: 1000.0
                },
                PieSlice {
                    name: "Debts".to_string(),
                    value: 200.0
                },
            ]
        );
    }

    /// When every row's currency is unavailable, the total goes to zero, but
    /// that must not collapse into `Ok(None)`: a caller needs to see the
    /// zeroed-out result and the populated `unavailable_currencies` list to
    /// tell "nothing to show because these currencies could not be
    /// converted" apart from a genuinely empty year. `months` stays fully
    /// populated so a chart still has an axis to render.
    #[test]
    #[serial_test::serial]
    fn all_rows_unavailable_still_returns_zeroed_data_not_none() {
        let _temp = with_temp_data_home();
        let year = 2026;
        seed_single_month_networth(year, "USD", "");
        let rates = MonthlyRates::for_test("EUR", BTreeMap::new());
        let unavailable = vec!["USD".to_string()];

        let evolution = networth_evolution_line(year, &rates, &unavailable)
            .expect("USD rows are excluded, not looked up")
            .expect("must not report as 'no data'");
        assert_eq!(evolution.months.len(), 12);
        assert!(evolution.components.iter().all(|c| c.values[0] == 0.0));
        assert_eq!(evolution.net_worth[0], 0.0);

        let pie = networth_allocation_pie(year, 1, &rates, &unavailable)
            .expect("USD rows are excluded, not looked up")
            .expect("must not report as 'no data'");
        assert!(pie.slices.is_empty());
    }
}
