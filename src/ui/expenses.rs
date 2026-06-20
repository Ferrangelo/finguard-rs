//! Expenses tab (iced 0.14).
//!
//! Ports the NiceGUI `ui_expenses.py` tab. It owns its own data — the
//! [`DetailedExpenses`] for the active month and the [`RecurringExpenses`] for
//! the active year — loading/refreshing them in [`reload`]. Four sub-tabs are
//! exposed (Detailed, Summary, Recurring, Mappings), selectable via a small row
//! of buttons.
//!
//! Forms (add/edit expense, add recurring, add mapping) are rendered as inline
//! panels toggled by state rather than overlay dialogs, which keeps the widget
//! lifetimes trivial in iced. Free-text amount fields are evaluated via
//! [`finguard_rs::expr::eval`] on submit (matching the Python `_safe_eval_expr`).

use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Theme};

use finguard_rs::config::{
    self, add_mapping, get_all_mappings, get_known_categories, get_mapping, remove_mapping,
};
use finguard_rs::df_operations::{
    DetailedExpenses, RecurringExpenses, normalize_category_value, resolve_category,
};
use finguard_rs::expr::eval;
use finguard_rs::paths::{PRIMARIES_FILENAME, SECONDARIES_FILENAME, get_year_summary_path};
use finguard_rs::plots::{
    BarChart, LineChart, PieChart, category_expenses_over_months, cumulative_expenses_pie,
    monthly_expenses_comparison, monthly_expenses_pie,
};

use crate::ui::charts;
use crate::ui::widgets::{self, MONTH_NAMES, fmt_2dp, header_cell, label_cell, section_title};
use crate::ui::{Ctx, Message as AppMessage};

use std::collections::BTreeSet;

// ======================================================================
// Sub-tab / summary-kind enums
// ======================================================================

/// The four sub-tabs of the Expenses tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubTab {
    /// The editable detailed-expenses table.
    #[default]
    Detailed,
    /// Per-month and cumulative-year summary tables + charts.
    Summary,
    /// Recurring-expense definitions.
    Recurring,
    /// Expense-name → category mappings.
    Mappings,
}

impl SubTab {
    const ALL: [SubTab; 4] = [
        SubTab::Detailed,
        SubTab::Summary,
        SubTab::Recurring,
        SubTab::Mappings,
    ];

    fn label(self) -> &'static str {
        match self {
            SubTab::Detailed => "Detailed expenses",
            SubTab::Summary => "Summary",
            SubTab::Recurring => "Recurring",
            SubTab::Mappings => "Mappings",
        }
    }
}

/// Whether summaries/charts use the primary or secondary category column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    /// `primary_category`.
    #[default]
    Primary,
    /// `secondary_category`.
    Secondary,
}

impl Kind {
    /// The library `kind` string (`"primary"` / `"secondary"`).
    fn as_str(self) -> &'static str {
        match self {
            Kind::Primary => "primary",
            Kind::Secondary => "secondary",
        }
    }

    /// The dataframe column name (`"primary_category"` / `"secondary_category"`).
    fn col(self) -> &'static str {
        match self {
            Kind::Primary => "primary_category",
            Kind::Secondary => "secondary_category",
        }
    }
}

// ======================================================================
// View-model row types (materialised in `reload`)
// ======================================================================

/// One row of the detailed-expenses table, already formatted for display.
///
/// `index` is the row's position in `de.expense_df` (the stable key used for
/// edit/delete), matching the Python `with_row_index("id")`.
#[derive(Debug, Clone)]
struct ExpenseRow {
    index: u32,
    name: String,
    day: i32,
    amount: f64,
    currency: String,
    ref_amount: f64,
    primary: String,
    secondary: String,
}

/// One recurring-expense definition row.
#[derive(Debug, Clone)]
struct RecurringRow {
    index: u32,
    name: String,
    day: i64,
    amount: f64,
    currency: String,
    primary: String,
    secondary: String,
}

/// One mapping row (expense name → categories).
#[derive(Debug, Clone)]
struct MappingRow {
    name: String,
    primary: String,
    secondary: String,
}

/// A generic wide table (header labels + string rows) for the summary panels.
#[derive(Debug, Clone, Default)]
struct WideTable {
    headers: Vec<String>,
    /// Per-row, per-column alignment-right flag (true for numeric columns).
    align_right: Vec<bool>,
    rows: Vec<Vec<String>>,
}

// ======================================================================
// Edit-form buffer (shared by add / edit expense)
// ======================================================================

/// Free-text buffers for the expense add/edit form.
#[derive(Debug, Clone, Default)]
struct ExpenseForm {
    name: String,
    day: String,
    amount: String,
    currency: String,
    primary: String,
    secondary: String,
}

/// Free-text buffers for the recurring add form.
#[derive(Debug, Clone)]
struct RecurringForm {
    name: String,
    day: String,
    amount: String,
    currency: String,
    primary: String,
    secondary: String,
}

impl Default for RecurringForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            day: "1".to_string(),
            amount: String::new(),
            currency: "E".to_string(),
            primary: String::new(),
            secondary: String::new(),
        }
    }
}

/// Free-text buffers for the mapping add form.
#[derive(Debug, Clone, Default)]
struct MappingForm {
    name: String,
    primary: String,
    secondary: String,
}

// ======================================================================
// State
// ======================================================================

/// State owned by the Expenses tab.
#[derive(Default)]
pub struct State {
    active_sub: SubTab,

    // -- shared data (owned by this tab) --
    de: Option<DetailedExpenses>,
    rec: Option<RecurringExpenses>,

    // -- detailed sub-tab --
    filter_name: String,
    filter_category: String,
    filter_amount_min: String,
    filter_amount_max: String,
    rows: Vec<ExpenseRow>,
    /// `Some` => the add form is open; `Some` with `editing == Some(idx)` => edit.
    expense_form: Option<ExpenseForm>,
    /// When editing, the row index being edited; `None` means adding.
    editing: Option<u32>,
    /// When set, a pending delete confirmation for this row index + label.
    pending_delete: Option<(u32, String)>,

    // category suggestion sets (sorted, normalised)
    primary_suggestions: Vec<String>,
    secondary_suggestions: Vec<String>,

    // -- summary sub-tab --
    kind: Kind,
    monthly_table: WideTable,
    monthly_pie: Option<PieChart>,
    cumulative_table: Option<WideTable>,
    cumulative_pie: Option<PieChart>,
    /// Months available in the cumulative summary (month-number, present order).
    available_months: Vec<u32>,
    selected_months: Vec<u32>,
    comparison_bar: Option<BarChart>,
    /// Categories available in the cumulative summary (excluding "Total").
    available_categories: Vec<String>,
    selected_categories: Vec<String>,
    category_line: Option<LineChart>,

