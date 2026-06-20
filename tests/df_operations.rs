//! Tests for `finguard_rs::df_operations`.

mod common;

use common::TestEnv;
use finguard_rs::Error;
use finguard_rs::df_operations::{
    Cashflow, CreditsDebts, DetailedExpenses, InvestmentHoldings, Liquidity, RecurringExpenses,
    get_category_totals_across_all_years, normalize_category_value,
    remove_category_from_all_summaries, resolve_category,
};
use finguard_rs::paths::{PRIMARIES_FILENAME, SECONDARIES_FILENAME, get_year_summary_path};
use polars::prelude::*;
use std::collections::HashSet;

// ------------------------------------------------------------------
// Small helpers
// ------------------------------------------------------------------

/// Value at (`category_col == cat`, `month_col`) of a summary frame.
fn cell(df: &DataFrame, cat_col: &str, cat: &str, month_col: &str) -> f64 {
    let row = df
        .clone()
        .lazy()
        .filter(col(cat_col).eq(lit(cat)))
        .collect()
        .unwrap();
    assert!(row.height() > 0, "category '{cat}' not found");
    row.column(month_col)
        .unwrap()
        .f64()
        .unwrap()
        .get(0)
        .unwrap()
}

fn cat_order(df: &DataFrame, cat_col: &str) -> Vec<String> {
    df.column(cat_col)
        .unwrap()
        .str()
        .unwrap()
        .iter()
        .map(|o| o.unwrap_or("").to_string())
        .collect()
}

// ------------------------------------------------------------------
// Free functions
// ------------------------------------------------------------------

#[test]
fn normalize_special_cases_and_capitalize() {
    assert_eq!(normalize_category_value("tv"), "TV");
    assert_eq!(normalize_category_value("TV"), "TV");
    assert_eq!(normalize_category_value("otherexpenses"), "OtherExpenses");
    assert_eq!(normalize_category_value("patreon-like"), "Patreon-Like");
    // generic: lower-case then capitalize first.
    assert_eq!(normalize_category_value("groceries"), "Groceries");
    assert_eq!(normalize_category_value("GROCERIES"), "Groceries");
    assert_eq!(normalize_category_value("hOuSiNg"), "Housing");
    assert_eq!(normalize_category_value(""), "");
}

#[test]
fn resolve_category_matches_existing_case_insensitively() {
    let mut existing = HashSet::new();
    existing.insert("Groceries".to_string());
    assert_eq!(resolve_category("groceries", &existing), "Groceries");
    assert_eq!(resolve_category("GROCERIES", &existing), "Groceries");
    // no match -> normalize
    assert_eq!(resolve_category("housing", &existing), "Housing");
    // empty stays empty
    assert_eq!(resolve_category("", &existing), "");
}

// ------------------------------------------------------------------
// DetailedExpenses: schema + round-trip
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn add_row_writes_exact_schema_and_round_trips() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 3).unwrap();
    de.add_row("Pam", 15, 12.5, Some("groceries"), "E", Some("supermarket"))
        .unwrap();

    // Schema: exact column names + dtypes.
    let schema = de.expense_df.schema();
    let expected: Vec<(&str, DataType)> = vec![
        ("expense_name", DataType::String),
        ("expense_date", DataType::Date),
        ("expense_amount", DataType::Float64),
        ("currency", DataType::String),
        ("expense_in_ref_currency", DataType::Float64),
        ("primary_category", DataType::String),
        ("secondary_category", DataType::String),
    ];
    let actual: Vec<(String, DataType)> = schema
        .iter()
        .map(|(n, d)| (n.to_string(), d.clone()))
        .collect();
    let actual_ref: Vec<(&str, DataType)> = actual
        .iter()
        .map(|(n, d)| (n.as_str(), d.clone()))
        .collect();
    assert_eq!(actual_ref, expected);

    // Reload from disk and compare.
    let reloaded = DetailedExpenses::new(2026, 3).unwrap();
    assert_eq!(reloaded.expense_df.shape(), (1, 7));
    assert_eq!(
        reloaded
            .expense_df
            .column("expense_name")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("Pam")
    );
    // Category normalization applied on write.
    assert_eq!(
        reloaded
            .expense_df
            .column("primary_category")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("Groceries")
    );
    // expense_in_ref_currency mirrors amount (1.0 conversion stub).
    assert_eq!(
        reloaded
            .expense_df
            .column("expense_in_ref_currency")
            .unwrap()
            .f64()
            .unwrap()
            .get(0),
        Some(12.5)
    );
    // Date is correct.
    assert_eq!(
        reloaded.expense_df.column("expense_date").unwrap().dtype(),
        &DataType::Date
    );
}

