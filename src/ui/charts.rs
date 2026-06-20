//! Native `iced::widget::canvas`-based chart widgets.
//!
//! This module renders the typed plot view-models produced by
//! [`finguard_rs::plots`] (`PieChart`, `BarChart`, `LineChart`,
//! `NetworthEvolution`) onto an [`iced`] [`Canvas`]. The widgets are
//! **non-interactive** (they never emit messages), so the public constructors
//! are generic over the message type `M` and return a ready-to-embed
//! [`iced::Element`] whose width fills the available space and whose height is
//! the caller-provided pixel value.
//!
//! ## Integration contract for tab agents
//!
//! - Tabs hold the *typed plot structs* (e.g. `Option<PieChart>`) in their state
//!   and unwrap the `Option` themselves: show a "No data" label when `None`, and
//!   call the chart constructor with the concrete data when `Some`.
//! - Example:
//!   ```ignore
//!   match &state.income_pie {
//!       Some(pie) => charts::pie(pie, charts::DEFAULT_CHART_HEIGHT),
//!       None => text("No data").into(),
//!   }
//!   ```
//! - The constructors are generic: `charts::pie::<crate::ui::Message>(&data, h)`
//!   (the `M` is usually inferred from the surrounding `column!`/`row!`).
//! - Degenerate data (no slices, all-zero values, a single point) is handled
//!   gracefully: the chart simply draws a centered "No data" / empty plot rather
//!   than panicking.
//!
//! ## `canvas::Program` trait shape targeted (iced 0.14)
//!
//! ```ignore
//! trait Program<Message, Theme = iced::Theme, Renderer = iced::Renderer> {
//!     type State: Default + 'static;
//!     fn draw(
//!         &self,
//!         state: &Self::State,
//!         renderer: &Renderer,
//!         theme: &Theme,
//!         bounds: iced::Rectangle,
//!         cursor: iced::mouse::Cursor,
//!     ) -> Vec<canvas::Geometry<Renderer>>;
//!     // `update` and `mouse_interaction` have defaults; we don't override them.
//! }
//! ```
//!
//! Each chart is a `Program` struct owning a **clone** of the data it needs, so
//! the resulting `Canvas`/`Element` has no borrow of the caller's state and the
//! lifetimes stay trivial. `State` is `()` (no interactivity).

use iced::alignment::{Horizontal, Vertical};
use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke, Text};
use iced::{Color, Element, Length, Point, Rectangle, Size, mouse};

use finguard_rs::plots::{BarChart, LineChart, NetworthEvolution, PieChart};

/// Default chart height in pixels (the Python originals used 320–420px).
pub const DEFAULT_CHART_HEIGHT: f32 = 360.0;

// ======================================================================
// Palette / theme constants
// ======================================================================

/// Categorical palette matching the original ECharts theme. Cycled as needed.
const PALETTE: [(u8, u8, u8); 8] = [
    (0x54, 0x70, 0xc6),
    (0x91, 0xcc, 0x75),
    (0xfa, 0xc8, 0x58),
    (0x73, 0xc0, 0xde),
    (0xee, 0x66, 0x66),
    (0x3b, 0xa2, 0x72),
    (0xfc, 0x84, 0x52),
    (0x9a, 0x60, 0xb4),
];

/// Near-white foreground for axes/text on the dark theme.
const FG: Color = Color::from_rgb(0.90, 0.90, 0.92);
/// Dimmer foreground for secondary text / tick labels.
const FG_DIM: Color = Color::from_rgb(0.66, 0.66, 0.70);
/// Subtle gridline color.
const GRID: Color = Color::from_rgb(0.30, 0.30, 0.34);

/// Outer padding inside the canvas, in pixels.
const PADDING: f32 = 12.0;
/// Default label / tick font size.
const LABEL_SIZE: f32 = 12.0;
/// Legend swatch size (square side), in pixels.
const SWATCH: f32 = 12.0;
/// Height reserved per legend row.
const LEGEND_ROW_H: f32 = 18.0;

/// Returns the palette color at `i`, cycling.
fn palette(i: usize) -> Color {
    let (r, g, b) = PALETTE[i % PALETTE.len()];
    Color::from_rgb8(r, g, b)
}