    // -- recurring sub-tab --
    recurring_rows: Vec<RecurringRow>,
    recurring_form: RecurringForm,

    // -- mappings sub-tab --
    mapping_rows: Vec<MappingRow>,
    mapping_form: MappingForm,

    // -- shared status line (last action result / error) --
    status: String,
}

// ======================================================================
// Messages
// ======================================================================

/// Messages emitted by the Expenses tab. All variants are `Clone`.
#[derive(Debug, Clone)]
pub enum Message {
    SubTabSelected(SubTab),

    // Detailed: filters
    FilterNameChanged(String),
    FilterCategoryChanged(String),
    FilterAmountMinChanged(String),
    FilterAmountMaxChanged(String),

    // Detailed: add / edit form
    OpenAddForm,
    OpenEditForm(u32),
    CloseForm,
    FormNameChanged(String),
    FormDayChanged(String),
    FormAmountChanged(String),
    FormCurrencyChanged(String),
    FormPrimaryChanged(String),
    FormSecondaryChanged(String),
    /// Look up `config::get_mapping` for the typed name and prefill categories.
    FormAutoMap,
    SubmitForm,

    // Detailed: delete
    RequestDelete(u32),
    ConfirmDelete,
    CancelDelete,

    // Summary
    KindSelected(Kind),
    ToggleMonth(u32),
    ToggleCategory(String),
    RegenerateSummaries,

    // Recurring
    RecNameChanged(String),
    RecDayChanged(String),
    RecAmountChanged(String),
    RecCurrencyChanged(String),
    RecPrimaryChanged(String),
    RecSecondaryChanged(String),
    RecAutoMap,
    SubmitRecurring,
    ApplyRecurring,
    DeleteRecurring(u32),

    // Mappings
    MapNameChanged(String),
    MapPrimaryChanged(String),
    MapSecondaryChanged(String),
    SubmitMapping,
    DeleteMapping(String),
}

// ======================================================================
// reload — load/refresh all owned data + caches
// ======================================================================

/// Called when the active period (year/month) or shared data changes, and after
/// every mutation, to reload owned data and rebuild every display cache.
pub fn reload(state: &mut State, ctx: Ctx) {
    state.de = match DetailedExpenses::new(ctx.year, ctx.month) {
        Ok(de) => Some(de),
        Err(err) => {
            eprintln!(
                "finguard: expenses tab failed to load detailed expenses for {}-{:02}: {err}",
                ctx.year, ctx.month
            );
            None
        }
    };
    state.rec = match RecurringExpenses::new(ctx.year) {
        Ok(rec) => Some(rec),
        Err(err) => {
            eprintln!(
                "finguard: expenses tab failed to load recurring expenses for {}: {err}",
                ctx.year
            );
            None
        }
    };

    rebuild_suggestions(state, ctx);
    rebuild_expense_rows(state);
    rebuild_summary(state, ctx);
    rebuild_recurring_rows(state);
    rebuild_mapping_rows(state);
}

/// Rebuild the filtered detailed-expenses rows from `de` + the active filters.
fn rebuild_expense_rows(state: &mut State) {
    state.rows.clear();
    let Some(de) = &state.de else { return };

    let name = some_if_nonempty(&state.filter_name);
    let cat = some_if_nonempty(&state.filter_category);
    let min = parse_opt_f64(&state.filter_amount_min);
    let max = parse_opt_f64(&state.filter_amount_max);

    // Filter on an indexed copy of the df so each displayed row carries its true
    // position in the *unfiltered* `expense_df` (the `_idx` column). This is the
    // stable key used by `edit_row` / `delete_row`, mirroring the Python
    // `with_row_index("id")`-before-filter approach. No fuzzy matching needed.
    let filtered = match de.filter_expenses_indexed(name, cat, min, max) {
        Ok(df) => df,
        Err(err) => {
            eprintln!("finguard: expenses filter failed: {err}");
            return;
        }
    };

    // `_idx` is UInt32; read it as i64 then narrow to u32 for edit/delete.
    let indices = column_i64(&filtered, "_idx");
    let names = column_strings(&filtered, "expense_name");
    let days = column_dates_day(&filtered, "expense_date");
    let amounts = column_f64(&filtered, "expense_amount");
    let currencies = column_strings(&filtered, "currency");
    let ref_amounts = column_f64(&filtered, "expense_in_ref_currency");
    let primaries = column_strings(&filtered, "primary_category");
    let secondaries = column_strings(&filtered, "secondary_category");

    for fi in 0..filtered.height() {
        state.rows.push(ExpenseRow {
            index: indices.get(fi).copied().unwrap_or(0) as u32,
            name: names.get(fi).cloned().unwrap_or_default(),
            day: days.get(fi).copied().unwrap_or(0),
            amount: amounts.get(fi).copied().unwrap_or(0.0),
            currency: currencies.get(fi).cloned().unwrap_or_default(),
            ref_amount: ref_amounts.get(fi).copied().unwrap_or(0.0),
            primary: primaries.get(fi).cloned().unwrap_or_default(),
            secondary: secondaries.get(fi).cloned().unwrap_or_default(),
        });
    }
}

/// Rebuild the primary/secondary category suggestion lists, mirroring the Python
/// `open_add_dialog` gathering (month df + all-year summaries + known config).
fn rebuild_suggestions(state: &mut State, _ctx: Ctx) {
    let mut pri: BTreeSet<String> = BTreeSet::new();
    let mut sec: BTreeSet<String> = BTreeSet::new();

    if let Some(de) = &state.de {
        for v in column_strings(&de.expense_df, "primary_category") {
            if !v.is_empty() {
                pri.insert(v);
            }
        }
        for v in column_strings(&de.expense_df, "secondary_category") {
            if !v.is_empty() {
                sec.insert(v);
            }
        }
    }

    // Categories from every year's cumulative summary parquet.
    collect_summary_categories("primary", &mut pri);
    collect_summary_categories("secondary", &mut sec);

    // Manually registered known categories.
    if let Ok(known) = get_known_categories() {
        pri.extend(known.primary.into_iter().filter(|c| !c.is_empty()));
        sec.extend(known.secondary.into_iter().filter(|c| !c.is_empty()));
    }

    state.primary_suggestions = pri
        .into_iter()
        .map(|c| normalize_category_value(&c))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    state.secondary_suggestions = sec
        .into_iter()
        .map(|c| normalize_category_value(&c))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
}