#[test]
#[serial_test::serial]
fn add_row_special_case_category_normalization() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("NowTV", 1, 10.0, Some("housing"), "E", Some("tv"))
        .unwrap();
    assert_eq!(
        de.expense_df
            .column("secondary_category")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("TV")
    );
}

#[test]
#[serial_test::serial]
fn add_row_without_mapping_and_no_primary_errors() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    let err = de.add_row("unknown", 1, 5.0, None, "E", None).unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)));
}

#[test]
#[serial_test::serial]
fn add_row_resolves_category_from_config_mapping() {
    let _env = TestEnv::new();
    finguard_rs::config::add_mapping("netflix", "leisure", "streaming", false).unwrap();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("netflix", 5, 9.99, None, "E", None).unwrap();
    assert_eq!(
        de.expense_df
            .column("primary_category")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("Leisure")
    );
    assert_eq!(
        de.expense_df
            .column("secondary_category")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("Streaming")
    );
}

// ------------------------------------------------------------------
// Summary tables
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn summaries_across_months_have_sorted_columns_and_total_row() {
    let _env = TestEnv::new();

    let mut jan = DetailedExpenses::new(2026, 1).unwrap();
    jan.add_row("rent", 1, 100.0, Some("housing"), "E", Some("rent"))
        .unwrap();
    jan.add_row("food", 2, 50.0, Some("groceries"), "E", Some("super"))
        .unwrap();

    let mut feb = DetailedExpenses::new(2026, 2).unwrap();
    feb.add_row("rent", 1, 110.0, Some("housing"), "E", Some("rent"))
        .unwrap();

    let prim_path = get_year_summary_path(2026, PRIMARIES_FILENAME).unwrap();
    let prim = ParquetReader::new(std::fs::File::open(&prim_path).unwrap())
        .finish()
        .unwrap();

    // Columns: category col + sorted YYYY-MM months.
    let names: Vec<String> = prim
        .get_column_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(names[0], "primary_category");
    assert_eq!(&names[1..], &["2026-01".to_string(), "2026-02".to_string()]);

    // Month columns are Float64.
    assert_eq!(prim.column("2026-01").unwrap().dtype(), &DataType::Float64);

    // Values.
    assert_eq!(cell(&prim, "primary_category", "Housing", "2026-01"), 100.0);
    assert_eq!(
        cell(&prim, "primary_category", "Groceries", "2026-01"),
        50.0
    );
    // Groceries absent in Feb -> backfilled 0.0.
    assert_eq!(cell(&prim, "primary_category", "Groceries", "2026-02"), 0.0);
    assert_eq!(cell(&prim, "primary_category", "Housing", "2026-02"), 110.0);

    // Total row = column sums.
    assert_eq!(cell(&prim, "primary_category", "Total", "2026-01"), 150.0);
    assert_eq!(cell(&prim, "primary_category", "Total", "2026-02"), 110.0);
}

#[test]
#[serial_test::serial]
fn primaries_rows_in_canonical_order_total_last() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    // Add in a scrambled order.
    de.add_row("a", 1, 1.0, Some("groceries"), "E", Some("x"))
        .unwrap();
    de.add_row("b", 1, 1.0, Some("housing"), "E", Some("x"))
        .unwrap();
    de.add_row("c", 1, 1.0, Some("transport"), "E", Some("x"))
        .unwrap();

    let prim_path = get_year_summary_path(2026, PRIMARIES_FILENAME).unwrap();
    let prim = ParquetReader::new(std::fs::File::open(&prim_path).unwrap())
        .finish()
        .unwrap();

    let order = cat_order(&prim, "primary_category");
    // Canonical order: Housing < Groceries < Transport, Total last.
    let h = order.iter().position(|c| c == "Housing").unwrap();
    let g = order.iter().position(|c| c == "Groceries").unwrap();
    let t = order.iter().position(|c| c == "Transport").unwrap();
    assert!(h < g && g < t, "got order {order:?}");
    assert_eq!(order.last().unwrap(), "Total");
}

