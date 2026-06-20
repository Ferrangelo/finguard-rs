//! Tests for `finguard_rs::plots`.

mod common;

use common::TestEnv;
use finguard_rs::df_operations::{
    Cashflow, CreditsDebts, DetailedExpenses, InvestmentHoldings, Liquidity,
};
use finguard_rs::plots::{
    cashflow_bar_chart, category_expenses_over_months, cumulative_expenses_pie, income_pie_chart,
    monthly_expenses_comparison, monthly_expenses_pie, networth_allocation_pie,
    networth_evolution_line,
};

// ------------------------------------------------------------------
// None conditions: missing files
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn expense_charts_none_when_no_summary_file() {
    let _env = TestEnv::new();
    assert!(
        monthly_expenses_comparison(2026, &[1], "primary")
            .unwrap()
            .is_none()
    );
    assert!(
        category_expenses_over_months(2026, &["Housing".into()], "primary")
            .unwrap()
            .is_none()
    );
    assert!(cumulative_expenses_pie(2026, "primary").unwrap().is_none());
}

#[test]
#[serial_test::serial]
fn cashflow_charts_none_when_all_zero() {
    let _env = TestEnv::new();
    // No income, no spending.
    assert!(cashflow_bar_chart(2026).unwrap().is_none());
    assert!(income_pie_chart(2026).unwrap().is_none());
}

#[test]
#[serial_test::serial]
fn networth_charts_none_when_all_zero() {
    let _env = TestEnv::new();
    assert!(networth_allocation_pie(2026, 1).unwrap().is_none());
    assert!(networth_evolution_line(2026).unwrap().is_none());
}

#[test]
#[serial_test::serial]
fn monthly_expenses_pie_none_when_empty_table() {
    let _env = TestEnv::new();
    let de = DetailedExpenses::new(2026, 1).unwrap();
    // Empty detailed expenses -> empty summary table -> None.
    assert!(monthly_expenses_pie(&de, "primary").unwrap().is_none());
}

// ------------------------------------------------------------------
// monthly_expenses_comparison
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn monthly_comparison_values_and_labels() {
    let _env = TestEnv::new();
    let mut jan = DetailedExpenses::new(2026, 1).unwrap();
    jan.add_row("a", 1, 100.0, Some("housing"), "E", Some("x"))
        .unwrap();
    let mut feb = DetailedExpenses::new(2026, 2).unwrap();
    feb.add_row("b", 1, 200.0, Some("housing"), "E", Some("x"))
        .unwrap();

    let chart = monthly_expenses_comparison(2026, &[1, 2], "primary")
        .unwrap()
        .unwrap();
    // Series labels are abbreviated month names, in requested order.
    let series_names: Vec<&str> = chart.series.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(series_names, vec!["Jan", "Feb"]);

    // "Total" excluded from categories.
    assert!(!chart.categories.contains(&"Total".to_string()));

    // Housing value present.
    let h = chart
        .categories
        .iter()
        .position(|c| c == "Housing")
        .unwrap();
    assert_eq!(chart.series[0].values[h], 100.0);
    assert_eq!(chart.series[1].values[h], 200.0);
}

#[test]
#[serial_test::serial]
fn monthly_comparison_none_when_requested_month_absent() {
    let _env = TestEnv::new();
    let mut jan = DetailedExpenses::new(2026, 1).unwrap();
    jan.add_row("a", 1, 100.0, Some("housing"), "E", Some("x"))
        .unwrap();
    // Request only month 5 (no column) -> None.
    assert!(
        monthly_expenses_comparison(2026, &[5], "primary")
            .unwrap()
            .is_none()
    );
}

// ------------------------------------------------------------------
// category_expenses_over_months
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn category_over_months_missing_category_is_zeros() {
    let _env = TestEnv::new();
    let mut jan = DetailedExpenses::new(2026, 1).unwrap();
    jan.add_row("a", 1, 100.0, Some("housing"), "E", Some("x"))
        .unwrap();

    let chart =
        category_expenses_over_months(2026, &["Housing".into(), "Travel".into()], "primary")
            .unwrap()
            .unwrap();

    assert_eq!(chart.x_labels, vec!["Jan"]);
    // Housing series has the value.
    let housing = chart.series.iter().find(|s| s.name == "Housing").unwrap();
    assert_eq!(housing.values, vec![100.0]);
    // Travel absent -> zeros.
    let travel = chart.series.iter().find(|s| s.name == "Travel").unwrap();
    assert_eq!(travel.values, vec![0.0]);
}

