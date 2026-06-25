//! Finguard Axum backend web server.
//!
//! Exposes a JSON REST API for finguard-zen to perform expenses, cashflow,
//! networth, and categories management operations using the backend Polars logic.

use axum::{
    extract::{Path, Query},
    routing::{get, post, delete, put},
    Json, Router,
};
use std::net::SocketAddr;
use tower_http::cors::CorsLayer;
use serde::{Deserialize, Serialize};

use finguard_rs::df_operations::{
    DetailedExpenses, RecurringExpenses, Cashflow, InvestmentHoldings, Liquidity, CreditsDebts,
};
use finguard_rs::config;
use finguard_rs::paths::{get_year_summary_path, PRIMARIES_FILENAME};
use polars::prelude::SerReader;

// ======================================================================
// JSON Serialisation Models
// ======================================================================

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
    pub field: String, // "qty" or "price"
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

fn read_parquet(path: &std::path::Path) -> Result<polars::prelude::DataFrame, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    polars::prelude::ParquetReader::new(file)
        .finish()
        .map_err(|e| e.to_string())
}

fn str_col_to_vec(df: &polars::prelude::DataFrame, name: &str) -> Result<Vec<String>, String> {
    df.column(name)
        .map_err(|e| e.to_string())?
        .str()
        .map_err(|e| e.to_string())?
        .iter()
        .map(|o| Ok(o.unwrap_or("").to_string()))
        .collect()
}

fn column_strings(df: &polars::prelude::DataFrame, name: &str) -> Vec<String> {
    match df.column(name).and_then(|c| c.str().cloned()) {
        Ok(s) => s.iter().map(|o| o.unwrap_or("").to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

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

fn set_df_str_where(
    df: &mut polars::prelude::DataFrame,
    key_col: &str,
    key: &str,
    target_col: &str,
    value: &str,
) -> Result<(), String> {
    use polars::prelude::*;
    let updated = df.clone()
        .lazy()
        .with_column(
            when(col(key_col).eq(lit(key)))
                .then(lit(value))
                .otherwise(col(target_col))
                .alias(target_col),
        )
        .collect()
        .map_err(|e| e.to_string())?;
    *df = updated;
    Ok(())
}

// ======================================================================
// Handlers
// ======================================================================

fn discover_years() -> Vec<i32> {
    let Ok(root) = finguard_rs::paths::get_dbs_root() else { return vec![]; };
    let Ok(entries) = std::fs::read_dir(&root) else { return vec![]; };
    let mut years: Vec<i32> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_type().ok()
                .filter(|t| t.is_dir())
                .and_then(|_| e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()))
        })
        .collect();
    years.sort_unstable_by(|a, b| b.cmp(a));
    years
}

async fn list_years_handler() -> Json<Vec<i32>> {
    Json(discover_years())
}

