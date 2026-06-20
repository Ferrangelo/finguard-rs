//! finguard_rs binary entry point.
//!
//! This is the native desktop GUI for finguard, built with [`iced`] 0.14. The
//! application logic lives in the `finguard_rs` library crate; this binary owns
//! the UI layer under the [`ui`] module tree.
//!
//! iced 0.14 uses the functional builder API: [`iced::application`] takes a
//! `boot`, `update`, and `view` and returns a builder on which `.title()`,
//! `.theme()` and `.run()` are chained. `update` returns an [`iced::Task`] and
//! `view` returns an [`iced::Element`].

mod ui;

fn main() -> iced::Result {
    iced::application(ui::App::new, ui::update, ui::view)
        .title(|_state: &ui::App| ui::TITLE.to_owned())
        .theme(ui::theme)
        .run()
}
