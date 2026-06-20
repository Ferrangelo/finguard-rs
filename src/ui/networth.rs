//! NetWorth tab: investments, liquidity, credits/debts and total net worth.
//!
//! Ports the behaviour of the Python `ui_networth.py` (and the shared table
//! builders in `ui_tables.py`) to iced 0.14, following the shared tab contract
//! documented in [`crate::ui`].
//!
//! # Sub-tabs
//!
//! * **Investments** — a wide `Asset | Category | 12 months` grid with three
//!   inner views: *Holdings* (editable quantities, `inv.df`), *Prices* (editable
//!   unit prices, `inv.df_prices`) and *Value* (read-only `quantity × price`,
//!   `inv.df_value()`), plus an add-asset form and per-row edit/delete.
//! * **Liquidity & Credits/Debts** — two stacked editable value grids bound to
//!   `liq.df` and `cd.df`, each with an add form, per-row rename/category edit,
//!   delete and a bold totals row.
//! * **Total NetWorth** — a computed summary grid (per investment category,
//!   Liquidity, Credits/Debts, Net Worth, NW Change, % NW Change) plus the
//!   allocation pie and net-worth-evolution charts.
//!
//! # Editable-cell state model
//!
//! Editable month cells follow the buffer model documented in
//! [`crate::ui::widgets`]: a per-cell text buffer is the source of truth for the
//! widget's displayed text. `on_input` writes into the buffer; `on_submit`
//! evaluates the buffered text via [`finguard_rs::expr::eval`], calls the
//! relevant library setter on success (silently ignoring eval errors) and then
//! reloads so derived rows/totals and the buffer are re-seeded.

use std::collections::HashMap;

use iced::widget::{button, column, container, pick_list, row, scrollable, text, text_input};
use iced::{Alignment, Element, Length};
use polars::prelude::*;

use finguard_rs::df_operations::{CreditsDebts, InvestmentHoldings, Liquidity};
use finguard_rs::expr;
use finguard_rs::plots::{NetworthEvolution, PieChart};

use crate::ui::charts;
use crate::ui::widgets::{
    self, MONTH_ABBR, fmt_cell, fmt_thousands, header_cell, label_cell, numeric_cell, section_title,
};
use crate::ui::{Ctx, Message as AppMessage};

/// Investment categories (mirrors the library `INVESTMENT_CATEGORIES`).
const INVESTMENT_CATEGORIES: [&str; 3] = ["Stocks/ETF", "Commodities", "Bonds"];
/// Liquidity categories (mirrors the library `LIQUIDITY_CATEGORIES`).
const LIQUIDITY_CATEGORIES: [&str; 3] = ["Bank/Broker account", "Cash", "Other"];

/// Width of the month columns, in pixels.
const MONTH_W: f32 = 78.0;
/// Width of the leading name column.
const NAME_W: f32 = 180.0;
/// Width of the category column.
const CAT_W: f32 = 150.0;
/// Width of the currency column.
const CUR_W: f32 = 50.0;
/// Width of the small action (delete) column.
const ACTION_W: f32 = 70.0;

// ======================================================================
// Sub-tab / inner-view selectors
// ======================================================================

/// The three top-level NetWorth sub-tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubTab {
    /// Investments (holdings / prices / value).
    #[default]
    Investments,
    /// Liquidity and credits/debts.
    Liquidity,
    /// Computed total net worth + charts.
    Total,
}

impl SubTab {
    const ALL: [SubTab; 3] = [SubTab::Investments, SubTab::Liquidity, SubTab::Total];

    fn label(self) -> &'static str {
        match self {
            SubTab::Investments => "Investments",
            SubTab::Liquidity => "Liquidity, Credits and Debts",
            SubTab::Total => "Total NetWorth",
        }
    }
}

/// The Investments inner views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InvView {
    /// Editable quantities.
    #[default]
    Holdings,
    /// Editable unit prices.
    Prices,
    /// Read-only quantity × price.
    Value,
}

impl InvView {
    const ALL: [InvView; 3] = [InvView::Holdings, InvView::Prices, InvView::Value];

    fn label(self) -> &'static str {
        match self {
            InvView::Holdings => "Holdings",
            InvView::Prices => "Prices",
            InvView::Value => "Value",
        }
    }
}

// ======================================================================
// Display caches (materialized during `reload`)
// ======================================================================

/// A single asset/entry row materialized for display.
#[derive(Debug, Clone, Default)]
struct DisplayRow {
    /// Asset / entry name (the row's identity for setters).
    name: String,
    /// Category (investment / liquidity); empty for credits/debts.
    category: String,
    /// Currency (liquidity / credits-debts); empty for investments.
    currency: String,
    /// Link URL (investments only).
    link: String,
    /// The 12 monthly values.
    months: [f64; 12],
}

/// A computed summary row for the Total NetWorth grid (values may be absent).
#[derive(Debug, Clone)]
struct SummaryRow {
    label: String,
    bold: bool,
    is_pct: bool,
    /// Per-month value; `None` renders as a blank cell.
    values: [Option<f64>; 12],
}

// ======================================================================
// State
// ======================================================================

/// State owned by the NetWorth tab.
///
/// Holds the (non-`Clone`) library structs alongside the materialized display
/// caches and per-cell edit buffers. The buffers are keyed by `(name, month)`
/// per grid so that a half-typed expression survives redraws.
#[derive(Default)]
pub struct State {
    /// Active sub-tab.
    sub_tab: SubTab,
    /// Active Investments inner view.
    inv_view: InvView,

    /// Loaded investment holdings (`None` if the load failed).
    inv: Option<InvestmentHoldings>,
    /// Loaded liquidity table.
    liq: Option<Liquidity>,
    /// Loaded credits/debts table.
    cd: Option<CreditsDebts>,

    /// Materialized holdings rows (`inv.df`).
    holdings_rows: Vec<DisplayRow>,
    /// Materialized price rows (`inv.df_prices`).
    price_rows: Vec<DisplayRow>,
    /// Materialized value rows (`inv.df_value()`).
    value_rows: Vec<DisplayRow>,
    /// Materialized liquidity rows.
    liq_rows: Vec<DisplayRow>,
    /// Materialized credits/debts rows.
    cd_rows: Vec<DisplayRow>,