#[test]
#[serial_test::serial]
fn category_over_months_limited_to_three() {
    let _env = TestEnv::new();
    let mut jan = DetailedExpenses::new(2026, 1).unwrap();
    jan.add_row("a", 1, 1.0, Some("housing"), "E", Some("x"))
        .unwrap();
    let chart = category_expenses_over_months(
        2026,
        &["A".into(), "B".into(), "C".into(), "D".into()],
        "primary",
    )
    .unwrap()
    .unwrap();
    assert_eq!(chart.series.len(), 3);
}

// ------------------------------------------------------------------
// cumulative_expenses_pie + rounding
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn cumulative_pie_only_positive_slices() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("a", 1, 100.0, Some("housing"), "E", Some("x"))
        .unwrap();
    de.add_row("b", 2, 50.0, Some("groceries"), "E", Some("y"))
        .unwrap();

    let pie = cumulative_expenses_pie(2026, "primary").unwrap().unwrap();
    let names: Vec<&str> = pie.slices.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Housing"));
    assert!(names.contains(&"Groceries"));
    assert!(!names.contains(&"Total"));
    // No zero slices.
    assert!(pie.slices.iter().all(|s| s.value > 0.0));
}

#[test]
#[serial_test::serial]
fn integer_rounding_is_half_to_even() {
    let _env = TestEnv::new();
    // Two categories whose totals are exactly 0.5 and 2.5.
    // round-half-even: 0.5 -> 0, 2.5 -> 2.  (half-away would give 1 and 3.)
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("half", 1, 0.5, Some("housing"), "E", Some("x"))
        .unwrap();
    de.add_row("twohalf", 2, 2.5, Some("groceries"), "E", Some("y"))
        .unwrap();

    let chart = monthly_expenses_comparison(2026, &[1], "primary")
        .unwrap()
        .unwrap();
    let cats = &chart.categories;
    let h = cats.iter().position(|c| c == "Housing").unwrap();
    let g = cats.iter().position(|c| c == "Groceries").unwrap();
    assert_eq!(chart.series[0].values[h], 0.0, "0.5 rounds to even 0");
    assert_eq!(chart.series[0].values[g], 2.0, "2.5 rounds to even 2");
}

// ------------------------------------------------------------------
// cashflow_bar_chart + two-decimal rounding
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn cashflow_bar_chart_values_and_two_decimal_half_even() {
    let _env = TestEnv::new();
    // Spending = 0.125 -> round2 half-even -> 0.12 (12.5 rounds to even 12).
    let mut de = DetailedExpenses::new(2026, 3).unwrap();
    de.add_row("x", 1, 0.125, Some("housing"), "E", Some("x"))
        .unwrap();

    let mut cf = Cashflow::new(2026).unwrap();
    cf.set_income(3, "Salary", 1000.0).unwrap();

    let chart = cashflow_bar_chart(2026).unwrap().unwrap();
    assert_eq!(chart.categories.len(), 12);
    assert_eq!(chart.categories[2], "Mar");

    let income = chart.series.iter().find(|s| s.name == "Income").unwrap();
    let spending = chart.series.iter().find(|s| s.name == "Spending").unwrap();
    assert_eq!(income.values[2], 1000.0);
    // 0.125 -> 0.12 (half to even), NOT 0.13.
    assert_eq!(spending.values[2], 0.12);
}

#[test]
#[serial_test::serial]
fn income_pie_chart_slices() {
    let _env = TestEnv::new();
    let mut cf = Cashflow::new(2026).unwrap();
    cf.set_income(1, "Salary", 1000.0).unwrap();
    cf.set_income(2, "Other", 200.0).unwrap();

    let pie = income_pie_chart(2026).unwrap().unwrap();
    let names: Vec<&str> = pie.slices.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Salary"));
    assert!(names.contains(&"Other"));
    let salary = pie.slices.iter().find(|s| s.name == "Salary").unwrap();
    assert_eq!(salary.value, 1000.0);
}

// ------------------------------------------------------------------
// networth_allocation_pie: Credits vs Debts sign split
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn networth_pie_credits_label_for_positive() {
    let _env = TestEnv::new();
    let mut cd = CreditsDebts::new(2026).unwrap();
    cd.add_entry("Friend", "E").unwrap();
    cd.set_value("Friend", 1, 300.0).unwrap();

    let pie = networth_allocation_pie(2026, 1).unwrap().unwrap();
    let slice = pie.slices.iter().find(|s| s.name == "Credits").unwrap();
    assert_eq!(slice.value, 300.0);
    assert!(pie.slices.iter().all(|s| s.name != "Debts"));
}