// ======================================================================
// Small drawing helpers
// ======================================================================

/// Draws left-aligned text at `(x, y)` (y is the top of the line).
fn text_left(frame: &mut Frame, content: String, x: f32, y: f32, size: f32, color: Color) {
    frame.fill_text(Text {
        content,
        position: Point::new(x, y),
        color,
        size: size.into(),
        align_x: Horizontal::Left.into(),
        align_y: Vertical::Top,
        ..Text::default()
    });
}

/// Draws text horizontally centered on `x`, with its vertical center at `y`.
fn text_center(frame: &mut Frame, content: String, x: f32, y: f32, size: f32, color: Color) {
    frame.fill_text(Text {
        content,
        position: Point::new(x, y),
        color,
        size: size.into(),
        align_x: Horizontal::Center.into(),
        align_y: Vertical::Center,
        ..Text::default()
    });
}

/// Draws right-aligned text whose vertical center is at `y`.
fn text_right(frame: &mut Frame, content: String, x: f32, y: f32, size: f32, color: Color) {
    frame.fill_text(Text {
        content,
        position: Point::new(x, y),
        color,
        size: size.into(),
        align_x: Horizontal::Right.into(),
        align_y: Vertical::Center,
        ..Text::default()
    });
}

/// Draws a centered "No data" placeholder filling `bounds`.
fn draw_no_data(frame: &mut Frame, bounds: Rectangle) {
    text_center(
        frame,
        "No data".to_string(),
        bounds.width / 2.0,
        bounds.height / 2.0,
        LABEL_SIZE + 2.0,
        FG_DIM,
    );
}

/// Truncates `label` to at most `max` chars, appending an ellipsis when cut.
fn truncate_label(label: &str, max: usize) -> String {
    if label.chars().count() <= max {
        label.to_string()
    } else if max <= 1 {
        label.chars().take(max).collect()
    } else {
        let kept: String = label.chars().take(max - 1).collect();
        format!("{kept}…")
    }
}

/// Formats a numeric tick/value compactly: no decimals for large magnitudes,
/// "k" suffix past 10 000, two decimals only for small fractional values.
fn fmt_value(v: f64) -> String {
    let a = v.abs();
    if a >= 10_000.0 {
        format!("{:.0}k", v / 1000.0)
    } else if a >= 1.0 || a == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

// ======================================================================
// Axis / scaling math
// ======================================================================

/// A computed linear axis: the (possibly negative) data-driven bounds plus a set
/// of evenly spaced "nice" tick values.
struct NiceAxis {
    min: f64,
    max: f64,
    ticks: Vec<f64>,
}

/// Rounds `x` to a "nice" number (1, 2, 5 × 10ⁿ). When `round` is true it rounds
/// to the nearest nice number; otherwise it rounds up (used for the step).
fn nice_num(x: f64, round: bool) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    let exp = x.log10().floor();
    let frac = x / 10f64.powf(exp);
    let nice = if round {
        if frac < 1.5 {
            1.0
        } else if frac < 3.0 {
            2.0
        } else if frac < 7.0 {
            5.0
        } else {
            10.0
        }
    } else if frac <= 1.0 {
        1.0
    } else if frac <= 2.0 {
        2.0
    } else if frac <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * 10f64.powf(exp)
}

/// Computes ~`target` nice ticks spanning `[data_min, data_max]`, always
/// including 0 in the range so a zero baseline is visible. Handles equal/zero
/// ranges and negative values.
fn nice_axis(data_min: f64, data_max: f64, target: usize) -> NiceAxis {
    // Always include zero so bars/areas have a visible baseline.
    let mut lo = data_min.min(0.0);
    let mut hi = data_max.max(0.0);

    if !(lo.is_finite() && hi.is_finite()) {
        lo = 0.0;
        hi = 1.0;
    }
    if (hi - lo).abs() < f64::EPSILON {
        // Degenerate (all values equal / all zero): give a unit window.
        hi = lo + 1.0;
    }

    let target = target.max(2);
    let range = nice_num(hi - lo, false);
    let step = nice_num(range / (target as f64 - 1.0), true).max(f64::MIN_POSITIVE);
    let nice_lo = (lo / step).floor() * step;
    let nice_hi = (hi / step).ceil() * step;

    let mut ticks = Vec::new();
    let mut t = nice_lo;
    // Guard the loop count in case of pathological steps.
    let mut guard = 0;
    while t <= nice_hi + step * 0.5 && guard < 1000 {
        // Snap tiny float noise to a clean zero.
        let val = if t.abs() < step * 1e-6 { 0.0 } else { t };
        ticks.push(val);
        t += step;
        guard += 1;
    }

    NiceAxis {
        min: nice_lo,
        max: nice_hi,
        ticks,
    }
}