    /// Computed summary rows for the Total NetWorth grid.
    summary_rows: Vec<SummaryRow>,

    /// Edit buffers: keyed by `(name, month)` per editable grid.
    holdings_buf: HashMap<(String, usize), String>,
    price_buf: HashMap<(String, usize), String>,
    liq_buf: HashMap<(String, usize), String>,
    cd_buf: HashMap<(String, usize), String>,

    /// Add-asset form (Investments).
    add_inv_name: String,
    add_inv_cat: String,
    add_inv_link: String,
    /// Add-asset form (Liquidity).
    add_liq_name: String,
    add_liq_cat: String,
    add_liq_cur: String,
    /// Add-entry form (Credits/Debts).
    add_cd_name: String,
    add_cd_cur: String,

    /// Inline row editor: which grid + row is being edited, with draft fields.
    editing: Option<RowEdit>,

    /// Charts (held as Options; rendered "No data" when `None`).
    pie: Option<PieChart>,
    evolution: Option<NetworthEvolution>,
}

/// Which grid a row-edit dialog targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditTarget {
    Investment,
    Liquidity,
    CreditDebt,
}

/// In-progress inline row edit (rename + optional category).
#[derive(Debug, Clone)]
struct RowEdit {
    target: EditTarget,
    /// Original name (the row's identity for the rename/category/link setters).
    original: String,
    /// Draft name.
    name: String,
    /// Draft category (investment / liquidity).
    category: String,
    /// Draft link (investments only).
    link: String,
}

// ======================================================================
// Messages
// ======================================================================

/// Messages emitted by the NetWorth tab.
#[derive(Debug, Clone)]
pub enum Message {
    /// Switch the active sub-tab.
    SubTabSelected(SubTab),
    /// Switch the active Investments inner view.
    InvViewSelected(InvView),

    // -- Add forms --
    AddInvNameChanged(String),
    AddInvCatChanged(String),
    AddInvLinkChanged(String),
    AddInvSubmit,
    AddLiqNameChanged(String),
    AddLiqCatChanged(String),
    AddLiqCurChanged(String),
    AddLiqSubmit,
    AddCdNameChanged(String),
    AddCdCurChanged(String),
    AddCdSubmit,

    // -- Editable cells: `on_input` buffers, `on_submit` commits. --
    /// Holdings cell text changed: `(asset, month, text)`.
    HoldingsInput(String, usize, String),
    /// Holdings cell submitted: `(asset, month)`.
    HoldingsSubmit(String, usize),
    PriceInput(String, usize, String),
    PriceSubmit(String, usize),
    LiqInput(String, usize, String),
    LiqSubmit(String, usize),
    CdInput(String, usize, String),
    CdSubmit(String, usize),

    // -- Deletes --
    RemoveInvestment(String),
    RemoveLiquidity(String),
    RemoveCreditDebt(String),

    // -- Inline row edit dialog --
    /// Open the row editor for `(target, name)`.
    BeginEdit(EditTarget, String),
    EditNameChanged(String),
    EditCatChanged(String),
    EditLinkChanged(String),
    EditSave,
    EditCancel,
}

// ======================================================================
// Update
// ======================================================================

