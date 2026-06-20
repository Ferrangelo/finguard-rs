//! Top-level UI module for the finguard desktop app (iced 0.14).
//!
//! # Architecture / integration contract
//!
//! This module owns the application *shell*: the window, the dark theme, the
//! header (title + year/month selectors), the tab bar, and the routing of
//! messages to the four tab modules. The four tabs themselves
//! ([`expenses`], [`cashflow`], [`networth`], [`categories`]) follow a uniform
//! contract so they can be filled in independently by downstream agents:
//!
//! Each tab module provides:
//! - `pub struct State` — the tab's own state (implements [`Default`]).
//! - `pub enum Message` — the tab's own messages (must be `Debug + Clone`).
//! - `pub fn update(state: &mut State, message: Message, ctx: Ctx)
//!   -> iced::Task<crate::ui::Message>` — handle a tab message.
//! - `pub fn view<'a>(state: &'a State, ctx: Ctx)
//!   -> iced::Element<'a, crate::ui::Message>` — render the tab.
//! - `pub fn reload(state: &mut State, ctx: Ctx)` — called whenever the active
//!   period (year/month) or the shared data changes, so the tab can refresh.
//!
//! The shell wraps each tab's `Message` in a top-level [`Message`] variant and
//! lifts each tab's `Element` into the shell `Message` space (tab views already
//! emit `crate::ui::Message`, so no `.map()` is needed at the call site).
//!
//! ## Shared data
//!
//! The currently-loaded [`DetailedExpenses`] for the active period lives on the
//! shell ([`App::detailed_expenses`]). On a year/month change the shell reloads
//! it and then calls `reload` on every tab. Tabs that need other library data
//! (cashflow, investments, …) should load it lazily inside their own `reload`
//! using the [`Ctx`] period; the shell does not own those yet.
//!
//! ## Context
//!
//! [`Ctx`] is a small `Copy` struct carrying the active `year`/`month`. It is
//! passed by value to every tab `view`/`update`/`reload` so tabs always know
//! the active period without reaching into shell state.

pub mod cashflow;
pub mod categories;
pub mod charts;
pub mod expenses;
pub mod networth;
pub mod widgets;

use chrono::{Datelike, Local};
use iced::widget::{button, column, container, pick_list, row, text};
use iced::{Element, Length, Task, Theme};

use finguard_rs::df_operations::DetailedExpenses;
use finguard_rs::paths;

/// Window title.
pub const TITLE: &str = "Finguard";

/// Full month names, indexed by `month - 1` (so `MONTH_NAMES[0]` is January).
pub const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The four top-level tabs of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// Detailed monthly expenses (table + per-month summary).
    #[default]
    Expenses,
    /// Yearly cashflow overview.
    Cashflow,
    /// Net-worth: investments, liquidity, credits/debts.
    NetWorth,
    /// Category management (primary/secondary categories).
    Categories,
}

impl Tab {
    /// All tabs in display order.
    pub const ALL: [Tab; 4] = [Tab::Expenses, Tab::Cashflow, Tab::NetWorth, Tab::Categories];

    /// Human-readable label for the tab button.
    pub fn label(self) -> &'static str {
        match self {
            Tab::Expenses => "Expenses",
            Tab::Cashflow => "Cashflow",
            Tab::NetWorth => "NetWorth",
            Tab::Categories => "Categories",
        }
    }
}

/// The active period (year/month) shared with every tab.
///
/// Cheap to copy; passed by value into each tab's `view`/`update`/`reload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ctx {
    /// Active calendar year.
    pub year: i32,
    /// Active month (1–12).
    pub month: u32,
}

/// A month entry for the month `pick_list`.
///
/// Wraps the month number (1–12) but renders as the full English month name,
/// satisfying the `ToString` bound `pick_list` requires while keeping the
/// numeric value for state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonthChoice(pub u32);

impl std::fmt::Display for MonthChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let idx = (self.0.clamp(1, 12) - 1) as usize;
        f.write_str(MONTH_NAMES[idx])
    }
}