/// Read every `<year>/<filename>` summary parquet, adding non-`Total` category
/// names of the given `kind` to `out`. Per-file errors are swallowed.
fn collect_summary_categories(kind: &str, out: &mut BTreeSet<String>) {
    use finguard_rs::paths;
    let Ok(root) = paths::get_dbs_root() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let (filename, cat_col) = if kind == "primary" {
        (PRIMARIES_FILENAME, "primary_category")
    } else {
        (SECONDARIES_FILENAME, "secondary_category")
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path().join(filename);
        if !path.exists() {
            continue;
        }
        if let Ok(df) = read_parquet(&path) {
            for c in column_strings(&df, cat_col) {
                if !c.is_empty() && c != "Total" {
                    out.insert(c);
                }
            }
        }
    }
}

/// Rebuild the summary sub-tab caches (monthly table + pie, cumulative table +
/// pie, comparison bar, category line).
fn rebuild_summary(state: &mut State, ctx: Ctx) {
    let kind = state.kind;
    let cat_col = kind.col();

    // Monthly summary table + pie.
    state.monthly_table = WideTable::default();
    state.monthly_pie = None;
    if let Some(de) = &state.de {
        if let Ok(df) = de.create_expenses_summary_table(cat_col) {
            let cats = column_strings(&df, cat_col);
            let totals = column_f64(&df, "total_expense_in_ref_currency");
            let mut t = WideTable {
                headers: vec!["Category".to_string(), "Total".to_string()],
                align_right: vec![false, true],
                rows: Vec::new(),
            };
            for i in 0..df.height() {
                t.rows.push(vec![
                    cats.get(i).cloned().unwrap_or_default(),
                    fmt_2dp(totals.get(i).copied().unwrap_or(0.0)),
                ]);
            }
            state.monthly_table = t;
        }
        state.monthly_pie = monthly_expenses_pie(de, kind.as_str()).unwrap_or(None);
    }

    // Cumulative year summary (only if the parquet exists).
    state.cumulative_table = None;
    state.cumulative_pie = None;
    state.available_months.clear();
    state.available_categories.clear();
    let filename = if matches!(kind, Kind::Primary) {
        PRIMARIES_FILENAME
    } else {
        SECONDARIES_FILENAME
    };
    if let Ok(path) = get_year_summary_path(ctx.year, filename)
        && path.exists()
        && let Ok(df) = read_parquet(&path)
    {
        let (table, months, categories) = build_cumulative_table(&df, cat_col, ctx.year);
        state.available_months = months;
        state.available_categories = categories;
        state.cumulative_table = Some(table);
        state.cumulative_pie = cumulative_expenses_pie(ctx.year, kind.as_str()).unwrap_or(None);

        // Default selections (mirroring Python defaults), clamped to availability.
        if state.selected_months.is_empty()
            || !state
                .selected_months
                .iter()
                .all(|m| state.available_months.contains(m))
        {
            state.selected_months = if state.available_months.contains(&ctx.month) {
                vec![ctx.month]
            } else {
                state.available_months.iter().take(1).copied().collect()
            };
        }
        if state.selected_categories.is_empty()
            || !state
                .selected_categories
                .iter()
                .all(|c| state.available_categories.contains(c))
        {
            state.selected_categories =
                state.available_categories.iter().take(1).cloned().collect();
        }
    }

    rebuild_summary_charts(state, ctx);
}

/// Recompute the comparison bar and category line charts from the current
/// month/category selections.
fn rebuild_summary_charts(state: &mut State, ctx: Ctx) {
    let kind = state.kind.as_str();

    let months: Vec<u32> = {
        let mut m = state.selected_months.clone();
        m.sort_unstable();
        m.truncate(3);
        m
    };
    state.comparison_bar = if months.is_empty() {
        None
    } else {
        monthly_expenses_comparison(ctx.year, &months, kind).unwrap_or(None)
    };

    let cats: Vec<String> = state.selected_categories.iter().take(3).cloned().collect();
    state.category_line = if cats.is_empty() {
        None
    } else {
        category_expenses_over_months(ctx.year, &cats, kind).unwrap_or(None)
    };
}