/// Maps a data value to a y pixel coordinate within `[top, bottom]` (pixels grow
/// downward, so the axis max is at `top`).
fn value_to_y(value: f64, axis: &NiceAxis, top: f32, bottom: f32) -> f32 {
    let span = (axis.max - axis.min).max(f64::MIN_POSITIVE);
    let frac = (value - axis.min) / span;
    bottom - (frac as f32) * (bottom - top)
}

/// Generates `segments + 1` points approximating a circular arc from
/// `start_angle` to `end_angle` (radians) around `center`.
fn arc_points(
    center: Point,
    radius: f32,
    start_angle: f32,
    end_angle: f32,
    segments: usize,
) -> Vec<Point> {
    let segments = segments.max(1);
    (0..=segments)
        .map(|i| {
            let t = i as f32 / segments as f32;
            let a = start_angle + (end_angle - start_angle) * t;
            Point::new(center.x + radius * a.cos(), center.y + radius * a.sin())
        })
        .collect()
}

// ======================================================================
// Legend
// ======================================================================

/// Draws a vertical legend (swatch + label per entry) with its top-left at
/// `(x, y)`. Returns the width consumed is implicit; callers reserve space.
fn draw_legend(frame: &mut Frame, entries: &[(Color, String)], x: f32, y: f32) {
    for (i, (color, label)) in entries.iter().enumerate() {
        let row_y = y + i as f32 * LEGEND_ROW_H;
        frame.fill_rectangle(
            Point::new(x, row_y + (LEGEND_ROW_H - SWATCH) / 2.0),
            Size::new(SWATCH, SWATCH),
            *color,
        );
        text_left(
            frame,
            truncate_label(label, 22),
            x + SWATCH + 6.0,
            row_y + (LEGEND_ROW_H - LABEL_SIZE) / 2.0,
            LABEL_SIZE,
            FG,
        );
    }
}

// ======================================================================
// Pie chart
// ======================================================================

/// A pie chart canvas program owning its data.
#[derive(Debug)]
struct PieProgram {
    data: PieChart,
}

impl<M> canvas::Program<M> for PieProgram {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());

        // Keep only positive slices (non-positive are meaningless for a pie).
        let slices: Vec<&_> = self.data.slices.iter().filter(|s| s.value > 0.0).collect();
        let total: f64 = slices.iter().map(|s| s.value).sum();

        if slices.is_empty() || total <= 0.0 {
            draw_no_data(&mut frame, bounds);
            return vec![frame.into_geometry()];
        }

        // Reserve the right ~40% for the legend, draw the pie on the left half.
        let legend_w = (bounds.width * 0.42).clamp(120.0, 260.0);
        let plot_w = bounds.width - legend_w - PADDING;
        let plot_h = bounds.height - 2.0 * PADDING;
        let center = Point::new(PADDING + plot_w / 2.0, PADDING + plot_h / 2.0);
        let radius = (plot_w.min(plot_h) / 2.0 - 4.0).max(8.0);

        // Draw wedges starting at the top (-90°), going clockwise.
        let mut angle = -std::f32::consts::FRAC_PI_2;
        let mut legend = Vec::with_capacity(slices.len());
        for (i, slice) in slices.iter().enumerate() {
            let sweep = (slice.value / total) as f32 * std::f32::consts::TAU;
            let end = angle + sweep;
            let color = palette(i);

            // Build the wedge: center -> arc start -> arc -> close.
            let pts = arc_points(center, radius, angle, end, 48);
            let wedge = Path::new(|p| {
                p.move_to(center);
                for pt in &pts {
                    p.line_to(*pt);
                }
                p.close();
            });
            frame.fill(&wedge, color);

            let pct = slice.value / total * 100.0;
            legend.push((
                color,
                format!("{}  {} ({:.0}%)", slice.name, fmt_value(slice.value), pct),
            ));
            angle = end;
        }

        // Legend, vertically centered against the pie.
        let legend_h = legend.len() as f32 * LEGEND_ROW_H;
        let legend_x = PADDING + plot_w + PADDING;
        let legend_y = (bounds.height - legend_h) / 2.0;
        draw_legend(&mut frame, &legend, legend_x, legend_y.max(PADDING));

        vec![frame.into_geometry()]
    }
}