#[test]
#[serial_test::serial]
fn re_adding_to_existing_month_recomputes_not_duplicates() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("a", 1, 100.0, Some("housing"), "E", Some("x"))
        .unwrap();
    de.add_row("b", 2, 25.0, Some("housing"), "E", Some("x"))
        .unwrap();

    let prim_path = get_year_summary_path(2026, PRIMARIES_FILENAME).unwrap();
    let prim = ParquetReader::new(std::fs::File::open(&prim_path).unwrap())
        .finish()
        .unwrap();

    // Only one "2026-01" column, summed (not duplicated).
    let month_cols: Vec<&str> = prim
        .get_column_names()
        .iter()
        .map(|s| s.as_str())
        .filter(|c| c.starts_with("2026-"))
        .collect();
    assert_eq!(month_cols, vec!["2026-01"]);
    assert_eq!(cell(&prim, "primary_category", "Housing", "2026-01"), 125.0);
}

#[test]
#[serial_test::serial]
fn secondaries_summary_total_row() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("a", 1, 10.0, Some("housing"), "E", Some("rent"))
        .unwrap();
    de.add_row("b", 2, 5.0, Some("groceries"), "E", Some("super"))
        .unwrap();

    let sec_path = get_year_summary_path(2026, SECONDARIES_FILENAME).unwrap();
    let sec = ParquetReader::new(std::fs::File::open(&sec_path).unwrap())
        .finish()
        .unwrap();
    assert_eq!(cell(&sec, "secondary_category", "Rent", "2026-01"), 10.0);
    assert_eq!(cell(&sec, "secondary_category", "Super", "2026-01"), 5.0);
    assert_eq!(cell(&sec, "secondary_category", "Total", "2026-01"), 15.0);
}

// ------------------------------------------------------------------
// edit_row / delete_row
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn edit_row_updates_fields_and_summaries() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("a", 1, 100.0, Some("housing"), "E", Some("x"))
        .unwrap();
    de.add_row("b", 2, 50.0, Some("groceries"), "E", Some("y"))
        .unwrap();

    // Edit row 0: change amount and category.
    de.edit_row(
        0,
        None,
        Some(10),
        Some(200.0),
        None,
        Some("transport"),
        None,
    )
    .unwrap();

    assert_eq!(
        de.expense_df
            .column("expense_amount")
            .unwrap()
            .f64()
            .unwrap()
            .get(0),
        Some(200.0)
    );
    assert_eq!(
        de.expense_df
            .column("expense_in_ref_currency")
            .unwrap()
            .f64()
            .unwrap()
            .get(0),
        Some(200.0)
    );
    assert_eq!(
        de.expense_df
            .column("primary_category")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("Transport")
    );

    // Summaries recomputed.
    let prim_path = get_year_summary_path(2026, PRIMARIES_FILENAME).unwrap();
    let prim = ParquetReader::new(std::fs::File::open(&prim_path).unwrap())
        .finish()
        .unwrap();
    assert_eq!(
        cell(&prim, "primary_category", "Transport", "2026-01"),
        200.0
    );
    assert_eq!(cell(&prim, "primary_category", "Housing", "2026-01"), 0.0);
    assert_eq!(cell(&prim, "primary_category", "Total", "2026-01"), 250.0);
}

