//! Cashflow tab.
//!
//! Renders the yearly cashflow overview as a wide editable grid (rows = the
//! four income categories plus four derived rows; columns = the twelve months)
//! followed by two charts. Mirrors the Python `ui_cashflow.py`.
//!
//! Follows the shared tab contract documented in [`crate::ui`] and the editable
//! month-grid state model documented in [`crate::ui::widgets`]: per-cell text
//! buffers are the source of truth for the editable income cells, seeded in
//! [`reload`] and committed on submit via [`finguard_rs::expr::eval`].

use std::collections::HashMap;

use iced::widget::{column, container, row, text};
use iced::{Color, Element, Length};

use finguard_rs::df_operations::Cashflow;
use finguard_rs::expr;
use finguard_rs::plots::{self, BarChart, PieChart};

use crate::ui::{Ctx, Message as AppMessage};
use crate::ui::{charts, widgets};

/// The four editable income-category rows (exact strings from
/// `df_operations::INCOME_CATEGORIES`).
const INCOME_CATEGORIES: [&str; 4] = [
    "Salary",
    "Interests Bank account",
    "Dividendi e Cedole",
    "Other",
];

/// The four read-only derived rows (exact strings from
/// `df_operations::DERIVED_CATEGORIES`).
const DERIVED_CATEGORIES: [&str; 4] = ["Income", "Spending", "Saving", "Saving %"];

/// Width of the left label column, in pixels.
const LABEL_W: f32 = 180.0;
/// Width of each month column, in pixels.
const CELL_W: f32 = 84.0;

/// State owned by the Cashflow tab.
#[derive(Default)]
pub struct State {
    /// The currently-loaded cashflow model for the active year, or `None` if
    /// loading failed (treated as "no data").
    cashflow: Option<Cashflow>,
    /// In-progress edit text for each editable cell, keyed by
    /// `(income_category_row, month_index_0_based)`. Seeded in [`reload`] and
    /// the source of truth for the income cells' displayed text.
    buffers: HashMap<(usize, usize), String>,
    /// Cached bar-chart view-model for the active year.
    bar: Option<BarChart>,
    /// Cached income pie-chart view-model for the active year.
    pie: Option<PieChart>,
}

/// Messages emitted by the Cashflow tab.
#[derive(Debug, Clone)]
pub enum Message {
    /// A keystroke in an income cell: `(row, month_index, new_text)`. Updates
    /// the buffer only.
    CellInput(usize, usize, String),
    /// Enter/blur on an income cell: `(row, month_index)`. Evaluates the buffer
    /// and commits on success.
    CellSubmit(usize, usize),
}

/// Handle a tab message. Returns [`iced::Task::none`] (all work is synchronous).
pub fn update(state: &mut State, message: Message, ctx: Ctx) -> iced::Task<AppMessage> {
    match message {
        Message::CellInput(rowidx, monthidx, value) => {
            state.buffers.insert((rowidx, monthidx), value);
        }
        Message::CellSubmit(rowidx, monthidx) => {
            let buf = state
                .buffers
                .get(&(rowidx, monthidx))
                .cloned()
                .unwrap_or_default();

            // An empty cell means zero (matches the Python blur handler).
            let parsed = if buf.trim().is_empty() {
                Ok(0.0)
            } else {
                expr::eval(&buf)
            };

            if let Ok(v) = parsed
                && let Some(cf) = state.cashflow.as_mut()
                && let Some(category) = INCOME_CATEGORIES.get(rowidx)
            {
                let month = (monthidx + 1) as u32;
                // `set_income` recomputes derived rows internally; on success we
                // re-seed buffers + refresh derived rows and charts via reload.
                if cf.set_income(month, category, v).is_ok() {
                    reload(state, ctx);
                }
            }
            // On eval/parse error: silently ignore (matches Python).
        }
    }
    iced::Task::none()
}

/// Called when the active period (year) or shared data changes: reload the
/// cashflow model, refresh derived rows, re-seed the edit buffers and re-fetch
/// the charts.
pub fn reload(state: &mut State, ctx: Ctx) {
    match Cashflow::new(ctx.year) {
        Ok(mut cf) => {
            // Ensure derived rows reflect the latest primaries data.
            if let Err(err) = cf.recompute() {
                eprintln!(
                    "finguard: cashflow recompute for {} failed: {err}",
                    ctx.year
                );
            }

            // Seed one buffer per editable (income category, month) cell.
            let mut buffers = HashMap::with_capacity(INCOME_CATEGORIES.len() * 12);
            for (rowidx, category) in INCOME_CATEGORIES.iter().enumerate() {
                for monthidx in 0..12usize {
                    let col = format!("{:02}", monthidx + 1);
                    let v = cf.get_value(category, &col).unwrap_or(0.0);
                    buffers.insert((rowidx, monthidx), widgets::fmt_cell(v));
                }
            }

            state.cashflow = Some(cf);
            state.buffers = buffers;
        }
        Err(err) => {
            eprintln!("finguard: failed to load cashflow for {}: {err}", ctx.year);
            state.cashflow = None;
            state.buffers.clear();
        }
    }

    state.bar = plots::cashflow_bar_chart(ctx.year).unwrap_or(None);
    state.pie = plots::income_pie_chart(ctx.year).unwrap_or(None);
}

