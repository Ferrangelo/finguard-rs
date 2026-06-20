//! Categories tab.
//!
//! Manages the primary/secondary categories that appear in the expense-entry
//! dropdowns (mirrors the Python `ui_categories.py`). Two columns side by side
//! (Primary on the left, Secondary on the right), each with:
//!
//! * an **add form** (text input + button) that normalizes the value and
//!   registers it via [`finguard_rs::config::add_known_category`];
//! * a **table** built from the union of the known-config categories and the
//!   categories found in the year-summary parquet files
//!   ([`finguard_rs::df_operations::get_category_totals_across_all_years`]),
//!   each row showing the name, its cumulative reference-currency total, and a
//!   **delete** button that is enabled only when that total is `0.0`.
//!
//! Deletion is guarded again in [`update`]: even though the button is disabled
//! when the total is non-zero, the handler re-checks the total before removing
//! the category from the config and from every summary parquet.

use iced::widget::{button, column, container, row, text, text_input};
use iced::{Element, Length};

use crate::ui::widgets::{fmt_2dp, header_cell, label_cell, section_title};
use crate::ui::{Ctx, Message as AppMessage};

/// A single category row in a column's table.
#[derive(Debug, Clone)]
struct CategoryRow {
    /// Category name (canonical spelling).
    name: String,
    /// Cumulative reference-currency total across all years (`0.0` if unused).
    total: f64,
}

/// Per-kind UI state: the in-progress add-form text, the cached rows, and the
/// last status message to surface to the user.
#[derive(Debug, Default)]
struct KindState {
    /// In-progress text of the "New … Category" input.
    input: String,
    /// Cached rows for the table (rebuilt in [`reload`]).
    rows: Vec<CategoryRow>,
    /// A transient status message (success / error / warning) for this column.
    status: Option<Status>,
}

/// A small status message shown beneath a column's add form.
#[derive(Debug, Clone)]
enum Status {
    /// A success message (e.g. "added" / "removed").
    Success(String),
    /// An error message (e.g. AlreadyExists, guarded delete).
    Error(String),
    /// A warning (e.g. empty input).
    Warning(String),
}

/// State owned by the Categories tab.
#[derive(Debug, Default)]
pub struct State {
    /// Primary-category column state.
    primary: KindState,
    /// Secondary-category column state.
    secondary: KindState,
}

/// Messages emitted by the Categories tab.
#[derive(Debug, Clone)]
pub enum Message {
    /// The add-form input for `kind` changed.
    InputChanged { kind: Kind, value: String },
    /// The "Add" button for `kind` was pressed.
    Add { kind: Kind },
    /// A row's delete button was pressed.
    Delete { kind: Kind, name: String },
}

/// Which of the two columns a message targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Primary categories.
    Primary,
    /// Secondary categories.
    Secondary,
}

impl Kind {
    /// The library-facing `kind` string (`"primary"` / `"secondary"`).
    fn as_str(self) -> &'static str {
        match self {
            Kind::Primary => "primary",
            Kind::Secondary => "secondary",
        }
    }

    /// Human-readable label used in titles and prompts.
    fn label(self) -> &'static str {
        match self {
            Kind::Primary => "Primary",
            Kind::Secondary => "Secondary",
        }
    }
}

/// Magnitude below which a total is treated as exactly zero (so float noise
/// from summing parquet cells does not wrongly block a delete).
const ZERO_EPS: f64 = 1e-9;

/// Borrow the [`KindState`] for `kind`.
fn kind_state(state: &mut State, kind: Kind) -> &mut KindState {
    match kind {
        Kind::Primary => &mut state.primary,
        Kind::Secondary => &mut state.secondary,
    }
}

/// Recompute the rows for a single `kind`: the union of the known-config
/// categories and the keys of the all-years totals map, deduped, sorted
/// case-insensitively, each carrying its total (or `0.0`).
fn build_rows(kind: Kind) -> Vec<CategoryRow> {
    use std::collections::BTreeMap;

    let kind_str = kind.as_str();

    // Known categories from the config file.
    let known: Vec<String> = match finguard_rs::config::get_known_categories() {
        Ok(cats) => match kind {
            Kind::Primary => cats.primary,
            Kind::Secondary => cats.secondary,
        },
        Err(_) => Vec::new(),
    };

    // Totals discovered across the year-summary parquet files.
    let totals = finguard_rs::df_operations::get_category_totals_across_all_years(kind_str)
        .unwrap_or_default();

    // Merge into a name -> total map (config-only names default to 0.0).
    let mut merged: BTreeMap<String, f64> = BTreeMap::new();
    for name in known {
        merged.entry(name).or_insert(0.0);
    }
    for (name, total) in totals.iter() {
        merged.insert(name.clone(), *total);
    }

    let mut rows: Vec<CategoryRow> = merged
        .into_iter()
        // Round to 2dp, matching Python `_build_rows` `round(total, 2)`, so the
        // delete-guard and display both use the rounded value.
        .map(|(name, total)| CategoryRow {
            name,
            total: (total * 100.0).round() / 100.0,
        })
        .collect();
    // Sort alphabetically, case-insensitively (mirrors the Python key).
    rows.sort_by_key(|r| r.name.to_lowercase());
    rows
}