/// `edit_row` must resolve edited categories against the existing-category set
/// (current month's values + known config categories), preserving an existing
/// spelling rather than re-normalizing. Mirrors Python `save_edit`'s use of
/// `resolve_category`.
#[test]
#[serial_test::serial]
fn edit_row_preserves_existing_category_spelling() {
    let _env = TestEnv::new();

    // Register a known primary category whose spelling differs from what
    // `normalize_category_value` would produce: normalize("paypal") == "Paypal",
    // but the canonical/known spelling is "PayPal".
    assert_eq!(normalize_category_value("paypal"), "Paypal");
    finguard_rs::config::add_known_category("PayPal", "primary").unwrap();
    // And a secondary one: normalize("ebay") == "Ebay", known spelling "eBay".
    assert_eq!(normalize_category_value("ebay"), "Ebay");
    finguard_rs::config::add_known_category("eBay", "secondary").unwrap();

    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("a", 1, 100.0, Some("housing"), "E", Some("rent"))
        .unwrap();

    // Edit the row to those categories using different input casing. Because the
    // known set already contains "PayPal" / "eBay", resolve_category must return
    // the existing spelling, NOT the normalized "Paypal" / "Ebay".
    de.edit_row(0, None, None, None, None, Some("paypal"), Some("EBAY"))
        .unwrap();

    assert_eq!(
        de.expense_df
            .column("primary_category")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("PayPal"),
        "existing known spelling must win over normalization"
    );
    assert_eq!(
        de.expense_df
            .column("secondary_category")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("eBay"),
        "existing known spelling must win over normalization"
    );

    // Sanity: an edited category with no existing match still normalizes.
    de.edit_row(0, None, None, None, None, Some("transport"), None)
        .unwrap();
    assert_eq!(
        de.expense_df
            .column("primary_category")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("Transport"),
    );
}

#[test]
#[serial_test::serial]
fn delete_row_removes_and_recomputes() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("a", 1, 100.0, Some("housing"), "E", Some("x"))
        .unwrap();
    de.add_row("b", 2, 50.0, Some("groceries"), "E", Some("y"))
        .unwrap();

    de.delete_row(0).unwrap();
    assert_eq!(de.expense_df.height(), 1);
    assert_eq!(
        de.expense_df
            .column("expense_name")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("b")
    );

    let prim_path = get_year_summary_path(2026, PRIMARIES_FILENAME).unwrap();
    let prim = ParquetReader::new(std::fs::File::open(&prim_path).unwrap())
        .finish()
        .unwrap();
    assert_eq!(cell(&prim, "primary_category", "Housing", "2026-01"), 0.0);
    assert_eq!(
        cell(&prim, "primary_category", "Groceries", "2026-01"),
        50.0
    );
    assert_eq!(cell(&prim, "primary_category", "Total", "2026-01"), 50.0);
}

// ------------------------------------------------------------------
// filter_expenses
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn filter_expenses_by_name_category_and_amount() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("Netflix", 1, 10.0, Some("leisure"), "E", Some("streaming"))
        .unwrap();
    de.add_row("Spotify", 2, 5.0, Some("leisure"), "E", Some("music"))
        .unwrap();
    de.add_row("Rent", 3, 800.0, Some("housing"), "E", Some("rent"))
        .unwrap();

    // name substring, case-insensitive.
    let r = de.filter_expenses(Some("net"), None, None, None).unwrap();
    assert_eq!(r.height(), 1);
    assert_eq!(
        r.column("expense_name").unwrap().str().unwrap().get(0),
        Some("Netflix")
    );

    // category substring matches either primary or secondary.
    let r = de
        .filter_expenses(None, Some("leisure"), None, None)
        .unwrap();
    assert_eq!(r.height(), 2);

    // amount min/max.
    let r = de
        .filter_expenses(None, None, Some(6.0), Some(500.0))
        .unwrap();
    assert_eq!(r.height(), 1);
    assert_eq!(
        r.column("expense_name").unwrap().str().unwrap().get(0),
        Some("Netflix")
    );

    // empty filters ignored.
    let r = de.filter_expenses(Some(""), Some(""), None, None).unwrap();
    assert_eq!(r.height(), 3);
}

