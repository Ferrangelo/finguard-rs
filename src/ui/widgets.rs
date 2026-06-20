//! Shared UI widgets and helpers for the four tabs (iced 0.14).
//!
//! This module holds small, reusable, **UI-only** building blocks: number
//! formatting helpers that mirror the Python UI's display conventions, month
//! name constants, and generic widget builders for the editable month-grids
//! used by the Cashflow, Investments (holdings/prices), Liquidity and
//! Credits/Debts tabs.
//!
//! All widget builders are generic over the caller's message type `M`, so each
//! tab can reuse them with its own `Message` enum and lift the result into the
//! shell's [`crate::ui::Message`] as usual. No `polars` types appear in any
//! signature — helpers consume already-materialized rows/strings produced
//! during a tab's `reload`.
//!
//! # Editable month-grid state model (recommended pattern)
//!
//! Tabs that render editable month-grids should all follow the same shape so
//! they behave identically and so [`numeric_cell`] can be wired up the same way
//! everywhere:
//!
//! 1. **Buffer in-progress text.** The tab keeps a buffer of the text the user
//!    is currently typing, keyed by cell identity — e.g.
//!    `HashMap<(usize /*row*/, usize /*month 0..11*/), String>`, or keyed by
//!    `(name: String, month: usize)` when rows are identified by name. This
//!    buffer is the *source of truth for the widget's displayed text*, so that
//!    a half-typed expression like `12.50 +` is not clobbered on every redraw.
//!
//! 2. **Seed the buffer in `reload()`.** When the period or shared data
//!    changes, rebuild the buffer from the core data, formatting each cell with
//!    [`fmt_cell`] (so zeros render as `""`, matching the Python inline cells).
//!
//! 3. **`on_input` updates the buffer only.** The `on_input` closure passed to
//!    [`numeric_cell`] should produce a tab message that writes the new string
//!    into the buffer for that cell. Nothing is evaluated yet.
//!
//! 4. **`on_submit` evaluates and commits.** The `on_submit` message (emitted
//!    on Enter/blur) should, in the tab's `update`, run
//!    [`finguard_rs::expr::eval`] on the buffered text for that cell:
//!    - on `Ok(v)`: call the relevant core setter — e.g.
//!      [`Cashflow::set_income`], `InvestmentHoldings::set_quantity` /
//!      `set_price`, `Liquidity::set_value`, `CreditsDebts::set_value` — then
//!      call the tab's `reload` to refresh derived rows/totals and re-seed the
//!      buffer (so the committed value is re-formatted via [`fmt_cell`]).
//!    - on `Err(_)`: **silently ignore** the edit (matching the Python cells,
//!      which swallow eval errors on blur). Optionally re-seed that one cell
//!      from the stored value so the bad text disappears.
//!
//! [`Cashflow::set_income`]: finguard_rs::df_operations
//! [`finguard_rs::expr::eval`]: finguard_rs::expr::eval

use iced::alignment::Horizontal;
use iced::font::Weight;
use iced::widget::{container, text, text_input};
use iced::{Element, Font, Length};

/// Three-letter English month abbreviations, indexed by `month - 1`
/// (so `MONTH_ABBR[0]` is `"Jan"`). Mirrors Python's `calendar.month_abbr`.
pub const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Full English month names, indexed by `month - 1`
/// (so `MONTH_NAMES[0]` is `"January"`).
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

/// Values whose absolute magnitude is below this are treated as zero (and
/// rendered as an empty string in editable/display cells).
const ZERO_EPS: f64 = 1e-9;

/// Format an editable cell value: empty string when the value is ~0.0,
/// otherwise the value with two decimal places.
///
/// Mirrors the Python inline cells which show `""` for zero and `str(v)`
/// (here normalized to 2dp) otherwise.
pub fn fmt_cell(v: f64) -> String {
    if v.abs() < ZERO_EPS {
        String::new()
    } else {
        format!("{v:.2}")
    }
}

/// Format a value with exactly two decimal places (no zero special-casing).
pub fn fmt_2dp(v: f64) -> String {
    format!("{v:.2}")
}

