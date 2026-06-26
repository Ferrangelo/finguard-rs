//! Seed realistic mock data for development screenshots.
//!
//! Run with:
//!   cargo run --bin seed_mock
//!
//! Writes 12 months of 2025 data (expenses, cashflow, investments, liquidity,
//! credits/debts, recurring templates, and category config) into the project's
//! `mock_data/` directory without touching real user data.
//!
//! The directory is wiped and recreated on every run so the result is always
//! deterministic. After seeding, use `./run_mock.sh` to start the app against
//! this dataset.

use std::path::PathBuf;

use finguard_rs_backend::{
    Result, config,
    df_operations::{
        Cashflow, CreditsDebts, DetailedExpenses, InvestmentHoldings, Liquidity, RecurringExpenses,
    },
};

fn main() -> Result<()> {
    // -----------------------------------------------------------------------
    // 1. Redirect XDG dirs to project-local mock_data/ before any finguard I/O.
    // -----------------------------------------------------------------------
    let mock_dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock_data");

    // Always reset for reproducibility.
    let _ = std::fs::remove_dir_all(&mock_dir);
    std::fs::create_dir_all(&mock_dir)?;

    let mock_abs = std::fs::canonicalize(&mock_dir).unwrap_or(mock_dir.clone());

    // SAFETY: single-threaded startup code; no other threads read the env yet.
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var("XDG_DATA_HOME", &mock_abs);
        std::env::set_var("XDG_CONFIG_HOME", &mock_abs);
    }

    println!("Seeding mock data → {}", mock_abs.display());

    let year: i32 = 2025;

    // -----------------------------------------------------------------------
    // 2. Known categories (primary + secondary).
    // -----------------------------------------------------------------------
    for cat in [
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
    ] {
        config::add_known_category(cat, "primary")?;
    }

    for cat in [
        "Rent",
        "Mortgage",
        "Medicine",
        "Pharmacy",
        "Othergroceries",
        "Bus",
        "Metro",
        "Taxi",
        "Fuel",
        "Cafe",
        "Restaurant",
        "Takeaway",
        "Delivery",
        "Tv",
        "Streaming",
        "Gym",
        "Cinema",
        "Concert",
        "Books",
        "Electronics",
        "Clothes",
        "Shoes",
        "Airplane",
        "Hotel",
        "Holiday",
        "Gift",
        "Birthday",
        "Christmas",
        "Insurance",
        "Bank",
        "Utilities",
        "Internet",
        "Phone",
        "Miscellaneous",
        "Mrleisure",
    ] {
        config::add_known_category(cat, "secondary")?;
    }

    // -----------------------------------------------------------------------
    // 3. Expense-name → category mappings.
    // -----------------------------------------------------------------------
    for (name, primary, secondary) in [
        ("rent", "housing", "rent"),
        ("supermarket", "groceries", "othergroceries"),
        ("lidl", "groceries", "othergroceries"),
        ("pharmacy", "health", "pharmacy"),
        ("metro card", "transport", "metro"),
        ("taxi", "transport", "taxi"),
        ("netflix", "leisure", "streaming"),
        ("gym", "leisure", "gym"),
        ("internet", "otherexpenses", "internet"),
        ("phone plan", "otherexpenses", "phone"),
        ("cafe", "lunchbreak", "cafe"),
        ("restaurant", "out", "restaurant"),
        ("insurance", "fees", "insurance"),
    ] {
        config::add_mapping(name, primary, secondary, false)?;
    }

    // -----------------------------------------------------------------------
    // 4. Recurring expense templates.
    // -----------------------------------------------------------------------
    let mut recurring = RecurringExpenses::new(year)?;
    recurring.add("Rent", 1, 1_500.00, "E", "housing", "rent")?;
    recurring.add("Netflix", 10, 15.99, "E", "leisure", "streaming")?;
    recurring.add("Gym", 5, 45.00, "E", "leisure", "gym")?;
    recurring.add("Internet", 15, 30.00, "E", "otherexpenses", "internet")?;
    recurring.add("Phone Plan", 20, 20.00, "E", "otherexpenses", "phone")?;

    // -----------------------------------------------------------------------
    // 5. Detailed expenses — January through December.
    //
    //    Each block: apply recurring templates first, then add variable costs,
    //    then update the cumulative primaries/secondaries summary tables so
    //    that Cashflow::recompute() has spending data later.
    // -----------------------------------------------------------------------

    // Helper: add a slice of (day, name, amount, primary, secondary) rows.
    fn add_rows(de: &mut DetailedExpenses, rows: &[(u32, &str, f64, &str, &str)]) -> Result<()> {
        for &(day, name, amount, primary, secondary) in rows {
            de.add_row(name, day, amount, Some(primary), "E", Some(secondary))?;
        }
        Ok(())
    }

    // January — quiet winter month
    {
        let mut de = DetailedExpenses::new(year, 1)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (3, "Supermarket", 85.50, "groceries", "othergroceries"),
                (12, "Supermarket", 92.30, "groceries", "othergroceries"),
                (23, "Supermarket", 78.40, "groceries", "othergroceries"),
                (8, "Pharmacy", 35.20, "health", "pharmacy"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (6, "Cafe", 13.50, "lunchbreak", "cafe"),
                (9, "Cafe", 12.00, "lunchbreak", "cafe"),
                (14, "Cafe", 14.50, "lunchbreak", "cafe"),
                (21, "Cafe", 11.80, "lunchbreak", "cafe"),
                (16, "Restaurant", 58.00, "out", "restaurant"),
                (25, "Insurance", 95.00, "fees", "insurance"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // February — Valentine's dinner splurge
    {
        let mut de = DetailedExpenses::new(year, 2)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (4, "Supermarket", 88.20, "groceries", "othergroceries"),
                (14, "Supermarket", 95.60, "groceries", "othergroceries"),
                (24, "Supermarket", 72.10, "groceries", "othergroceries"),
                (11, "Pharmacy", 22.50, "health", "pharmacy"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (5, "Cafe", 13.00, "lunchbreak", "cafe"),
                (11, "Cafe", 14.50, "lunchbreak", "cafe"),
                (19, "Cafe", 12.80, "lunchbreak", "cafe"),
                (8, "Restaurant", 48.00, "out", "restaurant"),
                (14, "Restaurant", 125.00, "out", "restaurant"),
                (22, "Cinema", 28.00, "leisure", "cinema"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // March — ski trip drives spending up
    {
        let mut de = DetailedExpenses::new(year, 3)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (3, "Supermarket", 90.00, "groceries", "othergroceries"),
                (20, "Supermarket", 88.50, "groceries", "othergroceries"),
                (28, "Supermarket", 75.30, "groceries", "othergroceries"),
                (7, "Pharmacy", 18.00, "health", "pharmacy"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (4, "Cafe", 12.50, "lunchbreak", "cafe"),
                (12, "Cafe", 14.00, "lunchbreak", "cafe"),
                (25, "Cafe", 11.50, "lunchbreak", "cafe"),
                (15, "Restaurant", 55.00, "out", "restaurant"),
                (8, "Ski Resort", 380.00, "travel", "holiday"),
                (8, "Ski Rental", 120.00, "travel", "holiday"),
                (9, "Ski Lessons", 180.00, "travel", "holiday"),
                (10, "Restaurant", 95.00, "out", "restaurant"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // April — spring wardrobe refresh
    {
        let mut de = DetailedExpenses::new(year, 4)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (5, "Supermarket", 82.40, "groceries", "othergroceries"),
                (15, "Supermarket", 88.60, "groceries", "othergroceries"),
                (26, "Supermarket", 74.20, "groceries", "othergroceries"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (7, "Cafe", 12.00, "lunchbreak", "cafe"),
                (14, "Cafe", 13.50, "lunchbreak", "cafe"),
                (22, "Cafe", 11.80, "lunchbreak", "cafe"),
                (18, "Restaurant", 45.00, "out", "restaurant"),
                (12, "Books", 38.00, "leisure", "books"),
                (20, "Clothes", 145.00, "clothing", "clothes"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // May — concert night out
    {
        let mut de = DetailedExpenses::new(year, 5)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (3, "Supermarket", 85.20, "groceries", "othergroceries"),
                (14, "Supermarket", 92.80, "groceries", "othergroceries"),
                (25, "Supermarket", 80.40, "groceries", "othergroceries"),
                (10, "Pharmacy", 15.00, "health", "pharmacy"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (18, "Taxi", 25.00, "transport", "taxi"),
                (6, "Cafe", 13.50, "lunchbreak", "cafe"),
                (13, "Cafe", 14.00, "lunchbreak", "cafe"),
                (20, "Cafe", 12.50, "lunchbreak", "cafe"),
                (28, "Cafe", 11.00, "lunchbreak", "cafe"),
                (9, "Restaurant", 52.00, "out", "restaurant"),
                (25, "Concert", 85.00, "leisure", "concert"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // June — summer prep (swimwear + headphones)
    {
        let mut de = DetailedExpenses::new(year, 6)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (4, "Supermarket", 88.50, "groceries", "othergroceries"),
                (16, "Supermarket", 95.20, "groceries", "othergroceries"),
                (27, "Supermarket", 82.40, "groceries", "othergroceries"),
                (8, "Pharmacy", 22.00, "health", "pharmacy"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (5, "Cafe", 12.80, "lunchbreak", "cafe"),
                (12, "Cafe", 14.20, "lunchbreak", "cafe"),
                (19, "Cafe", 13.50, "lunchbreak", "cafe"),
                (13, "Restaurant", 62.00, "out", "restaurant"),
                (27, "Restaurant", 55.00, "out", "restaurant"),
                (20, "Clothes", 85.00, "clothing", "clothes"),
                (15, "Headphones", 160.00, "leisure", "electronics"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // July — summer vacation (biggest spending month)
    {
        let mut de = DetailedExpenses::new(year, 7)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (3, "Supermarket", 65.00, "groceries", "othergroceries"),
                (28, "Supermarket", 58.50, "groceries", "othergroceries"),
                (7, "Taxi", 45.00, "transport", "taxi"),
                (8, "Flight Tickets", 480.00, "travel", "airplane"),
                (8, "Hotel", 920.00, "travel", "hotel"),
                (9, "Restaurant", 85.00, "out", "restaurant"),
                (10, "Restaurant", 78.00, "out", "restaurant"),
                (11, "Restaurant", 92.00, "out", "restaurant"),
                (12, "Souvenir", 65.00, "gifts", "gift"),
                (9, "Beach", 25.00, "leisure", "mrleisure"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // August — calm recovery month
    {
        let mut de = DetailedExpenses::new(year, 8)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (5, "Supermarket", 72.30, "groceries", "othergroceries"),
                (18, "Supermarket", 78.40, "groceries", "othergroceries"),
                (28, "Supermarket", 68.20, "groceries", "othergroceries"),
                (12, "Pharmacy", 18.50, "health", "pharmacy"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (6, "Cafe", 12.50, "lunchbreak", "cafe"),
                (20, "Cafe", 13.80, "lunchbreak", "cafe"),
                (14, "Restaurant", 55.00, "out", "restaurant"),
                (28, "Restaurant", 48.00, "out", "restaurant"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // September — back to routine, autumn clothes
    {
        let mut de = DetailedExpenses::new(year, 9)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (3, "Supermarket", 88.50, "groceries", "othergroceries"),
                (15, "Supermarket", 92.40, "groceries", "othergroceries"),
                (26, "Supermarket", 82.30, "groceries", "othergroceries"),
                (9, "Pharmacy", 28.00, "health", "pharmacy"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (4, "Cafe", 13.50, "lunchbreak", "cafe"),
                (11, "Cafe", 14.00, "lunchbreak", "cafe"),
                (18, "Cafe", 12.80, "lunchbreak", "cafe"),
                (12, "Restaurant", 58.00, "out", "restaurant"),
                (22, "Clothes", 135.00, "clothing", "clothes"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // October — Halloween + birthday gift
    {
        let mut de = DetailedExpenses::new(year, 10)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (4, "Supermarket", 85.20, "groceries", "othergroceries"),
                (16, "Supermarket", 89.80, "groceries", "othergroceries"),
                (27, "Supermarket", 76.50, "groceries", "othergroceries"),
                (8, "Pharmacy", 32.00, "health", "pharmacy"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (7, "Cafe", 12.50, "lunchbreak", "cafe"),
                (15, "Cafe", 14.00, "lunchbreak", "cafe"),
                (23, "Cafe", 13.80, "lunchbreak", "cafe"),
                (17, "Restaurant", 52.00, "out", "restaurant"),
                (25, "Gift", 60.00, "gifts", "birthday"),
                (31, "Halloween", 35.00, "leisure", "mrleisure"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // November — Black Friday + winter jacket
    {
        let mut de = DetailedExpenses::new(year, 11)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (3, "Supermarket", 90.50, "groceries", "othergroceries"),
                (14, "Supermarket", 95.40, "groceries", "othergroceries"),
                (25, "Supermarket", 82.80, "groceries", "othergroceries"),
                (11, "Pharmacy", 25.00, "health", "pharmacy"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (5, "Cafe", 12.80, "lunchbreak", "cafe"),
                (13, "Cafe", 14.50, "lunchbreak", "cafe"),
                (20, "Cafe", 13.20, "lunchbreak", "cafe"),
                (8, "Restaurant", 55.00, "out", "restaurant"),
                (29, "Electronics", 220.00, "leisure", "electronics"),
                (22, "Clothes", 180.00, "clothing", "clothes"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    // December — Christmas (highest spending month)
    {
        let mut de = DetailedExpenses::new(year, 12)?;
        recurring.apply_to_month(&mut de)?;
        add_rows(
            &mut de,
            &[
                (3, "Supermarket", 95.40, "groceries", "othergroceries"),
                (15, "Supermarket", 112.80, "groceries", "othergroceries"),
                (26, "Supermarket", 88.50, "groceries", "othergroceries"),
                (8, "Pharmacy", 22.00, "health", "pharmacy"),
                (2, "Metro Card", 45.00, "transport", "metro"),
                (4, "Cafe", 13.50, "lunchbreak", "cafe"),
                (12, "Cafe", 14.80, "lunchbreak", "cafe"),
                (19, "Cafe", 15.00, "lunchbreak", "cafe"),
                (24, "Restaurant", 180.00, "out", "restaurant"),
                (10, "Christmas Gifts", 350.00, "gifts", "christmas"),
                (15, "Gift", 85.00, "gifts", "birthday"),
                (31, "Restaurant", 120.00, "out", "restaurant"),
                (5, "Decoration", 65.00, "leisure", "mrleisure"),
            ],
        )?;
        de.update_all_summary_tables()?;
    }

    println!("  ✓ Expenses seeded (Jan–Dec 2025)");

    // -----------------------------------------------------------------------
    // 6. Cashflow: set income for each month, then recompute derived rows.
    // -----------------------------------------------------------------------
    let mut cashflow = Cashflow::new(year)?;

    // Dividends paid quarterly (March, June, September, December).
    let dividends = [
        0.0, 0.0, 120.0, // Q1
        0.0, 0.0, 120.0, // Q2
        0.0, 0.0, 120.0, // Q3
        0.0, 0.0, 150.0, // Q4
    ];

    for month in 1..=12u32 {
        cashflow.set_income(month, "Salary", 4_500.00)?;
        cashflow.set_income(month, "Interests Bank account", 8.50)?;
        cashflow.set_income(month, "Dividendi e Cedole", dividends[(month - 1) as usize])?;
    }
    cashflow.recompute()?;

    println!("  ✓ Cashflow seeded");

    // -----------------------------------------------------------------------
    // 7. Investment holdings — four assets across the year.
    // -----------------------------------------------------------------------
    let mut inv = InvestmentHoldings::new(year)?;

    inv.add_asset(
        "VWCE ETF",
        "Stocks/ETF",
        "https://finance.yahoo.com/quote/VWCE.DE",
    )?;
    inv.add_asset(
        "MSCI World ETF",
        "Stocks/ETF",
        "https://finance.yahoo.com/quote/IWDA.AS",
    )?;
    inv.add_asset("Gold", "Commodities", "")?;
    inv.add_asset(
        "EU Bond ETF",
        "Bonds",
        "https://finance.yahoo.com/quote/AGGH.MI",
    )?;

    // Monthly [Jan..Dec] quantity and price per asset.
    let vwce_qty = [
        50.0, 52.0, 54.0, 56.0, 58.0, 60.0, 62.0, 64.0, 66.0, 68.0, 70.0, 72.0,
    ];
    let vwce_price = [
        98.50, 101.20, 99.80, 103.50, 107.20, 110.40, 108.80, 112.30, 115.60, 118.20, 122.50,
        126.80,
    ];

    let msci_qty = [
        30.0, 30.0, 30.0, 32.0, 32.0, 34.0, 34.0, 34.0, 36.0, 36.0, 38.0, 38.0,
    ];
    let msci_price = [
        180.20, 185.40, 182.30, 188.50, 193.20, 197.80, 195.40, 200.10, 204.50, 208.30, 213.50,
        218.70,
    ];

    // Gold: oz, growing position with rising price
    let gold_qty = [3.0, 3.0, 4.0, 4.0, 4.0, 4.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0];
    let gold_price = [
        1_850.0, 1_880.0, 1_920.0, 1_895.0, 1_940.0, 1_965.0, 1_950.0, 2_010.0, 2_080.0, 2_120.0,
        2_160.0, 2_200.0,
    ];

    let bond_qty = [100.0f64; 12];
    let bond_price = [
        97.50, 97.80, 97.20, 97.60, 98.10, 98.40, 98.20, 98.60, 98.90, 99.10, 99.30, 99.50,
    ];

    for m in 1..=12usize {
        let month = m as u32;
        inv.set_quantity("VWCE ETF", month, vwce_qty[m - 1])?;
        inv.set_price("VWCE ETF", month, vwce_price[m - 1])?;
        inv.set_quantity("MSCI World ETF", month, msci_qty[m - 1])?;
        inv.set_price("MSCI World ETF", month, msci_price[m - 1])?;
        inv.set_quantity("Gold", month, gold_qty[m - 1])?;
        inv.set_price("Gold", month, gold_price[m - 1])?;
        inv.set_quantity("EU Bond ETF", month, bond_qty[m - 1])?;
        inv.set_price("EU Bond ETF", month, bond_price[m - 1])?;
    }

    println!("  ✓ Investments seeded");

    // -----------------------------------------------------------------------
    // 8. Liquidity — three accounts.
    // -----------------------------------------------------------------------
    let mut liq = Liquidity::new(year)?;

    liq.add_asset("Main Checking", "Bank/Broker account", "E")?;
    liq.add_asset("Savings Account", "Bank/Broker account", "E")?;
    liq.add_asset("Cash Wallet", "Cash", "E")?;

    // Main checking reflects spending fluctuations; savings grows steadily.
    let checking = [
        5_200.0, 5_850.0, 4_950.0, 5_400.0, 5_800.0, 5_600.0, 4_200.0, 4_800.0, 5_300.0, 5_700.0,
        5_400.0, 4_900.0,
    ];
    let savings = [
        18_000.0, 18_600.0, 18_600.0, 19_200.0, 19_800.0, 20_100.0, 20_100.0, 20_700.0, 21_200.0,
        21_700.0, 22_000.0, 22_000.0,
    ];
    let cash = [
        280.0, 350.0, 200.0, 420.0, 380.0, 310.0, 150.0, 220.0, 290.0, 350.0, 180.0, 260.0,
    ];

    for m in 1..=12usize {
        let month = m as u32;
        liq.set_value("Main Checking", month, checking[m - 1])?;
        liq.set_value("Savings Account", month, savings[m - 1])?;
        liq.set_value("Cash Wallet", month, cash[m - 1])?;
    }

    println!("  ✓ Liquidity seeded");

    // -----------------------------------------------------------------------
    // 9. Credits / Debts — mortgage (debt) + loan to a friend (credit).
    // -----------------------------------------------------------------------
    let mut cd = CreditsDebts::new(year)?;

    cd.add_entry("Mortgage", "E")?;
    cd.add_entry("Loan to Friend", "E")?;

    // Mortgage balance decreasing by ~€400/month (negative = liability).
    let mortgage = [
        -45_000.0, -44_600.0, -44_200.0, -43_800.0, -43_400.0, -43_000.0, -42_600.0, -42_200.0,
        -41_800.0, -41_400.0, -41_000.0, -40_600.0,
    ];
    let loan = [500.0f64; 12];

    for m in 1..=12usize {
        let month = m as u32;
        cd.set_value("Mortgage", month, mortgage[m - 1])?;
        cd.set_value("Loan to Friend", month, loan[m - 1])?;
    }

    println!("  ✓ Credits/Debts seeded");

    // -----------------------------------------------------------------------
    // Summary
    // -----------------------------------------------------------------------
    println!();
    println!("Mock data ready. Year: {year}");
    println!("  XDG_DATA_HOME   → {}", mock_abs.display());
    println!("  XDG_CONFIG_HOME → {}", mock_abs.display());
    println!();
    println!("Run the app with mock data:");
    println!("  ./run_mock.sh");

    Ok(())
}
