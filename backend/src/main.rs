//! Finguard Axum backend web server.
//!
//! Exposes a JSON REST API for finguard-zen to perform expenses, cashflow,
//! networth, and categories management operations using the backend Polars logic.
//!
//! This module is the HTTP surface only: it defines the route table, decodes
//! and validates request bodies/query strings into the DTOs below, and
//! translates domain errors ([`finguard_rs_backend::Error`], via [`AppError`])
//! into HTTP status codes. All persistence and business logic live one layer
//! down in [`finguard_rs_backend::df_operations`] (Parquet-backed dataframes)
//! and [`finguard_rs_backend::config`] (JSON category config); handlers here
//! should stay thin wrappers around those types.
//!
//! The `.route(...)` calls in [`main`] are the single source of truth for the
//! API surface. Grep for `.route(` rather than trusting a cached copy of the
//! route list in documentation, including `PROJECT_SUMMARY.md`.

use axum::{
    Json, Router,
    extract::{Path, Query},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tower_http::cors::CorsLayer;

use finguard_rs_backend::config;
use finguard_rs_backend::df_operations::{
    Cashflow, CreditsDebts, DetailedExpenses, InvestmentHoldings, Liquidity, RecurringExpenses,
};
use finguard_rs_backend::fx;
use finguard_rs_backend::plots;

mod http_error;
use http_error::AppError;

// ======================================================================
// JSON Serialisation Models
// ======================================================================
//
// These structs define the wire format of the REST API. Every one of them is
// mirrored by a hand-written TypeScript type in `frontend/src/services/types.ts`
// and consumed through the matching fetch call in `frontend/src/services/api.ts`.
// Adding, renaming, removing, or retyping a field here requires the matching
// change on the frontend side; there is no shared schema generation between
// the two, so nothing else will catch a mismatch until it fails at runtime.

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExpenseJson {
    pub id: String, // String representation of index "_idx"
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub name: String,
    pub amount: f64,
    pub currency: String,
    pub primary: String,
    pub secondary: String,
    /// Multiplier applied to `amount` to get the reference-currency amount
    /// (always 1.0 when `currency` already is the reference currency). This
    /// is derived at read time, not stored: the server always resolves it
    /// itself from `currency` and the effective `expense_date`. Ignored on
    /// write (`#[serde(default)]` so a request built against the old
    /// contract, without this field, still deserializes).
    #[serde(default)]
    pub fx_rate: f64,
    /// The date `fx_rate` was published for, as `"YYYY-MM-DD"`. Ignored on
    /// write, for the same reason as `fx_rate`.
    #[serde(default)]
    pub rate_date: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RecurringTemplateJson {
    pub id: String, // Stringified index "_idx"
    pub name: String,
    pub day: i64,
    pub amount: f64,
    pub currency: String,
    pub primary: String,
    pub secondary: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MappingRuleJson {
    pub id: String,
    pub match_str: String,
    pub primary: String,
    pub secondary: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CategoriesJson {
    pub primary: Vec<String>,
    pub secondary: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct QtyPrice {
    pub qty: f64,
    pub price: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InvestmentAssetJson {
    pub id: String, // asset name
    pub name: String,
    pub category: String,
    pub link: Option<String>,
    /// Defaults to the empty string for a client that predates this field, the
    /// same tolerance `fx_rate`/`rate_date` got in the earlier currency step.
    /// An empty string is a caller bug, not a valid currency; `InvestmentHoldings::new`
    /// itself never produces one, since it backfills from settings.
    #[serde(default)]
    pub currency: String,
    pub data: std::collections::HashMap<i32, std::collections::HashMap<u32, QtyPrice>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LiquidityRowJson {
    pub id: String, // asset name
    pub name: String,
    pub category: String,
    pub currency: String,
    pub data: std::collections::HashMap<i32, std::collections::HashMap<u32, f64>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CreditDebtRowJson {
    pub id: String, // name
    pub name: String,
    pub currency: String,
    pub data: std::collections::HashMap<i32, std::collections::HashMap<u32, f64>>,
}

/// One slice of [`NetworthAllocationJson`], mirroring [`plots::PieSlice`].
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NetworthPieSliceJson {
    pub name: String,
    pub value: f64,
}

/// `GET /api/networth/allocation` response body, mirroring [`plots::PieChart`].
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NetworthAllocationJson {
    pub slices: Vec<NetworthPieSliceJson>,
    /// Currencies present in the year's data that could not be resolved into
    /// the reference currency, and were therefore excluded from every slice
    /// rather than failing the whole request. Empty when everything resolved.
    pub unavailable_currencies: Vec<String>,
}

/// One stacked component of [`NetworthEvolutionJson`], mirroring
/// [`plots::Series`].
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NetworthSeriesJson {
    pub name: String,
    pub values: Vec<f64>,
}

/// `GET /api/networth/evolution` response body, mirroring
/// [`plots::NetworthEvolution`].
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NetworthEvolutionJson {
    pub months: Vec<String>,
    pub components: Vec<NetworthSeriesJson>,
    pub net_worth: Vec<f64>,
    /// Currencies present in the year's data that could not be resolved into
    /// the reference currency, and were therefore excluded from every
    /// component series and the total rather than failing the whole request.
    /// Empty when everything resolved.
    pub unavailable_currencies: Vec<String>,
}

/// `GET`/`PUT /api/settings/currency` request and response body, mirroring
/// [`config::CurrencySettings`].
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CurrencySettingsJson {
    pub reference_currency: String,
    /// `"previous_month_end"` or `"live"`; see [`config::CurrentMonthRateMode`].
    pub current_month_rate_mode: config::CurrentMonthRateMode,
}

/// One calendar month's resolved rates in [`MonthlyFxRatesJson`], keyed by
/// currency code.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MonthlyFxRateJson {
    pub month: u32,
    /// Currency code -> multiplier that converts an amount **in that
    /// currency** into [`MonthlyFxRatesJson::reference_currency`]. To convert
    /// a reference-currency amount into a display currency, divide by this
    /// value instead of multiplying. Never contains the reference currency
    /// itself (always an implicit `1.0`, so it is left out) or a currency
    /// listed in [`MonthlyFxRatesJson::unavailable_currencies`].
    pub rate_to_reference: std::collections::HashMap<String, f64>,
}

/// `GET /api/fx/monthly-rates` response body: one resolved rate per currency
/// per calendar month of `year`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MonthlyFxRatesJson {
    pub year: i32,
    /// Every rate in `months` converts *into* this currency; the numbers are
    /// meaningless without it.
    pub reference_currency: String,
    pub months: Vec<MonthlyFxRateJson>,
    /// Currencies that could not be resolved for any month, typically because
    /// the network is unavailable and nothing is cached yet. Omitted from
    /// `months` rather than failing the whole request, so a reference-only
    /// portfolio still gets a working response with no network access.
    pub unavailable_currencies: Vec<String>,
}

// ======================================================================
// Query/Payload Structs
// ======================================================================
//
// Request-side shapes (query strings and JSON bodies). Same cross-language
// contract as the DTOs above: each of these mirrors the request shape built
// by the corresponding call in `frontend/src/services/api.ts`.

#[derive(Deserialize, Debug)]
pub struct YearQuery {
    pub year: i32,
}

#[derive(Deserialize, Debug)]
pub struct NetworthAllocationQuery {
    pub year: i32,
    pub month: u32,
}

#[derive(Deserialize, Debug)]
pub struct MonthlyFxRatesQuery {
    pub year: i32,
    /// Comma-separated extra currency codes to resolve alongside whatever
    /// `year`'s investment, liquidity, and credits/debts rows already use
    /// (see [`networth_currencies`]). Absent or empty asks for exactly the
    /// currencies already present in that data.
    pub currencies: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct GetExpensesQuery {
    pub year: i32,
    pub month: Option<u32>,
    pub name: Option<String>,
    pub category: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Deserialize, Debug)]
pub struct DeleteExpenseQuery {
    pub year: i32,
    pub month: u32,
}

#[derive(Deserialize, Debug)]
pub struct ApplyRecurringPayload {
    pub year: i32,
    pub month: u32,
}

#[derive(Deserialize, Debug)]
pub struct AddRecurringPayload {
    pub year: i32,
    pub name: String,
    pub day: i64,
    pub amount: f64,
    pub currency: String,
    pub primary: String,
    pub secondary: String,
}

#[derive(Deserialize, Debug)]
pub struct AddCategoryPayload {
    pub kind: String, // "primary" or "secondary"
    pub name: String,
}

#[derive(Deserialize, Debug)]
pub struct KindQuery {
    pub kind: String, // "primary" or "secondary"
}

#[derive(Deserialize, Debug)]
pub struct SetIncomeCellPayload {
    pub year: i32,
    pub month: u32,
    pub category: String,
    pub amount: f64,
}

#[derive(Deserialize, Debug)]
pub struct SetInvestmentCellPayload {
    pub id: String,
    pub year: i32,
    pub month: u32,
    pub field: String, // "quantity" or "price"; see `InvestmentHoldings::set_quantity_or_price`
    pub value: f64,
}

#[derive(Deserialize, Debug)]
pub struct SetLiquidityCellPayload {
    pub id: String,
    pub year: i32,
    pub month: u32,
    pub value: f64,
}

#[derive(Deserialize, Debug)]
pub struct SetCreditDebtCellPayload {
    pub id: String,
    pub year: i32,
    pub month: u32,
    pub value: f64,
}

// ======================================================================
// Helper Functions
// ======================================================================

/// Read a required string column, propagating any error (missing column, wrong
/// dtype) to the caller. Nulls become `""`. Use this for columns the caller
/// cannot proceed without.
fn str_col_to_vec(
    df: &polars::prelude::DataFrame,
    name: &str,
) -> finguard_rs_backend::Result<Vec<String>> {
    Ok(df
        .column(name)?
        .str()?
        .iter()
        .map(|o| o.unwrap_or("").to_string())
        .collect())
}

/// Read a string column leniently: a missing column or dtype mismatch yields
/// an empty `Vec` instead of an error. Nulls become `""`. Used for columns
/// that may legitimately be absent from an older parquet file.
fn column_strings(df: &polars::prelude::DataFrame, name: &str) -> Vec<String> {
    match df.column(name).and_then(|c| c.str().cloned()) {
        Ok(s) => s.iter().map(|o| o.unwrap_or("").to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Read a numeric column as `f64`, casting if necessary. Missing column, a
/// non-numeric dtype, or nulls all fall back silently (missing column yields
/// an empty `Vec`; nulls become `0.0`), matching [`column_strings`]'s lenient
/// contract.
fn column_f64(df: &polars::prelude::DataFrame, name: &str) -> Vec<f64> {
    let Ok(col) = df.column(name) else {
        return Vec::new();
    };
    let casted = col.cast(&polars::prelude::DataType::Float64);
    match casted.as_ref().unwrap_or(col).f64() {
        Ok(s) => s.iter().map(|o| o.unwrap_or(0.0)).collect(),
        Err(_) => Vec::new(),
    }
}

/// Read a numeric column as `i64`, casting if necessary. Same lenient
/// fallbacks as [`column_f64`] (empty `Vec` if the column is missing or not
/// numeric, `0` for nulls).
fn column_i64(df: &polars::prelude::DataFrame, name: &str) -> Vec<i64> {
    let Ok(col) = df.column(name) else {
        return Vec::new();
    };
    let casted = col.cast(&polars::prelude::DataType::Int64);
    match casted.as_ref().unwrap_or(col).i64() {
        Ok(s) => s.iter().map(|o| o.unwrap_or(0)).collect(),
        Err(_) => Vec::new(),
    }
}

/// Read a `Date` column and return only the day-of-month component (1-31) of
/// each value, not the full date; `expense_date`'s year/month are already
/// known from the request (see [`get_expenses_handler`]), so only the day is
/// needed to build [`ExpenseJson`]. Same lenient fallbacks as [`column_f64`]
/// (empty `Vec` if the column is missing or not date-castable, `0` for nulls).
fn column_dates_day(df: &polars::prelude::DataFrame, name: &str) -> Vec<i32> {
    use chrono::Datelike;
    let Ok(col) = df.column(name) else {
        return Vec::new();
    };
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    match col.cast(&polars::prelude::DataType::Int32) {
        Ok(c) => match c.i32() {
            Ok(s) => s
                .iter()
                .map(|o| match o {
                    Some(days) => (epoch + chrono::Duration::days(days as i64)).day() as i32,
                    None => 0,
                })
                .collect(),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    }
}

/// Set `target_col` to `value` on every row where `key_col` equals `key`,
/// leaving other rows unchanged. Does not save; callers persist the mutated
/// dataframe (e.g. via `liq.save()`) afterward. Used by the metadata-update
/// handlers to patch a single field (such as `currency`) on an already-loaded
/// wide table without going through a dedicated setter on the domain type.
fn set_df_str_where(
    df: &mut polars::prelude::DataFrame,
    key_col: &str,
    key: &str,
    target_col: &str,
    value: &str,
) -> finguard_rs_backend::Result<()> {
    use polars::prelude::*;
    let updated = df
        .clone()
        .lazy()
        .with_column(
            when(col(key_col).eq(lit(key)))
                .then(lit(value))
                .otherwise(col(target_col))
                .alias(target_col),
        )
        .collect()?;
    *df = updated;
    Ok(())
}

// ======================================================================
// Handlers
// ======================================================================
//
// Grouped below by resource, matching the route groups in [`main`]. Each
// handler validates its Axum extractors (Axum itself rejects a request whose
// query string or JSON body cannot deserialize into the target struct before
// the handler body runs), calls into `df_operations`/`config`, and maps any
// resulting [`finguard_rs_backend::Error`] to an HTTP status through
// [`AppError`] (see `http_error.rs` for the status mapping).

/// List the years that have any data on disk, newest first.
///
/// Scans the immediate subdirectories of the dbs root and keeps the ones whose
/// name parses as an `i32`. Returns an empty list (never an error) if the dbs
/// root or its listing cannot be read, since "no years yet" is a normal state
/// for a fresh installation.
fn discover_years() -> Vec<i32> {
    let Ok(root) = finguard_rs_backend::paths::get_dbs_root() else {
        return vec![];
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return vec![];
    };
    let mut years: Vec<i32> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_type()
                .ok()
                .filter(|t| t.is_dir())
                .and_then(|_| e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()))
        })
        .collect();
    years.sort_unstable_by(|a, b| b.cmp(a));
    years
}

/// `GET /api/years`: list years with any on-disk data. See [`discover_years`].
async fn list_years_handler() -> Json<Vec<i32>> {
    Json(discover_years())
}

/// The reference currencies the frontend currency picker offers. An update
/// naming any other code is rejected before it reaches disk (see
/// [`update_currency_settings_handler`]), because every later rate lookup
/// ([`fx::rate_on`], [`fx::monthly_rates`]) resolves against whatever is
/// saved here, and an unsupported code would only fail those lookups later,
/// far from this request and hard to trace back to it.
const SUPPORTED_REFERENCE_CURRENCIES: [&str; 5] = ["EUR", "USD", "GBP", "CHF", "JPY"];

/// `GET /api/settings/currency`: the current reference currency and
/// in-progress-month rate mode (see [`config::CurrencySettings`]).
async fn get_currency_settings_handler() -> Result<Json<CurrencySettingsJson>, AppError> {
    let settings = config::get_currency_settings()?;
    Ok(Json(CurrencySettingsJson {
        reference_currency: settings.reference_currency,
        current_month_rate_mode: settings.current_month_rate_mode,
    }))
}

/// `PUT /api/settings/currency`: replace the reference currency and
/// in-progress-month rate mode, and return the saved values.
///
/// Returns [`Error::InvalidArgument`] (`400`) for a `payload.reference_currency`
/// outside [`SUPPORTED_REFERENCE_CURRENCIES`], without persisting it.
///
/// [`Error::InvalidArgument`]: finguard_rs_backend::Error::InvalidArgument
async fn update_currency_settings_handler(
    Json(payload): Json<CurrencySettingsJson>,
) -> Result<Json<CurrencySettingsJson>, AppError> {
    let reference_currency = payload.reference_currency.trim().to_uppercase();
    if !SUPPORTED_REFERENCE_CURRENCIES.contains(&reference_currency.as_str()) {
        return Err(finguard_rs_backend::Error::InvalidArgument(format!(
            "unsupported reference currency '{reference_currency}'; expected one of \
             {SUPPORTED_REFERENCE_CURRENCIES:?}"
        ))
        .into());
    }

    let settings = config::CurrencySettings {
        reference_currency,
        current_month_rate_mode: payload.current_month_rate_mode,
    };
    config::set_currency_settings(&settings)?;
    Ok(Json(CurrencySettingsJson {
        reference_currency: settings.reference_currency,
        current_month_rate_mode: settings.current_month_rate_mode,
    }))
}

/// `GET /api/expenses`: list expense rows for `year`, optionally narrowed to
/// one `month` (otherwise all 12 months are read and concatenated) and
/// further filtered by `name`/`category` substring and `min`/`max` amount.
///
/// A month with no parquet file yet is treated as empty (skipped), not an
/// error, so requesting a full year with partial data still succeeds. Each
/// returned [`ExpenseJson::id`] is that row's position in the month's
/// unfiltered dataframe, which [`delete_expense_handler`] and
/// [`upsert_expense_handler`] expect back verbatim.
async fn get_expenses_handler(
    Query(q): Query<GetExpensesQuery>,
) -> Result<Json<Vec<ExpenseJson>>, AppError> {
    // Each returned row's fixed fields, paired with the `ExpenseFact` needed
    // to resolve its reference-currency rate below. `fx_rate`/`rate_date`
    // are filled in once every row is collected, so distinct `(date,
    // currency)` pairs across the whole response can be resolved once each
    // instead of once per row (see `df_operations::distinct_rate_keys`).
    let mut rows: Vec<(ExpenseJson, finguard_rs_backend::df_operations::ExpenseFact)> = Vec::new();
    let months = if let Some(m) = q.month {
        vec![m]
    } else {
        (1..=12).collect()
    };

    for m in months {
        let de = match DetailedExpenses::new(q.year, m) {
            Ok(de) => de,
            Err(_) => continue,
        };

        let filtered_df =
            de.filter_expenses_indexed(q.name.as_deref(), q.category.as_deref(), q.min, q.max)?;

        let indices = column_i64(&filtered_df, "_idx");
        let names = column_strings(&filtered_df, "expense_name");
        let days = column_dates_day(&filtered_df, "expense_date");
        let amounts = column_f64(&filtered_df, "expense_amount");
        let currencies = column_strings(&filtered_df, "currency");
        let primaries = column_strings(&filtered_df, "primary_category");
        let secondaries = column_strings(&filtered_df, "secondary_category");

        for fi in 0..filtered_df.height() {
            let day = days.get(fi).copied().unwrap_or(1) as u32;
            let Some(expense_date) = chrono::NaiveDate::from_ymd_opt(q.year, m, day) else {
                continue;
            };
            let amount = amounts.get(fi).copied().unwrap_or(0.0);
            let currency = currencies.get(fi).cloned().unwrap_or_default();
            let primary = primaries.get(fi).cloned().unwrap_or_default();
            let secondary = secondaries.get(fi).cloned().unwrap_or_default();

            rows.push((
                ExpenseJson {
                    id: indices.get(fi).copied().unwrap_or(0).to_string(),
                    year: q.year,
                    month: m,
                    day,
                    name: names.get(fi).cloned().unwrap_or_default(),
                    amount,
                    currency: currency.clone(),
                    primary: primary.clone(),
                    secondary: secondary.clone(),
                    fx_rate: 0.0,
                    rate_date: String::new(),
                },
                finguard_rs_backend::df_operations::ExpenseFact {
                    expense_date,
                    currency,
                    expense_amount: amount,
                    primary_category: primary,
                    secondary_category: secondary,
                },
            ));
        }
    }

    let reference_currency = config::get_currency_settings()?.reference_currency;
    let facts: Vec<_> = rows.iter().map(|(_, fact)| fact.clone()).collect();
    let keys = finguard_rs_backend::df_operations::distinct_rate_keys(&facts, &reference_currency);
    let mut rates = std::collections::HashMap::with_capacity(keys.len());
    for (date, currency) in keys {
        let resolved = fx::rate_on(date, &currency).await?;
        rates.insert((date, currency), resolved);
    }

    let mut all = Vec::with_capacity(rows.len());
    for (mut expense, fact) in rows {
        let (rate, rate_date) =
            finguard_rs_backend::df_operations::resolve_fact(&fact, &reference_currency, &rates)?;
        expense.fx_rate = rate;
        expense.rate_date = rate_date.format("%Y-%m-%d").to_string();
        all.push(expense);
    }

    Ok(Json(all))
}

/// `POST /api/expenses`: create or update an expense row in
/// `payload.year`/`payload.month`'s monthly parquet file.
///
/// An empty `payload.id` means "create": the row is appended and the response
/// carries the new row's index as its `id`. A non-empty `id` means "update
/// the row at that index"; it must parse as a `u32` or this returns
/// [`Error::InvalidArgument`] (`400`). Either path rewrites the month's
/// parquet file; only `expense_amount`/`currency` and the other facts are
/// stored, since the reference-currency amount is derived at read time (see
/// [`get_expenses_handler`]) rather than stored here.
///
/// `payload.fx_rate`/`payload.rate_date` are ignored on write: the response
/// still reports the rate resolved fresh here, via [`fx::rate_on`], for
/// `payload.currency` on the expense's own date
/// (`payload.year`/`payload.month`/`payload.day`), not for today, purely as
/// provenance for the client that just saved the row.
async fn upsert_expense_handler(
    Json(payload): Json<ExpenseJson>,
) -> Result<Json<ExpenseJson>, AppError> {
    let year = payload.year;
    let month = payload.month;
    let expense_date =
        chrono::NaiveDate::from_ymd_opt(year, month, payload.day).ok_or_else(|| {
            finguard_rs_backend::Error::InvalidArgument(format!(
                "invalid date {year}-{month:02}-{:02}",
                payload.day
            ))
        })?;
    let resolved = fx::rate_on(expense_date, &payload.currency).await?;
    // `rate_date` is `None` only for the reference-currency identity case
    // (see `fx::ResolvedRate`), which has no external publication date to
    // report; the expense's own date is the correct value to report there.
    let rate_date = resolved.rate_date.unwrap_or(expense_date);

    let mut de = DetailedExpenses::new(year, month)?;

    if payload.id.is_empty() {
        de.add_row(
            &payload.name,
            payload.day,
            payload.amount,
            Some(&payload.primary),
            &payload.currency,
            Some(&payload.secondary),
        )?;

        let new_id = (de.expense_df.height() as i32 - 1).to_string();
        let mut response = payload;
        response.id = new_id;
        response.fx_rate = resolved.rate;
        response.rate_date = rate_date.format("%Y-%m-%d").to_string();
        Ok(Json(response))
    } else {
        let idx = payload
            .id
            .parse::<u32>()
            .map_err(|e| finguard_rs_backend::Error::InvalidArgument(e.to_string()))?;
        de.edit_row(
            idx,
            Some(&payload.name),
            Some(payload.day),
            Some(payload.amount),
            Some(&payload.currency),
            Some(&payload.primary),
            Some(&payload.secondary),
        )?;
        let mut response = payload;
        response.fx_rate = resolved.rate;
        response.rate_date = rate_date.format("%Y-%m-%d").to_string();
        Ok(Json(response))
    }
}

/// `DELETE /api/expenses/:id`: remove the expense at row index `id` from
/// `q.year`/`q.month`'s monthly parquet file. Category summaries are no
/// longer maintained as separate tables; [`get_expenses_handler`] derives
/// them at read time, so nothing else needs recomputing here.
///
/// Returns [`Error::InvalidArgument`] (`400`) if `id` does not parse as a
/// `u32`.
async fn delete_expense_handler(
    Path(id): Path<String>,
    Query(q): Query<DeleteExpenseQuery>,
) -> Result<(), AppError> {
    let idx = id
        .parse::<u32>()
        .map_err(|e| finguard_rs_backend::Error::InvalidArgument(e.to_string()))?;
    let mut de = DetailedExpenses::new(q.year, q.month)?;
    de.delete_row(idx)?;
    Ok(())
}

/// `GET /api/recurring`: list `q.year`'s recurring expense templates. Each
/// [`RecurringTemplateJson::id`] is that row's position in the year's
/// recurring-expenses table, which [`delete_recurring_handler`] expects back
/// verbatim.
async fn get_recurring_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<Vec<RecurringTemplateJson>>, AppError> {
    let rec = RecurringExpenses::new(q.year)?;
    let df = &rec.df;

    let names = column_strings(df, "expense_name");
    let days = column_i64(df, "expense_day");
    let amounts = column_f64(df, "expense_amount");
    let currencies = column_strings(df, "currency");
    let primaries = column_strings(df, "primary_category");
    let secondaries = column_strings(df, "secondary_category");

    let mut list = Vec::new();
    for i in 0..df.height() {
        list.push(RecurringTemplateJson {
            id: i.to_string(),
            name: names[i].clone(),
            day: days[i],
            amount: amounts[i],
            currency: currencies[i].clone(),
            primary: primaries[i].clone(),
            secondary: secondaries[i].clone(),
        });
    }
    Ok(Json(list))
}

/// `POST /api/recurring`: append a recurring expense template for
/// `payload.year` and save the table.
///
/// Returns [`Error::InvalidArgument`] (`400`) if `payload.day` is outside
/// 1..=28 (a template must fire in every month, including February).
async fn add_recurring_handler(
    Json(payload): Json<AddRecurringPayload>,
) -> Result<Json<RecurringTemplateJson>, AppError> {
    let mut rec = RecurringExpenses::new(payload.year)?;
    rec.add(
        &payload.name,
        payload.day,
        payload.amount,
        &payload.currency,
        &payload.primary,
        &payload.secondary,
    )?;

    let new_id = (rec.df.height() as i32 - 1).to_string();
    Ok(Json(RecurringTemplateJson {
        id: new_id,
        name: payload.name,
        day: payload.day,
        amount: payload.amount,
        currency: payload.currency,
        primary: payload.primary,
        secondary: payload.secondary,
    }))
}

/// `DELETE /api/recurring/:id`: remove the recurring template at row index
/// `id` from `q.year`'s table and save.
///
/// Returns [`Error::InvalidArgument`] (`400`) if `id` does not parse as a
/// `u32`.
async fn delete_recurring_handler(
    Path(id): Path<String>,
    Query(q): Query<YearQuery>,
) -> Result<(), AppError> {
    let idx = id
        .parse::<u32>()
        .map_err(|e| finguard_rs_backend::Error::InvalidArgument(e.to_string()))?;
    let mut rec = RecurringExpenses::new(q.year)?;
    rec.remove(idx)?;
    Ok(())
}

/// `POST /api/recurring/apply`: insert every recurring template for
/// `payload.year` into `payload.month`'s detailed-expenses table, skipping
/// templates that already have a matching row (same name and day-of-month) so
/// the endpoint is safe to call more than once for the same month. Returns
/// the number of rows actually added.
async fn apply_recurring_handler(
    Json(payload): Json<ApplyRecurringPayload>,
) -> Result<Json<u32>, AppError> {
    let rec = RecurringExpenses::new(payload.year)?;
    let mut de = DetailedExpenses::new(payload.year, payload.month)?;

    let pending = rec.pending_for_month(&de)?;
    let added_names = rec.insert_resolved(&mut de, &pending)?;
    Ok(Json(added_names.len() as u32))
}

/// `GET /api/mappings`: list every stored expense-name-to-category mapping.
/// Each [`MappingRuleJson::id`] is the mapping's lower-cased key (the expense
/// name), which [`delete_mapping_handler`] expects back.
async fn get_mappings_handler() -> Result<Json<Vec<MappingRuleJson>>, AppError> {
    let map = config::get_all_mappings()?;
    let mut list = Vec::new();
    for (name, m) in map {
        list.push(MappingRuleJson {
            id: name.clone(),
            match_str: name,
            primary: m.primary_category,
            secondary: m.secondary_category,
        });
    }
    Ok(Json(list))
}

/// `POST /api/mappings`: add or overwrite the mapping for `payload.match_str`.
///
/// Always overwrites an existing mapping for the same (trimmed, lower-cased)
/// key rather than returning [`Error::AlreadyExists`] (unlike
/// [`config::add_mapping`]'s general `overwrite` parameter, this handler
/// hardcodes `overwrite = true`). The response echoes back the stored
/// (trimmed, lower-cased) values, not the raw payload.
async fn add_mapping_handler(
    Json(payload): Json<MappingRuleJson>,
) -> Result<Json<MappingRuleJson>, AppError> {
    config::add_mapping(
        &payload.match_str,
        &payload.primary,
        &payload.secondary,
        true,
    )?;
    let key = payload.match_str.trim().to_lowercase();
    Ok(Json(MappingRuleJson {
        id: key.clone(),
        match_str: key,
        primary: payload.primary.trim().to_lowercase(),
        secondary: payload.secondary.trim().to_lowercase(),
    }))
}

/// `DELETE /api/mappings/:id`: remove the mapping keyed by `id`.
///
/// Returns [`Error::NotFound`] (`404`) if no mapping exists for that key.
async fn delete_mapping_handler(Path(id): Path<String>) -> Result<(), AppError> {
    config::remove_mapping(&id)?;
    Ok(())
}

/// `GET /api/categories`: return every manually registered primary and
/// secondary category name.
async fn get_categories_handler() -> Result<Json<CategoriesJson>, AppError> {
    let known = config::get_known_categories()?;
    Ok(Json(CategoriesJson {
        primary: known.primary,
        secondary: known.secondary,
    }))
}

/// `POST /api/categories/:kind`: register `payload.name` as a new category of
/// `kind` (`"primary"` or `"secondary"`; the `kind` in the path is
/// authoritative, `payload.kind` is not read) and return the updated category
/// lists.
///
/// Returns [`Error::InvalidArgument`] (`400`) for an unknown `kind`, or
/// [`Error::AlreadyExists`] (`409`) if the name is already registered for
/// that kind.
async fn add_category_handler(
    Path(kind): Path<String>,
    Json(payload): Json<AddCategoryPayload>,
) -> Result<Json<CategoriesJson>, AppError> {
    config::add_known_category(&payload.name, &kind)?;
    let known = config::get_known_categories()?;
    Ok(Json(CategoriesJson {
        primary: known.primary,
        secondary: known.secondary,
    }))
}

/// Return the cumulative reference-currency total for every category of
/// `kind`, summed across every stored monthly detailed-expense file, with
/// each row converted at its own date's rate. `kind` must be `"primary"` or
/// `"secondary"`.
///
/// Reads raw facts (see [`finguard_rs_backend::df_operations::all_expense_facts`])
/// rather than the persisted `primaries.parquet`/`secondaries.parquet`
/// summaries, so the result always reflects the *current* reference
/// currency: those files were denominated in whatever the reference
/// currency was when each was last written, which goes stale the moment a
/// user changes it (see [`upsert_expense_handler`]'s module-level rationale
/// for the same fix one level down, on a single row).
async fn category_totals_across_all_years(
    kind: &str,
) -> Result<std::collections::HashMap<String, f64>, AppError> {
    if kind != "primary" && kind != "secondary" {
        return Err(finguard_rs_backend::Error::InvalidArgument(format!(
            "kind must be 'primary' or 'secondary', got '{kind}'"
        ))
        .into());
    }

    let facts = finguard_rs_backend::df_operations::all_expense_facts()?;
    let reference_currency = config::get_currency_settings()?.reference_currency;
    let keys = finguard_rs_backend::df_operations::distinct_rate_keys(&facts, &reference_currency);
    let mut rates = std::collections::HashMap::with_capacity(keys.len());
    for (date, currency) in keys {
        let resolved = fx::rate_on(date, &currency).await?;
        rates.insert((date, currency), resolved);
    }

    let mut totals = std::collections::HashMap::new();
    for fact in &facts {
        let category = if kind == "primary" {
            &fact.primary_category
        } else {
            &fact.secondary_category
        };
        let (rate, _) =
            finguard_rs_backend::df_operations::resolve_fact(fact, &reference_currency, &rates)?;
        *totals.entry(category.clone()).or_insert(0.0) += fact.expense_amount * rate;
    }
    Ok(totals)
}

/// `DELETE /api/categories/:kind/:name`: unregister `name` from `kind`'s
/// known-category list and strip its row from every year's (now-unmaintained,
/// see [`category_totals_across_all_years`]) primaries or secondaries summary
/// file.
///
/// Refuses to delete (returns [`Error::InvalidArgument`], `400`) while the
/// category still has a nonzero total across all years (checked with a
/// `1e-9` tolerance for float rounding), so a category cannot be removed out
/// from under existing expenses.
async fn delete_category_handler(
    Path((kind, name)): Path<(String, String)>,
) -> Result<Json<CategoriesJson>, AppError> {
    let totals = category_totals_across_all_years(&kind).await?;
    let total = totals.get(&name).copied().unwrap_or(0.0);
    if total.abs() >= 1e-9 {
        return Err(finguard_rs_backend::Error::InvalidArgument(format!(
            "Cannot delete \"{name}\": it still has {} in existing expenses.",
            total
        ))
        .into());
    }

    config::remove_known_category(&name, &kind)?;
    finguard_rs_backend::df_operations::remove_category_from_all_summaries(&name, &kind)?;

    let known = config::get_known_categories()?;
    Ok(Json(CategoriesJson {
        primary: known.primary,
        secondary: known.secondary,
    }))
}

/// `GET /api/categories/totals?kind=`: return the cumulative reference-
/// currency total per category of `kind` (see
/// [`category_totals_across_all_years`]). Backs [`delete_category_handler`]'s
/// "still in use" check on the frontend before a user attempts a delete.
async fn get_category_totals_handler(
    Query(q): Query<KindQuery>,
) -> Result<Json<std::collections::HashMap<String, f64>>, AppError> {
    Ok(Json(category_totals_across_all_years(&q.kind).await?))
}

/// `GET /api/cashflow/income`: return `q.year`'s manually entered income
/// values as `{ month => { category => amount } }` for the four income
/// categories (`"Salary"`, `"Interests Bank account"`, `"Dividendi e Cedole"`,
/// `"Other"`). A category/month with no stored row reads as `0.0` rather than
/// being omitted.
async fn get_income_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<std::collections::HashMap<u32, std::collections::HashMap<String, f64>>>, AppError>
{
    let cf = Cashflow::new(q.year)?;
    let mut out = std::collections::HashMap::new();
    for m in 1..=12 {
        let mut m_map = std::collections::HashMap::new();
        for cat in &[
            "Salary",
            "Interests Bank account",
            "Dividendi e Cedole",
            "Other",
        ] {
            let val = cf.get_value(cat, &format!("{m:02}")).unwrap_or(0.0);
            m_map.insert((*cat).to_string(), val);
        }
        out.insert(m, m_map);
    }
    Ok(Json(out))
}

/// `POST /api/cashflow/income`: set one income cell (`payload.category` for
/// `payload.month` of `payload.year`), then recompute and save the derived
/// `Income`/`Spending`/`Saving`/`Saving %` rows for every month of the year.
///
/// Returns [`Error::InvalidArgument`] (`400`) if `payload.category` is not
/// one of the four known income categories or `payload.month` is outside
/// 1..=12.
async fn set_income_cell_handler(
    Json(payload): Json<SetIncomeCellPayload>,
) -> Result<(), AppError> {
    let mut cf = Cashflow::new(payload.year)?;
    cf.set_income(payload.month, &payload.category, payload.amount)?;
    Ok(())
}

/// `GET /api/cashflow/spending`: return `q.year`'s per-category spending as
/// `{ month => { category => amount } }`, computed at read time from
/// `q.year`'s monthly detailed-expense files (each row converted at its own
/// date's rate), rather than the persisted `primaries.parquet` summary,
/// which would report whatever the reference currency was when it was last
/// written (see [`category_totals_across_all_years`] for the same fix
/// applied to the all-years total).
///
/// Every month is present in the result even when it has no expense rows;
/// such months map to an empty category map rather than being omitted.
async fn get_monthly_spending_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<std::collections::HashMap<u32, std::collections::HashMap<String, f64>>>, AppError>
{
    let facts = finguard_rs_backend::df_operations::expense_facts_for_year(q.year)?;
    let reference_currency = config::get_currency_settings()?.reference_currency;
    let keys = finguard_rs_backend::df_operations::distinct_rate_keys(&facts, &reference_currency);
    let mut rates = std::collections::HashMap::with_capacity(keys.len());
    for (date, currency) in keys {
        let resolved = fx::rate_on(date, &currency).await?;
        rates.insert((date, currency), resolved);
    }

    let mut out = std::collections::HashMap::new();
    for m in 1..=12u32 {
        out.insert(m, std::collections::HashMap::new());
    }
    for fact in &facts {
        let (rate, _) =
            finguard_rs_backend::df_operations::resolve_fact(fact, &reference_currency, &rates)?;
        let month = chrono::Datelike::month(&fact.expense_date);
        let m_map = out.entry(month).or_default();
        *m_map.entry(fact.primary_category.clone()).or_insert(0.0) += fact.expense_amount * rate;
    }
    Ok(Json(out))
}

/// `GET /api/investments`: list `q.year`'s investment assets with their
/// monthly quantity/price pairs.
///
/// [`InvestmentAssetJson::data`] is nested `{ year => { month => QtyPrice } }`
/// even though this handler only ever fills in `q.year`, matching the
/// frontend's multi-year-capable data shape (see
/// `frontend/src/services/types.ts`). Missing or non-numeric monthly cells
/// read as `0.0` for both quantity and price.
async fn get_investments_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<Vec<InvestmentAssetJson>>, AppError> {
    let inv = InvestmentHoldings::new(q.year)?;
    let assets = str_col_to_vec(&inv.df, "asset_name")?;
    let categories = str_col_to_vec(&inv.df, "category")?;
    let links = str_col_to_vec(&inv.df, "link")?;
    let currencies = str_col_to_vec(&inv.df, "currency")?;

    let mut list = Vec::new();
    for (i, name) in assets.iter().enumerate() {
        let mut data_map = std::collections::HashMap::new();
        let mut m_map = std::collections::HashMap::new();
        for m in 1..=12 {
            let col = format!("{m:02}");
            let qty = inv
                .df
                .column(&col)
                .and_then(|c| c.cast(&polars::prelude::DataType::Float64))
                .and_then(|c| c.f64().map(|s| s.get(i).unwrap_or(0.0)))
                .unwrap_or(0.0);
            let price = inv
                .df_prices
                .column(&col)
                .and_then(|c| c.cast(&polars::prelude::DataType::Float64))
                .and_then(|c| c.f64().map(|s| s.get(i).unwrap_or(0.0)))
                .unwrap_or(0.0);
            m_map.insert(m, QtyPrice { qty, price });
        }
        data_map.insert(q.year, m_map);

        list.push(InvestmentAssetJson {
            id: name.clone(),
            name: name.clone(),
            category: categories
                .get(i)
                .cloned()
                .unwrap_or_else(|| "Stocks/ETF".to_string()),
            link: links.get(i).filter(|s| !s.is_empty()).cloned(),
            currency: currencies.get(i).cloned().unwrap_or_default(),
            data: data_map,
        });
    }
    Ok(Json(list))
}

/// `POST /api/investments`: add a new investment asset to `payload.year` with
/// all monthly quantities and prices initialized to `0.0`, then save.
///
/// An empty or absent `payload.currency` (the current frontend does not send
/// one yet) falls back to the configured reference currency, the same
/// backfill [`InvestmentHoldings::new`] applies to a pre-existing file.
///
/// Returns [`Error::InvalidArgument`] (`400`) for an unrecognized
/// `payload.category`, or [`Error::AlreadyExists`] (`409`) if an asset with
/// that name already exists for the year.
async fn add_investment_handler(
    Json(payload): Json<AddInvestmentPayload>,
) -> Result<Json<InvestmentAssetJson>, AppError> {
    let mut inv = InvestmentHoldings::new(payload.year)?;
    let link_str = payload.link.as_deref().unwrap_or("");
    let currency = if payload.currency.is_empty() {
        config::get_currency_settings()?.reference_currency
    } else {
        payload.currency.clone()
    };
    inv.add_asset(&payload.name, &payload.category, link_str, &currency)?;

    let mut data_map = std::collections::HashMap::new();
    let mut m_map = std::collections::HashMap::new();
    for m in 1..=12 {
        m_map.insert(
            m,
            QtyPrice {
                qty: 0.0,
                price: 0.0,
            },
        );
    }
    data_map.insert(payload.year, m_map);

    Ok(Json(InvestmentAssetJson {
        id: payload.name.clone(),
        name: payload.name,
        category: payload.category,
        link: payload.link,
        currency,
        data: data_map,
    }))
}

#[derive(Deserialize, Debug)]
pub struct AddInvestmentPayload {
    pub year: i32,
    pub name: String,
    pub category: String,
    pub link: Option<String>,
    /// Falls back to the reference currency in the handler when empty; see
    /// [`add_investment_handler`]. Defaulted so a client that predates this
    /// field still deserializes.
    #[serde(default)]
    pub currency: String,
}

#[derive(Deserialize, Debug)]
pub struct UpdateInvestmentPayload {
    pub year: i32,
    pub name: Option<String>,
    pub category: Option<String>,
    pub link: Option<String>,
    pub currency: Option<String>,
}

/// `PUT /api/investments/:id`: update metadata for the asset named `id` in
/// `payload.year`. Each field is applied only when present in the payload; a
/// present `payload.name` different from `id` renames the asset first, and
/// the rename takes effect before any `category`/`link`/`currency` update
/// below it, so those updates target the asset under its new name.
///
/// Unlike `category` and `link`, `currency` has no dedicated setter on
/// [`InvestmentHoldings`] (it lives on `df` only), so this handler patches it
/// directly with [`set_df_str_where`] and saves, matching
/// [`update_liquidity_meta_handler`].
///
/// Returns [`Error::NotFound`] (`404`) if `id` does not exist, or
/// [`Error::AlreadyExists`] (`409`) if renaming to `payload.name` collides
/// with an existing asset.
async fn update_investment_meta_handler(
    Path(id): Path<String>,
    Json(payload): Json<UpdateInvestmentPayload>,
) -> Result<(), AppError> {
    let mut inv = InvestmentHoldings::new(payload.year)?;
    let final_name = if let Some(new_name) = &payload.name {
        if new_name != &id {
            inv.rename_asset(&id, new_name)?;
        }
        new_name.clone()
    } else {
        id.clone()
    };

    if let Some(cat) = &payload.category {
        inv.set_category(&final_name, cat)?;
    }
    if let Some(cur) = &payload.currency {
        set_df_str_where(&mut inv.df, "asset_name", &final_name, "currency", cur)?;
        inv.save_df()?;
    }
    if let Some(lnk) = &payload.link {
        inv.set_link(&final_name, lnk)?;
    }
    Ok(())
}

/// `DELETE /api/investments/:id?year=`: remove the asset named `id` from both
/// the quantities and prices tables for `q.year`, then save. Removing a
/// nonexistent asset is not an error: the filter simply matches zero rows.
async fn delete_investment_handler(
    Path(id): Path<String>,
    Query(q): Query<DeleteAssetQuery>,
) -> Result<(), AppError> {
    let mut inv = InvestmentHoldings::new(q.year)?;
    inv.remove_asset(&id)?;
    Ok(())
}

#[derive(Deserialize, Debug)]
pub struct DeleteAssetQuery {
    pub year: i32,
}

/// `POST /api/investments/cell`: set one quantity or price cell for the asset
/// named `payload.id` in `payload.year`/`payload.month`, saving only the
/// affected table (`payload.field` selects `"quantity"` or `"price"`).
///
/// Returns [`Error::NotFound`] (`404`) if the asset does not exist,
/// [`Error::InvalidArgument`] (`400`) for an out-of-range month or an
/// unrecognized `payload.field`.
async fn set_investment_cell_handler(
    Json(payload): Json<SetInvestmentCellPayload>,
) -> Result<(), AppError> {
    let mut inv = InvestmentHoldings::new(payload.year)?;
    inv.set_quantity_or_price(&payload.id, payload.month, payload.value, &payload.field)?;
    Ok(())
}

/// `GET /api/liquidity`: list `q.year`'s liquidity (cash/bank) rows with
/// their monthly balances, nested `{ year => { month => amount } }` like
/// [`get_investments_handler`].
///
/// `category` and `currency` fall back to `"Bank/Broker account"` and
/// `"EUR"` only if the row index is out of bounds for those columns; in
/// practice this cannot happen because [`Liquidity::new`] always backfills a
/// `currency` column of matching length before this handler runs.
async fn get_liquidity_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<Vec<LiquidityRowJson>>, AppError> {
    let liq = Liquidity::new(q.year)?;
    let assets = str_col_to_vec(&liq.df, "asset_name")?;
    let categories = str_col_to_vec(&liq.df, "category")?;
    let currencies = str_col_to_vec(&liq.df, "currency")?;

    let mut list = Vec::new();
    for (i, name) in assets.iter().enumerate() {
        let mut data_map = std::collections::HashMap::new();
        let mut m_map = std::collections::HashMap::new();
        for m in 1..=12 {
            let col = format!("{m:02}");
            let val = liq
                .df
                .column(&col)
                .and_then(|c| c.cast(&polars::prelude::DataType::Float64))
                .and_then(|c| c.f64().map(|s| s.get(i).unwrap_or(0.0)))
                .unwrap_or(0.0);
            m_map.insert(m, val);
        }
        data_map.insert(q.year, m_map);

        list.push(LiquidityRowJson {
            id: name.clone(),
            name: name.clone(),
            category: categories
                .get(i)
                .cloned()
                .unwrap_or_else(|| "Bank/Broker account".to_string()),
            currency: currencies
                .get(i)
                .cloned()
                .unwrap_or_else(|| "EUR".to_string()),
            data: data_map,
        });
    }
    Ok(Json(list))
}

#[derive(Deserialize, Debug)]
pub struct AddLiquidityPayload {
    pub year: i32,
    pub name: String,
    pub category: String,
    pub currency: String,
}

/// `POST /api/liquidity`: add a new liquidity row to `payload.year` with all
/// monthly values initialized to `0.0`, then save.
///
/// Returns [`Error::InvalidArgument`] (`400`) for an unrecognized
/// `payload.category`, or [`Error::AlreadyExists`] (`409`) if an asset with
/// that name already exists for the year.
async fn add_liquidity_handler(
    Json(payload): Json<AddLiquidityPayload>,
) -> Result<Json<LiquidityRowJson>, AppError> {
    let mut liq = Liquidity::new(payload.year)?;
    liq.add_asset(&payload.name, &payload.category, &payload.currency)?;

    let mut data_map = std::collections::HashMap::new();
    let mut m_map = std::collections::HashMap::new();
    for m in 1..=12 {
        m_map.insert(m, 0.0);
    }
    data_map.insert(payload.year, m_map);

    Ok(Json(LiquidityRowJson {
        id: payload.name.clone(),
        name: payload.name,
        category: payload.category,
        currency: payload.currency,
        data: data_map,
    }))
}

#[derive(Deserialize, Debug)]
pub struct UpdateLiquidityPayload {
    pub year: i32,
    pub name: Option<String>,
    pub category: Option<String>,
    pub currency: Option<String>,
}

/// `PUT /api/liquidity/:id`: update metadata for the liquidity row named `id`
/// in `payload.year`. Same field-by-field, rename-first semantics as
/// [`update_investment_meta_handler`]. Unlike `category` and `name`,
/// `currency` has no dedicated setter on [`Liquidity`], so this handler
/// patches it directly with [`set_df_str_where`] and saves explicitly.
///
/// Returns [`Error::NotFound`] (`404`) if `id` does not exist, or
/// [`Error::AlreadyExists`] (`409`) if renaming to `payload.name` collides
/// with an existing row.
async fn update_liquidity_meta_handler(
    Path(id): Path<String>,
    Json(payload): Json<UpdateLiquidityPayload>,
) -> Result<(), AppError> {
    let mut liq = Liquidity::new(payload.year)?;
    let final_name = if let Some(new_name) = &payload.name {
        if new_name != &id {
            liq.rename_asset(&id, new_name)?;
        }
        new_name.clone()
    } else {
        id.clone()
    };

    if let Some(cat) = &payload.category {
        liq.set_category(&final_name, cat)?;
    }
    if let Some(cur) = &payload.currency {
        set_df_str_where(&mut liq.df, "asset_name", &final_name, "currency", cur)?;
        liq.save()?;
    }
    Ok(())
}

/// `DELETE /api/liquidity/:id?year=`: remove the liquidity row named `id` for
/// `q.year`, then save. Removing a nonexistent row is not an error.
async fn delete_liquidity_handler(
    Path(id): Path<String>,
    Query(q): Query<DeleteAssetQuery>,
) -> Result<(), AppError> {
    let mut liq = Liquidity::new(q.year)?;
    liq.remove_asset(&id)?;
    Ok(())
}

/// `POST /api/liquidity/cell`: set the balance for `payload.id` in
/// `payload.year`/`payload.month`, then save.
///
/// Returns [`Error::NotFound`] (`404`) if the row does not exist,
/// [`Error::InvalidArgument`] (`400`) for an out-of-range month.
async fn set_liquidity_cell_handler(
    Json(payload): Json<SetLiquidityCellPayload>,
) -> Result<(), AppError> {
    let mut liq = Liquidity::new(payload.year)?;
    liq.set_value(&payload.id, payload.month, payload.value)?;
    Ok(())
}

/// `GET /api/credits_debts`: list `q.year`'s credit/debt entries with their
/// monthly outstanding amounts, nested `{ year => { month => amount } }` like
/// [`get_investments_handler`]. A positive amount is a credit owed to the
/// user; a negative amount is a debt owed by the user (see [`CreditsDebts`]).
/// `currency` defaults to `"EUR"` only on an out-of-bounds index, which
/// should not happen since [`CreditsDebts::new`] always backfills a
/// `currency` column of matching length before this handler runs.
async fn get_credits_debts_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<Vec<CreditDebtRowJson>>, AppError> {
    let cd = CreditsDebts::new(q.year)?;
    let entries = str_col_to_vec(&cd.df, "name")?;
    let currencies = str_col_to_vec(&cd.df, "currency")?;

    let mut list = Vec::new();
    for (i, name) in entries.iter().enumerate() {
        let mut data_map = std::collections::HashMap::new();
        let mut m_map = std::collections::HashMap::new();
        for m in 1..=12 {
            let col = format!("{m:02}");
            let val = cd
                .df
                .column(&col)
                .and_then(|c| c.cast(&polars::prelude::DataType::Float64))
                .and_then(|c| c.f64().map(|s| s.get(i).unwrap_or(0.0)))
                .unwrap_or(0.0);
            m_map.insert(m, val);
        }
        data_map.insert(q.year, m_map);

        list.push(CreditDebtRowJson {
            id: name.clone(),
            name: name.clone(),
            currency: currencies
                .get(i)
                .cloned()
                .unwrap_or_else(|| "EUR".to_string()),
            data: data_map,
        });
    }
    Ok(Json(list))
}

#[derive(Deserialize, Debug)]
pub struct AddCreditDebtPayload {
    pub year: i32,
    pub name: String,
    pub currency: String,
}

/// `POST /api/credits_debts`: add a new credit/debt entry to `payload.year`
/// with all monthly amounts initialized to `0.0`, then save.
///
/// Returns [`Error::AlreadyExists`] (`409`) if an entry with that name
/// already exists for the year.
async fn add_credit_debt_handler(
    Json(payload): Json<AddCreditDebtPayload>,
) -> Result<Json<CreditDebtRowJson>, AppError> {
    let mut cd = CreditsDebts::new(payload.year)?;
    cd.add_entry(&payload.name, &payload.currency)?;

    let mut data_map = std::collections::HashMap::new();
    let mut m_map = std::collections::HashMap::new();
    for m in 1..=12 {
        m_map.insert(m, 0.0);
    }
    data_map.insert(payload.year, m_map);

    Ok(Json(CreditDebtRowJson {
        id: payload.name.clone(),
        name: payload.name,
        currency: payload.currency,
        data: data_map,
    }))
}

#[derive(Deserialize, Debug)]
pub struct UpdateCreditDebtPayload {
    pub year: i32,
    pub name: Option<String>,
    pub currency: Option<String>,
}

/// `PUT /api/credits_debts/:id`: update metadata for the entry named `id` in
/// `payload.year`. Same field-by-field, rename-first semantics as
/// [`update_investment_meta_handler`]; like liquidity, `currency` is patched
/// directly with [`set_df_str_where`] since [`CreditsDebts`] has no dedicated
/// currency setter.
///
/// Returns [`Error::NotFound`] (`404`) if `id` does not exist, or
/// [`Error::AlreadyExists`] (`409`) if renaming to `payload.name` collides
/// with an existing entry.
async fn update_credit_debt_meta_handler(
    Path(id): Path<String>,
    Json(payload): Json<UpdateCreditDebtPayload>,
) -> Result<(), AppError> {
    let mut cd = CreditsDebts::new(payload.year)?;
    let final_name = if let Some(new_name) = &payload.name {
        if new_name != &id {
            cd.rename_entry(&id, new_name)?;
        }
        new_name.clone()
    } else {
        id.clone()
    };

    if let Some(cur) = &payload.currency {
        set_df_str_where(&mut cd.df, "name", &final_name, "currency", cur)?;
        cd.save()?;
    }
    Ok(())
}

/// `DELETE /api/credits_debts/:id?year=`: remove the entry named `id` for
/// `q.year`, then save. Removing a nonexistent entry is not an error.
async fn delete_credits_debts_handler(
    Path(id): Path<String>,
    Query(q): Query<DeleteAssetQuery>,
) -> Result<(), AppError> {
    let mut cd = CreditsDebts::new(q.year)?;
    cd.remove_entry(&id)?;
    Ok(())
}

/// `POST /api/credits_debts/cell`: set the outstanding amount for
/// `payload.id` in `payload.year`/`payload.month`, then save.
///
/// Returns [`Error::NotFound`] (`404`) if the entry does not exist,
/// [`Error::InvalidArgument`] (`400`) for an out-of-range month.
async fn set_credits_debts_cell_handler(
    Json(payload): Json<SetCreditDebtCellPayload>,
) -> Result<(), AppError> {
    let mut cd = CreditsDebts::new(payload.year)?;
    cd.set_value(&payload.id, payload.month, payload.value)?;
    Ok(())
}

/// Collect every distinct currency code across `year`'s investment holdings,
/// liquidity, and credits/debts rows, so [`fx::monthly_rates`] resolves
/// exactly the currencies the net-worth charts need and nothing else.
fn networth_currencies(year: i32) -> finguard_rs_backend::Result<Vec<String>> {
    let inv = InvestmentHoldings::new(year)?;
    let liq = Liquidity::new(year)?;
    let cd = CreditsDebts::new(year)?;

    let mut currencies = str_col_to_vec(&inv.df, "currency")?;
    currencies.extend(str_col_to_vec(&liq.df, "currency")?);
    currencies.extend(str_col_to_vec(&cd.df, "currency")?);
    currencies.sort();
    currencies.dedup();
    Ok(currencies)
}

/// `GET /api/networth/evolution?year=`: the net-worth evolution line chart
/// for `q.year` (see [`plots::networth_evolution_line`]), with every row
/// converted into the reference currency month by month. `null` only when
/// `q.year` truly has no rows: every net-worth value is zero *and* nothing
/// was dropped for being unresolvable.
///
/// A currency present in `q.year`'s data that cannot be resolved (typically
/// no network and nothing cached) does not fail the request: its rows are
/// excluded from every component and the total instead, and the currency is
/// named in [`NetworthEvolutionJson::unavailable_currencies`]. That case
/// still returns a non-`null` body (`months` populated, every component
/// zero) rather than `null`, so the client can tell "nothing to show because
/// these currencies could not be converted" apart from a genuinely empty
/// year. Matches [`get_monthly_fx_rates_handler`]'s degrade-per-currency
/// behavior, since both endpoints serve the same page.
async fn get_networth_evolution_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<Option<NetworthEvolutionJson>>, AppError> {
    let currencies = networth_currencies(q.year)?;
    let (rates, unavailable_currencies) = fx::monthly_rates_lenient(q.year, &currencies).await?;
    let evolution = plots::networth_evolution_line(q.year, &rates, &unavailable_currencies)?;
    Ok(Json(evolution.map(|e| {
        NetworthEvolutionJson {
            months: e.months,
            components: e
                .components
                .into_iter()
                .map(|s| NetworthSeriesJson {
                    name: s.name,
                    values: s.values,
                })
                .collect(),
            net_worth: e.net_worth,
            unavailable_currencies,
        }
    })))
}

/// `GET /api/networth/allocation?year=&month=`: the net-worth allocation pie
/// chart for `q.year`/`q.month` (see [`plots::networth_allocation_pie`]),
/// with every row converted into the reference currency. `null` only when no
/// slice qualifies *and* nothing was dropped for being unresolvable.
///
/// See [`get_networth_evolution_handler`] for the same per-currency degrade
/// behavior: an unresolvable currency's rows are excluded rather than
/// failing the request, named in
/// [`NetworthAllocationJson::unavailable_currencies`], and that case still
/// returns a non-`null` body (an empty `slices` list) instead of `null`.
async fn get_networth_allocation_handler(
    Query(q): Query<NetworthAllocationQuery>,
) -> Result<Json<Option<NetworthAllocationJson>>, AppError> {
    let currencies = networth_currencies(q.year)?;
    let (rates, unavailable_currencies) = fx::monthly_rates_lenient(q.year, &currencies).await?;
    let pie = plots::networth_allocation_pie(q.year, q.month, &rates, &unavailable_currencies)?;
    Ok(Json(pie.map(|p| {
        NetworthAllocationJson {
            slices: p
                .slices
                .into_iter()
                .map(|s| NetworthPieSliceJson {
                    name: s.name,
                    value: s.value,
                })
                .collect(),
            unavailable_currencies,
        }
    })))
}

/// `GET /api/fx/monthly-rates?year=&currencies=`: one resolved rate per
/// currency per calendar month of `year`, so a client can convert an amount
/// already stored in the reference currency into a chosen display currency
/// at each month's own rate, rather than one current rate.
///
/// Resolves every currency already present in `year`'s investment, liquidity,
/// and credits/debts rows (see [`networth_currencies`]), plus every code in
/// `q.currencies` (a comma-separated list). Every rate in the response
/// converts *from* the named currency *into* [`CurrencySettingsJson::reference_currency`]:
/// see [`MonthlyFxRateJson::rate_to_reference`] for the exact direction and
/// how to invert it.
///
/// A currency that cannot be resolved for any month, typically because the
/// network is unavailable and nothing is cached yet, is left out of `months`
/// and reported in [`MonthlyFxRatesJson::unavailable_currencies`] instead of
/// failing the whole request; a portfolio held entirely in the reference
/// currency never triggers a lookup at all, so it always gets a complete
/// response with no network access.
async fn get_monthly_fx_rates_handler(
    Query(q): Query<MonthlyFxRatesQuery>,
) -> Result<Json<MonthlyFxRatesJson>, AppError> {
    let reference_currency = config::get_currency_settings()?.reference_currency;

    let mut currencies = networth_currencies(q.year)?;
    if let Some(extra) = &q.currencies {
        currencies.extend(
            extra
                .split(',')
                .map(|c| c.trim().to_uppercase())
                .filter(|c| !c.is_empty()),
        );
    }
    currencies.sort();
    currencies.dedup();

    let (rates, unavailable_currencies) = fx::monthly_rates_lenient(q.year, &currencies).await?;

    // `fx::monthly_rates_lenient` already resolved (or skipped) every
    // currency, so this just reshapes its per-currency table into the
    // per-month shape the response uses.
    let months: Vec<MonthlyFxRateJson> = (1..=12u32)
        .map(|month| {
            let mut rate_to_reference = std::collections::HashMap::new();
            for currency in &currencies {
                if currency == &reference_currency {
                    continue;
                }
                if let Ok(rate) = rates.rate(month, currency) {
                    rate_to_reference.insert(currency.clone(), rate);
                }
            }
            MonthlyFxRateJson {
                month,
                rate_to_reference,
            }
        })
        .collect();

    Ok(Json(MonthlyFxRatesJson {
        year: q.year,
        reference_currency,
        months,
        unavailable_currencies,
    }))
}

// ======================================================================
// Server Initialization
// ======================================================================

/// Build the route table and CORS layer, then bind and serve.
///
/// `FINGUARD_HOST`/`FINGUARD_PORT` override the default bind address
/// (`127.0.0.1:3111`); both are read once at startup, not per request. CORS
/// is fully permissive (any origin, method, header), which accommodates the
/// dev frontend running on a different port (`:5173`); this has no
/// authentication or origin restriction of its own, so exposing this port
/// beyond a trusted local network or reverse proxy would let any origin call
/// the API.
#[tokio::main]
async fn main() {
    let cors = CorsLayer::permissive();

    let app = Router::new()
        // Configuration / global APIs
        .route("/api/years", get(list_years_handler))
        .route("/api/categories", get(get_categories_handler))
        .route("/api/categories/:kind", post(add_category_handler))
        .route(
            "/api/categories/:kind/:name",
            delete(delete_category_handler),
        )
        .route("/api/categories/totals", get(get_category_totals_handler))
        .route(
            "/api/settings/currency",
            get(get_currency_settings_handler).put(update_currency_settings_handler),
        )
        // Expense transactions
        .route(
            "/api/expenses",
            get(get_expenses_handler).post(upsert_expense_handler),
        )
        .route("/api/expenses/:id", delete(delete_expense_handler))
        // Recurring Templates
        .route(
            "/api/recurring",
            get(get_recurring_handler).post(add_recurring_handler),
        )
        .route("/api/recurring/:id", delete(delete_recurring_handler))
        .route("/api/recurring/apply", post(apply_recurring_handler))
        // Mappings
        .route(
            "/api/mappings",
            get(get_mappings_handler).post(add_mapping_handler),
        )
        .route("/api/mappings/:id", delete(delete_mapping_handler))
        // Cashflow
        .route(
            "/api/cashflow/income",
            get(get_income_handler).post(set_income_cell_handler),
        )
        .route("/api/cashflow/spending", get(get_monthly_spending_handler))
        // Net Worth - Investments
        .route(
            "/api/investments",
            get(get_investments_handler).post(add_investment_handler),
        )
        .route(
            "/api/investments/:id",
            put(update_investment_meta_handler).delete(delete_investment_handler),
        )
        .route("/api/investments/cell", post(set_investment_cell_handler))
        // Net Worth - Liquidity
        .route(
            "/api/liquidity",
            get(get_liquidity_handler).post(add_liquidity_handler),
        )
        .route(
            "/api/liquidity/:id",
            put(update_liquidity_meta_handler).delete(delete_liquidity_handler),
        )
        .route("/api/liquidity/cell", post(set_liquidity_cell_handler))
        // Net Worth - Credits/Debts
        .route(
            "/api/credits_debts",
            get(get_credits_debts_handler).post(add_credit_debt_handler),
        )
        .route(
            "/api/credits_debts/:id",
            put(update_credit_debt_meta_handler).delete(delete_credits_debts_handler),
        )
        .route(
            "/api/credits_debts/cell",
            post(set_credits_debts_cell_handler),
        )
        // Net Worth - Charts
        .route(
            "/api/networth/evolution",
            get(get_networth_evolution_handler),
        )
        .route(
            "/api/networth/allocation",
            get(get_networth_allocation_handler),
        )
        .route("/api/fx/monthly-rates", get(get_monthly_fx_rates_handler))
        .layer(cors);

    let host = std::env::var("FINGUARD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("FINGUARD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3111);

    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .expect("Invalid address");

    println!("Finguard server running on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points `XDG_DATA_HOME`, `XDG_CONFIG_HOME` and `HOME` at a fresh temp
    /// dir and forces offline FX mode, so tests never touch real user data or
    /// make a network call. Matches `fx.rs`'s own `with_temp_env_offline`.
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

    /// Write the FX rate cache directly at its on-disk path (see the module
    /// docs on `finguard_rs_backend::fx`), since the cache's `RateCache` type
    /// and (de)serialization helpers are private to that module and this is
    /// the binary crate, not the library crate, so it cannot reach them even
    /// via `pub(crate)`.
    fn seed_fx_cache(dates: &[(&str, &[(&str, f64)])]) {
        let mut days = Vec::new();
        for (date, pairs) in dates {
            let entries: Vec<String> = pairs
                .iter()
                .map(|(currency, rate)| format!("\"{currency}\":{rate}"))
                .collect();
            days.push(format!("\"{date}\":{{{}}}", entries.join(",")));
        }
        let body = format!("{{\"base\":\"EUR\",\"rates\":{{{}}}}}", days.join(","));
        let path = finguard_rs_backend::paths::get_fx_rates_path().expect("fx rates path");
        std::fs::write(path, body).expect("seed fx cache");
    }

    /// `POST /api/expenses` for a reference-currency row must report rate
    /// `1.0` in the response, and must succeed offline with nothing cached:
    /// the identity case needs no lookup at all.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn upsert_expense_reports_identity_rate_for_reference_currency() {
        let _temp = with_temp_env_offline();

        let response = upsert_expense_handler(Json(ExpenseJson {
            id: String::new(),
            year: 2026,
            month: 9,
            day: 4,
            name: "Rent".to_string(),
            amount: 1_000.0,
            currency: "EUR".to_string(),
            primary: "Housing".to_string(),
            secondary: "Rent".to_string(),
            fx_rate: 0.0,
            rate_date: String::new(),
        }))
        .await
        .unwrap_or_else(|AppError(err)| panic!("upsert succeeds: {err}"));

        assert_eq!(response.0.fx_rate, 1.0);
        assert_eq!(response.0.rate_date, "2026-09-04");
    }

    /// `POST /api/expenses` for a foreign-currency row must report the rate
    /// and date resolved for the expense's *own* date, not for today: this
    /// pins the row-level fix `main.rs` module docs describe, independent
    /// of whatever the real current date happens to be when the test runs.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn upsert_expense_reports_the_rate_resolved_for_its_own_date() {
        let _temp = with_temp_env_offline();
        seed_fx_cache(&[("2026-09-04", &[("USD", 1.20)])]);

        let response = upsert_expense_handler(Json(ExpenseJson {
            id: String::new(),
            year: 2026,
            month: 9,
            day: 4,
            name: "Groceries".to_string(),
            amount: 100.0,
            currency: "USD".to_string(),
            primary: "Groceries".to_string(),
            secondary: "OtherGroceries".to_string(),
            fx_rate: 0.0,
            rate_date: String::new(),
        }))
        .await
        .unwrap_or_else(|AppError(err)| panic!("upsert succeeds: {err}"));

        assert_eq!(response.0.fx_rate, 1.0 / 1.20);
        assert_eq!(response.0.rate_date, "2026-09-04");
    }

    /// `GET /api/expenses` must resolve every row's rate itself at read time
    /// (no `fx_rate`/`fx_date` is stored), reusing the same rate for two
    /// rows sharing a `(date, currency)` pair.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn get_expenses_resolves_rates_at_read_time() {
        let _temp = with_temp_env_offline();
        seed_fx_cache(&[("2026-09-04", &[("USD", 1.20)])]);

        let mut de = DetailedExpenses::new(2026, 9).expect("load detailed expenses");
        de.add_row("Rent", 4, 1_000.0, Some("Housing"), "EUR", Some("Rent"))
            .expect("add EUR row");
        de.add_row(
            "Groceries",
            4,
            100.0,
            Some("Groceries"),
            "USD",
            Some("OtherGroceries"),
        )
        .expect("add USD row");

        let expenses = get_expenses_handler(Query(GetExpensesQuery {
            year: 2026,
            month: Some(9),
            name: None,
            category: None,
            min: None,
            max: None,
        }))
        .await
        .unwrap_or_else(|AppError(err)| panic!("get succeeds: {err}"))
        .0;

        let eur_row = expenses.iter().find(|e| e.currency == "EUR").unwrap();
        assert_eq!(eur_row.fx_rate, 1.0);
        let usd_row = expenses.iter().find(|e| e.currency == "USD").unwrap();
        assert_eq!(usd_row.fx_rate, 1.0 / 1.20);
        assert_eq!(usd_row.rate_date, "2026-09-04");
    }

    /// Category totals summed across a mix of currencies must weight each
    /// row by its own date's rate rather than summing raw amounts, so a
    /// `100` EUR row plus a `100` USD row at `0.80` totals `180`, not `200`.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn category_totals_weight_each_row_by_its_own_currency() {
        let _temp = with_temp_env_offline();
        seed_fx_cache(&[("2026-09-04", &[("USD", 1.0 / 0.80)])]);

        let mut de = DetailedExpenses::new(2026, 9).expect("load detailed expenses");
        de.add_row("Rent", 4, 100.0, Some("Housing"), "EUR", Some("Rent"))
            .expect("add EUR row");
        de.add_row("Groceries", 4, 100.0, Some("Housing"), "USD", Some("Rent"))
            .expect("add USD row");

        let totals = category_totals_across_all_years("primary")
            .await
            .unwrap_or_else(|AppError(err)| panic!("category totals succeed: {err}"));
        assert_eq!(totals.get("Housing").copied(), Some(180.0));
    }

    /// Changing the reference currency must change the reported category
    /// total immediately, with no rewrite of any stored expense row: this is
    /// the entire point of deriving reference-currency amounts at read time
    /// instead of storing them.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn changing_reference_currency_changes_totals_with_no_data_rewrite() {
        let _temp = with_temp_env_offline();
        // EUR -> USD at 1.20, and (for the later GBP switch) EUR -> GBP at 0.85.
        seed_fx_cache(&[("2026-09-04", &[("USD", 1.20), ("GBP", 0.85)])]);

        let mut de = DetailedExpenses::new(2026, 9).expect("load detailed expenses");
        de.add_row(
            "Groceries",
            4,
            100.0,
            Some("Groceries"),
            "EUR",
            Some("OtherGroceries"),
        )
        .expect("add EUR row");
        let stored_amount_before = de
            .expense_df
            .column("expense_amount")
            .unwrap()
            .f64()
            .unwrap()
            .get(0);

        let eur_totals = category_totals_across_all_years("primary")
            .await
            .unwrap_or_else(|AppError(err)| panic!("EUR totals succeed: {err}"));
        assert_eq!(eur_totals.get("Groceries").copied(), Some(100.0));

        config::set_currency_settings(&config::CurrencySettings {
            reference_currency: "GBP".to_string(),
            current_month_rate_mode: Default::default(),
        })
        .expect("switch reference currency to GBP");

        let gbp_totals = category_totals_across_all_years("primary")
            .await
            .unwrap_or_else(|AppError(err)| panic!("GBP totals succeed: {err}"));
        assert_eq!(gbp_totals.get("Groceries").copied(), Some(85.0));

        // The stored row itself must be byte-for-byte unchanged by the
        // settings switch: no write happened, only the read-time conversion.
        let reloaded = DetailedExpenses::new(2026, 9).expect("reload detailed expenses");
        assert_eq!(
            reloaded
                .expense_df
                .column("expense_amount")
                .unwrap()
                .f64()
                .unwrap()
                .get(0),
            stored_amount_before
        );
    }

    /// `PUT /api/investments/:id` must update the stored currency when the
    /// payload includes one. `InvestmentHoldings` has no dedicated currency
    /// setter, so this exercises the same direct-patch path as
    /// [`update_liquidity_meta_handler`].
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn update_investment_meta_handler_updates_currency() {
        let _temp = with_temp_env_offline();

        let mut inv = InvestmentHoldings::new(2026).expect("load holdings");
        inv.add_asset("Test Asset", "Stocks/ETF", "", "EUR")
            .expect("add asset");

        update_investment_meta_handler(
            Path("Test Asset".to_string()),
            Json(UpdateInvestmentPayload {
                year: 2026,
                name: None,
                category: None,
                link: None,
                currency: Some("USD".to_string()),
            }),
        )
        .await
        // `AppError` does not implement `Debug` (see `http_error.rs`), so
        // `.expect` cannot be used directly; unwrap through its `Display`.
        .unwrap_or_else(|AppError(err)| panic!("update succeeds: {err}"));

        let reloaded = InvestmentHoldings::new(2026).expect("reload holdings");
        assert_eq!(
            str_col_to_vec(&reloaded.df, "currency").expect("read currency column"),
            vec!["USD".to_string()]
        );
    }

    /// `GET` then `PUT` then `GET` again on `/api/settings/currency` must
    /// round-trip the saved values, including normalizing a lowercase
    /// reference currency to uppercase.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn currency_settings_round_trip_through_the_endpoints() {
        let _temp = with_temp_env_offline();

        let initial = get_currency_settings_handler()
            .await
            .unwrap_or_else(|AppError(err)| panic!("get succeeds: {err}"));
        assert_eq!(initial.0.reference_currency, "EUR");
        assert_eq!(
            initial.0.current_month_rate_mode,
            config::CurrentMonthRateMode::PreviousMonthEnd
        );

        let updated = update_currency_settings_handler(Json(CurrencySettingsJson {
            reference_currency: "usd".to_string(),
            current_month_rate_mode: config::CurrentMonthRateMode::Live,
        }))
        .await
        .unwrap_or_else(|AppError(err)| panic!("update succeeds: {err}"));
        assert_eq!(updated.0.reference_currency, "USD");
        assert_eq!(
            updated.0.current_month_rate_mode,
            config::CurrentMonthRateMode::Live
        );

        let reloaded = get_currency_settings_handler()
            .await
            .unwrap_or_else(|AppError(err)| panic!("get succeeds: {err}"));
        assert_eq!(reloaded.0.reference_currency, "USD");
        assert_eq!(
            reloaded.0.current_month_rate_mode,
            config::CurrentMonthRateMode::Live
        );
    }

    /// `PUT /api/settings/currency` must reject a reference currency outside
    /// the frontend's supported set, and must not persist it: a bad reference
    /// currency would make every later rate lookup fail far from this
    /// request.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn currency_settings_update_rejects_an_unsupported_reference_currency() {
        let _temp = with_temp_env_offline();

        let err = update_currency_settings_handler(Json(CurrencySettingsJson {
            reference_currency: "XXX".to_string(),
            current_month_rate_mode: config::CurrentMonthRateMode::Live,
        }))
        .await
        .expect_err("an unsupported reference currency must be rejected");
        assert!(matches!(
            err.0,
            finguard_rs_backend::Error::InvalidArgument(_)
        ));

        // Nothing must have been persisted: the default settings still load.
        let settings = config::get_currency_settings().expect("load settings");
        assert_eq!(settings.reference_currency, "EUR");
    }

    /// A brand-new year has no investment, liquidity, or credits/debts rows
    /// in any currency, and the request names no extra currency either, so
    /// the reference currency is the only one involved. That must succeed
    /// with an empty rate table and nothing marked unavailable, even with the
    /// network disabled and nothing cached: the reference currency never
    /// needs a lookup.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn monthly_fx_rates_reference_currency_only_needs_no_network() {
        let _temp = with_temp_env_offline();

        let response = get_monthly_fx_rates_handler(Query(MonthlyFxRatesQuery {
            year: 2026,
            currencies: None,
        }))
        .await
        .unwrap_or_else(|AppError(err)| panic!("reference-only request succeeds: {err}"));

        assert_eq!(response.0.reference_currency, "EUR");
        assert_eq!(response.0.months.len(), 12);
        assert!(
            response
                .0
                .months
                .iter()
                .all(|m| m.rate_to_reference.is_empty())
        );
        assert!(response.0.unavailable_currencies.is_empty());
    }

    /// A requested currency with nothing cached and the network disabled must
    /// be reported in `unavailable_currencies` and left out of every month's
    /// `rate_to_reference`, instead of failing the whole request.
    ///
    /// Uses a year (2000) far enough in the past that every month resolves
    /// through `month_end_rate` (the "completed month" path) regardless of
    /// today's real date, so the assertion cannot depend on when the test
    /// actually runs.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn monthly_fx_rates_reports_an_unresolvable_currency_as_unavailable() {
        let _temp = with_temp_env_offline();

        let response = get_monthly_fx_rates_handler(Query(MonthlyFxRatesQuery {
            year: 2000,
            currencies: Some("USD".to_string()),
        }))
        .await
        .unwrap_or_else(|AppError(err)| panic!("degraded request still succeeds: {err}"));

        assert_eq!(response.0.unavailable_currencies, vec!["USD".to_string()]);
        assert!(
            response
                .0
                .months
                .iter()
                .all(|m| !m.rate_to_reference.contains_key("USD"))
        );
    }

    /// Build a one-asset, one-month net worth entirely in `currency`: an
    /// investment (`10 * 50 = 500`), a liquidity balance (`1000`), and a
    /// credit/debt entry (`-200`), for `year`'s month `01`. `suffix`
    /// disambiguates asset names when called more than once for the same
    /// year. Mirrors `plots.rs`'s test helper of the same shape.
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

    /// A portfolio holding one resolvable and one unresolvable currency must
    /// still return `200`: the unresolvable currency is reported and its
    /// rows are excluded, leaving the resolvable rows' totals untouched
    /// (`500` invested, `1000` liquid, `-200` credits/debts, net worth
    /// `1300`), rather than the whole request failing.
    ///
    /// Year 2000 keeps every month on the `month_end_rate` "completed month"
    /// path regardless of today's real date, so the USD lookup fails for a
    /// reason unrelated to the current date, and the EUR rows need no
    /// lookup at all.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn networth_endpoints_degrade_for_an_unresolvable_currency() {
        let _temp = with_temp_env_offline();
        let year = 2000;
        seed_single_month_networth(year, "EUR", "Eur");
        seed_single_month_networth(year, "USD", "Usd");
        // No USD rate is ever cached, so every USD lookup fails offline.

        let evolution = get_networth_evolution_handler(Query(YearQuery { year }))
            .await
            .unwrap_or_else(|AppError(err)| panic!("degraded request still succeeds: {err}"))
            .0
            .expect("the EUR rows still produce a non-zero net worth");
        assert_eq!(evolution.unavailable_currencies, vec!["USD".to_string()]);
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

        let allocation =
            get_networth_allocation_handler(Query(NetworthAllocationQuery { year, month: 1 }))
                .await
                .unwrap_or_else(|AppError(err)| panic!("degraded request still succeeds: {err}"))
                .0
                .expect("non-empty allocation");
        assert_eq!(allocation.unavailable_currencies, vec!["USD".to_string()]);
        let slice_value = |name: &str| {
            allocation
                .slices
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("no '{name}' slice"))
                .value
        };
        assert_eq!(slice_value("Stocks/ETF"), 500.0);
        assert_eq!(slice_value("Liquidity"), 1000.0);
        assert_eq!(slice_value("Debts"), 200.0);
    }

    /// A portfolio held entirely in the reference currency needs no cached
    /// rate at all, and must return `200` with an empty
    /// `unavailable_currencies`, even offline with nothing cached.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn networth_endpoints_reference_currency_only_needs_no_cached_rate() {
        let _temp = with_temp_env_offline();
        let year = 2000;
        seed_single_month_networth(year, "EUR", "");

        let evolution = get_networth_evolution_handler(Query(YearQuery { year }))
            .await
            .unwrap_or_else(|AppError(err)| panic!("reference-only request succeeds: {err}"))
            .0
            .expect("non-zero net worth");
        assert!(evolution.unavailable_currencies.is_empty());
        assert_eq!(evolution.net_worth[0], 1300.0);

        let allocation =
            get_networth_allocation_handler(Query(NetworthAllocationQuery { year, month: 1 }))
                .await
                .unwrap_or_else(|AppError(err)| panic!("reference-only request succeeds: {err}"))
                .0
                .expect("non-empty allocation");
        assert!(allocation.unavailable_currencies.is_empty());
    }

    /// Regression guard: when every currency resolves, the totals must be
    /// exactly what the pre-degrade code produced, and
    /// `unavailable_currencies` must be empty. `500` EUR invested plus `500`
    /// USD invested at a `0.90` rate is `950`, and likewise for liquidity
    /// (`1000` EUR + `900` from `1000` USD) and credits/debts (`-200` EUR +
    /// `-180` from `-200` USD), for a net worth of `950 + 1900 - 380 = 2470`.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn networth_endpoints_all_resolvable_totals_are_unchanged() {
        let _temp = with_temp_env_offline();
        let year = 2000;
        seed_single_month_networth(year, "EUR", "Eur");
        seed_single_month_networth(year, "USD", "Usd");
        seed_fx_cache(&[("2000-01-31", &[("USD", 1.0 / 0.90)])]);

        let evolution = get_networth_evolution_handler(Query(YearQuery { year }))
            .await
            .unwrap_or_else(|AppError(err)| panic!("fully resolvable request succeeds: {err}"))
            .0
            .expect("non-zero net worth");
        assert!(evolution.unavailable_currencies.is_empty());
        let component = |name: &str| {
            evolution
                .components
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("no '{name}' component"))
        };
        assert_eq!(component("Stocks/ETF").values[0], 950.0);
        assert_eq!(component("Liquidity").values[0], 1900.0);
        assert_eq!(component("Credits/Debts").values[0], -380.0);
        assert_eq!(evolution.net_worth[0], 2470.0);

        let allocation =
            get_networth_allocation_handler(Query(NetworthAllocationQuery { year, month: 1 }))
                .await
                .unwrap_or_else(|AppError(err)| panic!("fully resolvable request succeeds: {err}"))
                .0
                .expect("non-empty allocation");
        assert!(allocation.unavailable_currencies.is_empty());
        let slice_value = |name: &str| {
            allocation
                .slices
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("no '{name}' slice"))
                .value
        };
        assert_eq!(slice_value("Stocks/ETF"), 950.0);
        assert_eq!(slice_value("Liquidity"), 1900.0);
        // The combined credits/debts total is negative (-380), so the pie
        // chart reports it as a "Debts" slice holding the absolute value,
        // matching `networth_allocation_pie`'s sign convention.
        assert_eq!(slice_value("Debts"), 380.0);
    }

    /// A portfolio held entirely in an unresolvable currency must not report
    /// as "no data": excluding its only rows would otherwise zero out every
    /// series and the handler would return `null`, telling the user they
    /// have no net worth when the truth is that their money could not be
    /// converted. The response must stay non-`null`, with `months`
    /// populated so a chart still has an axis, every component zero, and
    /// the currency named in `unavailable_currencies`.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn networth_endpoints_report_data_when_every_currency_is_unresolvable() {
        let _temp = with_temp_env_offline();
        let year = 2000;
        seed_single_month_networth(year, "USD", "");
        // No USD rate is ever cached, so the only currency present fails to
        // resolve.

        let evolution = get_networth_evolution_handler(Query(YearQuery { year }))
            .await
            .unwrap_or_else(|AppError(err)| panic!("degraded request still succeeds: {err}"))
            .0
            .expect("an unresolvable currency must not report as 'no data'");
        assert_eq!(evolution.unavailable_currencies, vec!["USD".to_string()]);
        assert_eq!(evolution.months.len(), 12);
        assert!(evolution.components.iter().all(|c| c.values[0] == 0.0));
        assert_eq!(evolution.net_worth[0], 0.0);

        let allocation =
            get_networth_allocation_handler(Query(NetworthAllocationQuery { year, month: 1 }))
                .await
                .unwrap_or_else(|AppError(err)| panic!("degraded request still succeeds: {err}"))
                .0
                .expect("an unresolvable currency must not report as 'no data'");
        assert_eq!(allocation.unavailable_currencies, vec!["USD".to_string()]);
        assert!(allocation.slices.is_empty());
    }
}