/// Build the wide cumulative table view-model from the summary dataframe,
/// returning `(table, available_months, available_categories)`. Hides rows that
/// are all-zero across month columns, except the `"Total"` row (Python lines
/// ~493-512).
fn build_cumulative_table(
    df: &polars::prelude::DataFrame,
    cat_col: &str,
    _year: i32,
) -> (WideTable, Vec<u32>, Vec<String>) {
    use polars::prelude::*;

    let col_names: Vec<String> = df
        .get_column_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    // Month columns: those of the form "YYYY-MM" where MM is digits.
    let month_cols: Vec<String> = col_names
        .iter()
        .filter(|c| {
            c.split_once('-')
                .map(|(_, mm)| !mm.is_empty() && mm.chars().all(|ch| ch.is_ascii_digit()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    let cats = column_strings(df, cat_col);

    // Per-month numeric columns.
    let month_values: Vec<Vec<f64>> = month_cols.iter().map(|c| column_f64(df, c)).collect();

    // Determine which rows to keep (drop all-zero non-Total rows).
    let n_rows = df.height();
    let mut keep = vec![true; n_rows];
    for (r, keep_r) in keep.iter_mut().enumerate() {
        let is_total = cats.get(r).map(|s| s == "Total").unwrap_or(false);
        if is_total {
            continue;
        }
        let all_zero = month_values
            .iter()
            .all(|mv| mv.get(r).map(|v| v.abs() < 1e-9).unwrap_or(true));
        if all_zero {
            *keep_r = false;
        }
    }

    // Headers: category label + month names.
    let mut headers = vec![col_label(cat_col)];
    let mut align_right = vec![false];
    for mc in &month_cols {
        headers.push(col_label(mc));
        align_right.push(true);
    }

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut categories: Vec<String> = Vec::new();
    for (r, &keep_r) in keep.iter().enumerate() {
        if !keep_r {
            continue;
        }
        let cat = cats.get(r).cloned().unwrap_or_default();
        let mut line = vec![cat.clone()];
        for mv in &month_values {
            line.push(fmt_2dp(mv.get(r).copied().unwrap_or(0.0)));
        }
        rows.push(line);
        if cat != "Total" {
            categories.push(cat);
        }
    }

    // Available months (numbers), in column order.
    let months: Vec<u32> = month_cols
        .iter()
        .filter_map(|c| c.split_once('-').and_then(|(_, mm)| mm.parse::<u32>().ok()))
        .collect();

    let _ = JoinType::Left; // keep polars prelude referenced without warnings
    (
        WideTable {
            headers,
            align_right,
            rows,
        },
        months,
        categories,
    )
}

/// Convert a summary column name to a display label: `"YYYY-MM"` → month name,
/// `"snake_case"` → Title Case (Python `_col_label`).
fn col_label(c: &str) -> String {
    if let Some((_, mm)) = c.split_once('-')
        && let Ok(m) = mm.parse::<usize>()
        && (1..=12).contains(&m)
    {
        return MONTH_NAMES[m - 1].to_string();
    }
    if c.contains('_') {
        return c
            .split('_')
            .map(title_case_word)
            .collect::<Vec<_>>()
            .join(" ");
    }
    c.to_string()
}

fn title_case_word(w: &str) -> String {
    let mut chars = w.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Rebuild the recurring-expenses table rows from `rec.df`.
fn rebuild_recurring_rows(state: &mut State) {
    state.recurring_rows.clear();
    let Some(rec) = &state.rec else { return };
    let df = &rec.df;
    let names = column_strings(df, "expense_name");
    let days = column_i64(df, "expense_day");
    let amounts = column_f64(df, "expense_amount");
    let currencies = column_strings(df, "currency");
    let primaries = column_strings(df, "primary_category");
    let secondaries = column_strings(df, "secondary_category");
    for i in 0..df.height() {
        state.recurring_rows.push(RecurringRow {
            index: i as u32,
            name: names.get(i).cloned().unwrap_or_default(),
            day: days.get(i).copied().unwrap_or(0),
            amount: amounts.get(i).copied().unwrap_or(0.0),
            currency: currencies.get(i).cloned().unwrap_or_default(),
            primary: primaries.get(i).cloned().unwrap_or_default(),
            secondary: secondaries.get(i).cloned().unwrap_or_default(),
        });
    }
}

/// Rebuild the mappings table rows from config.
fn rebuild_mapping_rows(state: &mut State) {
    state.mapping_rows.clear();
    if let Ok(map) = get_all_mappings() {
        for (name, m) in map {
            state.mapping_rows.push(MappingRow {
                name,
                primary: m.primary_category,
                secondary: m.secondary_category,
            });
        }
    }
}

// ======================================================================
// update
// ======================================================================

/// Handle a tab message. All work is synchronous, so we always return
/// [`iced::Task::none`].
pub fn update(state: &mut State, message: Message, ctx: Ctx) -> iced::Task<AppMessage> {
    match message {
        Message::SubTabSelected(sub) => {
            state.active_sub = sub;
            // Refresh summary when entering it (Python refreshes on tab switch).
            if sub == SubTab::Summary {
                rebuild_summary(state, ctx);
            }
        }

        // -- detailed filters --
        Message::FilterNameChanged(v) => {
            state.filter_name = v;
            rebuild_expense_rows(state);
        }
        Message::FilterCategoryChanged(v) => {
            state.filter_category = v;
            rebuild_expense_rows(state);
        }
        Message::FilterAmountMinChanged(v) => {
            state.filter_amount_min = v;
            rebuild_expense_rows(state);
        }
        Message::FilterAmountMaxChanged(v) => {
            state.filter_amount_max = v;
            rebuild_expense_rows(state);
        }

        // -- add / edit form --
        Message::OpenAddForm => {
            let today = chrono::Local::now().date_naive();
            use chrono::Datelike;
            let mut f = ExpenseForm {
                currency: "E".to_string(),
                day: today.day().to_string(),
                ..Default::default()
            };
            // If the active period differs from "today", a 1 is still a fine default.
            f.day = if ctx.year == today.year() && ctx.month == today.month() {
                today.day().to_string()
            } else {
                "1".to_string()
            };
            state.expense_form = Some(f);
            state.editing = None;
            state.status.clear();
        }
        Message::OpenEditForm(idx) => {
            if let Some(r) = state.rows.iter().find(|r| r.index == idx) {
                state.expense_form = Some(ExpenseForm {
                    name: r.name.clone(),
                    day: r.day.to_string(),
                    amount: fmt_2dp(r.amount),
                    currency: r.currency.clone(),
                    primary: r.primary.clone(),
                    secondary: r.secondary.clone(),
                });
                state.editing = Some(idx);
                state.status.clear();
            }
        }
        Message::CloseForm => {
            state.expense_form = None;
            state.editing = None;
        }
        Message::FormNameChanged(v) => set_form(state, |f| f.name = v),
        Message::FormDayChanged(v) => set_form(state, |f| f.day = v),
        Message::FormAmountChanged(v) => set_form(state, |f| f.amount = v),
        Message::FormCurrencyChanged(v) => set_form(state, |f| f.currency = v),
        Message::FormPrimaryChanged(v) => set_form(state, |f| f.primary = v),
        Message::FormSecondaryChanged(v) => set_form(state, |f| f.secondary = v),
        Message::FormAutoMap => {
            if let Some(f) = &mut state.expense_form
                && let Ok(Some(mapping)) = get_mapping(&f.name)
            {
                f.primary = normalize_category_value(&mapping.primary_category);
                f.secondary = normalize_category_value(&mapping.secondary_category);
                state.status = format!("Auto-mapped to {}", f.primary);
            }
        }
        Message::SubmitForm => submit_expense_form(state, ctx),

        // -- delete --
        Message::RequestDelete(idx) => {
            if let Some(r) = state.rows.iter().find(|r| r.index == idx) {
                state.pending_delete =
                    Some((idx, format!("\"{}\" ({})", r.name, fmt_2dp(r.amount))));
            }
        }
        Message::ConfirmDelete => {
            if let Some((idx, _)) = state.pending_delete.take()
                && let Some(de) = &mut state.de
            {
                match de.delete_row(idx) {
                    Ok(()) => state.status = "Expense deleted".to_string(),
                    Err(err) => state.status = format!("Delete failed: {err}"),
                }
                reload(state, ctx);
            }
        }
        Message::CancelDelete => state.pending_delete = None,

        // -- summary --
        Message::KindSelected(k) => {
            state.kind = k;
            // Reset selections so defaults recompute for the new kind.
            state.selected_months.clear();
            state.selected_categories.clear();
            rebuild_summary(state, ctx);
        }
        Message::ToggleMonth(m) => {
            toggle(&mut state.selected_months, m);
            rebuild_summary_charts(state, ctx);
        }
        Message::ToggleCategory(c) => {
            toggle(&mut state.selected_categories, c);
            rebuild_summary_charts(state, ctx);
        }
        Message::RegenerateSummaries => {
            if let Some(de) = &state.de {
                match de.update_all_summary_tables() {
                    Ok(_) => {
                        state.status =
                            "Summary tables updated (cashflow refreshes on tab switch)".to_string()
                    }
                    Err(err) => state.status = format!("Regenerate failed: {err}"),
                }
            }
            reload(state, ctx);
        }

        // -- recurring --
        Message::RecNameChanged(v) => state.recurring_form.name = v,
        Message::RecDayChanged(v) => state.recurring_form.day = v,
        Message::RecAmountChanged(v) => state.recurring_form.amount = v,
        Message::RecCurrencyChanged(v) => state.recurring_form.currency = v,
        Message::RecPrimaryChanged(v) => state.recurring_form.primary = v,
        Message::RecSecondaryChanged(v) => state.recurring_form.secondary = v,
        Message::RecAutoMap => {
            if let Ok(Some(mapping)) = get_mapping(&state.recurring_form.name) {
                state.recurring_form.primary = normalize_category_value(&mapping.primary_category);
                state.recurring_form.secondary =
                    normalize_category_value(&mapping.secondary_category);
            }
        }
        Message::SubmitRecurring => submit_recurring_form(state, ctx),
        Message::ApplyRecurring => {
            // Apply needs a fresh recurring + the mutable detailed expenses.
            let year = ctx.year;
            if let Some(de) = &mut state.de {
                match RecurringExpenses::new(year) {
                    Ok(rec) => match rec.apply_to_month(de) {
                        Ok(added) if added.is_empty() => {
                            state.status =
                                "All recurring expenses already present this month".to_string()
                        }
                        Ok(added) => {
                            state.status = format!(
                                "Applied {} recurring expense(s): {}",
                                added.len(),
                                added.join(", ")
                            )
                        }
                        Err(err) => state.status = format!("Apply failed: {err}"),
                    },
                    Err(err) => state.status = format!("Apply failed: {err}"),
                }
            } else {
                state.status = "No data loaded".to_string();
            }
            reload(state, ctx);
        }
        Message::DeleteRecurring(idx) => {
            if let Some(rec) = &mut state.rec {
                match rec.remove(idx) {
                    Ok(()) => state.status = "Recurring expense removed".to_string(),
                    Err(err) => state.status = format!("Remove failed: {err}"),
                }
            }
            reload(state, ctx);
        }

        // -- mappings --
        Message::MapNameChanged(v) => state.mapping_form.name = v,
        Message::MapPrimaryChanged(v) => state.mapping_form.primary = v,
        Message::MapSecondaryChanged(v) => state.mapping_form.secondary = v,
        Message::SubmitMapping => {
            let f = &state.mapping_form;
            if f.name.trim().is_empty() || f.primary.trim().is_empty() {
                state.status = "Name and primary category are required".to_string();
            } else {
                match add_mapping(&f.name, &f.primary, &f.secondary, false) {
                    Ok(()) => {
                        state.status = format!("Mapping added for \"{}\"", f.name);
                        state.mapping_form = MappingForm::default();
                    }
                    Err(err) => state.status = format!("{err}"),
                }
                reload(state, ctx);
            }
        }
        Message::DeleteMapping(name) => {
            match remove_mapping(&name) {
                Ok(_) => state.status = "Mapping removed".to_string(),
                Err(err) => state.status = format!("Remove failed: {err}"),
            }
            reload(state, ctx);
        }
    }
    iced::Task::none()
}

/// Mutate the open expense form, if any.
fn set_form(state: &mut State, f: impl FnOnce(&mut ExpenseForm)) {
    if let Some(form) = &mut state.expense_form {
        f(form);
    }
}

/// Submit the add/edit expense form: validate, eval the amount, resolve
/// categories, and call the appropriate `de` method.
fn submit_expense_form(state: &mut State, ctx: Ctx) {
    let Some(form) = state.expense_form.clone() else {
        return;
    };
    if form.name.trim().is_empty() {
        state.status = "Expense name is required".to_string();
        return;
    }
    if form.amount.trim().is_empty() {
        state.status = "Amount is required".to_string();
        return;
    }
    let day: u32 = match form.day.trim().parse() {
        Ok(d) => d,
        Err(_) => {
            state.status = "Day must be a number".to_string();
            return;
        }
    };
    let amount = match eval(&form.amount) {
        Ok(v) => v,
        Err(err) => {
            state.status = format!("Invalid amount: {err}");
            return;
        }
    };

    let pri_set = state.primary_suggestions.iter().cloned().collect();
    let sec_set = state.secondary_suggestions.iter().cloned().collect();
    let primary = if form.primary.trim().is_empty() {
        None
    } else {
        Some(resolve_category(&form.primary, &pri_set))
    };
    let secondary = if form.secondary.trim().is_empty() {
        None
    } else {
        Some(resolve_category(&form.secondary, &sec_set))
    };
    let currency = if form.currency.trim().is_empty() {
        "E"
    } else {
        form.currency.as_str()
    };

    let editing = state.editing;
    let result = {
        let Some(de) = &mut state.de else {
            state.status = "No data loaded".to_string();
            return;
        };
        match editing {
            Some(idx) => de.edit_row(
                idx,
                Some(form.name.as_str()),
                Some(day),
                Some(amount),
                Some(currency),
                primary.as_deref(),
                secondary.as_deref(),
            ),
            None => de.add_row(
                form.name.as_str(),
                day,
                amount,
                primary.as_deref(),
                currency,
                secondary.as_deref(),
            ),
        }
    };

    match result {
        Ok(()) => {
            state.status = if editing.is_some() {
                "Expense updated".to_string()
            } else {
                "Expense added".to_string()
            };
            state.expense_form = None;
            state.editing = None;
            reload(state, ctx);
        }
        Err(err) => state.status = format!("{err}"),
    }
}

/// Submit the recurring add form.
fn submit_recurring_form(state: &mut State, ctx: Ctx) {
    let form = state.recurring_form.clone();
    if form.name.trim().is_empty() || form.amount.trim().is_empty() {
        state.status = "Name and amount are required".to_string();
        return;
    }
    let day: i64 = match form.day.trim().parse() {
        Ok(d) => d,
        Err(_) => {
            state.status = "Day must be a number".to_string();
            return;
        }
    };
    let amount = match eval(&form.amount) {
        Ok(v) => v,
        Err(err) => {
            state.status = format!("Invalid amount: {err}");
            return;
        }
    };
    let pri_set = state.primary_suggestions.iter().cloned().collect();
    let sec_set = state.secondary_suggestions.iter().cloned().collect();
    let primary = if form.primary.trim().is_empty() {
        String::new()
    } else {
        resolve_category(&form.primary, &pri_set)
    };
    let secondary = if form.secondary.trim().is_empty() {
        String::new()
    } else {
        resolve_category(&form.secondary, &sec_set)
    };
    let currency = if form.currency.trim().is_empty() {
        "E"
    } else {
        form.currency.as_str()
    };

    if let Some(rec) = &mut state.rec {
        match rec.add(&form.name, day, amount, currency, &primary, &secondary) {
            Ok(()) => {
                state.status = format!("Recurring \"{}\" added", form.name);
                state.recurring_form = RecurringForm::default();
            }
            Err(err) => state.status = format!("{err}"),
        }
    }
    reload(state, ctx);
}

/// Toggle membership of `value` in `list` (selection multiselect helper).
fn toggle<T: PartialEq>(list: &mut Vec<T>, value: T) {
    if let Some(pos) = list.iter().position(|x| *x == value) {
        list.remove(pos);
    } else {
        list.push(value);
    }
}

// ======================================================================
// view
// ======================================================================

/// Render the tab body: sub-tab selector + active panel + status line.
pub fn view<'a>(state: &'a State, ctx: Ctx) -> Element<'a, AppMessage> {
    let mut selector = row![].spacing(8);
    for sub in SubTab::ALL {
        let style: fn(&Theme, button::Status) -> button::Style = if sub == state.active_sub {
            subtab_active_style
        } else {
            subtab_inactive_style
        };
        selector = selector.push(
            button(text(sub.label()))
                .style(style)
                .on_press(wrap(Message::SubTabSelected(sub))),
        );
    }
    let selector = container(selector).center_x(Length::Fill).width(Length::Fill);

    let body = match state.active_sub {
        SubTab::Detailed => view_detailed(state, ctx),
        SubTab::Summary => view_summary(state, ctx),
        SubTab::Recurring => view_recurring(state, ctx),
        SubTab::Mappings => view_mappings(state),
    };

    let mut col = column![selector].spacing(12);
    if !state.status.is_empty() {
        col = col.push(text(state.status.clone()).size(14));
    }
    col = col.push(scrollable(body).height(Length::Fill));
    col.into()
}

/// Wrap a tab message into the shell message space.
fn wrap(m: Message) -> AppMessage {
    AppMessage::Expenses(m)
}

// -- Detailed --------------------------------------------------------------

fn view_detailed<'a>(state: &'a State, _ctx: Ctx) -> Element<'a, AppMessage> {
    let filters = row![
        text_input("Search name", &state.filter_name)
            .on_input(|v| wrap(Message::FilterNameChanged(v)))
            .width(Length::Fixed(180.0)),
        text_input("Filter category", &state.filter_category)
            .on_input(|v| wrap(Message::FilterCategoryChanged(v)))
            .width(Length::Fixed(180.0)),
        text_input("Min amount", &state.filter_amount_min)
            .on_input(|v| wrap(Message::FilterAmountMinChanged(v)))
            .width(Length::Fixed(120.0)),
        text_input("Max amount", &state.filter_amount_max)
            .on_input(|v| wrap(Message::FilterAmountMaxChanged(v)))
            .width(Length::Fixed(120.0)),
        button(text("Add Expense")).on_press(wrap(Message::OpenAddForm)),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    // Column widths for the table.
    const W_NAME: f32 = 200.0;
    const W_DATE: f32 = 60.0;
    const W_AMT: f32 = 100.0;
    const W_CUR: f32 = 50.0;
    const W_REF: f32 = 100.0;
    const W_PRI: f32 = 150.0;
    const W_SEC: f32 = 150.0;

    let header = row![
        header_cell("Name", W_NAME),
        header_cell("Date", W_DATE),
        header_cell("Amount", W_AMT),
        header_cell("Cur", W_CUR),
        header_cell("Ref Amount", W_REF),
        header_cell("Primary", W_PRI),
        header_cell("Secondary", W_SEC),
        header_cell("", 120.0),
    ]
    .spacing(6);

    let mut table = column![header].spacing(4);
    if state.rows.is_empty() {
        table = table.push(text("No expenses.").size(14));
    }
    for r in &state.rows {
        let idx = r.index;
        let line = row![
            label_cell(&r.name, W_NAME, false),
            label_cell(&r.day.to_string(), W_DATE, true),
            label_cell(&fmt_2dp(r.amount), W_AMT, true),
            label_cell(&r.currency, W_CUR, false),
            label_cell(&fmt_2dp(r.ref_amount), W_REF, true),
            label_cell(&r.primary, W_PRI, false),
            label_cell(&r.secondary, W_SEC, false),
            row![
                button(text("edit").size(12))
                    .style(button::secondary)
                    .on_press(wrap(Message::OpenEditForm(idx))),
                button(text("del").size(12))
                    .style(button::danger)
                    .on_press(wrap(Message::RequestDelete(idx))),
            ]
            .spacing(4),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        table = table.push(line);
    }

    let mut col = column![filters].spacing(12);

    // Inline add/edit form panel.
    if let Some(form) = &state.expense_form {
        col = col.push(view_expense_form(form, state.editing.is_some()));
    }
    // Delete confirmation panel.
    if let Some((_, label)) = &state.pending_delete {
        col = col.push(
            container(
                row![
                    text(format!("Delete {label}?")),
                    button(text("Delete"))
                        .style(button::danger)
                        .on_press(wrap(Message::ConfirmDelete)),
                    button(text("Cancel"))
                        .style(button::secondary)
                        .on_press(wrap(Message::CancelDelete)),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .padding(8),
        );
    }

    col = col.push(table);
    col.into()
}

fn view_expense_form<'a>(form: &'a ExpenseForm, editing: bool) -> Element<'a, AppMessage> {
    let title = if editing {
        "Edit Expense"
    } else {
        "Add Expense"
    };
    let fields = column![
        labeled_input("Name", &form.name, |v| wrap(Message::FormNameChanged(v))),
        labeled_input("Day of Month", &form.day, |v| wrap(
            Message::FormDayChanged(v)
        )),
        labeled_input("Amount", &form.amount, |v| wrap(
            Message::FormAmountChanged(v)
        )),
        labeled_input("Currency", &form.currency, |v| wrap(
            Message::FormCurrencyChanged(v)
        )),
        labeled_input("Primary", &form.primary, |v| wrap(
            Message::FormPrimaryChanged(v)
        )),
        labeled_input("Secondary", &form.secondary, |v| wrap(
            Message::FormSecondaryChanged(v)
        )),
        row![
            button(text("Auto-map by name"))
                .style(button::secondary)
                .on_press(wrap(Message::FormAutoMap)),
            button(text("Save")).on_press(wrap(Message::SubmitForm)),
            button(text("Cancel"))
                .style(button::secondary)
                .on_press(wrap(Message::CloseForm)),
        ]
        .spacing(8),
    ]
    .spacing(6);

    container(column![section_title(title), fields].spacing(8))
        .padding(12)
        .width(Length::Fixed(420.0))
        .style(container::rounded_box)
        .into()
}

// -- Summary --------------------------------------------------------------

fn view_summary<'a>(state: &'a State, ctx: Ctx) -> Element<'a, AppMessage> {
    // Kind toggle.
    let kind_toggle = row![
        kind_button("Primary", Kind::Primary, state.kind),
        kind_button("Secondary", Kind::Secondary, state.kind),
        button(text("Regenerate summaries"))
            .style(button::secondary)
            .on_press(wrap(Message::RegenerateSummaries)),
    ]
    .spacing(8);

    let mut col = column![kind_toggle].spacing(16);

    // Monthly section.
    col = col.push(section_title(&format!(
        "Monthly — {} {}",
        MONTH_NAMES[(ctx.month.clamp(1, 12) - 1) as usize],
        ctx.year
    )));
    if state.monthly_table.rows.is_empty() {
        col = col.push(text("No data for this month.").size(14));
    } else {
        col = col.push(render_wide_table(&state.monthly_table));
    }
    col = col.push(match &state.monthly_pie {
        Some(p) => charts::pie(p, charts::DEFAULT_CHART_HEIGHT),
        None => text("No data").size(14).into(),
    });

    // Cumulative section.
    if let Some(table) = &state.cumulative_table {
        col = col.push(section_title(&format!("Cumulative — {}", ctx.year)));
        col = col.push(scrollable(render_wide_table(table)).direction(
            scrollable::Direction::Horizontal(scrollable::Scrollbar::new()),
        ));
        col = col.push(match &state.cumulative_pie {
            Some(p) => charts::pie(p, charts::DEFAULT_CHART_HEIGHT),
            None => text("No data").size(14).into(),
        });

        // Comparison bar chart (multi-select of up to 3 months).
        col = col.push(section_title("Monthly comparison (max 3 months)"));
        let mut month_chips = row![].spacing(6);
        for m in &state.available_months {
            let selected = state.selected_months.contains(m);
            let style = if selected {
                button::primary
            } else {
                button::secondary
            };
            month_chips = month_chips.push(
                button(text(MONTH_NAMES[(m.clamp(&1, &12) - 1) as usize]).size(12))
                    .style(style)
                    .on_press(wrap(Message::ToggleMonth(*m))),
            );
        }
        col = col.push(month_chips);
        col = col.push(match &state.comparison_bar {
            Some(b) => charts::grouped_bar(b, charts::DEFAULT_CHART_HEIGHT),
            None => text("No data").size(14).into(),
        });

        // Category line chart (multi-select of up to 3 categories).
        col = col.push(section_title("Category trend (max 3 categories)"));
        let mut cat_chips = row![].spacing(6);
        for c in &state.available_categories {
            let selected = state.selected_categories.contains(c);
            let style = if selected {
                button::primary
            } else {
                button::secondary
            };
            cat_chips = cat_chips.push(
                button(text(c.clone()).size(12))
                    .style(style)
                    .on_press(wrap(Message::ToggleCategory(c.clone()))),
            );
        }
        col = col.push(cat_chips.wrap());
        col = col.push(match &state.category_line {
            Some(l) => charts::line(l, charts::DEFAULT_CHART_HEIGHT),
            None => text("No data").size(14).into(),
        });
    } else {
        col = col.push(text("No cumulative summary for this year yet.").size(14));
    }

    col.into()
}

fn kind_button<'a>(label: &'a str, this: Kind, active: Kind) -> Element<'a, AppMessage> {
    let style = if this == active {
        button::primary
    } else {
        button::secondary
    };
    button(text(label))
        .style(style)
        .on_press(wrap(Message::KindSelected(this)))
        .into()
}