/// Reddish color used for the Spending row and negative savings.
const NEGATIVE: Color = Color::from_rgb(0.94, 0.40, 0.40);
/// Greenish color used for non-negative savings.
const POSITIVE: Color = Color::from_rgb(0.40, 0.80, 0.50);

/// Render the tab body.
pub fn view<'a>(state: &'a State, ctx: Ctx) -> Element<'a, AppMessage> {
    let title = widgets::section_title::<AppMessage>(&format!("Cashflow — {}", ctx.year));

    // Header row: empty label column + 12 month abbreviations.
    let mut header = row![widgets::label_cell::<AppMessage>("", LABEL_W, false)].spacing(0);
    for abbr in widgets::MONTH_ABBR {
        header = header.push(widgets::header_cell::<AppMessage>(abbr, CELL_W));
    }

    let mut grid = column![header].spacing(4);

    // Editable income rows.
    for (rowidx, category) in INCOME_CATEGORIES.iter().enumerate() {
        let mut line = row![widgets::label_cell::<AppMessage>(category, LABEL_W, false)].spacing(0);
        for monthidx in 0..12usize {
            let display = state
                .buffers
                .get(&(rowidx, monthidx))
                .map(String::as_str)
                .unwrap_or("");
            let cell = widgets::numeric_cell::<AppMessage, _>(
                display,
                CELL_W,
                move |s| AppMessage::Cashflow(Message::CellInput(rowidx, monthidx, s)),
                AppMessage::Cashflow(Message::CellSubmit(rowidx, monthidx)),
            );
            line = line.push(cell);
        }
        grid = grid.push(line);
    }

    // Visual separator before the derived rows.
    grid = grid.push(
        container(text(""))
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(Color::from_rgb(0.4, 0.4, 0.45))),
                ..container::Style::default()
            })
            .width(Length::Fill)
            .height(Length::Fixed(1.0)),
    );

    // Read-only derived rows.
    for category in DERIVED_CATEGORIES {
        let mut line = row![widgets::label_cell::<AppMessage>(category, LABEL_W, false)].spacing(0);
        for monthidx in 0..12usize {
            let col = format!("{:02}", monthidx + 1);
            let value = state
                .cashflow
                .as_ref()
                .and_then(|cf| cf.get_value(category, &col).ok())
                .unwrap_or(0.0);

            let txt = if category == "Saving %" {
                widgets::fmt_pct(value)
            } else {
                widgets::fmt_2dp(value)
            };

            // Color cues: Spending reddish; Saving/Saving % green when >= 0,
            // red when < 0.
            let color = match category {
                "Spending" => Some(NEGATIVE),
                "Saving" | "Saving %" => Some(if value < 0.0 { NEGATIVE } else { POSITIVE }),
                _ => None,
            };

            let mut cell_text = text(txt).align_x(iced::alignment::Horizontal::Right);
            if let Some(c) = color {
                cell_text = cell_text.color(c);
            }
            line = line.push(container(cell_text.width(Length::Fill)).width(Length::Fixed(CELL_W)));
        }
        grid = grid.push(line);
    }

    // Charts below the grid.
    let bar_chart: Element<'a, AppMessage> = match &state.bar {
        Some(b) => charts::grouped_bar(b, charts::DEFAULT_CHART_HEIGHT),
        None => text("No cashflow data to chart.").into(),
    };
    let pie_chart: Element<'a, AppMessage> = match &state.pie {
        Some(p) => charts::pie(p, charts::DEFAULT_CHART_HEIGHT),
        None => text("No income data to chart.").into(),
    };

    let chart_row = row![
        container(bar_chart).width(Length::FillPortion(1)),
        container(pie_chart).width(Length::FillPortion(1)),
    ]
    .spacing(16);

    column![title, grid, chart_row].spacing(16).into()
}

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
        // Must construct without panicking even with no loaded data.
        let _ = view(&state, ctx);
    }

    #[test]
    fn row_constants_match_library() {
        assert_eq!(INCOME_CATEGORIES.len(), 4);
        assert_eq!(
            DERIVED_CATEGORIES,
            ["Income", "Spending", "Saving", "Saving %"]
        );
    }
}