async fn get_expenses_handler(
    Query(q): Query<GetExpensesQuery>,
) -> Result<Json<Vec<ExpenseJson>>, String> {
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

        let filtered_df = de
            .filter_expenses_indexed(
                q.name.as_deref(),
                q.category.as_deref(),
                q.min,
                q.max,
            )
            .map_err(|e| e.to_string())?;

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

async fn upsert_expense_handler(
    Json(payload): Json<ExpenseJson>,
) -> Result<Json<ExpenseJson>, String> {
    let year = payload.year;
    let month = payload.month;
    let mut de = DetailedExpenses::new(year, month).map_err(|e| e.to_string())?;

    if payload.id.is_empty() {
        de.add_row(
            &payload.name,
            payload.day,
            payload.amount,
            Some(&payload.primary),
            &payload.currency,
            Some(&payload.secondary),
        )
        .map_err(|e| e.to_string())?;

        let new_id = (de.expense_df.height() as i32 - 1).to_string();
        let mut response = payload;
        response.id = new_id;
        Ok(Json(response))
    } else {
        let idx = payload.id.parse::<u32>().map_err(|e| e.to_string())?;
        de.edit_row(
            idx,
            Some(&payload.name),
            Some(payload.day),
            Some(payload.amount),
            Some(&payload.currency),
            Some(&payload.primary),
            Some(&payload.secondary),
        )
        .map_err(|e| e.to_string())?;
        Ok(Json(payload))
    }
}

async fn delete_expense_handler(
    Path(id): Path<String>,
    Query(q): Query<DeleteExpenseQuery>,
) -> Result<(), String> {
    let idx = id.parse::<u32>().map_err(|e| e.to_string())?;
    let mut de = DetailedExpenses::new(q.year, q.month).map_err(|e| e.to_string())?;
    de.delete_row(idx).map_err(|e| e.to_string())?;
    Ok(())
}

async fn get_recurring_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<Vec<RecurringTemplateJson>>, String> {
    let rec = RecurringExpenses::new(q.year).map_err(|e| e.to_string())?;
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

async fn add_recurring_handler(
    Json(payload): Json<AddRecurringPayload>,
) -> Result<Json<RecurringTemplateJson>, String> {
    let mut rec = RecurringExpenses::new(payload.year).map_err(|e| e.to_string())?;
    rec.add(
        &payload.name,
        payload.day,
        payload.amount,
        &payload.currency,
        &payload.primary,
        &payload.secondary,
    )
    .map_err(|e| e.to_string())?;

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

async fn delete_recurring_handler(
    Path(id): Path<String>,
    Query(q): Query<YearQuery>,
) -> Result<(), String> {
    let idx = id.parse::<u32>().map_err(|e| e.to_string())?;
    let mut rec = RecurringExpenses::new(q.year).map_err(|e| e.to_string())?;
    rec.remove(idx).map_err(|e| e.to_string())?;
    Ok(())
}

async fn apply_recurring_handler(
    Json(payload): Json<ApplyRecurringPayload>,
) -> Result<Json<u32>, String> {
    let rec = RecurringExpenses::new(payload.year).map_err(|e| e.to_string())?;
    let mut de = DetailedExpenses::new(payload.year, payload.month).map_err(|e| e.to_string())?;
    let added_names = rec.apply_to_month(&mut de).map_err(|e| e.to_string())?;
    Ok(Json(added_names.len() as u32))
}

async fn get_mappings_handler() -> Result<Json<Vec<MappingRuleJson>>, String> {
    let map = config::get_all_mappings().map_err(|e| e.to_string())?;
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

async fn add_mapping_handler(
    Json(payload): Json<MappingRuleJson>,
) -> Result<Json<MappingRuleJson>, String> {
    config::add_mapping(&payload.match_str, &payload.primary, &payload.secondary, true)
        .map_err(|e| e.to_string())?;
    let key = payload.match_str.trim().to_lowercase();
    Ok(Json(MappingRuleJson {
        id: key.clone(),
        match_str: key,
        primary: payload.primary.trim().to_lowercase(),
        secondary: payload.secondary.trim().to_lowercase(),
    }))
}

async fn delete_mapping_handler(Path(id): Path<String>) -> Result<(), String> {
    config::remove_mapping(&id).map_err(|e| e.to_string())?;
    Ok(())
}

async fn get_categories_handler() -> Result<Json<CategoriesJson>, String> {
    let known = config::get_known_categories().map_err(|e| e.to_string())?;
    Ok(Json(CategoriesJson {
        primary: known.primary,
        secondary: known.secondary,
    }))
}

async fn add_category_handler(
    Path(kind): Path<String>,
    Json(payload): Json<AddCategoryPayload>,
) -> Result<Json<CategoriesJson>, String> {
    config::add_known_category(&payload.name, &kind).map_err(|e| e.to_string())?;
    let known = config::get_known_categories().map_err(|e| e.to_string())?;
    Ok(Json(CategoriesJson {
        primary: known.primary,
        secondary: known.secondary,
    }))
}

async fn delete_category_handler(
    Path((kind, name)): Path<(String, String)>,
) -> Result<Json<CategoriesJson>, String> {
    let totals = finguard_rs::df_operations::get_category_totals_across_all_years(&kind)
        .map_err(|e| e.to_string())?;
    let total = totals.get(&name).copied().unwrap_or(0.0);
    if total.abs() >= 1e-9 {
        return Err(format!(
            "Cannot delete \"{name}\": it still has {} in existing expenses.",
            total
        ));
    }

    config::remove_known_category(&name, &kind).map_err(|e| e.to_string())?;
    finguard_rs::df_operations::remove_category_from_all_summaries(&name, &kind)
        .map_err(|e| e.to_string())?;

    let known = config::get_known_categories().map_err(|e| e.to_string())?;
    Ok(Json(CategoriesJson {
        primary: known.primary,
        secondary: known.secondary,
    }))
}

async fn get_category_totals_handler(
    Query(q): Query<KindQuery>,
) -> Result<Json<std::collections::HashMap<String, f64>>, String> {
    let totals = finguard_rs::df_operations::get_category_totals_across_all_years(&q.kind)
        .map_err(|e| e.to_string())?;
    let mut hm = std::collections::HashMap::new();
    for (k, v) in totals {
        hm.insert(k, v);
    }
    Ok(Json(hm))
}

async fn get_income_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<std::collections::HashMap<u32, std::collections::HashMap<String, f64>>>, String> {
    let cf = Cashflow::new(q.year).map_err(|e| e.to_string())?;
    let mut out = std::collections::HashMap::new();
    for m in 1..=12 {
        let mut m_map = std::collections::HashMap::new();
        for cat in &["Salary", "Interests Bank account", "Dividendi e Cedole", "Other"] {
            let val = cf.get_value(cat, &format!("{m:02}")).unwrap_or(0.0);
            m_map.insert((*cat).to_string(), val);
        }
        out.insert(m, m_map);
    }
    Ok(Json(out))
}

async fn set_income_cell_handler(
    Json(payload): Json<SetIncomeCellPayload>,
) -> Result<(), String> {
    let mut cf = Cashflow::new(payload.year).map_err(|e| e.to_string())?;
    cf.set_income(payload.month, &payload.category, payload.amount)
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn get_monthly_spending_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<std::collections::HashMap<u32, std::collections::HashMap<String, f64>>>, String> {
    let primaries_path = get_year_summary_path(q.year, PRIMARIES_FILENAME).map_err(|e| e.to_string())?;
    let mut out = std::collections::HashMap::new();
    for m in 1..=12 {
        out.insert(m, std::collections::HashMap::new());
    }

    if primaries_path.exists() {
        let df = read_parquet(&primaries_path).map_err(|e| e.to_string())?;
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

async fn get_investments_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<Vec<InvestmentAssetJson>>, String> {
    let inv = InvestmentHoldings::new(q.year).map_err(|e| e.to_string())?;
    let assets = str_col_to_vec(&inv.df, "asset_name").map_err(|e| e.to_string())?;
    let categories = str_col_to_vec(&inv.df, "category").map_err(|e| e.to_string())?;
    let links = str_col_to_vec(&inv.df, "link").map_err(|e| e.to_string())?;

    let mut list = Vec::new();
    for (i, name) in assets.iter().enumerate() {
        let mut data_map = std::collections::HashMap::new();
        let mut m_map = std::collections::HashMap::new();
        for m in 1..=12 {
            let col = format!("{m:02}");
            let qty = inv.df.column(&col)
                .and_then(|c| c.cast(&polars::prelude::DataType::Float64))
                .and_then(|c| c.f64().map(|s| s.get(i).unwrap_or(0.0)))
                .unwrap_or(0.0);
            let price = inv.df_prices.column(&col)
                .and_then(|c| c.cast(&polars::prelude::DataType::Float64))
                .and_then(|c| c.f64().map(|s| s.get(i).unwrap_or(0.0)))
                .unwrap_or(0.0);
            m_map.insert(m, QtyPrice { qty, price });
        }
        data_map.insert(q.year, m_map);

        list.push(InvestmentAssetJson {
            id: name.clone(),
            name: name.clone(),
            category: categories.get(i).cloned().unwrap_or_else(|| "Stocks/ETF".to_string()),
            link: links.get(i).filter(|s| !s.is_empty()).cloned(),
            data: data_map,
        });
    }
    Ok(Json(list))
}

async fn add_investment_handler(
    Json(payload): Json<AddInvestmentPayload>,
) -> Result<Json<InvestmentAssetJson>, String> {
    let mut inv = InvestmentHoldings::new(payload.year).map_err(|e| e.to_string())?;
    let link_str = payload.link.as_deref().unwrap_or("");
    inv.add_asset(&payload.name, &payload.category, link_str)
        .map_err(|e| e.to_string())?;

    let mut data_map = std::collections::HashMap::new();
    let mut m_map = std::collections::HashMap::new();
    for m in 1..=12 {
        m_map.insert(m, QtyPrice { qty: 0.0, price: 0.0 });
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

async fn update_investment_meta_handler(
    Path(id): Path<String>,
    Json(payload): Json<UpdateInvestmentPayload>,
) -> Result<(), String> {
    let mut inv = InvestmentHoldings::new(payload.year).map_err(|e| e.to_string())?;
    let final_name = if let Some(new_name) = &payload.name {
        if new_name != &id {
            inv.rename_asset(&id, new_name).map_err(|e| e.to_string())?;
        }
        new_name.clone()
    } else {
        id.clone()
    };

    if let Some(cat) = &payload.category {
        inv.set_category(&final_name, cat).map_err(|e| e.to_string())?;
    }
    if let Some(lnk) = &payload.link {
        inv.set_link(&final_name, lnk).map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn delete_investment_handler(
    Path(id): Path<String>,
    Query(q): Query<DeleteAssetQuery>,
) -> Result<(), String> {
    let mut inv = InvestmentHoldings::new(q.year).map_err(|e| e.to_string())?;
    inv.remove_asset(&id).map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Deserialize, Debug)]
pub struct DeleteAssetQuery {
    pub year: i32,
}

async fn set_investment_cell_handler(
    Json(payload): Json<SetInvestmentCellPayload>,
) -> Result<(), String> {
    let mut inv = InvestmentHoldings::new(payload.year).map_err(|e| e.to_string())?;
    inv.set_quantity_or_price(&payload.id, payload.month, payload.value, &payload.field)
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn get_liquidity_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<Vec<LiquidityRowJson>>, String> {
    let liq = Liquidity::new(q.year).map_err(|e| e.to_string())?;
    let assets = str_col_to_vec(&liq.df, "asset_name").map_err(|e| e.to_string())?;
    let categories = str_col_to_vec(&liq.df, "category").map_err(|e| e.to_string())?;
    let currencies = str_col_to_vec(&liq.df, "currency").map_err(|e| e.to_string())?;

    let mut list = Vec::new();
    for (i, name) in assets.iter().enumerate() {
        let mut data_map = std::collections::HashMap::new();
        let mut m_map = std::collections::HashMap::new();
        for m in 1..=12 {
            let col = format!("{m:02}");
            let val = liq.df.column(&col)
                .and_then(|c| c.cast(&polars::prelude::DataType::Float64))
                .and_then(|c| c.f64().map(|s| s.get(i).unwrap_or(0.0)))
                .unwrap_or(0.0);
            m_map.insert(m, val);
        }
        data_map.insert(q.year, m_map);

        list.push(LiquidityRowJson {
            id: name.clone(),
            name: name.clone(),
            category: categories.get(i).cloned().unwrap_or_else(|| "Bank/Broker account".to_string()),
            currency: currencies.get(i).cloned().unwrap_or_else(|| "EUR".to_string()),
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

async fn add_liquidity_handler(
    Json(payload): Json<AddLiquidityPayload>,
) -> Result<Json<LiquidityRowJson>, String> {
    let mut liq = Liquidity::new(payload.year).map_err(|e| e.to_string())?;
    liq.add_asset(&payload.name, &payload.category, &payload.currency)
        .map_err(|e| e.to_string())?;

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

async fn update_liquidity_meta_handler(
    Path(id): Path<String>,
    Json(payload): Json<UpdateLiquidityPayload>,
) -> Result<(), String> {
    let mut liq = Liquidity::new(payload.year).map_err(|e| e.to_string())?;
    let final_name = if let Some(new_name) = &payload.name {
        if new_name != &id {
            liq.rename_asset(&id, new_name).map_err(|e| e.to_string())?;
        }
        new_name.clone()
    } else {
        id.clone()
    };

    if let Some(cat) = &payload.category {
        liq.set_category(&final_name, cat).map_err(|e| e.to_string())?;
    }
    if let Some(cur) = &payload.currency {
        set_df_str_where(&mut liq.df, "asset_name", &final_name, "currency", cur)?;
        liq.save().map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn delete_liquidity_handler(
    Path(id): Path<String>,
    Query(q): Query<DeleteAssetQuery>,
) -> Result<(), String> {
    let mut liq = Liquidity::new(q.year).map_err(|e| e.to_string())?;
    liq.remove_asset(&id).map_err(|e| e.to_string())?;
    Ok(())
}

async fn set_liquidity_cell_handler(
    Json(payload): Json<SetLiquidityCellPayload>,
) -> Result<(), String> {
    let mut liq = Liquidity::new(payload.year).map_err(|e| e.to_string())?;
    liq.set_value(&payload.id, payload.month, payload.value).map_err(|e| e.to_string())?;
    Ok(())
}

async fn get_credits_debts_handler(
    Query(q): Query<YearQuery>,
) -> Result<Json<Vec<CreditDebtRowJson>>, String> {
    let cd = CreditsDebts::new(q.year).map_err(|e| e.to_string())?;
    let entries = str_col_to_vec(&cd.df, "name").map_err(|e| e.to_string())?;
    let currencies = str_col_to_vec(&cd.df, "currency").map_err(|e| e.to_string())?;

    let mut list = Vec::new();
    for (i, name) in entries.iter().enumerate() {
        let mut data_map = std::collections::HashMap::new();
        let mut m_map = std::collections::HashMap::new();
        for m in 1..=12 {
            let col = format!("{m:02}");
            let val = cd.df.column(&col)
                .and_then(|c| c.cast(&polars::prelude::DataType::Float64))
                .and_then(|c| c.f64().map(|s| s.get(i).unwrap_or(0.0)))
                .unwrap_or(0.0);
            m_map.insert(m, val);
        }
        data_map.insert(q.year, m_map);

        list.push(CreditDebtRowJson {
            id: name.clone(),
            name: name.clone(),
            currency: currencies.get(i).cloned().unwrap_or_else(|| "EUR".to_string()),
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

async fn add_credit_debt_handler(
    Json(payload): Json<AddCreditDebtPayload>,
) -> Result<Json<CreditDebtRowJson>, String> {
    let mut cd = CreditsDebts::new(payload.year).map_err(|e| e.to_string())?;
    cd.add_entry(&payload.name, &payload.currency).map_err(|e| e.to_string())?;

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

async fn update_credit_debt_meta_handler(
    Path(id): Path<String>,
    Json(payload): Json<UpdateCreditDebtPayload>,
) -> Result<(), String> {
    let mut cd = CreditsDebts::new(payload.year).map_err(|e| e.to_string())?;
    let final_name = if let Some(new_name) = &payload.name {
        if new_name != &id {
            cd.rename_entry(&id, new_name).map_err(|e| e.to_string())?;
        }
        new_name.clone()
    } else {
        id.clone()
    };

    if let Some(cur) = &payload.currency {
        set_df_str_where(&mut cd.df, "name", &final_name, "currency", cur)?;
        cd.save().map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn delete_credits_debts_handler(
    Path(id): Path<String>,
    Query(q): Query<DeleteAssetQuery>,
) -> Result<(), String> {
    let mut cd = CreditsDebts::new(q.year).map_err(|e| e.to_string())?;
    cd.remove_entry(&id).map_err(|e| e.to_string())?;
    Ok(())
}

async fn set_credits_debts_cell_handler(
    Json(payload): Json<SetCreditDebtCellPayload>,
) -> Result<(), String> {
    let mut cd = CreditsDebts::new(payload.year).map_err(|e| e.to_string())?;
    cd.set_value(&payload.id, payload.month, payload.value).map_err(|e| e.to_string())?;
    Ok(())
}

// ======================================================================
// Server Initialization
// ======================================================================

#[tokio::main]
async fn main() {
    let cors = CorsLayer::permissive();

    let app = Router::new()
        // Configuration / global APIs
        .route("/api/years", get(list_years_handler))
        .route("/api/categories", get(get_categories_handler))
        .route("/api/categories/:kind", post(add_category_handler))
        .route("/api/categories/:kind/:name", delete(delete_category_handler))
        .route("/api/categories/totals", get(get_category_totals_handler))
        
        // Expense transactions
        .route("/api/expenses", get(get_expenses_handler).post(upsert_expense_handler))
        .route("/api/expenses/:id", delete(delete_expense_handler))
        
        // Recurring Templates
        .route("/api/recurring", get(get_recurring_handler).post(add_recurring_handler))
        .route("/api/recurring/:id", delete(delete_recurring_handler))
        .route("/api/recurring/apply", post(apply_recurring_handler))
        
        // Mappings
        .route("/api/mappings", get(get_mappings_handler).post(add_mapping_handler))
        .route("/api/mappings/:id", delete(delete_mapping_handler))
        
        // Cashflow
        .route("/api/cashflow/income", get(get_income_handler).post(set_income_cell_handler))
        .route("/api/cashflow/spending", get(get_monthly_spending_handler))
        
        // Net Worth - Investments
        .route("/api/investments", get(get_investments_handler).post(add_investment_handler))
        .route("/api/investments/:id", put(update_investment_meta_handler).delete(delete_investment_handler))
        .route("/api/investments/cell", post(set_investment_cell_handler))
        
        // Net Worth - Liquidity
        .route("/api/liquidity", get(get_liquidity_handler).post(add_liquidity_handler))
        .route("/api/liquidity/:id", put(update_liquidity_meta_handler).delete(delete_liquidity_handler))
        .route("/api/liquidity/cell", post(set_liquidity_cell_handler))
        
        // Net Worth - Credits/Debts
        .route("/api/credits_debts", get(get_credits_debts_handler).post(add_credit_debt_handler))
        .route("/api/credits_debts/:id", put(update_credit_debt_meta_handler).delete(delete_credits_debts_handler))
        .route("/api/credits_debts/cell", post(set_credits_debts_cell_handler))
        
        .layer(cors);

    // let addr = SocketAddr::from(([127, 0, 0, 1], 3111));
    // println!("Finguard server running on http://{}", addr);
    // let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    // axum::serve(listener, app).await.unwrap();
    
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