/// Handle a tab message. All work here is synchronous, so we always return
/// [`iced::Task::none`].
pub fn update(state: &mut State, message: Message, ctx: Ctx) -> iced::Task<AppMessage> {
    match message {
        Message::InputChanged { kind, value } => {
            kind_state(state, kind).input = value;
        }
        Message::Add { kind } => {
            let value = kind_state(state, kind).input.trim().to_string();
            if value.is_empty() {
                kind_state(state, kind).status =
                    Some(Status::Warning("Category name is required".to_string()));
                return iced::Task::none();
            }

            let normalized = finguard_rs::df_operations::normalize_category_value(&value);
            match finguard_rs::config::add_known_category(&normalized, kind.as_str()) {
                Ok(()) => {
                    let ks = kind_state(state, kind);
                    ks.input.clear();
                    ks.status = Some(Status::Success(format!(
                        "\"{normalized}\" added to {} categories",
                        kind.as_str()
                    )));
                    reload(state, ctx);
                }
                Err(err) => {
                    kind_state(state, kind).status = Some(Status::Error(err.to_string()));
                }
            }
        }
        Message::Delete { kind, name } => {
            // Re-derive the current total for this name from the cached rows
            // and guard the delete (mirrors the disabled button server-side).
            let current_total = kind_state(state, kind)
                .rows
                .iter()
                .find(|r| r.name == name)
                .map(|r| r.total)
                .unwrap_or(0.0);

            if current_total.abs() >= ZERO_EPS {
                kind_state(state, kind).status = Some(Status::Error(format!(
                    "Cannot delete \"{name}\": it still has {} in existing expenses. \
                     Remove or re-categorise all linked expenses first.",
                    fmt_2dp(current_total)
                )));
                return iced::Task::none();
            }

            // Remove from config (ignore NotFound — it may exist only in
            // parquet summaries), then strip it from every summary parquet.
            match finguard_rs::config::remove_known_category(&name, kind.as_str()) {
                Ok(()) => {}
                Err(finguard_rs::Error::NotFound(_)) => {}
                Err(err) => {
                    kind_state(state, kind).status = Some(Status::Error(err.to_string()));
                    return iced::Task::none();
                }
            }

            match finguard_rs::df_operations::remove_category_from_all_summaries(
                &name,
                kind.as_str(),
            ) {
                Ok(()) => {
                    kind_state(state, kind).status =
                        Some(Status::Success(format!("\"{name}\" removed")));
                    reload(state, ctx);
                }
                Err(err) => {
                    kind_state(state, kind).status = Some(Status::Error(err.to_string()));
                }
            }
        }
    }
    iced::Task::none()
}

/// Called when the active period or shared data changes. Rebuilds the cached
/// primary/secondary rows. The period is irrelevant here (totals scan all
/// years) but the signature is kept for the shared tab contract.
pub fn reload(state: &mut State, _ctx: Ctx) {
    state.primary.rows = build_rows(Kind::Primary);
    state.secondary.rows = build_rows(Kind::Secondary);
}

/// Column widths for the table.
const NAME_W: f32 = 220.0;
const TOTAL_W: f32 = 120.0;

/// Render the tab body: the two columns side by side.
pub fn view<'a>(state: &'a State, _ctx: Ctx) -> Element<'a, AppMessage> {
    row![
        kind_column(&state.primary, Kind::Primary),
        kind_column(&state.secondary, Kind::Secondary),
    ]
    .spacing(40)
    .into()
}