#[test]
#[serial_test::serial]
fn networth_pie_debts_label_for_negative_abs_value() {
    let _env = TestEnv::new();
    // Need at least one positive slice so the pie isn't None overall; add some
    // liquidity, plus a net-negative credits/debts -> Debts slice with abs value.
    let mut liq = Liquidity::new(2026).unwrap();
    liq.add_asset("Bank", "Cash", "E").unwrap();
    liq.set_value("Bank", 1, 1000.0).unwrap();

    let mut cd = CreditsDebts::new(2026).unwrap();
    cd.add_entry("Loan", "E").unwrap();
    cd.set_value("Loan", 1, -250.0).unwrap();

    let pie = networth_allocation_pie(2026, 1).unwrap().unwrap();
    let debts = pie.slices.iter().find(|s| s.name == "Debts").unwrap();
    assert_eq!(debts.value, 250.0, "abs value of negative");
    // Liquidity slice present.
    assert!(pie.slices.iter().any(|s| s.name == "Liquidity"));
}

#[test]
#[serial_test::serial]
fn networth_pie_includes_investment_categories() {
    let _env = TestEnv::new();
    let mut inv = InvestmentHoldings::new(2026).unwrap();
    inv.add_asset("VWCE", "Stocks/ETF", "").unwrap();
    inv.set_quantity("VWCE", 1, 10.0).unwrap();
    inv.set_price("VWCE", 1, 5.0).unwrap();

    let pie = networth_allocation_pie(2026, 1).unwrap().unwrap();
    let s = pie.slices.iter().find(|s| s.name == "Stocks/ETF").unwrap();
    assert_eq!(s.value, 50.0);
}

// ------------------------------------------------------------------
// networth_evolution_line: net_worth = sum of components
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn networth_evolution_net_worth_is_component_sum() {
    let _env = TestEnv::new();
    let mut inv = InvestmentHoldings::new(2026).unwrap();
    inv.add_asset("VWCE", "Stocks/ETF", "").unwrap();
    inv.set_quantity("VWCE", 1, 10.0).unwrap();
    inv.set_price("VWCE", 1, 5.0).unwrap(); // 50

    let mut liq = Liquidity::new(2026).unwrap();
    liq.add_asset("Bank", "Cash", "E").unwrap();
    liq.set_value("Bank", 1, 1000.0).unwrap();

    let mut cd = CreditsDebts::new(2026).unwrap();
    cd.add_entry("Loan", "E").unwrap();
    cd.set_value("Loan", 1, -200.0).unwrap();

    let ev = networth_evolution_line(2026).unwrap().unwrap();
    assert_eq!(ev.months.len(), 12);
    assert_eq!(ev.months[0], "Jan");
    assert_eq!(ev.months[11], "Dec");

    // Components present in order: investment cats, Liquidity, Credits/Debts.
    let names: Vec<&str> = ev.components.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "Stocks/ETF",
            "Commodities",
            "Bonds",
            "Liquidity",
            "Credits/Debts"
        ]
    );

    // Jan net worth = 50 + 1000 - 200 = 850.
    assert_eq!(ev.net_worth[0], 850.0);
    // net_worth[i] == sum of components[i].
    for i in 0..12 {
        let s: f64 = ev.components.iter().map(|c| c.values[i]).sum();
        assert_eq!(ev.net_worth[i], s, "month {i}");
    }
}

// ------------------------------------------------------------------
// monthly_expenses_pie: non-empty but all-zero -> Some(empty slices)
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn monthly_expenses_pie_values() {
    let _env = TestEnv::new();
    let mut de = DetailedExpenses::new(2026, 1).unwrap();
    de.add_row("a", 1, 30.0, Some("housing"), "E", Some("rent"))
        .unwrap();
    de.add_row("b", 2, 20.0, Some("groceries"), "E", Some("super"))
        .unwrap();

    let pie = monthly_expenses_pie(&de, "primary").unwrap().unwrap();
    let total: f64 = pie.slices.iter().map(|s| s.value).sum();
    assert_eq!(total, 50.0);
    assert_eq!(pie.slices.len(), 2);
}