/// Regression: `filter_expenses_indexed` must carry each row's TRUE position in
/// the unfiltered df via `_idx`, even with a non-contiguous filtered subset and
/// duplicate name+day+amount rows. The old UI used fuzzy positional matching,
/// which would target the wrong underlying row here.
#[test]
#[serial_test::serial]
fn filter_expenses_indexed_carries_true_row_index() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    // idx 0: distinct
    de.add_row("Rent", 3, 800.0, Some("housing"), "E", Some("rent"))
        .unwrap();
    // idx 1 and idx 3: identical name+day+amount duplicates
    de.add_row("Netflix", 1, 10.0, Some("leisure"), "E", Some("streaming"))
        .unwrap();
    // idx 2: distinct, also matches the "net" name filter via secondary? no.
    de.add_row("Spotify", 2, 5.0, Some("leisure"), "E", Some("music"))
        .unwrap();
    de.add_row("Netflix", 1, 10.0, Some("leisure"), "E", Some("streaming"))
        .unwrap();

    // Filter selects a NON-CONTIGUOUS subset: the two "Netflix" duplicates at
    // underlying indices 1 and 3 (skipping 0 and 2).
    let filtered = de
        .filter_expenses_indexed(Some("netflix"), None, None, None)
        .unwrap();
    assert_eq!(filtered.height(), 2);

    let idx = filtered
        .column("_idx")
        .unwrap()
        .cast(&polars::prelude::DataType::UInt32)
        .unwrap();
    let idx = idx.u32().unwrap();
    let got: Vec<u32> = idx.iter().map(|o| o.unwrap()).collect();
    // The TRUE original indices, not 0 and 1 (which fuzzy positional matching
    // would have produced).
    assert_eq!(got, vec![1, 3]);

    // Deleting via the second filtered row's `_idx` (3) must remove the LAST
    // Netflix, leaving Rent, the first Netflix, and Spotify.
    de.delete_row(got[1]).unwrap();
    let names: Vec<String> = de
        .expense_df
        .column("expense_name")
        .unwrap()
        .str()
        .unwrap()
        .iter()
        .map(|o| o.unwrap_or("").to_string())
        .collect();
    assert_eq!(names, vec!["Rent", "Netflix", "Spotify"]);
}

// ------------------------------------------------------------------
// Cashflow
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn set_income_validates() {
    let _env = TestEnv::new();
    let mut cf = Cashflow::new(2026).unwrap();
    assert!(matches!(
        cf.set_income(1, "Nope", 100.0).unwrap_err(),
        Error::InvalidArgument(_)
    ));
    assert!(matches!(
        cf.set_income(13, "Salary", 100.0).unwrap_err(),
        Error::InvalidArgument(_)
    ));
    assert!(matches!(
        cf.set_income(0, "Salary", 100.0).unwrap_err(),
        Error::InvalidArgument(_)
    ));
}

#[test]
#[serial_test::serial]
fn cashflow_recompute_derives_rows() {
    let _env = TestEnv::new();

    // Build expenses -> primaries Total for Jan = 300.
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("rent", 1, 300.0, Some("housing"), "E", Some("rent"))
        .unwrap();

    let mut cf = Cashflow::new(2026).unwrap();
    cf.set_income(1, "Salary", 1000.0).unwrap();
    cf.set_income(1, "Other", 200.0).unwrap();

    // Income = 1200, Spending = 300, Saving = 900, Saving% = 75.
    assert_eq!(cf.get_value("Income", "01").unwrap(), 1200.0);
    assert_eq!(cf.get_value("Spending", "01").unwrap(), 300.0);
    assert_eq!(cf.get_value("Saving", "01").unwrap(), 900.0);
    assert_eq!(cf.get_value("Saving %", "01").unwrap(), 75.0);
}

#[test]
#[serial_test::serial]
fn cashflow_saving_pct_zero_when_no_income() {
    let _env = TestEnv::new();
    let mut cf = Cashflow::new(2026).unwrap();
    cf.recompute().unwrap();
    // No income, no spending: saving % must be 0 (not NaN/inf).
    assert_eq!(cf.get_value("Saving %", "01").unwrap(), 0.0);
    assert_eq!(cf.get_value("Income", "01").unwrap(), 0.0);
    assert_eq!(cf.get_value("Saving", "01").unwrap(), 0.0);
}