/// Builds a pie-chart [`Element`] from `data`, filling width and using `height`
/// pixels of height. Non-positive slices are skipped; empty data shows
/// "No data".
pub fn pie<'a, M: 'a>(data: &PieChart, height: f32) -> Element<'a, M> {
    Canvas::new(PieProgram { data: data.clone() })
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .into()
}

// ======================================================================
// Shared Cartesian-plot frame (axes + legend layout)
// ======================================================================

/// Pixel rectangle of the inner plotting region, plus the legend origin.
struct PlotArea {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    legend_x: f32,
    legend_y: f32,
}

/// Computes the plot rectangle, reserving margins for the y tick labels (left),
/// the x labels (bottom), the legend (right) and a small top margin.
fn plot_area(bounds: Rectangle, legend_entries: usize) -> PlotArea {
    let legend_w = if legend_entries == 0 {
        0.0
    } else {
        (bounds.width * 0.24).clamp(110.0, 200.0)
    };
    let left = PADDING + 48.0; // room for y tick labels
    let right = bounds.width - PADDING - legend_w;
    let top = PADDING + 6.0;
    let bottom = bounds.height - PADDING - 26.0; // room for x labels
    let legend_h = legend_entries as f32 * LEGEND_ROW_H;
    PlotArea {
        left,
        right: right.max(left + 10.0),
        top,
        bottom: bottom.max(top + 10.0),
        legend_x: bounds.width - legend_w + 4.0,
        legend_y: ((bounds.height - legend_h) / 2.0).max(PADDING),
    }
}

/// Draws horizontal gridlines + y tick labels and the axis frame for `axis`.
fn draw_y_axis(frame: &mut Frame, axis: &NiceAxis, area: &PlotArea) {
    for &tick in &axis.ticks {
        let y = value_to_y(tick, axis, area.top, area.bottom);
        // Gridline (emphasize the zero line).
        let is_zero = tick.abs() < f64::EPSILON;
        let line = Path::line(Point::new(area.left, y), Point::new(area.right, y));
        frame.stroke(
            &line,
            Stroke::default()
                .with_width(if is_zero { 1.4 } else { 1.0 })
                .with_color(if is_zero { FG_DIM } else { GRID }),
        );
        text_right(
            frame,
            fmt_value(tick),
            area.left - 6.0,
            y,
            LABEL_SIZE - 1.0,
            FG_DIM,
        );
    }
}

// ======================================================================
// Grouped bar chart
// ======================================================================

/// A grouped-bar chart canvas program owning its data.
#[derive(Debug)]
struct BarProgram {
    data: BarChart,
}