/// Handle a tab message. All work is synchronous, so this always returns
/// [`iced::Task::none`].
pub fn update(state: &mut State, message: Message, ctx: Ctx) -> iced::Task<AppMessage> {
    match message {
        Message::SubTabSelected(t) => state.sub_tab = t,
        Message::InvViewSelected(v) => state.inv_view = v,

        // -- Investments add form --
        Message::AddInvNameChanged(s) => state.add_inv_name = s,
        Message::AddInvCatChanged(s) => state.add_inv_cat = s,
        Message::AddInvLinkChanged(s) => state.add_inv_link = s,
        Message::AddInvSubmit => {
            let name = state.add_inv_name.trim().to_string();
            if !name.is_empty()
                && let Some(inv) = state.inv.as_mut()
            {
                let cat = if state.add_inv_cat.is_empty() {
                    INVESTMENT_CATEGORIES[0].to_string()
                } else {
                    state.add_inv_cat.clone()
                };
                let link = state.add_inv_link.trim().to_string();
                if inv.add_asset(&name, &cat, &link).is_ok() {
                    state.add_inv_name.clear();
                    state.add_inv_link.clear();
                    state.add_inv_cat.clear();
                }
            }
            reload(state, ctx);
        }

        // -- Liquidity add form --
        Message::AddLiqNameChanged(s) => state.add_liq_name = s,
        Message::AddLiqCatChanged(s) => state.add_liq_cat = s,
        Message::AddLiqCurChanged(s) => state.add_liq_cur = s,
        Message::AddLiqSubmit => {
            let name = state.add_liq_name.trim().to_string();
            if !name.is_empty()
                && let Some(liq) = state.liq.as_mut()
            {
                let cat = if state.add_liq_cat.is_empty() {
                    LIQUIDITY_CATEGORIES[0].to_string()
                } else {
                    state.add_liq_cat.clone()
                };
                let cur = {
                    let c = state.add_liq_cur.trim();
                    if c.is_empty() {
                        "E".to_string()
                    } else {
                        c.to_string()
                    }
                };
                if liq.add_asset(&name, &cat, &cur).is_ok() {
                    state.add_liq_name.clear();
                    state.add_liq_cat.clear();
                }
            }
            reload(state, ctx);
        }

        // -- Credits/Debts add form --
        Message::AddCdNameChanged(s) => state.add_cd_name = s,
        Message::AddCdCurChanged(s) => state.add_cd_cur = s,
        Message::AddCdSubmit => {
            let name = state.add_cd_name.trim().to_string();
            if !name.is_empty()
                && let Some(cd) = state.cd.as_mut()
            {
                let cur = {
                    let c = state.add_cd_cur.trim();
                    if c.is_empty() {
                        "E".to_string()
                    } else {
                        c.to_string()
                    }
                };
                if cd.add_entry(&name, &cur).is_ok() {
                    state.add_cd_name.clear();
                }
            }
            reload(state, ctx);
        }

        // -- Editable cells (buffer on input) --
        Message::HoldingsInput(name, m, txt) => {
            state.holdings_buf.insert((name, m), txt);
        }
        Message::PriceInput(name, m, txt) => {
            state.price_buf.insert((name, m), txt);
        }
        Message::LiqInput(name, m, txt) => {
            state.liq_buf.insert((name, m), txt);
        }
        Message::CdInput(name, m, txt) => {
            state.cd_buf.insert((name, m), txt);
        }

        // -- Editable cells (commit on submit) --
        Message::HoldingsSubmit(name, m) => {
            if let Some(v) = eval_buf(&state.holdings_buf, &name, m)
                && let Some(inv) = state.inv.as_mut()
            {
                let _ = inv.set_quantity(&name, (m + 1) as u32, v);
            }
            reload(state, ctx);
        }
        Message::PriceSubmit(name, m) => {
            if let Some(v) = eval_buf(&state.price_buf, &name, m)
                && let Some(inv) = state.inv.as_mut()
            {
                let _ = inv.set_price(&name, (m + 1) as u32, v);
            }
            reload(state, ctx);
        }
        Message::LiqSubmit(name, m) => {
            if let Some(v) = eval_buf(&state.liq_buf, &name, m)
                && let Some(liq) = state.liq.as_mut()
            {
                let _ = liq.set_value(&name, (m + 1) as u32, v);
            }
            reload(state, ctx);
        }
        Message::CdSubmit(name, m) => {
            if let Some(v) = eval_buf(&state.cd_buf, &name, m)
                && let Some(cd) = state.cd.as_mut()
            {
                let _ = cd.set_value(&name, (m + 1) as u32, v);
            }
            reload(state, ctx);
        }

        // -- Deletes --
        Message::RemoveInvestment(name) => {
            if let Some(inv) = state.inv.as_mut() {
                let _ = inv.remove_asset(&name);
            }
            reload(state, ctx);
        }
        Message::RemoveLiquidity(name) => {
            if let Some(liq) = state.liq.as_mut() {
                let _ = liq.remove_asset(&name);
            }
            reload(state, ctx);
        }
        Message::RemoveCreditDebt(name) => {
            if let Some(cd) = state.cd.as_mut() {
                let _ = cd.remove_entry(&name);
            }
            reload(state, ctx);
        }

        // -- Inline row edit --
        Message::BeginEdit(target, name) => {
            let rows = match target {
                EditTarget::Investment => &state.holdings_rows,
                EditTarget::Liquidity => &state.liq_rows,
                EditTarget::CreditDebt => &state.cd_rows,
            };
            let found = rows.iter().find(|r| r.name == name).cloned();
            let row = found.unwrap_or_default();
            state.editing = Some(RowEdit {
                target,
                original: name,
                name: row.name,
                category: row.category,
                link: row.link,
            });
        }
        Message::EditNameChanged(s) => {
            if let Some(e) = state.editing.as_mut() {
                e.name = s;
            }
        }
        Message::EditCatChanged(s) => {
            if let Some(e) = state.editing.as_mut() {
                e.category = s;
            }
        }
        Message::EditLinkChanged(s) => {
            if let Some(e) = state.editing.as_mut() {
                e.link = s;
            }
        }
        Message::EditSave => {
            if let Some(edit) = state.editing.take() {
                apply_row_edit(state, &edit);
            }
            reload(state, ctx);
        }
        Message::EditCancel => state.editing = None,
    }
    iced::Task::none()
}

/// Apply a saved inline row edit (rename, category, link) using the relevant
/// library setters. Errors are swallowed (mirroring the Python dialog).
fn apply_row_edit(state: &mut State, edit: &RowEdit) {
    let new_name = edit.name.trim();
    if new_name.is_empty() {
        return;
    }
    match edit.target {
        EditTarget::Investment => {
            if let Some(inv) = state.inv.as_mut() {
                if new_name != edit.original {
                    let _ = inv.rename_asset(&edit.original, new_name);
                }
                if !edit.category.is_empty() {
                    let _ = inv.set_category(new_name, &edit.category);
                }
                let _ = inv.set_link(new_name, edit.link.trim());
            }
        }
        EditTarget::Liquidity => {
            if let Some(liq) = state.liq.as_mut() {
                if new_name != edit.original {
                    let _ = liq.rename_asset(&edit.original, new_name);
                }
                if !edit.category.is_empty() {
                    let _ = liq.set_category(new_name, &edit.category);
                }
            }
        }
        EditTarget::CreditDebt => {
            if let Some(cd) = state.cd.as_mut()
                && new_name != edit.original
            {
                let _ = cd.rename_entry(&edit.original, new_name);
            }
        }
    }
}

/// Evaluate the buffered text for a cell. Returns `None` (ignore the edit) on a
/// parse error; an empty buffer evaluates to `0.0` (clearing the cell).
fn eval_buf(buf: &HashMap<(String, usize), String>, name: &str, m: usize) -> Option<f64> {
    let text = buf
        .get(&(name.to_string(), m))
        .map(String::as_str)
        .unwrap_or("");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        Some(0.0)
    } else {
        expr::eval(trimmed).ok()
    }
}

// ======================================================================
// Reload
// ======================================================================