// ------------------------------------------------------------------
// InvestmentHoldings
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn investment_add_value_and_dup_missing() {
    let _env = TestEnv::new();
    let mut inv = InvestmentHoldings::new(2026).unwrap();
    inv.add_asset("VWCE", "Stocks/ETF", "http://x").unwrap();

    // dup
    assert!(matches!(
        inv.add_asset("VWCE", "Stocks/ETF", "").unwrap_err(),
        Error::AlreadyExists(_)
    ));
    // invalid category
    assert!(matches!(
        inv.add_asset("Y", "Nope", "").unwrap_err(),
        Error::InvalidArgument(_)
    ));
    // missing asset ops
    assert!(matches!(
        inv.set_price("Ghost", 1, 1.0).unwrap_err(),
        Error::NotFound(_)
    ));

    inv.set_quantity("VWCE", 1, 10.0).unwrap();
    inv.set_price("VWCE", 1, 5.0).unwrap();

    let v = inv.df_value().unwrap();
    let row = v
        .clone()
        .lazy()
        .filter(col("asset_name").eq(lit("VWCE")))
        .collect()
        .unwrap();
    // 10 * 5 = 50
    assert_eq!(row.column("01").unwrap().f64().unwrap().get(0), Some(50.0));
    // untouched month stays 0
    assert_eq!(row.column("02").unwrap().f64().unwrap().get(0), Some(0.0));
}

#[test]
#[serial_test::serial]
fn investment_rename_keeps_holdings_and_prices_in_sync() {
    let _env = TestEnv::new();
    let mut inv = InvestmentHoldings::new(2026).unwrap();
    inv.add_asset("OLD", "Bonds", "").unwrap();
    inv.set_quantity("OLD", 1, 2.0).unwrap();
    inv.set_price("OLD", 1, 3.0).unwrap();

    inv.rename_asset("OLD", "NEW").unwrap();
    assert!(matches!(
        inv.rename_asset("GHOST", "X").unwrap_err(),
        Error::NotFound(_)
    ));

    // Both frames renamed.
    let q = inv.df.column("asset_name").unwrap().str().unwrap().get(0);
    let p = inv
        .df_prices
        .column("asset_name")
        .unwrap()
        .str()
        .unwrap()
        .get(0);
    assert_eq!(q, Some("NEW"));
    assert_eq!(p, Some("NEW"));
    // value still computes
    let v = inv.df_value().unwrap();
    assert_eq!(v.column("01").unwrap().f64().unwrap().get(0), Some(6.0));
}

#[test]
#[serial_test::serial]
fn investment_remove_removes_from_both() {
    let _env = TestEnv::new();
    let mut inv = InvestmentHoldings::new(2026).unwrap();
    inv.add_asset("A", "Bonds", "").unwrap();
    inv.add_asset("B", "Bonds", "").unwrap();
    inv.remove_asset("A").unwrap();
    assert_eq!(inv.df.height(), 1);
    assert_eq!(inv.df_prices.height(), 1);
    assert_eq!(
        inv.df.column("asset_name").unwrap().str().unwrap().get(0),
        Some("B")
    );
}

#[test]
#[serial_test::serial]
fn investment_set_category_and_link() {
    let _env = TestEnv::new();
    let mut inv = InvestmentHoldings::new(2026).unwrap();
    inv.add_asset("A", "Bonds", "old").unwrap();
    inv.set_category("A", "Commodities").unwrap();
    inv.set_link("A", "newlink").unwrap();
    assert_eq!(
        inv.df.column("category").unwrap().str().unwrap().get(0),
        Some("Commodities")
    );
    assert_eq!(
        inv.df.column("link").unwrap().str().unwrap().get(0),
        Some("newlink")
    );
    assert!(matches!(
        inv.set_category("A", "Bad").unwrap_err(),
        Error::InvalidArgument(_)
    ));
}