// -- Recurring -------------------------------------------------------------

fn view_recurring<'a>(state: &'a State, ctx: Ctx) -> Element<'a, AppMessage> {
    let f = &state.recurring_form;
    let form = column![
        section_title("Add recurring expense"),
        labeled_input("Name", &f.name, |v| wrap(Message::RecNameChanged(v))),
        labeled_input("Day of Month", &f.day, |v| wrap(Message::RecDayChanged(v))),
        labeled_input("Amount", &f.amount, |v| wrap(Message::RecAmountChanged(v))),
        labeled_input("Currency", &f.currency, |v| wrap(
            Message::RecCurrencyChanged(v)
        )),
        labeled_input("Primary", &f.primary, |v| wrap(Message::RecPrimaryChanged(
            v
        ))),
        labeled_input("Secondary", &f.secondary, |v| wrap(
            Message::RecSecondaryChanged(v)
        )),
        row![
            button(text("Auto-map by name"))
                .style(button::secondary)
                .on_press(wrap(Message::RecAutoMap)),
            button(text("Add Recurring")).on_press(wrap(Message::SubmitRecurring)),
        ]
        .spacing(8),
    ]
    .spacing(6);

    let apply = button(text(format!(
        "Apply to {} {}",
        MONTH_NAMES[(ctx.month.clamp(1, 12) - 1) as usize],
        ctx.year
    )))
    .on_press(wrap(Message::ApplyRecurring));

    const W_NAME: f32 = 180.0;
    const W_DAY: f32 = 50.0;
    const W_AMT: f32 = 100.0;
    const W_CUR: f32 = 50.0;
    const W_CAT: f32 = 150.0;

    let header = row![
        header_cell("Name", W_NAME),
        header_cell("Day", W_DAY),
        header_cell("Amount", W_AMT),
        header_cell("Cur", W_CUR),
        header_cell("Primary", W_CAT),
        header_cell("Secondary", W_CAT),
        header_cell("", 70.0),
    ]
    .spacing(6);

    let mut table = column![header].spacing(4);
    if state.recurring_rows.is_empty() {
        table = table.push(text("No recurring expenses defined yet.").size(14));
    }
    for r in &state.recurring_rows {
        let idx = r.index;
        table = table.push(
            row![
                label_cell(&r.name, W_NAME, false),
                label_cell(&r.day.to_string(), W_DAY, true),
                label_cell(&fmt_2dp(r.amount), W_AMT, true),
                label_cell(&r.currency, W_CUR, false),
                label_cell(&r.primary, W_CAT, false),
                label_cell(&r.secondary, W_CAT, false),
                button(text("del").size(12))
                    .style(button::danger)
                    .on_press(wrap(Message::DeleteRecurring(idx))),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );
    }

    column![
        container(form).padding(12).style(container::rounded_box),
        apply,
        table,
    ]
    .spacing(16)
    .into()
}