/// Reload all library structs for the active year, recompute the display caches,
/// re-seed the edit buffers and rebuild the chart view-models.
pub fn reload(state: &mut State, ctx: Ctx) {
    state.inv = InvestmentHoldings::new(ctx.year).ok();
    state.liq = Liquidity::new(ctx.year).ok();
    state.cd = CreditsDebts::new(ctx.year).ok();

    // Materialize the investment grids.
    state.holdings_rows = state
        .inv
        .as_ref()
        .map(|inv| rows_from_df(&inv.df, "asset_name", true, false))
        .unwrap_or_default();
    state.price_rows = state
        .inv
        .as_ref()
        .map(|inv| rows_from_df(&inv.df_prices, "asset_name", true, false))
        .unwrap_or_default();
    state.value_rows = state
        .inv
        .as_ref()
        .and_then(|inv| inv.df_value().ok())
        .map(|df| rows_from_df(&df, "asset_name", true, false))
        .unwrap_or_default();

    // Materialize liquidity and credits/debts.
    state.liq_rows = state
        .liq
        .as_ref()
        .map(|liq| rows_from_df(&liq.df, "asset_name", true, true))
        .unwrap_or_default();
    state.cd_rows = state
        .cd
        .as_ref()
        .map(|cd| rows_from_df(&cd.df, "name", false, true))
        .unwrap_or_default();

    // Re-seed edit buffers from the freshly-loaded values.
    state.holdings_buf = seed_buffer(&state.holdings_rows);
    state.price_buf = seed_buffer(&state.price_rows);
    state.liq_buf = seed_buffer(&state.liq_rows);
    state.cd_buf = seed_buffer(&state.cd_rows);

    // Compute the Total NetWorth summary grid.
    state.summary_rows = compute_summary(state, ctx);

    // Build the charts (held as Options).
    state.pie = finguard_rs::plots::networth_allocation_pie(ctx.year, ctx.month)
        .ok()
        .flatten();
    state.evolution = finguard_rs::plots::networth_evolution_line(ctx.year)
        .ok()
        .flatten();

    // Drop any open editor whose row may no longer exist.
    state.editing = None;
}

/// Seed a per-cell text buffer from materialized rows, using [`fmt_cell`] (so
/// zeros render as empty strings).
fn seed_buffer(rows: &[DisplayRow]) -> HashMap<(String, usize), String> {
    let mut buf = HashMap::new();
    for r in rows {
        for (m, v) in r.months.iter().enumerate() {
            buf.insert((r.name.clone(), m), fmt_cell(*v));
        }
    }
    buf
}

/// Read a numeric column from `df` into a `Vec<f64>` (nulls → 0.0).
fn f64_col(df: &DataFrame, name: &str) -> Vec<f64> {
    df.column(name)
        .and_then(|c| {
            c.f64()
                .map(|s| s.iter().map(|o| o.unwrap_or(0.0)).collect())
        })
        .unwrap_or_default()
}

/// Read a string column from `df` into a `Vec<String>` (nulls → "").
fn str_col(df: &DataFrame, name: &str) -> Vec<String> {
    df.column(name)
        .and_then(|c| {
            c.str()
                .map(|s| s.iter().map(|o| o.unwrap_or("").to_string()).collect())
        })
        .unwrap_or_default()
}

/// Materialize a wide dataframe into [`DisplayRow`]s.
///
/// `name_col` is the identity column; `has_category` pulls a `category` column;
/// `has_currency` pulls a `currency` column. A `link` column is read when
/// present (investments).
fn rows_from_df(
    df: &DataFrame,
    name_col: &str,
    has_category: bool,
    has_currency: bool,
) -> Vec<DisplayRow> {
    let names = str_col(df, name_col);
    let n = names.len();
    if n == 0 {
        return Vec::new();
    }

    let categories = if has_category {
        str_col(df, "category")
    } else {
        Vec::new()
    };
    let currencies = if has_currency {
        str_col(df, "currency")
    } else {
        Vec::new()
    };
    let links = if df.get_column_names().iter().any(|c| c.as_str() == "link") {
        str_col(df, "link")
    } else {
        Vec::new()
    };

    let month_cols: Vec<Vec<f64>> = (1..=12).map(|m| f64_col(df, &format!("{m:02}"))).collect();

    (0..n)
        .map(|i| {
            let mut months = [0.0_f64; 12];
            for (m, col) in month_cols.iter().enumerate() {
                months[m] = col.get(i).copied().unwrap_or(0.0);
            }
            DisplayRow {
                name: names[i].clone(),
                category: categories.get(i).cloned().unwrap_or_default(),
                currency: currencies.get(i).cloned().unwrap_or_default(),
                link: links.get(i).cloned().unwrap_or_default(),
                months,
            }
        })
        .collect()
}

// ======================================================================
// Total NetWorth computation
// ======================================================================

/// Sum the 12 month columns of `df`, returning a per-month total.
fn sum_months(df: &DataFrame) -> [f64; 12] {
    let mut totals = [0.0_f64; 12];
    if df.height() == 0 {
        return totals;
    }
    for (m, total) in totals.iter_mut().enumerate() {
        let col = format!("{:02}", m + 1);
        *total = df
            .column(&col)
            .ok()
            .and_then(|c| c.f64().ok())
            .and_then(|s| s.sum())
            .unwrap_or(0.0);
    }
    totals
}

/// Sum the 12 month columns of `df` restricted to rows whose `category` equals
/// `cat`.
fn sum_months_for_category(df: &DataFrame, cat: &str) -> [f64; 12] {
    let filtered = df
        .clone()
        .lazy()
        .filter(col("category").eq(lit(cat)))
        .collect();
    match filtered {
        Ok(f) => sum_months(&f),
        Err(_) => [0.0; 12],
    }
}

/// Load the previous-year December net worth, summing investment value,
/// liquidity and credits/debts. Returns `None` on any error (mirrors Python's
/// broad `try/except`).
fn prev_december_networth(year: i32) -> Option<f64> {
    let prev = year - 1;
    let inv = InvestmentHoldings::new(prev).ok()?;
    let liq = Liquidity::new(prev).ok()?;
    let cd = CreditsDebts::new(prev).ok()?;

    let dec = "12";
    let inv_val = inv.df_value().ok()?;
    let inv_dec = if inv_val.height() > 0 {
        inv_val.column(dec).ok()?.f64().ok()?.sum().unwrap_or(0.0)
    } else {
        0.0
    };
    let liq_dec = if liq.df.height() > 0 {
        liq.df.column(dec).ok()?.f64().ok()?.sum().unwrap_or(0.0)
    } else {
        0.0
    };
    let cd_dec = if cd.df.height() > 0 {
        cd.df.column(dec).ok()?.f64().ok()?.sum().unwrap_or(0.0)
    } else {
        0.0
    };
    Some(inv_dec + liq_dec + cd_dec)
}