// ------------------------------------------------------------------
// Liquidity
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn liquidity_crud() {
    let _env = TestEnv::new();
    let mut liq = Liquidity::new(2026).unwrap();
    liq.add_asset("Bank", "Bank/Broker account", "E").unwrap();
    assert!(matches!(
        liq.add_asset("Bank", "Cash", "E").unwrap_err(),
        Error::AlreadyExists(_)
    ));
    assert!(matches!(
        liq.add_asset("X", "Bad", "E").unwrap_err(),
        Error::InvalidArgument(_)
    ));
    liq.set_value("Bank", 1, 1234.0).unwrap();
    assert_eq!(
        liq.df.column("01").unwrap().f64().unwrap().get(0),
        Some(1234.0)
    );
    liq.rename_asset("Bank", "MyBank").unwrap();
    assert_eq!(
        liq.df.column("asset_name").unwrap().str().unwrap().get(0),
        Some("MyBank")
    );
    liq.set_category("MyBank", "Cash").unwrap();
    assert_eq!(
        liq.df.column("category").unwrap().str().unwrap().get(0),
        Some("Cash")
    );
    liq.remove_asset("MyBank").unwrap();
    assert_eq!(liq.df.height(), 0);
}

// ------------------------------------------------------------------
// CreditsDebts
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn credits_debts_allows_negative_and_crud() {
    let _env = TestEnv::new();
    let mut cd = CreditsDebts::new(2026).unwrap();
    cd.add_entry("Friend", "E").unwrap();
    cd.add_entry("Loan", "E").unwrap();
    assert!(matches!(
        cd.add_entry("Friend", "E").unwrap_err(),
        Error::AlreadyExists(_)
    ));

    // Positive = credit, negative = debt.
    cd.set_value("Friend", 1, 100.0).unwrap();
    cd.set_value("Loan", 1, -250.0).unwrap();
    assert_eq!(
        cd.df
            .clone()
            .lazy()
            .filter(col("name").eq(lit("Loan")))
            .collect()
            .unwrap()
            .column("01")
            .unwrap()
            .f64()
            .unwrap()
            .get(0),
        Some(-250.0)
    );

    cd.rename_entry("Friend", "BestFriend").unwrap();
    assert!(matches!(
        cd.set_value("Ghost", 1, 1.0).unwrap_err(),
        Error::NotFound(_)
    ));
    cd.remove_entry("Loan").unwrap();
    assert_eq!(cd.df.height(), 1);
}

#[test]
#[serial_test::serial]
fn credits_debts_legacy_type_column_dropped_and_currency_migrated() {
    let _env = TestEnv::new();
    // Write a legacy file with a `type` column and no `currency`.
    let path = get_year_summary_path(2026, finguard_rs::paths::CREDITS_DEBTS_FILENAME).unwrap();
    let mut legacy = df!(
        "name" => &["Old"],
        "type" => &["debt"],
        "01" => &[-5.0_f64],
    )
    .unwrap();
    ParquetWriter::new(std::fs::File::create(&path).unwrap())
        .finish(&mut legacy)
        .unwrap();

    let cd = CreditsDebts::new(2026).unwrap();
    let names: Vec<&str> = cd
        .df
        .get_column_names()
        .iter()
        .map(|s| s.as_str())
        .collect();
    assert!(!names.contains(&"type"), "legacy 'type' dropped");
    assert!(names.contains(&"currency"), "currency migrated");
    assert_eq!(
        cd.df.column("currency").unwrap().str().unwrap().get(0),
        Some("E")
    );
}

#[test]
#[serial_test::serial]
fn liquidity_legacy_currency_migrated() {
    let _env = TestEnv::new();
    let path = get_year_summary_path(2026, finguard_rs::paths::LIQUIDITY_FILENAME).unwrap();
    let mut legacy = df!(
        "asset_name" => &["Bank"],
        "category" => &["Cash"],
        "01" => &[10.0_f64],
    )
    .unwrap();
    ParquetWriter::new(std::fs::File::create(&path).unwrap())
        .finish(&mut legacy)
        .unwrap();

    let liq = Liquidity::new(2026).unwrap();
    assert!(
        liq.df
            .get_column_names()
            .iter()
            .any(|c| c.as_str() == "currency")
    );
    assert_eq!(
        liq.df.column("currency").unwrap().str().unwrap().get(0),
        Some("E")
    );
}