// -- Mappings --------------------------------------------------------------

fn view_mappings(state: &State) -> Element<'_, AppMessage> {
    let f = &state.mapping_form;
    let form = row![
        labeled_input("Expense Name", &f.name, |v| wrap(Message::MapNameChanged(
            v
        ))),
        labeled_input("Primary", &f.primary, |v| wrap(Message::MapPrimaryChanged(
            v
        ))),
        labeled_input("Secondary", &f.secondary, |v| wrap(
            Message::MapSecondaryChanged(v)
        )),
        button(text("Add Mapping")).on_press(wrap(Message::SubmitMapping)),
    ]
    .spacing(8)
    .align_y(iced::Alignment::End);

    const W_NAME: f32 = 220.0;
    const W_CAT: f32 = 180.0;

    let header = row![
        header_cell("Expense Name", W_NAME),
        header_cell("Primary", W_CAT),
        header_cell("Secondary", W_CAT),
        header_cell("", 70.0),
    ]
    .spacing(6);

    let mut table = column![header].spacing(4);
    if state.mapping_rows.is_empty() {
        table = table.push(text("No mappings configured yet.").size(14));
    }
    for m in &state.mapping_rows {
        let name = m.name.clone();
        table = table.push(
            row![
                label_cell(&m.name, W_NAME, false),
                label_cell(&m.primary, W_CAT, false),
                label_cell(&m.secondary, W_CAT, false),
                button(text("del").size(12))
                    .style(button::danger)
                    .on_press(wrap(Message::DeleteMapping(name))),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );
    }

    column![form, table].spacing(16).into()
}