/// Compute the Total NetWorth summary rows from the loaded tables.
///
/// Ports `ui_networth.py` lines ~179-236: per-category investment sums (from
/// `df_value`), liquidity and credits/debts sums, net worth, NW change and
/// % change (January uses the previous-year December net worth if available).
fn compute_summary(state: &State, ctx: Ctx) -> Vec<SummaryRow> {
    let inv_value = state.inv.as_ref().and_then(|inv| inv.df_value().ok());

    // Per-category investment monthly sums.
    let mut inv_by_cat: Vec<(String, [f64; 12])> = Vec::new();
    for cat in INVESTMENT_CATEGORIES {
        let totals = inv_value
            .as_ref()
            .map(|df| sum_months_for_category(df, cat))
            .unwrap_or([0.0; 12]);
        inv_by_cat.push((cat.to_string(), totals));
    }

    let inv_totals: [f64; 12] = {
        let mut t = [0.0_f64; 12];
        for (_, vals) in &inv_by_cat {
            for (i, v) in vals.iter().enumerate() {
                t[i] += v;
            }
        }
        t
    };

    let liq_totals = state
        .liq
        .as_ref()
        .map(|l| sum_months(&l.df))
        .unwrap_or([0.0; 12]);
    let cd_totals = state
        .cd
        .as_ref()
        .map(|c| sum_months(&c.df))
        .unwrap_or([0.0; 12]);

    let mut net_totals = [0.0_f64; 12];
    for i in 0..12 {
        net_totals[i] = inv_totals[i] + liq_totals[i] + cd_totals[i];
    }

    let prev_dec_nw = prev_december_networth(ctx.year);

    let mut nw_change: [Option<f64>; 12] = [None; 12];
    let mut pct_change: [Option<f64>; 12] = [None; 12];
    for i in 0..12 {
        let prev = if i == 0 {
            prev_dec_nw
        } else {
            Some(net_totals[i - 1])
        };
        let cur = net_totals[i];
        match prev {
            Some(p) if p != 0.0 && cur != 0.0 => {
                nw_change[i] = Some(cur - p);
                pct_change[i] = Some((cur - p) / p * 100.0);
            }
            Some(p) if cur != 0.0 => {
                nw_change[i] = Some(cur - p);
                pct_change[i] = None;
            }
            _ => {
                nw_change[i] = None;
                pct_change[i] = None;
            }
        }
    }

    let to_opt = |arr: [f64; 12]| -> [Option<f64>; 12] {
        let mut out = [None; 12];
        for (i, v) in arr.iter().enumerate() {
            out[i] = Some(*v);
        }
        out
    };

    let mut rows: Vec<SummaryRow> = Vec::new();
    for (cat, vals) in &inv_by_cat {
        rows.push(SummaryRow {
            label: cat.clone(),
            bold: false,
            is_pct: false,
            values: to_opt(*vals),
        });
    }
    rows.push(SummaryRow {
        label: "Liquidity".to_string(),
        bold: false,
        is_pct: false,
        values: to_opt(liq_totals),
    });
    rows.push(SummaryRow {
        label: "Credits/Debts".to_string(),
        bold: false,
        is_pct: false,
        values: to_opt(cd_totals),
    });
    rows.push(SummaryRow {
        label: "Net Worth".to_string(),
        bold: true,
        is_pct: false,
        values: to_opt(net_totals),
    });
    rows.push(SummaryRow {
        label: "NW Change".to_string(),
        bold: false,
        is_pct: false,
        values: nw_change,
    });
    rows.push(SummaryRow {
        label: "% NW Change".to_string(),
        bold: false,
        is_pct: true,
        values: pct_change,
    });
    rows
}

// ======================================================================
// View
// ======================================================================

/// Render the tab body.
pub fn view<'a>(state: &'a State, ctx: Ctx) -> Element<'a, AppMessage> {
    let selector = sub_tab_bar(state.sub_tab);

    let body: Element<'a, AppMessage> = match state.sub_tab {
        SubTab::Investments => investments_view(state, ctx),
        SubTab::Liquidity => liquidity_view(state, ctx),
        SubTab::Total => total_view(state, ctx),
    };

    let content = column![selector, body].spacing(12);

    // If an inline editor is open, stack it above the body.
    let content: Element<'a, AppMessage> = if let Some(edit) = &state.editing {
        column![edit_dialog(edit), content].spacing(12).into()
    } else {
        content.into()
    };

    scrollable(content).height(Length::Fill).into()
}

/// The sub-tab selector (row of buttons).
fn sub_tab_bar<'a>(active: SubTab) -> Element<'a, AppMessage> {
    let mut bar = row![].spacing(8);
    for t in SubTab::ALL {
        let style = if t == active {
            button::primary
        } else {
            button::secondary
        };
        bar = bar.push(
            button(text(t.label()))
                .style(style)
                .on_press(AppMessage::NetWorth(Message::SubTabSelected(t))),
        );
    }
    bar.into()
}

// ----------------------------------------------------------------------
// Investments sub-tab
// ----------------------------------------------------------------------

fn investments_view<'a>(state: &'a State, ctx: Ctx) -> Element<'a, AppMessage> {
    // Add-asset form.
    let cats: Vec<String> = INVESTMENT_CATEGORIES
        .iter()
        .map(|s| s.to_string())
        .collect();
    let selected = if state.add_inv_cat.is_empty() {
        Some(INVESTMENT_CATEGORIES[0].to_string())
    } else {
        Some(state.add_inv_cat.clone())
    };
    let form = row![
        text("Add Asset"),
        text_input("Asset name", &state.add_inv_name)
            .on_input(|s| AppMessage::NetWorth(Message::AddInvNameChanged(s)))
            .width(Length::Fixed(180.0)),
        pick_list(cats, selected, |s| AppMessage::NetWorth(
            Message::AddInvCatChanged(s)
        ))
        .width(Length::Fixed(150.0)),
        text_input("Link (optional)", &state.add_inv_link)
            .on_input(|s| AppMessage::NetWorth(Message::AddInvLinkChanged(s)))
            .width(Length::Fixed(220.0)),
        button(text("Add")).on_press(AppMessage::NetWorth(Message::AddInvSubmit)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    // Inner view selector.
    let mut inner_bar = row![].spacing(8);
    for v in InvView::ALL {
        let style = if v == state.inv_view {
            button::primary
        } else {
            button::secondary
        };
        inner_bar = inner_bar.push(
            button(text(v.label()))
                .style(style)
                .on_press(AppMessage::NetWorth(Message::InvViewSelected(v))),
        );
    }

    let grid: Element<'a, AppMessage> = match state.inv_view {
        InvView::Holdings => investment_grid(
            &state.holdings_rows,
            &state.holdings_buf,
            ctx.year,
            "Holdings",
            CellKind::Holdings,
        ),
        InvView::Prices => investment_grid(
            &state.price_rows,
            &state.price_buf,
            ctx.year,
            "Prices",
            CellKind::Price,
        ),
        InvView::Value => investment_value_grid(&state.value_rows, ctx.year),
    };

    column![form, inner_bar, grid].spacing(12).into()
}