impl<M> canvas::Program<M> for BarProgram {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());

        let n_cats = self.data.categories.len();
        let n_series = self.data.series.len();
        if n_cats == 0 || n_series == 0 {
            draw_no_data(&mut frame, bounds);
            return vec![frame.into_geometry()];
        }

        // Data range across every series value (supports negatives).
        let (mut dmin, mut dmax) = (f64::INFINITY, f64::NEG_INFINITY);
        for s in &self.data.series {
            for &v in &s.values {
                dmin = dmin.min(v);
                dmax = dmax.max(v);
            }
        }
        if !dmin.is_finite() {
            dmin = 0.0;
            dmax = 0.0;
        }
        let axis = nice_axis(dmin, dmax, 5);

        let legend: Vec<(Color, String)> = self
            .data
            .series
            .iter()
            .enumerate()
            .map(|(i, s)| (palette(i), s.name.clone()))
            .collect();
        let area = plot_area(bounds, legend.len());
        draw_y_axis(&mut frame, &axis, &area);

        let zero_y = value_to_y(0.0, &axis, area.top, area.bottom);
        let plot_w = area.right - area.left;
        let group_w = plot_w / n_cats as f32;
        let group_pad = group_w * 0.16;
        let inner_w = (group_w - 2.0 * group_pad).max(2.0);
        let bar_w = (inner_w / n_series as f32).max(1.0);

        for (ci, cat) in self.data.categories.iter().enumerate() {
            let gx = area.left + ci as f32 * group_w + group_pad;
            for (si, series) in self.data.series.iter().enumerate() {
                let value = series.values.get(ci).copied().unwrap_or(0.0);
                let y = value_to_y(value, &axis, area.top, area.bottom);
                let bx = gx + si as f32 * bar_w;
                let (top, h) = if y <= zero_y {
                    (y, zero_y - y) // positive bar grows upward
                } else {
                    (zero_y, y - zero_y) // negative bar grows downward
                };
                if h > 0.0 {
                    frame.fill_rectangle(
                        Point::new(bx, top),
                        Size::new((bar_w - 1.0).max(1.0), h),
                        palette(si),
                    );
                }
            }
            // Category label centered under the group; abbreviate to avoid
            // collisions when groups are narrow.
            let max_chars = ((group_w / 7.0) as usize).max(2);
            text_center(
                &mut frame,
                truncate_label(cat, max_chars),
                gx + inner_w / 2.0,
                area.bottom + 13.0,
                LABEL_SIZE - 1.0,
                FG_DIM,
            );
        }

        draw_legend(&mut frame, &legend, area.legend_x, area.legend_y);
        vec![frame.into_geometry()]
    }
}

/// Builds a grouped-bar-chart [`Element`]. One adjacent bar per series within
/// each category group; supports negative values (zero baseline). Empty data
/// shows "No data".
pub fn grouped_bar<'a, M: 'a>(data: &BarChart, height: f32) -> Element<'a, M> {
    Canvas::new(BarProgram { data: data.clone() })
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .into()
}

// ======================================================================
// Line chart
// ======================================================================

/// A line chart canvas program owning its data.
#[derive(Debug)]
struct LineProgram {
    data: LineChart,
}

/// Computes the x pixel for the `i`-th of `n` points across `[left, right]`.
/// A single point is centered.
fn x_for_index(i: usize, n: usize, left: f32, right: f32) -> f32 {
    if n <= 1 {
        (left + right) / 2.0
    } else {
        left + (i as f32 / (n - 1) as f32) * (right - left)
    }
}

impl<M> canvas::Program<M> for LineProgram {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());

        let n = self.data.x_labels.len();
        let has_values = self.data.series.iter().any(|s| !s.values.is_empty());
        if n == 0 || self.data.series.is_empty() || !has_values {
            draw_no_data(&mut frame, bounds);
            return vec![frame.into_geometry()];
        }

        let (mut dmin, mut dmax) = (f64::INFINITY, f64::NEG_INFINITY);
        for s in &self.data.series {
            for &v in &s.values {
                dmin = dmin.min(v);
                dmax = dmax.max(v);
            }
        }
        if !dmin.is_finite() {
            dmin = 0.0;
            dmax = 0.0;
        }
        let axis = nice_axis(dmin, dmax, 5);

        let legend: Vec<(Color, String)> = self
            .data
            .series
            .iter()
            .enumerate()
            .map(|(i, s)| (palette(i), s.name.clone()))
            .collect();
        let area = plot_area(bounds, legend.len());
        draw_y_axis(&mut frame, &axis, &area);

        draw_x_labels(&mut frame, &self.data.x_labels, &area);

        for (si, series) in self.data.series.iter().enumerate() {
            let color = palette(si);
            let pts: Vec<Point> = series
                .values
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    Point::new(
                        x_for_index(i, n, area.left, area.right),
                        value_to_y(v, &axis, area.top, area.bottom),
                    )
                })
                .collect();

            if pts.len() >= 2 {
                let line = Path::new(|p| {
                    p.move_to(pts[0]);
                    for pt in &pts[1..] {
                        p.line_to(*pt);
                    }
                });
                frame.stroke(&line, Stroke::default().with_width(2.0).with_color(color));
            }
            // Markers (also covers the single-point case).
            for pt in &pts {
                frame.fill(&Path::circle(*pt, 3.0), color);
            }
        }

        draw_legend(&mut frame, &legend, area.legend_x, area.legend_y);
        vec![frame.into_geometry()]
    }
}