/// Top-level application message.
///
/// Header actions are handled directly by the shell; the four wrapping variants
/// route to the corresponding tab module's `update`.
#[derive(Debug, Clone)]
pub enum Message {
    /// The year selector changed.
    YearSelected(i32),
    /// The month selector changed.
    MonthSelected(u32),
    /// A tab button was pressed.
    TabSelected(Tab),
    /// A message destined for the Expenses tab.
    Expenses(expenses::Message),
    /// A message destined for the Cashflow tab.
    Cashflow(cashflow::Message),
    /// A message destined for the NetWorth tab.
    NetWorth(networth::Message),
    /// A message destined for the Categories tab.
    Categories(categories::Message),
}

/// The whole application state (shell + per-tab state).
pub struct App {
    /// Active calendar year.
    pub year: i32,
    /// Active month (1–12).
    pub month: u32,
    /// Currently selected tab.
    pub active_tab: Tab,
    /// Years offered by the year selector.
    pub years: Vec<i32>,
    /// Shared, currently-loaded detailed expenses for the active period.
    ///
    /// `None` if loading failed (e.g. missing/corrupt data); tabs should treat
    /// this as "no data yet" rather than panicking.
    pub detailed_expenses: Option<DetailedExpenses>,
    /// Per-tab state.
    pub expenses: expenses::State,
    /// Per-tab state.
    pub cashflow: cashflow::State,
    /// Per-tab state.
    pub networth: networth::State,
    /// Per-tab state.
    pub categories: categories::State,
}

impl App {
    /// Build the initial application state (the iced `boot` function).
    ///
    /// Defaults to the current year/month and the Expenses tab, discovers the
    /// available years, and performs the first data load.
    pub fn new() -> Self {
        let today = Local::now().date_naive();
        let year = today.year();
        let month = today.month();

        let mut app = Self {
            year,
            month,
            active_tab: Tab::default(),
            years: discover_years(),
            detailed_expenses: None,
            expenses: expenses::State::default(),
            cashflow: cashflow::State::default(),
            networth: networth::State::default(),
            categories: categories::State::default(),
        };
        app.reload_all();
        app
    }

    /// The active period as a [`Ctx`].
    fn ctx(&self) -> Ctx {
        Ctx {
            year: self.year,
            month: self.month,
        }
    }

    /// Reload the shared [`DetailedExpenses`] for the active period and notify
    /// every tab via its `reload` hook.
    fn reload_all(&mut self) {
        self.detailed_expenses = match DetailedExpenses::new(self.year, self.month) {
            Ok(de) => Some(de),
            Err(err) => {
                eprintln!(
                    "finguard: failed to load expenses for {}-{:02}: {err}",
                    self.year, self.month
                );
                None
            }
        };

        let ctx = self.ctx();
        expenses::reload(&mut self.expenses, ctx);
        cashflow::reload(&mut self.cashflow, ctx);
        networth::reload(&mut self.networth, ctx);
        categories::reload(&mut self.categories, ctx);
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// The iced `theme` callback: a fixed dark theme.
pub fn theme(_state: &App) -> Theme {
    Theme::Dark
}

/// The iced `update` callback.
pub fn update(state: &mut App, message: Message) -> Task<Message> {
    let ctx = state.ctx();
    match message {
        Message::YearSelected(year) => {
            if year != state.year {
                state.year = year;
                state.reload_all();
            }
            Task::none()
        }
        Message::MonthSelected(month) => {
            if month != state.month {
                state.month = month;
                state.reload_all();
            }
            Task::none()
        }
        Message::TabSelected(tab) => {
            state.active_tab = tab;
            // Refresh the newly-selected tab from disk so cross-tab changes
            // (e.g. editing expenses updating Cashflow/Categories) show up.
            match tab {
                Tab::Expenses => expenses::reload(&mut state.expenses, ctx),
                Tab::Cashflow => cashflow::reload(&mut state.cashflow, ctx),
                Tab::NetWorth => networth::reload(&mut state.networth, ctx),
                Tab::Categories => categories::reload(&mut state.categories, ctx),
            }
            Task::none()
        }
        Message::Expenses(msg) => expenses::update(&mut state.expenses, msg, ctx),
        Message::Cashflow(msg) => cashflow::update(&mut state.cashflow, msg, ctx),
        Message::NetWorth(msg) => networth::update(&mut state.networth, msg, ctx),
        Message::Categories(msg) => categories::update(&mut state.categories, msg, ctx),
    }
}

/// The iced `view` callback: header + tab bar + active tab body.
pub fn view(state: &App) -> Element<'_, Message> {
    let ctx = state.ctx();

    let body = match state.active_tab {
        Tab::Expenses => expenses::view(&state.expenses, ctx),
        Tab::Cashflow => cashflow::view(&state.cashflow, ctx),
        Tab::NetWorth => networth::view(&state.networth, ctx),
        Tab::Categories => categories::view(&state.categories, ctx),
    };

    column![header(state), tab_bar(state), body]
        .spacing(12)
        .padding(16)
        .into()
}

/// The header row: title + year and month selectors.
fn header(state: &App) -> Element<'_, Message> {
    let title = text(TITLE).size(28);