// ======================================================================
// Small view helpers
// ======================================================================

/// A labeled single-line text input (label above the field), fixed width.
fn labeled_input<'a, F>(label: &'a str, value: &str, on_input: F) -> Element<'a, AppMessage>
where
    F: Fn(String) -> AppMessage + 'a,
{
    column![
        text(label).size(12),
        text_input("", value)
            .on_input(on_input)
            .width(Length::Fixed(180.0)),
    ]
    .spacing(2)
    .into()
}

/// Render a [`WideTable`] as a column of rows of [`label_cell`]s.
fn render_wide_table(table: &WideTable) -> Element<'_, AppMessage> {
    let cell_w = |i: usize| if i == 0 { 200.0 } else { 110.0 };

    let mut header = row![].spacing(6);
    for (i, h) in table.headers.iter().enumerate() {
        header = header.push(header_cell(h, cell_w(i)));
    }
    let mut col = column![header].spacing(4);
    for r in &table.rows {
        let mut line = row![].spacing(6);
        for (i, cell) in r.iter().enumerate() {
            let right = table.align_right.get(i).copied().unwrap_or(false);
            line = line.push(label_cell(cell, cell_w(i), right));
        }
        col = col.push(line);
    }
    col.into()
}

// ======================================================================
// polars column readers (small, defensive helpers)
// ======================================================================