/// Draws x-axis labels under the plot, thinning them out if they would collide.
fn draw_x_labels(frame: &mut Frame, labels: &[String], area: &PlotArea) {
    let n = labels.len();
    if n == 0 {
        return;
    }
    // Estimate how many labels fit (~ each label needs ~36px) and skip the rest.
    let plot_w = area.right - area.left;
    let max_labels = (plot_w / 36.0).floor().max(1.0) as usize;
    let stride = n.div_ceil(max_labels).max(1);
    for (i, label) in labels.iter().enumerate() {
        if i % stride != 0 {
            continue;
        }
        let x = x_for_index(i, n, area.left, area.right);
        text_center(
            frame,
            truncate_label(label, 6),
            x,
            area.bottom + 13.0,
            LABEL_SIZE - 1.0,
            FG_DIM,
        );
    }
}

/// Builds a line-chart [`Element`]: one polyline + circular markers per series,
/// y gridlines/ticks, x labels and a legend. Empty data shows "No data".
pub fn line<'a, M: 'a>(data: &LineChart, height: f32) -> Element<'a, M> {
    Canvas::new(LineProgram { data: data.clone() })
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .into()
}

// ======================================================================
// Net-worth evolution (stacked areas + bold total line)
// ======================================================================

/// A net-worth-evolution chart program owning its data.
#[derive(Debug)]
struct NetworthProgram {
    data: NetworthEvolution,
}