/// Which setter an investment editable cell drives.
#[derive(Clone, Copy)]
enum CellKind {
    Holdings,
    Price,
}

/// Private helper for the wide investment grid (`Asset | Category | 12 months`),
/// editable. Mirrors `ui_tables._build_investment_table` for the editable case.
fn investment_grid<'a>(
    rows: &'a [DisplayRow],
    buf: &'a HashMap<(String, usize), String>,
    year: i32,
    title: &str,
    kind: CellKind,
) -> Element<'a, AppMessage> {
    let title_el = section_title::<AppMessage>(&format!("{title} — {year}"));

    if rows.is_empty() {
        return column![title_el, text("No assets yet.")].spacing(8).into();
    }

    let mut grid = column![header_row_inv()].spacing(2);
    for r in rows {
        let mut line = row![].spacing(2).align_y(Alignment::Center);

        // Name + edit button.
        line = line.push(label_cell::<AppMessage>(&r.name, NAME_W, false));
        line = line.push(button(text("edit").size(12)).style(button::text).on_press(
            AppMessage::NetWorth(Message::BeginEdit(EditTarget::Investment, r.name.clone())),
        ));
        line = line.push(label_cell::<AppMessage>(&r.category, CAT_W, false));

        for m in 0..12 {
            let display = buf
                .get(&(r.name.clone(), m))
                .map(String::as_str)
                .unwrap_or("");
            let name = r.name.clone();
            let (on_input, on_submit): (Box<dyn Fn(String) -> AppMessage>, AppMessage) = match kind
            {
                CellKind::Holdings => (
                    {
                        let name = name.clone();
                        Box::new(move |s| {
                            AppMessage::NetWorth(Message::HoldingsInput(name.clone(), m, s))
                        })
                    },
                    AppMessage::NetWorth(Message::HoldingsSubmit(name.clone(), m)),
                ),
                CellKind::Price => (
                    {
                        let name = name.clone();
                        Box::new(move |s| {
                            AppMessage::NetWorth(Message::PriceInput(name.clone(), m, s))
                        })
                    },
                    AppMessage::NetWorth(Message::PriceSubmit(name.clone(), m)),
                ),
            };
            line = line.push(numeric_cell(display, MONTH_W, on_input, on_submit));
        }

        // Delete (holdings view only mirrors Python show_delete=True; we offer
        // delete on both editable investment views for convenience).
        line = line.push(button(text("x").size(12)).style(button::danger).on_press(
            AppMessage::NetWorth(Message::RemoveInvestment(r.name.clone())),
        ));

        grid = grid.push(line);
    }

    column![title_el, grid].spacing(8).into()
}

/// Private helper for the read-only investment value grid (with a bold Totals
/// row). Mirrors `ui_tables._build_investment_table` for `editable=False`.
fn investment_value_grid<'a>(rows: &'a [DisplayRow], year: i32) -> Element<'a, AppMessage> {
    let title_el = section_title::<AppMessage>(&format!("Value — {year} (quantity × price)"));

    if rows.is_empty() {
        return column![title_el, text("No assets yet.")].spacing(8).into();
    }

    let mut grid = column![header_row_inv()].spacing(2);
    let mut totals = [0.0_f64; 12];

    for r in rows {
        let mut line = row![].spacing(2).align_y(Alignment::Center);
        line = line.push(label_cell::<AppMessage>(&r.name, NAME_W, false));
        line = line.push(label_cell::<AppMessage>(&r.category, CAT_W, false));
        for (total, &month) in totals.iter_mut().zip(r.months.iter()) {
            *total += month;
            line = line.push(label_cell::<AppMessage>(
                &fmt_thousands(month),
                MONTH_W,
                true,
            ));
        }
        grid = grid.push(line);
    }

    // Bold totals row.
    let mut total_line = row![].spacing(2).align_y(Alignment::Center);
    total_line = total_line.push(header_cell::<AppMessage>("Total", NAME_W));
    total_line = total_line.push(label_cell::<AppMessage>("", CAT_W, false));
    for total in totals {
        total_line = total_line.push(header_cell::<AppMessage>(&fmt_thousands(total), MONTH_W));
    }
    grid = grid.push(total_line);

    column![title_el, grid].spacing(8).into()
}

/// Header row for the investment grids (`Asset | Category | Jan..Dec`).
fn header_row_inv<'a>() -> Element<'a, AppMessage> {
    let mut head = row![].spacing(2);
    head = head.push(header_cell::<AppMessage>("Asset", NAME_W + 40.0));
    head = head.push(header_cell::<AppMessage>("Category", CAT_W));
    for abbr in MONTH_ABBR {
        head = head.push(header_cell::<AppMessage>(abbr, MONTH_W));
    }
    head.into()
}

// ----------------------------------------------------------------------
// Liquidity / Credits-Debts sub-tab
// ----------------------------------------------------------------------