use polars::prelude::DataFrame;

fn read_parquet(path: &std::path::Path) -> polars::prelude::PolarsResult<DataFrame> {
    use polars::prelude::*;
    let file = std::fs::File::open(path).map_err(|e| PolarsError::IO {
        error: std::sync::Arc::new(e),
        msg: None,
    })?;
    ParquetReader::new(file).finish()
}

/// Read a string column as a `Vec<String>` (nulls → empty string).
fn column_strings(df: &DataFrame, name: &str) -> Vec<String> {
    match df.column(name).and_then(|c| c.str().cloned()) {
        Ok(s) => s.iter().map(|o| o.unwrap_or("").to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Read an f64 column (nulls → 0.0). Casts integer columns if needed.
fn column_f64(df: &DataFrame, name: &str) -> Vec<f64> {
    use polars::prelude::*;
    let Ok(col) = df.column(name) else {
        return Vec::new();
    };
    let casted = col.cast(&DataType::Float64);
    match casted.as_ref().unwrap_or(col).f64() {
        Ok(s) => s.iter().map(|o| o.unwrap_or(0.0)).collect(),
        Err(_) => Vec::new(),
    }
}

/// Read an i64 column (nulls → 0). Casts if needed.
fn column_i64(df: &DataFrame, name: &str) -> Vec<i64> {
    use polars::prelude::*;
    let Ok(col) = df.column(name) else {
        return Vec::new();
    };
    let casted = col.cast(&DataType::Int64);
    match casted.as_ref().unwrap_or(col).i64() {
        Ok(s) => s.iter().map(|o| o.unwrap_or(0)).collect(),
        Err(_) => Vec::new(),
    }
}

/// Read a Date column and return the day-of-month for each row (nulls → 0).
fn column_dates_day(df: &DataFrame, name: &str) -> Vec<i32> {
    use chrono::Datelike;
    use polars::prelude::*;
    let Ok(col) = df.column(name) else {
        return Vec::new();
    };
    // Dates are stored as days since epoch (i32). Convert each to a NaiveDate.
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    match col.cast(&DataType::Int32) {
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

// ======================================================================
// misc helpers
// ======================================================================

fn some_if_nonempty(s: &str) -> Option<&str> {
    if s.trim().is_empty() { None } else { Some(s) }
}

fn parse_opt_f64(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        // Allow expressions in filter fields too; fall back to plain parse.
        eval(t).ok().or_else(|| t.parse().ok())
    }
}

/// Custom button style for active subtab (yellow background, black text).
fn subtab_active_style(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::primary(theme, status);
    style.background = Some(iced::Background::Color(iced::Color::from_rgb(1.0, 0.85, 0.0)));
    style.text_color = iced::Color::BLACK;
    style
}

/// Custom button style for inactive subtab (blue background, white text).
fn subtab_inactive_style(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::secondary(theme, status);
    style.background = Some(iced::Background::Color(iced::Color::from_rgb(0.2, 0.5, 0.9)));
    style.text_color = iced::Color::WHITE;
    style
}

// Silence the unused-import lint for re-exported helpers used only in some
// builds (kept for the documented contract).
#[allow(unused_imports)]
use config as _config;
#[allow(unused_imports)]
use widgets as _widgets;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_builds_from_default_state() {
        let state = State::default();
        let ctx = Ctx {
            year: 2026,
            month: 6,
        };
        let _ = view(&state, ctx);
    }

    #[test]
    fn col_label_converts_year_month_to_name() {
        assert_eq!(col_label("2026-03"), "March");
        assert_eq!(col_label("total_expense"), "Total Expense");
        assert_eq!(col_label("primary_category"), "Primary Category");
    }

    #[test]
    fn toggle_adds_and_removes() {
        let mut v = vec![1u32, 2];
        toggle(&mut v, 3);
        assert_eq!(v, vec![1, 2, 3]);
        toggle(&mut v, 2);
        assert_eq!(v, vec![1, 3]);
    }
}