/// Format a value with two decimals **and** thousands separators, e.g.
/// `1234.5 -> "1,234.50"`, `-1234.5 -> "-1,234.50"`. Returns an empty string
/// when the value is ~0.0.
///
/// Rust's standard library has no thousands grouping, so the integer part is
/// grouped by hand. Negatives are handled by emitting the sign first and
/// grouping the magnitude.
pub fn fmt_thousands(v: f64) -> String {
    if v.abs() < ZERO_EPS {
        return String::new();
    }

    let negative = v < 0.0;
    // Format the magnitude to 2dp first, then group the integer portion.
    let formatted = format!("{:.2}", v.abs());
    let (int_part, frac_part) = match formatted.split_once('.') {
        Some((i, f)) => (i, f),
        None => (formatted.as_str(), "00"),
    };

    let grouped = group_thousands(int_part);

    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&grouped);
    out.push('.');
    out.push_str(frac_part);
    out
}

/// Format a value as a one-decimal percentage, e.g. `12.34 -> "12.3%"`.
/// Mirrors the Python cashflow "Saving %" format (`{:.1f}%`).
pub fn fmt_pct(v: f64) -> String {
    format!("{v:.1}%")
}

/// Format a value as a two-decimal percentage with thousands separators, e.g.
/// `1234.5 -> "1,234.50%"`. Unlike [`fmt_thousands`], this does *not* blank out
/// zero (a 0% change is meaningful), so `0.0 -> "0.00%"`. Used by the
/// net-worth %-change row.
pub fn fmt_pct_thousands(v: f64) -> String {
    let negative = v < 0.0;
    let formatted = format!("{:.2}", v.abs());
    let (int_part, frac_part) = match formatted.split_once('.') {
        Some((i, f)) => (i, f),
        None => (formatted.as_str(), "00"),
    };
    let grouped = group_thousands(int_part);

    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&grouped);
    out.push('.');
    out.push_str(frac_part);
    out.push('%');
    out
}

/// Group an unsigned integer string (digits only) into comma-separated
/// thousands, e.g. `"1234567" -> "1,234,567"`, `"42" -> "42"`.
fn group_thousands(digits: &str) -> String {
    let len = digits.len();
    if len <= 3 {
        return digits.to_string();
    }

    // Capacity: digits + one comma per group boundary.
    let mut out = String::with_capacity(len + (len - 1) / 3);
    // Number of leading digits before the first comma boundary.
    let first = len % 3;
    let bytes = digits.as_bytes();

    let mut idx = 0;
    if first > 0 {
        out.push_str(&digits[..first]);
        idx = first;
    }
    while idx < len {
        if !out.is_empty() {
            out.push(',');
        }
        // Safe: digits are ASCII bytes.
        out.push(bytes[idx] as char);
        out.push(bytes[idx + 1] as char);
        out.push(bytes[idx + 2] as char);
        idx += 3;
    }
    out
}

/// A bold [`Font`] derived from the default UI font (used for headers/titles).
const BOLD: Font = Font {
    weight: Weight::Bold,
    ..Font::DEFAULT
};

/// A bold, larger section title, e.g. `"Holdings — 2026"` or `"Cashflow — 2026"`.
pub fn section_title<'a, M: 'a>(t: &str) -> Element<'a, M> {
    text(t.to_string()).size(20).font(BOLD).into()
}

/// A fixed-width, centered, bold column-header cell (e.g. a month abbreviation).
pub fn header_cell<'a, M: 'a>(label: &str, width: f32) -> Element<'a, M> {
    container(
        text(label.to_string())
            .font(BOLD)
            .align_x(Horizontal::Center)
            .width(Length::Fill),
    )
    .width(Length::Fixed(width))
    .into()
}

/// A fixed-width text cell, left- or right-aligned.
pub fn label_cell<'a, M: 'a>(text_value: &str, width: f32, align_right: bool) -> Element<'a, M> {
    let align = if align_right {
        Horizontal::Right
    } else {
        Horizontal::Left
    };
    container(
        text(text_value.to_string())
            .align_x(align)
            .width(Length::Fill),
    )
    .width(Length::Fixed(width))
    .into()
}