fn liquidity_view<'a>(state: &'a State, ctx: Ctx) -> Element<'a, AppMessage> {
    // -- Liquidity add form --
    let liq_cats: Vec<String> = LIQUIDITY_CATEGORIES.iter().map(|s| s.to_string()).collect();
    let liq_selected = if state.add_liq_cat.is_empty() {
        Some(LIQUIDITY_CATEGORIES[0].to_string())
    } else {
        Some(state.add_liq_cat.clone())
    };
    let liq_cur = if state.add_liq_cur.is_empty() {
        "E"
    } else {
        &state.add_liq_cur
    };
    let liq_form = row![
        text("Add Liquidity Asset"),
        text_input("Asset name", &state.add_liq_name)
            .on_input(|s| AppMessage::NetWorth(Message::AddLiqNameChanged(s)))
            .width(Length::Fixed(180.0)),
        pick_list(liq_cats, liq_selected, |s| AppMessage::NetWorth(
            Message::AddLiqCatChanged(s)
        ))
        .width(Length::Fixed(160.0)),
        text_input("Cur", liq_cur)
            .on_input(|s| AppMessage::NetWorth(Message::AddLiqCurChanged(s)))
            .width(Length::Fixed(60.0)),
        button(text("Add")).on_press(AppMessage::NetWorth(Message::AddLiqSubmit)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let liq_grid = simple_value_grid(
        &state.liq_rows,
        &state.liq_buf,
        ctx.year,
        "Liquidity",
        true,
        GridKind::Liquidity,
    );

    // -- Credits/Debts add form --
    let cd_cur = if state.add_cd_cur.is_empty() {
        "E"
    } else {
        &state.add_cd_cur
    };
    let cd_form = column![
        text("Add Credit / Debt"),
        text("Positive = credit, negative = debt").size(12),
        row![
            text_input("Name", &state.add_cd_name)
                .on_input(|s| AppMessage::NetWorth(Message::AddCdNameChanged(s)))
                .width(Length::Fixed(180.0)),
            text_input("Cur", cd_cur)
                .on_input(|s| AppMessage::NetWorth(Message::AddCdCurChanged(s)))
                .width(Length::Fixed(60.0)),
            button(text("Add")).on_press(AppMessage::NetWorth(Message::AddCdSubmit)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    ]
    .spacing(4);

    let cd_grid = simple_value_grid(
        &state.cd_rows,
        &state.cd_buf,
        ctx.year,
        "Credits / Debts",
        false,
        GridKind::CreditDebt,
    );

    column![
        liq_form,
        liq_grid,
        container(text("")).height(Length::Fixed(16.0)),
        cd_form,
        cd_grid,
    ]
    .spacing(12)
    .into()
}

/// Which grid (and setter target) a simple value grid drives.
#[derive(Clone, Copy)]
enum GridKind {
    Liquidity,
    CreditDebt,
}

/// Private helper for the simple value grid (`Name | [Category] | Cur | 12
/// months`), editable, with a bold totals row. Mirrors
/// `ui_tables._build_simple_value_table`.
///
/// `has_category` adds a category column (liquidity) and a category field in the
/// row editor; credits/debts has none.
fn simple_value_grid<'a>(
    rows: &'a [DisplayRow],
    buf: &'a HashMap<(String, usize), String>,
    year: i32,
    title: &str,
    has_category: bool,
    kind: GridKind,
) -> Element<'a, AppMessage> {
    let title_el = section_title::<AppMessage>(&format!("{title} — {year}"));

    let empty_msg = match kind {
        GridKind::Liquidity => "No liquidity assets yet.",
        GridKind::CreditDebt => "No credits or debts yet.",
    };
    if rows.is_empty() {
        return column![title_el, text(empty_msg)].spacing(8).into();
    }

    // Header.
    let mut head = row![].spacing(2);
    head = head.push(header_cell::<AppMessage>("Name", NAME_W + 40.0));
    if has_category {
        head = head.push(header_cell::<AppMessage>("Category", CAT_W));
    }
    head = head.push(header_cell::<AppMessage>("Cur", CUR_W));
    for abbr in MONTH_ABBR {
        head = head.push(header_cell::<AppMessage>(abbr, MONTH_W));
    }
    head = head.push(header_cell::<AppMessage>("", ACTION_W));

    let mut grid = column![head].spacing(2);
    let mut totals = [0.0_f64; 12];

    let edit_target = match kind {
        GridKind::Liquidity => EditTarget::Liquidity,
        GridKind::CreditDebt => EditTarget::CreditDebt,
    };

    for r in rows {
        let mut line = row![].spacing(2).align_y(Alignment::Center);

        line = line.push(label_cell::<AppMessage>(&r.name, NAME_W, false));
        line = line.push(button(text("edit").size(12)).style(button::text).on_press(
            AppMessage::NetWorth(Message::BeginEdit(edit_target, r.name.clone())),
        ));
        if has_category {
            line = line.push(label_cell::<AppMessage>(&r.category, CAT_W, false));
        }
        line = line.push(label_cell::<AppMessage>(&r.currency, CUR_W, false));

        for (m, (total, &month)) in totals.iter_mut().zip(r.months.iter()).enumerate() {
            *total += month;
            let display = buf
                .get(&(r.name.clone(), m))
                .map(String::as_str)
                .unwrap_or("");
            let name = r.name.clone();
            let (on_input, on_submit): (Box<dyn Fn(String) -> AppMessage>, AppMessage) = match kind
            {
                GridKind::Liquidity => (
                    {
                        let name = name.clone();
                        Box::new(move |s| {
                            AppMessage::NetWorth(Message::LiqInput(name.clone(), m, s))
                        })
                    },
                    AppMessage::NetWorth(Message::LiqSubmit(name.clone(), m)),
                ),
                GridKind::CreditDebt => (
                    {
                        let name = name.clone();
                        Box::new(move |s| {
                            AppMessage::NetWorth(Message::CdInput(name.clone(), m, s))
                        })
                    },
                    AppMessage::NetWorth(Message::CdSubmit(name.clone(), m)),
                ),
            };
            line = line.push(numeric_cell(display, MONTH_W, on_input, on_submit));
        }

        let remove_msg = match kind {
            GridKind::Liquidity => Message::RemoveLiquidity(r.name.clone()),
            GridKind::CreditDebt => Message::RemoveCreditDebt(r.name.clone()),
        };
        line = line.push(
            button(text("delete").size(12))
                .style(button::danger)
                .on_press(AppMessage::NetWorth(remove_msg)),
        );

        grid = grid.push(line);
    }

    // Bold totals row.
    let mut total_line = row![].spacing(2).align_y(Alignment::Center);
    total_line = total_line.push(header_cell::<AppMessage>("Total", NAME_W + 40.0));
    if has_category {
        total_line = total_line.push(label_cell::<AppMessage>("", CAT_W, false));
    }
    total_line = total_line.push(label_cell::<AppMessage>("", CUR_W, false));
    for total in totals {
        total_line = total_line.push(header_cell::<AppMessage>(&fmt_thousands(total), MONTH_W));
    }
    total_line = total_line.push(label_cell::<AppMessage>("", ACTION_W, false));
    grid = grid.push(total_line);

    column![title_el, grid].spacing(8).into()
}

// ----------------------------------------------------------------------
// Total NetWorth sub-tab
// ----------------------------------------------------------------------

fn total_view<'a>(state: &'a State, ctx: Ctx) -> Element<'a, AppMessage> {
    let title_el = section_title::<AppMessage>(&format!("Net Worth — {}", ctx.year));

    // Header: label column + month abbreviations.
    let mut head = row![].spacing(2);
    head = head.push(header_cell::<AppMessage>("", NAME_W));
    for abbr in MONTH_ABBR {
        head = head.push(header_cell::<AppMessage>(abbr, MONTH_W));
    }

    let mut grid = column![head].spacing(2);
    for r in &state.summary_rows {
        let mut line = row![].spacing(2).align_y(Alignment::Center);
        if r.bold {
            line = line.push(header_cell::<AppMessage>(&r.label, NAME_W));
        } else {
            line = line.push(label_cell::<AppMessage>(&r.label, NAME_W, false));
        }
        for v in r.values {
            let txt = match v {
                None => String::new(),
                Some(0.0) => String::new(),
                Some(val) if r.is_pct => widgets::fmt_pct_thousands(val),
                Some(val) => fmt_thousands(val),
            };
            if r.bold {
                line = line.push(header_cell::<AppMessage>(&txt, MONTH_W));
            } else {
                line = line.push(label_cell::<AppMessage>(&txt, MONTH_W, true));
            }
        }
        grid = grid.push(line);
    }

    // Charts.
    let pie: Element<'a, AppMessage> = match &state.pie {
        Some(p) => charts::pie(p, charts::DEFAULT_CHART_HEIGHT),
        None => container(text("No data"))
            .height(Length::Fixed(charts::DEFAULT_CHART_HEIGHT))
            .into(),
    };
    let evolution: Element<'a, AppMessage> = match &state.evolution {
        Some(e) => charts::networth_evolution(e, charts::DEFAULT_CHART_HEIGHT),
        None => container(text("No data"))
            .height(Length::Fixed(charts::DEFAULT_CHART_HEIGHT))
            .into(),
    };

    let charts_row = row![
        column![text("Allocation"), pie]
            .spacing(4)
            .width(Length::FillPortion(1)),
        column![text("Net Worth Evolution"), evolution]
            .spacing(4)
            .width(Length::FillPortion(1)),
    ]
    .spacing(16);

    column![title_el, grid, charts_row].spacing(16).into()
}