impl<M> canvas::Program<M> for NetworthProgram {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());

        let n = self.data.months.len();
        if n == 0 || self.data.components.is_empty() {
            draw_no_data(&mut frame, bounds);
            return vec![frame.into_geometry()];
        }

        // Stacked positive contributions per month: stacks[i] is the cumulative
        // top of the stack after adding each (clamped non-negative) component.
        let n_comp = self.data.components.len();
        let mut tops: Vec<Vec<f64>> = vec![vec![0.0; n]; n_comp];
        let mut running = vec![0.0_f64; n];
        for (ci, comp) in self.data.components.iter().enumerate() {
            for (i, top) in tops[ci].iter_mut().enumerate() {
                let v = comp.values.get(i).copied().unwrap_or(0.0).max(0.0);
                running[i] += v;
                *top = running[i];
            }
        }

        // Y range: from min(0, min net worth) to max(stack top, max net worth).
        let stack_max = running.iter().copied().fold(0.0_f64, f64::max);
        let (mut nw_min, mut nw_max) = (f64::INFINITY, f64::NEG_INFINITY);
        for &v in &self.data.net_worth {
            nw_min = nw_min.min(v);
            nw_max = nw_max.max(v);
        }
        if !nw_min.is_finite() {
            nw_min = 0.0;
            nw_max = 0.0;
        }
        let axis = nice_axis(nw_min.min(0.0), stack_max.max(nw_max), 5);

        let mut legend: Vec<(Color, String)> = self
            .data
            .components
            .iter()
            .enumerate()
            .map(|(i, c)| (palette(i), c.name.clone()))
            .collect();
        legend.push((FG, "Net Worth".to_string()));

        let area = plot_area(bounds, legend.len());
        draw_y_axis(&mut frame, &axis, &area);
        draw_x_labels(&mut frame, &self.data.months, &area);

        let xs: Vec<f32> = (0..n)
            .map(|i| x_for_index(i, n, area.left, area.right))
            .collect();

        // Draw stacked areas top-down so each band sits over the previous one.
        // Band `ci` is between baseline `tops[ci-1]` (or 0) and `tops[ci]`.
        for ci in 0..n_comp {
            let color = palette(ci);
            let upper: &[f64] = &tops[ci];
            // Build a filled polygon: upper edge left→right, lower edge right→left.
            if n == 1 {
                // Degenerate single month: draw a thin vertical bar for the band.
                let lower = if ci == 0 { 0.0 } else { tops[ci - 1][0] };
                let y_up = value_to_y(upper[0], &axis, area.top, area.bottom);
                let y_lo = value_to_y(lower, &axis, area.top, area.bottom);
                let w = 10.0_f32;
                let h = (y_lo - y_up).abs();
                if h > 0.0 {
                    frame.fill_rectangle(
                        Point::new(xs[0] - w / 2.0, y_up.min(y_lo)),
                        Size::new(w, h),
                        with_alpha(color, 0.75),
                    );
                }
                continue;
            }
            let area_path = Path::new(|p| {
                // Upper edge, left to right.
                p.move_to(Point::new(
                    xs[0],
                    value_to_y(upper[0], &axis, area.top, area.bottom),
                ));
                for i in 1..n {
                    p.line_to(Point::new(
                        xs[i],
                        value_to_y(upper[i], &axis, area.top, area.bottom),
                    ));
                }
                // Lower edge, right to left.
                for i in (0..n).rev() {
                    let lower = if ci == 0 { 0.0 } else { tops[ci - 1][i] };
                    p.line_to(Point::new(
                        xs[i],
                        value_to_y(lower, &axis, area.top, area.bottom),
                    ));
                }
                p.close();
            });
            frame.fill(&area_path, with_alpha(color, 0.75));
        }

        // Bold near-white net-worth line on top, with markers.
        let nw_pts: Vec<Point> = self
            .data
            .net_worth
            .iter()
            .enumerate()
            .map(|(i, &v)| Point::new(xs[i], value_to_y(v, &axis, area.top, area.bottom)))
            .collect();
        if nw_pts.len() >= 2 {
            let line = Path::new(|p| {
                p.move_to(nw_pts[0]);
                for pt in &nw_pts[1..] {
                    p.line_to(*pt);
                }
            });
            frame.stroke(&line, Stroke::default().with_width(2.6).with_color(FG));
        }
        for pt in &nw_pts {
            frame.fill(&Path::circle(*pt, 3.2), FG);
        }

        draw_legend(&mut frame, &legend, area.legend_x, area.legend_y);
        vec![frame.into_geometry()]
    }
}

/// Returns `color` with its alpha replaced by `a`.
fn with_alpha(color: Color, a: f32) -> Color {
    Color { a, ..color }
}