/// A right-aligned, fixed-width inline numeric text input for an editable grid
/// cell.
///
/// `display` is the text currently shown (the caller's buffered cell text — see
/// the module-level state model). `on_input` is called on every keystroke with
/// the new string; `on_submit` is emitted on Enter/blur. Styling is minimal
/// (borderless, transparent background) to resemble the Python inline cells.
///
/// The caller is responsible for evaluating the text via
/// [`finguard_rs::expr::eval`] in its `update` when `on_submit` fires; this
/// helper is purely the widget.
///
/// [`finguard_rs::expr::eval`]: finguard_rs::expr::eval
pub fn numeric_cell<'a, M, FIn>(
    display: &str,
    width: f32,
    on_input: FIn,
    on_submit: M,
) -> Element<'a, M>
where
    M: Clone + 'a,
    FIn: Fn(String) -> M + 'a,
{
    text_input("", display)
        .on_input(on_input)
        .on_submit(on_submit)
        .align_x(Horizontal::Right)
        .padding(2)
        .width(Length::Fixed(width))
        .style(|theme, status| {
            // Start from the default style, then make it borderless with a
            // transparent background to mimic the Python inline cell.
            let mut style = text_input::default(theme, status);
            style.background = iced::Background::Color(iced::Color::TRANSPARENT);
            style.border.width = 0.0;
            style
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_constants() {
        assert_eq!(MONTH_ABBR[0], "Jan");
        assert_eq!(MONTH_ABBR[11], "Dec");
        assert_eq!(MONTH_NAMES[0], "January");
        assert_eq!(MONTH_NAMES[11], "December");
    }

    #[test]
    fn fmt_cell_blanks_zero_else_2dp() {
        assert_eq!(fmt_cell(0.0), "");
        assert_eq!(fmt_cell(-0.0), "");
        assert_eq!(fmt_cell(1e-12), "");
        assert_eq!(fmt_cell(12.5), "12.50");
        assert_eq!(fmt_cell(-3.0), "-3.00");
        assert_eq!(fmt_cell(999.0), "999.00");
    }

    #[test]
    fn fmt_2dp_always_two_decimals() {
        assert_eq!(fmt_2dp(0.0), "0.00");
        assert_eq!(fmt_2dp(1.5), "1.50");
        assert_eq!(fmt_2dp(-1.5), "-1.50");
    }

    #[test]
    fn fmt_thousands_groups_and_handles_negatives() {
        assert_eq!(fmt_thousands(0.0), "");
        assert_eq!(fmt_thousands(999.0), "999.00");
        assert_eq!(fmt_thousands(1234.5), "1,234.50");
        assert_eq!(fmt_thousands(-1234.5), "-1,234.50");
        assert_eq!(fmt_thousands(1234.56), "1,234.56");
        assert_eq!(fmt_thousands(1000000.0), "1,000,000.00");
        assert_eq!(fmt_thousands(-1000000.0), "-1,000,000.00");
        assert_eq!(fmt_thousands(12.0), "12.00");
        assert_eq!(fmt_thousands(100.0), "100.00");
        assert_eq!(fmt_thousands(12345.678), "12,345.68");
    }

    #[test]
    fn fmt_pct_one_decimal() {
        assert_eq!(fmt_pct(0.0), "0.0%");
        assert_eq!(fmt_pct(12.34), "12.3%");
        assert_eq!(fmt_pct(-5.0), "-5.0%");
    }

    #[test]
    fn fmt_pct_thousands_keeps_zero() {
        assert_eq!(fmt_pct_thousands(0.0), "0.00%");
        assert_eq!(fmt_pct_thousands(1234.5), "1,234.50%");
        assert_eq!(fmt_pct_thousands(-1234.5), "-1,234.50%");
    }

    #[test]
    fn group_thousands_boundaries() {
        assert_eq!(group_thousands("0"), "0");
        assert_eq!(group_thousands("42"), "42");
        assert_eq!(group_thousands("999"), "999");
        assert_eq!(group_thousands("1000"), "1,000");
        assert_eq!(group_thousands("12345"), "12,345");
        assert_eq!(group_thousands("1234567"), "1,234,567");
    }

    #[test]
    fn widget_builders_construct() {
        // Use a concrete unit message type to exercise the generics.
        #[derive(Debug, Clone)]
        struct Msg(String);

        // Exercise the `on_input` closure so the payload field is read.
        assert_eq!(Msg("hi".to_string()).0, "hi");

        let _: Element<'_, Msg> = section_title("Holdings — 2026");
        let _: Element<'_, Msg> = header_cell("Jan", 72.0);
        let _: Element<'_, Msg> = label_cell("Total", 160.0, false);
        let _: Element<'_, Msg> = label_cell("1,234.50", 80.0, true);
        let _: Element<'_, Msg> = numeric_cell("12.50", 80.0, Msg, Msg("x".into()));
    }
}
