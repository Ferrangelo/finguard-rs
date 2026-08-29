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
use finguard_rs_backend::paths::{PRIMARIES_FILENAME, get_year_summary_path};
use polars::prelude::SerReader;

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

/// Read a parquet file eagerly. Returns an error if `path` cannot be opened or
/// does not contain a valid parquet stream; callers are expected to check
/// [`std::path::Path::exists`] first when a missing file is a normal case.
fn read_parquet(path: &std::path::Path) -> finguard_rs_backend::Result<polars::prelude::DataFrame> {
    let file = std::fs::File::open(path)?;
    Ok(polars::prelude::ParquetReader::new(file).finish()?)
}

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
    let mut all = Vec::new();
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
            all.push(ExpenseJson {
                id: indices.get(fi).copied().unwrap_or(0).to_string(),
                year: q.year,
                month: m,
                day: days.get(fi).copied().unwrap_or(1) as u32,
                name: names.get(fi).cloned().unwrap_or_default(),
                amount: amounts.get(fi).copied().unwrap_or(0.0),
                currency: currencies.get(fi).cloned().unwrap_or_default(),
                primary: primaries.get(fi).cloned().unwrap_or_default(),
                secondary: secondaries.get(fi).cloned().unwrap_or_default(),
            });
        }
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
/// parquet file and recomputes the primaries/secondaries summary tables for
/// the year.
async fn upsert_expense_handler(
    Json(payload): Json<ExpenseJson>,
) -> Result<Json<ExpenseJson>, AppError> {
    let year = payload.year;
    let month = payload.month;
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
        Ok(Json(payload))
    }
}

/// `DELETE /api/expenses/:id`: remove the expense at row index `id` from
/// `q.year`/`q.month`'s monthly parquet file, then recompute the
/// primaries/secondaries summary tables for the year.
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
    let added_names = rec.apply_to_month(&mut de)?;
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

/// `DELETE /api/categories/:kind/:name`: unregister `name` from `kind`'s
/// known-category list and strip its row from every year's primaries or
/// secondaries summary file.
///
/// Refuses to delete (returns [`Error::InvalidArgument`], `400`) while the
/// category still has a nonzero total across all years (checked with a
/// `1e-9` tolerance for float rounding), so a category cannot be removed out
/// from under existing expenses. This deletion of summary rows across every
/// year is irreversible; it does not touch the underlying monthly detailed
/// expense rows, only the cached per-year totals.
async fn delete_category_handler(
    Path((kind, name)): Path<(String, String)>,
) -> Result<Json<CategoriesJson>, AppError> {
    let totals = finguard_rs_backend::df_operations::get_category_totals_across_all_years(&kind)?;
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

/// `GET /api/categories/totals?kind=`: return the cumulative
/// `expense_in_ref_currency` total per category of `kind`, summed across every
/// year's summary file. Backs [`delete_category_handler`]'s "still in use"
/// check on the frontend before a user attempts a delete.
async fn get_category_totals_handler(
    Query(q): Query<KindQuery>,
) -> Result<Json<std::collections::HashMap<String, f64>>, AppError> {
    let totals = finguard_rs_backend::df_operations::get_category_totals_across_all_years(&q.kind)?;
    let mut hm = std::collections::HashMap::new();
    for (k, v) in totals {
        hm.insert(k, v);
    }
    Ok(Json(hm))
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
/// `{ month => { category => amount } }`, read from the `Total`-excluded rows
/// of the year's `primaries.parquet` summary file for each `YYYY-MM` column
/// present.
///
/// Every month is present in the result even when `primaries.parquet` does
/// not exist yet or has no column for that month; such months map to an
/// empty category map rather than being omitted or erroring.
async fn get_monthly_spending_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<std::collections::HashMap<u32, std::collections::HashMap<String, f64>>>, AppError>
{
    let primaries_path = get_year_summary_path(q.year, PRIMARIES_FILENAME)?;
    let mut out = std::collections::HashMap::new();
    for m in 1..=12 {
        out.insert(m, std::collections::HashMap::new());
    }

    if primaries_path.exists() {
        let df = read_parquet(&primaries_path)?;
        let categories = column_strings(&df, "primary_category");
        for m in 1..=12 {
            let col_name = format!("{}-{:02}", q.year, m);
            if df.get_column_names().iter().any(|c| c.as_str() == col_name) {
                let values = column_f64(&df, &col_name);
                let m_map = out.get_mut(&m).unwrap();
                for (i, cat) in categories.iter().enumerate() {
                    if cat != "Total" {
                        m_map.insert(cat.clone(), values.get(i).copied().unwrap_or(0.0));
                    }
                }
            }
        }
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
            data: data_map,
        });
    }
    Ok(Json(list))
}

/// `POST /api/investments`: add a new investment asset to `payload.year` with
/// all monthly quantities and prices initialized to `0.0`, then save.
///
/// Returns [`Error::InvalidArgument`] (`400`) for an unrecognized
/// `payload.category`, or [`Error::AlreadyExists`] (`409`) if an asset with
/// that name already exists for the year.
async fn add_investment_handler(
    Json(payload): Json<AddInvestmentPayload>,
) -> Result<Json<InvestmentAssetJson>, AppError> {
    let mut inv = InvestmentHoldings::new(payload.year)?;
    let link_str = payload.link.as_deref().unwrap_or("");
    inv.add_asset(&payload.name, &payload.category, link_str)?;

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
        data: data_map,
    }))
}

#[derive(Deserialize, Debug)]
pub struct AddInvestmentPayload {
    pub year: i32,
    pub name: String,
    pub category: String,
    pub link: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct UpdateInvestmentPayload {
    pub year: i32,
    pub name: Option<String>,
    pub category: Option<String>,
    pub link: Option<String>,
}

/// `PUT /api/investments/:id`: update metadata for the asset named `id` in
/// `payload.year`. Each field is applied only when present in the payload; a
/// present `payload.name` different from `id` renames the asset first, and
/// the rename takes effect before any `category`/`link` update below it, so
/// those updates target the asset under its new name.
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