/// Builds a net-worth-evolution [`Element`]: stacked filled component areas with
/// a bold net-worth line on top. Empty data shows "No data".
pub fn networth_evolution<'a, M: 'a>(data: &NetworthEvolution, height: f32) -> Element<'a, M> {
    Canvas::new(NetworthProgram { data: data.clone() })
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use finguard_rs::plots::{BarChart, LineChart, NetworthEvolution, PieChart, PieSlice, Series};

    // The chart constructors return an `Element`; the draw pass needs a real
    // renderer and won't run here. Constructing the Element exercises the
    // public API (cloning data, building the Canvas) without panicking.
    type M = ();

    fn sample_pie() -> PieChart {
        PieChart {
            slices: vec![
                PieSlice {
                    name: "Salary".into(),
                    value: 1200.0,
                },
                PieSlice {
                    name: "Dividends".into(),
                    value: 300.0,
                },
                PieSlice {
                    name: "Other".into(),
                    value: 50.0,
                },
            ],
        }
    }

    fn sample_bar() -> BarChart {
        BarChart {
            categories: vec!["Jan".into(), "Feb".into(), "Mar".into()],
            series: vec![
                Series {
                    name: "Income".into(),
                    values: vec![1000.0, 1100.0, 900.0],
                },
                Series {
                    name: "Spending".into(),
                    values: vec![700.0, 800.0, 650.0],
                },
                // Negative values must be supported (e.g. cashflow "Saving").
                Series {
                    name: "Saving".into(),
                    values: vec![300.0, -50.0, 250.0],
                },
            ],
        }
    }

    fn sample_line() -> LineChart {
        LineChart {
            x_labels: vec!["Jan".into(), "Feb".into(), "Mar".into(), "Apr".into()],
            series: vec![
                Series {
                    name: "Food".into(),
                    values: vec![200.0, 250.0, 180.0, 220.0],
                },
                Series {
                    name: "Rent".into(),
                    values: vec![800.0, 800.0, 800.0, 800.0],
                },
            ],
        }
    }

    fn sample_networth() -> NetworthEvolution {
        NetworthEvolution {
            months: vec!["Jan".into(), "Feb".into(), "Mar".into()],
            components: vec![
                Series {
                    name: "Stocks/ETF".into(),
                    values: vec![5000.0, 5200.0, 5400.0],
                },
                Series {
                    name: "Liquidity".into(),
                    values: vec![2000.0, 2100.0, 1900.0],
                },
                Series {
                    name: "Credits/Debts".into(),
                    values: vec![-500.0, -400.0, -300.0],
                },
            ],
            net_worth: vec![6500.0, 6900.0, 7000.0],
        }
    }

    #[test]
    fn build_charts_from_sample_data() {
        let _: Element<'_, M> = pie(&sample_pie(), DEFAULT_CHART_HEIGHT);
        let _: Element<'_, M> = grouped_bar(&sample_bar(), DEFAULT_CHART_HEIGHT);
        let _: Element<'_, M> = line(&sample_line(), DEFAULT_CHART_HEIGHT);
        let _: Element<'_, M> = networth_evolution(&sample_networth(), DEFAULT_CHART_HEIGHT);
    }

    #[test]
    fn build_charts_from_empty_data() {
        let _: Element<'_, M> = pie(&PieChart { slices: vec![] }, 200.0);
        let _: Element<'_, M> = grouped_bar(
            &BarChart {
                categories: vec![],
                series: vec![],
            },
            200.0,
        );
        let _: Element<'_, M> = line(
            &LineChart {
                x_labels: vec![],
                series: vec![],
            },
            200.0,
        );
        let _: Element<'_, M> = networth_evolution(
            &NetworthEvolution {
                months: vec![],
                components: vec![],
                net_worth: vec![],
            },
            200.0,
        );
    }

    #[test]
    fn build_charts_from_degenerate_data() {
        // All-zero pie (no positive slices).
        let _: Element<'_, M> = pie(
            &PieChart {
                slices: vec![PieSlice {
                    name: "Z".into(),
                    value: 0.0,
                }],
            },
            200.0,
        );
        // Single data point per series.
        let _: Element<'_, M> = line(
            &LineChart {
                x_labels: vec!["Jan".into()],
                series: vec![Series {
                    name: "A".into(),
                    values: vec![42.0],
                }],
            },
            200.0,
        );
        // All-zero bars.
        let _: Element<'_, M> = grouped_bar(
            &BarChart {
                categories: vec!["A".into(), "B".into()],
                series: vec![Series {
                    name: "S".into(),
                    values: vec![0.0, 0.0],
                }],
            },
            200.0,
        );
        // Single-month net worth.
        let _: Element<'_, M> = networth_evolution(
            &NetworthEvolution {
                months: vec!["Jan".into()],
                components: vec![Series {
                    name: "C".into(),
                    values: vec![100.0],
                }],
                net_worth: vec![100.0],
            },
            200.0,
        );
    }

    #[test]
    fn nice_axis_includes_zero_and_handles_negatives() {
        let a = nice_axis(-50.0, 300.0, 5);
        assert!(a.min <= 0.0 && a.max >= 300.0);
        assert!(a.ticks.iter().any(|t| t.abs() < 1e-9));
        // Degenerate equal range must not panic and must produce ticks.
        let b = nice_axis(0.0, 0.0, 5);
        assert!(!b.ticks.is_empty());
    }

    #[test]
    fn truncate_label_adds_ellipsis() {
        assert_eq!(truncate_label("short", 10), "short");
        assert_eq!(truncate_label("abcdefgh", 4), "abc…");
    }
}