// ----------------------------------------------------------------------
// Inline row-edit dialog
// ----------------------------------------------------------------------

/// Render the inline row editor (rename + optional category/link).
fn edit_dialog<'a>(edit: &'a RowEdit) -> Element<'a, AppMessage> {
    let mut fields = column![
        text(format!("Edit {}", edit.original)),
        text_input("Name", &edit.name)
            .on_input(|s| AppMessage::NetWorth(Message::EditNameChanged(s)))
            .width(Length::Fixed(260.0)),
    ]
    .spacing(8);

    match edit.target {
        EditTarget::Investment => {
            let cats: Vec<String> = INVESTMENT_CATEGORIES
                .iter()
                .map(|s| s.to_string())
                .collect();
            let sel = if edit.category.is_empty() {
                Some(INVESTMENT_CATEGORIES[0].to_string())
            } else {
                Some(edit.category.clone())
            };
            fields = fields.push(
                pick_list(cats, sel, |s| {
                    AppMessage::NetWorth(Message::EditCatChanged(s))
                })
                .width(Length::Fixed(260.0)),
            );
            fields = fields.push(
                text_input("Link URL", &edit.link)
                    .on_input(|s| AppMessage::NetWorth(Message::EditLinkChanged(s)))
                    .width(Length::Fixed(260.0)),
            );
        }
        EditTarget::Liquidity => {
            let cats: Vec<String> = LIQUIDITY_CATEGORIES.iter().map(|s| s.to_string()).collect();
            let sel = if edit.category.is_empty() {
                Some(LIQUIDITY_CATEGORIES[0].to_string())
            } else {
                Some(edit.category.clone())
            };
            fields = fields.push(
                pick_list(cats, sel, |s| {
                    AppMessage::NetWorth(Message::EditCatChanged(s))
                })
                .width(Length::Fixed(260.0)),
            );
        }
        EditTarget::CreditDebt => {}
    }

    let buttons = row![
        button(text("Save")).on_press(AppMessage::NetWorth(Message::EditSave)),
        button(text("Cancel"))
            .style(button::secondary)
            .on_press(AppMessage::NetWorth(Message::EditCancel)),
    ]
    .spacing(8);

    container(column![fields, buttons].spacing(12))
        .padding(12)
        .style(container::bordered_box)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_default_view_without_panicking() {
        let state = State::default();
        let ctx = Ctx {
            year: 2026,
            month: 6,
        };
        let _ = view(&state, ctx);
    }

    #[test]
    fn builds_all_subtabs() {
        let mut state = State::default();
        let ctx = Ctx {
            year: 2026,
            month: 6,
        };
        for t in SubTab::ALL {
            state.sub_tab = t;
            let _ = view(&state, ctx);
        }
        for v in InvView::ALL {
            state.sub_tab = SubTab::Investments;
            state.inv_view = v;
            let _ = view(&state, ctx);
        }
    }

    #[test]
    fn eval_buf_empty_is_zero_and_errors_ignored() {
        let mut buf: HashMap<(String, usize), String> = HashMap::new();
        buf.insert(("A".into(), 0), "".into());
        buf.insert(("A".into(), 1), "2 + 3".into());
        buf.insert(("A".into(), 2), "not a number".into());
        assert_eq!(eval_buf(&buf, "A", 0), Some(0.0));
        assert_eq!(eval_buf(&buf, "A", 1), Some(5.0));
        assert_eq!(eval_buf(&buf, "A", 2), None);
    }
}