// ------------------------------------------------------------------
// RecurringExpenses
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn recurring_add_day_validation() {
    let _env = TestEnv::new();
    let mut re = RecurringExpenses::new(2026).unwrap();
    assert!(matches!(
        re.add("rent", 0, 10.0, "E", "housing", "rent").unwrap_err(),
        Error::InvalidArgument(_)
    ));
    assert!(matches!(
        re.add("rent", 29, 10.0, "E", "housing", "rent")
            .unwrap_err(),
        Error::InvalidArgument(_)
    ));
    re.add("rent", 1, 10.0, "E", "housing", "rent").unwrap();
    re.add("rent", 28, 10.0, "E", "housing", "rent").unwrap();
    assert_eq!(re.df.height(), 2);
    // category normalized
    assert_eq!(
        re.df
            .column("primary_category")
            .unwrap()
            .str()
            .unwrap()
            .get(0),
        Some("Housing")
    );
}

#[test]
#[serial_test::serial]
fn recurring_remove_by_index() {
    let _env = TestEnv::new();
    let mut re = RecurringExpenses::new(2026).unwrap();
    re.add("a", 1, 1.0, "E", "housing", "x").unwrap();
    re.add("b", 2, 2.0, "E", "groceries", "y").unwrap();
    re.remove(0).unwrap();
    assert_eq!(re.df.height(), 1);
    assert_eq!(
        re.df.column("expense_name").unwrap().str().unwrap().get(0),
        Some("b")
    );
}

#[test]
#[serial_test::serial]
fn recurring_apply_to_month_inserts_and_skips_duplicates() {
    let _env = TestEnv::new();
    let mut re = RecurringExpenses::new(2026).unwrap();
    re.add("rent", 1, 800.0, "E", "housing", "rent").unwrap();
    re.add("gym", 5, 30.0, "E", "leisure", "sport").unwrap();

    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    let added = re.apply_to_month(&mut de).unwrap();
    assert_eq!(added, vec!["rent".to_string(), "gym".to_string()]);
    assert_eq!(de.expense_df.height(), 2);

    // Apply again: both already present (same name + day) -> skipped.
    let added2 = re.apply_to_month(&mut de).unwrap();
    assert!(added2.is_empty());
    assert_eq!(de.expense_df.height(), 2);
}

// ------------------------------------------------------------------
// get_category_totals_across_all_years / remove_category_from_all_summaries
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn category_totals_across_years() {
    let _env = TestEnv::new();

    let mut y26 = DetailedExpenses::new(2026, 1).unwrap();
    y26.add_row("a", 1, 100.0, Some("housing"), "E", Some("x"))
        .unwrap();
    let mut y27 = DetailedExpenses::new(2027, 1).unwrap();
    y27.add_row("b", 1, 50.0, Some("housing"), "E", Some("x"))
        .unwrap();
    y27.add_row("c", 1, 20.0, Some("groceries"), "E", Some("y"))
        .unwrap();

    let totals = get_category_totals_across_all_years("primary").unwrap();
    assert_eq!(totals.get("Housing").copied(), Some(150.0));
    assert_eq!(totals.get("Groceries").copied(), Some(20.0));
    // Total row excluded.
    assert!(!totals.contains_key("Total"));

    assert!(matches!(
        get_category_totals_across_all_years("bad").unwrap_err(),
        Error::InvalidArgument(_)
    ));
}

#[test]
#[serial_test::serial]
fn remove_category_from_all_summaries_works() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("a", 1, 100.0, Some("housing"), "E", Some("x"))
        .unwrap();
    de.add_row("b", 1, 50.0, Some("groceries"), "E", Some("y"))
        .unwrap();

    remove_category_from_all_summaries("Housing", "primary").unwrap();

    let prim_path = get_year_summary_path(2026, PRIMARIES_FILENAME).unwrap();
    let prim = ParquetReader::new(std::fs::File::open(&prim_path).unwrap())
        .finish()
        .unwrap();
    let cats = cat_order(&prim, "primary_category");
    assert!(!cats.contains(&"Housing".to_string()));
    assert!(cats.contains(&"Groceries".to_string()));

    assert!(matches!(
        remove_category_from_all_summaries("X", "bad").unwrap_err(),
        Error::InvalidArgument(_)
    ));
}