/// Render a single column (primary or secondary).
fn kind_column<'a>(ks: &'a KindState, kind: Kind) -> Element<'a, AppMessage> {
    let title = section_title(&format!("{} Categories", kind.label()));

    // ---- add form -------------------------------------------------------
    let input = text_input(&format!("New {} Category", kind.label()), &ks.input)
        .on_input(move |value| AppMessage::Categories(Message::InputChanged { kind, value }))
        .on_submit(AppMessage::Categories(Message::Add { kind }))
        .width(Length::Fixed(220.0))
        .padding(4);

    let add_btn = button(text("Add"))
        .style(button::primary)
        .on_press(AppMessage::Categories(Message::Add { kind }));

    let add_form = row![input, add_btn]
        .spacing(8)
        .align_y(iced::Alignment::Center);

    // ---- status line ----------------------------------------------------
    let status: Element<'a, AppMessage> = match &ks.status {
        Some(Status::Success(msg)) => text(msg.clone())
            .size(13)
            .color(iced::Color::from_rgb(0.45, 0.8, 0.45))
            .into(),
        Some(Status::Error(msg)) => text(msg.clone())
            .size(13)
            .color(iced::Color::from_rgb(0.9, 0.45, 0.45))
            .into(),
        Some(Status::Warning(msg)) => text(msg.clone())
            .size(13)
            .color(iced::Color::from_rgb(0.9, 0.75, 0.4))
            .into(),
        None => text("").size(13).into(),
    };

    // ---- table ----------------------------------------------------------
    let mut table = column![header_row()].spacing(4);

    if ks.rows.is_empty() {
        table = table.push(
            text("No categories yet.")
                .size(14)
                .color(iced::Color::from_rgb(0.6, 0.6, 0.6)),
        );
    } else {
        for r in &ks.rows {
            table = table.push(category_row(r, kind));
        }
    }

    column![title, add_form, status, table]
        .spacing(12)
        .width(Length::Fixed(NAME_W + TOTAL_W + 100.0))
        .into()
}

/// The table header row (Name | Total (ref €) | actions).
fn header_row<'a>() -> Element<'a, AppMessage> {
    row![
        header_cell("Category", NAME_W),
        header_cell("Total (ref €)", TOTAL_W),
        header_cell("", 100.0),
    ]
    .spacing(8)
    .into()
}

/// A single category row: name, total, and a delete button that is enabled
/// only when the total is zero.
fn category_row<'a>(r: &CategoryRow, kind: Kind) -> Element<'a, AppMessage> {
    let deletable = r.total.abs() < ZERO_EPS;

    let action: Element<'a, AppMessage> = if deletable {
        container(
            button(text("Delete").size(13))
                .style(button::danger)
                .on_press(AppMessage::Categories(Message::Delete {
                    kind,
                    name: r.name.clone(),
                })),
        )
        .width(Length::Fixed(100.0))
        .into()
    } else {
        // Disabled (no `on_press`) plus a note explaining why.
        container(
            row![
                button(text("Delete").size(13)).style(button::secondary),
                text("has existing expenses")
                    .size(11)
                    .color(iced::Color::from_rgb(0.6, 0.6, 0.6)),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fixed(100.0))
        .into()
    };

    row![
        label_cell(&r.name, NAME_W, false),
        label_cell(&fmt_2dp(r.total), TOTAL_W, true),
        action,
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center)
    .into()
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
        let _ = view(&state, ctx);
    }

    #[test]
    fn kind_strings_and_labels() {
        assert_eq!(Kind::Primary.as_str(), "primary");
        assert_eq!(Kind::Secondary.as_str(), "secondary");
        assert_eq!(Kind::Primary.label(), "Primary");
        assert_eq!(Kind::Secondary.label(), "Secondary");
    }

    #[test]
    fn empty_input_add_sets_warning() {
        let mut state = State::default();
        let ctx = Ctx {
            year: 2026,
            month: 6,
        };
        let _ = update(
            &mut state,
            Message::Add {
                kind: Kind::Primary,
            },
            ctx,
        );
        assert!(matches!(state.primary.status, Some(Status::Warning(_))));
    }

    #[test]
    fn delete_guard_blocks_nonzero_total() {
        let mut state = State::default();
        state.primary.rows = vec![CategoryRow {
            name: "Food".to_string(),
            total: 42.0,
        }];
        let ctx = Ctx {
            year: 2026,
            month: 6,
        };
        let _ = update(
            &mut state,
            Message::Delete {
                kind: Kind::Primary,
                name: "Food".to_string(),
            },
            ctx,
        );
        assert!(matches!(state.primary.status, Some(Status::Error(_))));
        // Row must still be present — nothing was deleted.
        assert_eq!(state.primary.rows.len(), 1);
    }
}