    let year_select = pick_list(state.years.clone(), Some(state.year), Message::YearSelected)
        .placeholder("Year")
        .width(Length::Fixed(120.0));

    let months: Vec<MonthChoice> = (1..=12).map(MonthChoice).collect();
    let month_select = pick_list(months, Some(MonthChoice(state.month)), |choice| {
        Message::MonthSelected(choice.0)
    })
    .placeholder("Month")
    .width(Length::Fixed(160.0));

    row![title, year_select, month_select]
        .spacing(16)
        .align_y(iced::Alignment::Center)
        .into()
}

/// The tab bar: one button per [`Tab`], the active one highlighted.
fn tab_bar(state: &App) -> Element<'_, Message> {
    let mut bar = row![].spacing(8);
    for tab in Tab::ALL {
        let style = if tab == state.active_tab {
            button::primary
        } else {
            button::secondary
        };
        bar = bar.push(
            button(text(tab.label()))
                .style(style)
                .on_press(Message::TabSelected(tab)),
        );
    }
    container(bar).width(Length::Fill).into()
}

/// Discover the years that have a data directory, always including the current
/// year. Mirrors the Python `_discover_years()` helper: scans
/// [`paths::get_dbs_root`] for subdirectories named as integers, returns a
/// sorted, de-duplicated list.
pub fn discover_years() -> Vec<i32> {
    use std::collections::BTreeSet;

    let mut years: BTreeSet<i32> = BTreeSet::new();
    years.insert(Local::now().date_naive().year());

    if let Ok(root) = paths::get_dbs_root()
        && let Ok(entries) = std::fs::read_dir(root)
    {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Some(name) = entry.file_name().to_str()
                && let Ok(year) = name.parse::<i32>()
            {
                years.insert(year);
            }
        }
    }

    years.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_choice_displays_full_name() {
        assert_eq!(MonthChoice(1).to_string(), "January");
        assert_eq!(MonthChoice(12).to_string(), "December");
    }

    #[test]
    fn discover_years_includes_current_year() {
        let current = Local::now().date_naive().year();
        assert!(discover_years().contains(&current));
    }

    #[test]
    fn app_boots_and_tabs_render_without_panicking() {
        let mut app = App::new();
        let ctx = app.ctx();

        // Each tab's view must build for the initial state.
        let _ = expenses::view(&app.expenses, ctx);
        let _ = cashflow::view(&app.cashflow, ctx);
        let _ = networth::view(&app.networth, ctx);
        let _ = categories::view(&app.categories, ctx);

        // The shell view must build too.
        let _ = view(&app);

        // Header updates must not panic.
        let _ = update(&mut app, Message::TabSelected(Tab::Cashflow));
        assert_eq!(app.active_tab, Tab::Cashflow);
        let _ = update(&mut app, Message::MonthSelected(6));
        assert_eq!(app.month, 6);
    }
}
