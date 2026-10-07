//! Rendering. `draw` lays the panes out for the terminal size (full,
//! medium, tiny) and draws them from the state in `App`.

use std::sync::atomic::Ordering::Relaxed;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Sparkline, Table, Wrap};

use super::app::{App, Focus, Overlay};
use super::model::{Lat, OpEntry, OpRow, Ring, WINDOW_SECS, fmt_clock, fmt_count, fmt_duration, fmt_rate};
use crate::config::MismatchMode;
use crate::policy::Mismatch;
use crate::stats::{OpKind, fmt_bytes, fmt_ns};
use crate::sys::errno_name;

const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const COLOR_PRI: Color = Color::Green;
const COLOR_SEC: Color = Color::Magenta;
const MISMATCH_ROW: Style = Style::new().fg(Color::LightRed).add_modifier(Modifier::BOLD);

/// All five panes (and the full layout) need at least this much.
const FULL_MIN: (u16, u16) = (120, 38);
const MEDIUM_MIN: (u16, u16) = (70, 20);

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)]).areas(area);
    draw_header(f, app, header);
    draw_footer(f, app, footer);

    if area.width >= FULL_MIN.0 && area.height >= FULL_MIN.1 {
        draw_full(f, app, body);
    } else if area.width >= MEDIUM_MIN.0 && area.height >= MEDIUM_MIN.1 {
        draw_medium(f, app, body);
    } else {
        app.cycle = vec![Focus::OpLog];
        app.focus = Focus::OpLog;
        draw_oplog(f, app, body);
    }

    if !app.pending.is_empty() {
        draw_freeze(f, app, area);
    }
    match app.overlay {
        Overlay::None => {}
        Overlay::Help => draw_help(f, area),
        Overlay::Filter => draw_filter_input(f, app, footer),
        Overlay::ConfirmQuit => {
            let mut lines = vec![Line::from("Quit and unmount?")];
            if !app.pending.is_empty() {
                lines.push(Line::styled(
                    "Frozen operations are released with 'continue'.",
                    Style::new().fg(Color::Yellow),
                ));
            }
            lines.push(Line::default());
            lines.push(hint_line(&[("y", "yes"), ("any other key", "no")]));
            dialog(f, area, 52, "Quit", Color::Cyan, lines);
        }
        Overlay::ConfirmDetach => {
            let lines = vec![
                Line::from("Detach the secondary file system?"),
                Line::styled(
                    "Operations go to the primary only from now on; nothing is compared. This cannot be undone.",
                    Style::new().fg(Color::Yellow),
                ),
                Line::default(),
                hint_line(&[("y", "detach"), ("any other key", "cancel")]),
            ];
            dialog(f, area, 62, "Detach", Color::Yellow, lines);
        }
        Overlay::AllowChoice => draw_allow(f, app, area),
        Overlay::MismatchDetail => {
            if let Some(m) = app.selected_mismatch() {
                let mut lines = mismatch_lines(m);
                lines.push(Line::default());
                lines.push(hint_line(&[("Esc/Enter", "close")]));
                dialog(f, area, 110, "Mismatch details", Color::Red, lines);
            }
        }
    }
}

// ---- layouts ----

fn draw_full(f: &mut Frame, app: &mut App, body: Rect) {
    app.cycle = vec![
        Focus::OpLog,
        Focus::Stats,
        Focus::Inflight,
        Focus::Mismatches,
        Focus::Logs,
    ];
    let top_h = (app.rows.len() as u16 + 3).clamp(14, (body.height * 2 / 5).max(14));
    let bottom_h = if body.height >= 36 { 11 } else { 9 };
    let [top, mid, bottom] = Layout::vertical([
        Constraint::Length(top_h),
        Constraint::Min(8),
        Constraint::Length(bottom_h),
    ])
    .areas(body);
    let [stats, side] = Layout::horizontal([Constraint::Min(60), Constraint::Length(46)]).areas(top);
    let [thr, counters] = Layout::vertical([Constraint::Length(8), Constraint::Min(4)]).areas(side);
    let [inflight, mism, logs] = Layout::horizontal([
        Constraint::Percentage(28),
        Constraint::Percentage(44),
        Constraint::Percentage(28),
    ])
    .areas(bottom);

    draw_stats(f, app, stats);
    draw_throughput(f, app, thr);
    draw_counters(f, app, counters);
    draw_oplog(f, app, mid);
    draw_inflight(f, app, inflight);
    draw_mismatches(f, app, mism);
    draw_logs(f, app, logs);
}

/// One secondary pane at a time above the operations log: the statistics
/// table, or the focused pane.
fn draw_medium(f: &mut Frame, app: &mut App, body: Rect) {
    app.cycle = vec![
        Focus::OpLog,
        Focus::Stats,
        Focus::Inflight,
        Focus::Mismatches,
        Focus::Logs,
    ];
    let [summary, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(body);
    draw_summary(f, app, summary);
    let mid_h = (rest.height * 2 / 5)
        .clamp(6, 14)
        .min(rest.height.saturating_sub(6));
    let [mid, log] = Layout::vertical([Constraint::Length(mid_h), Constraint::Min(6)]).areas(rest);
    match app.focus {
        Focus::Inflight => draw_inflight(f, app, mid),
        Focus::Mismatches => draw_mismatches(f, app, mid),
        Focus::Logs => draw_logs(f, app, mid),
        Focus::OpLog | Focus::Stats => draw_stats(f, app, mid),
    }
    draw_oplog(f, app, log);
}

// ---- small helpers ----

fn pane(title: Vec<Span<'static>>, focused: bool) -> Block<'static> {
    let border = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        DIM
    };
    Block::bordered().border_style(border).title(Line::from(title))
}

fn title(text: impl Into<String>, focused: bool) -> Vec<Span<'static>> {
    let style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Gray)
    };
    vec![Span::styled(format!(" {} ", text.into()), style)]
}

fn hint_line(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (k, v) in keys {
        spans.push(Span::styled(
            format!("[{k}]"),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!(" {v}  ")));
    }
    Line::from(spans)
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// A bordered, cleared box in the middle of the screen, sized to its text.
fn dialog(f: &mut Frame, area: Rect, width: u16, title: &str, color: Color, lines: Vec<Line<'static>>) {
    let w = width.min(area.width.saturating_sub(2)).max(10);
    let h = (wrapped_height(&lines, w.saturating_sub(2)) + 2).min(area.height);
    let r = centered(area, w, h);
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color).add_modifier(Modifier::BOLD))
        .title(Line::styled(
            format!(" {title} "),
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), r);
}

/// Rows `lines` need when wrapped to `width` columns (an estimate: wrapping
/// happens at word boundaries).
fn wrapped_height(lines: &[Line<'_>], width: u16) -> u16 {
    let width = width.max(1);
    lines
        .iter()
        .map(|l| (l.width() as u16).div_ceil(width).max(1))
        .sum()
}

fn kv(label: &str, value: String, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), DIM),
        Span::styled(value, style),
    ])
}

fn mode_color(m: MismatchMode) -> Color {
    match m {
        MismatchMode::Resync => Color::Cyan,
        MismatchMode::Log => Color::Green,
        MismatchMode::Fail => Color::Yellow,
        MismatchMode::Freeze => Color::LightRed,
        MismatchMode::Detach => Color::Red,
    }
}

fn level_style(l: tracing::Level) -> Style {
    match l {
        tracing::Level::ERROR => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        tracing::Level::WARN => Style::new().fg(Color::Yellow),
        tracing::Level::INFO => Style::new().fg(Color::Green),
        tracing::Level::DEBUG => Style::new().fg(Color::Blue),
        tracing::Level::TRACE => DIM,
    }
}

fn total_ops(app: &App) -> u64 {
    OpKind::ALL
        .iter()
        .map(|&k| app.stats.op(k).count.load(Relaxed))
        .sum()
}

fn num_cell(s: String) -> Cell<'static> {
    Cell::from(Line::from(s).alignment(Alignment::Right))
}

fn lat_cell(ns: u64, l: &Lat) -> Cell<'static> {
    if l.n == 0 {
        Cell::from(Line::styled("-", DIM).alignment(Alignment::Right))
    } else if l.stale {
        // nothing in the window: since-start value, dimmed
        Cell::from(Line::styled(fmt_ns(ns), DIM).alignment(Alignment::Right))
    } else {
        num_cell(fmt_ns(ns))
    }
}

// ---- header / footer ----

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let [l1, l2] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let frozen = app.policy.is_frozen();
    let detached = app.stats.detached.load(Relaxed);
    let (badge, badge_style, line_bg) = if frozen {
        (
            format!(" FROZEN: {} pending ", app.pending.len().max(1)),
            Style::new()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
            Style::new().bg(Color::Indexed(52)),
        )
    } else if detached {
        (
            " DETACHED ".into(),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            Style::new(),
        )
    } else {
        (
            " RUNNING ".into(),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Style::new(),
        )
    };

    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(badge.len() as u16 + 1)]).areas(l1);
    let i = &app.info;
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " xcheckfs ",
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(i.mountpoint.display().to_string(), BOLD),
            Span::raw("  "),
            Span::styled(i.primary.display().to_string(), Style::new().fg(COLOR_PRI)),
            Span::styled(" \u{2192} ", DIM),
            Span::styled(i.secondary.display().to_string(), Style::new().fg(COLOR_SEC)),
        ]))
        .style(line_bg),
        left,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(badge, badge_style)))
            .alignment(Alignment::Right)
            .style(line_bg),
        right,
    );

    let mode = app.policy.mode();
    let mm = app.stats.mismatches.load(Relaxed);
    let ops_rate = app
        .history
        .window(WINDOW_SECS)
        .map_or(0.0, |w| w.rate(|s| s.total_ops()));
    let sep = Span::styled(" \u{2502} ", DIM);
    let mut spans = vec![
        Span::styled(" check ", DIM),
        Span::styled(i.check.name(), Style::new().fg(Color::Cyan)),
        sep.clone(),
        Span::styled("mode ", DIM),
        Span::styled(
            mode.name().to_uppercase(),
            Style::new().fg(mode_color(mode)).add_modifier(Modifier::BOLD),
        ),
        sep.clone(),
        Span::styled("up ", DIM),
        Span::raw(fmt_duration(app.stats.started.elapsed().as_secs())),
        sep.clone(),
        Span::styled("ops ", DIM),
        Span::raw(format!(
            "{} ({}/s)",
            fmt_count(total_ops(app)),
            fmt_rate(ops_rate)
        )),
        sep.clone(),
        Span::styled("errors ", DIM),
        Span::styled(
            mm.to_string(),
            if mm > 0 {
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Green)
            },
        ),
        sep.clone(),
        Span::styled("allowed ", DIM),
        Span::raw(app.stats.allowed.load(Relaxed).to_string()),
        Span::styled(" repeats ", DIM),
        Span::raw(app.stats.repeats.load(Relaxed).to_string()),
    ];
    if let Some(oldest) = app.inflight.first() {
        let age = oldest.started.elapsed().as_secs_f64();
        if age >= 1.0 {
            spans.push(sep);
            spans.push(Span::styled(
                format!("stuck {} {:.0}s", oldest.op.name(), age),
                if age >= 10.0 {
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::Yellow)
                },
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(line_bg), l2);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    if let Some(s) = app.live_status() {
        let style = if s.error {
            Style::new()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Black).bg(Color::Green)
        };
        spans.push(Span::styled(format!(" {} ", s.text), style));
        spans.push(Span::raw(" "));
    }
    let keys: &[(&str, &str)] = if app.pending.is_empty() {
        &[
            ("q", "quit"),
            ("?", "help"),
            ("Tab", "focus"),
            ("m", "mode"),
            ("D", "detach"),
            ("w", "window"),
            ("e", "errors"),
            ("/", "filter"),
            ("f", "follow"),
        ]
    } else {
        &[
            ("c", "continue"),
            ("a", "allow"),
            ("r", "retry"),
            ("s", "resync"),
            ("e", "EIO"),
            ("d", "detach"),
            ("\u{2190}\u{2192}", "switch"),
            ("?", "help"),
        ]
    };
    for (k, v) in keys {
        spans.push(Span::styled(*k, Style::new().fg(Color::Black).bg(Color::Gray)));
        spans.push(Span::styled(format!(" {v} "), DIM));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_filter_input(f: &mut Frame, app: &App, area: Rect) {
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" / ", Style::new().fg(Color::Black).bg(Color::Yellow)),
            Span::raw(format!(" {}", app.filter_input)),
            Span::styled("\u{2588}", Style::new().fg(Color::Yellow)),
            Span::styled(
                "   Enter: apply   Esc: clear   (matches op, path/args, result names)",
                DIM,
            ),
        ])),
        area,
    );
}

/// One-line replacement for the throughput and counters panes.
fn draw_summary(f: &mut Frame, app: &App, area: Rect) {
    let w = app.history.window(WINDOW_SECS);
    let (rd, wr) = w.as_ref().map_or((0.0, 0.0), |w| {
        (w.rate(|s| s.bytes_read), w.rate(|s| s.bytes_written))
    });
    let s = &app.stats;
    let l = |a: &std::sync::atomic::AtomicU64| a.load(Relaxed);
    let item = |k: &str, v: u64, warn: bool| {
        vec![
            Span::styled(format!(" {k} "), DIM),
            Span::styled(
                v.to_string(),
                if warn && v > 0 {
                    Style::new().fg(Color::Yellow)
                } else {
                    Style::new()
                },
            ),
        ]
    };
    let mut spans = vec![
        Span::styled(" R ", Style::new().fg(Color::Green)),
        Span::raw(format!("{}/s", fmt_bytes(rd))),
        Span::styled("  W ", Style::new().fg(Color::Magenta)),
        Span::raw(format!("{}/s ", fmt_bytes(wr))),
        Span::styled("\u{2502}", DIM),
    ];
    for (k, v, warn) in [
        ("nodes", l(&s.nodes), false),
        ("files", l(&s.open_files), false),
        ("dirs", l(&s.open_dirs), false),
        ("lockw", l(&s.lock_waiters), true),
        ("verif", l(&s.verifications), false),
        ("fixed", l(&s.resyncs), false),
        ("unfixed", l(&s.resync_failures) + l(&s.resync_giveups), true),
        ("skip", l(&s.secondary_skipped), false),
        ("drop", l(&s.events_dropped), true),
        ("rules", app.rules as u64, false),
    ] {
        spans.extend(item(k, v, warn));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---- statistics table ----

#[derive(Clone, Copy)]
enum Col {
    Op,
    Count,
    Rate,
    Mism,
    Bytes,
    Bps,
    TotP50,
    TotP99,
    TotMax,
    PriP50,
    PriP99,
    SecP50,
    SecP99,
    Ratio,
}

/// Column, header, width, minimum pane width to be shown. The optional
/// columns appear one after the other as the pane gets wider.
const COLS: &[(Col, &str, u16, u16)] = &[
    (Col::Op, "op", 11, 0),
    (Col::Count, "count", 8, 0),
    (Col::Rate, "/s", 7, 0),
    // An errno from the primary is a normal result (the secondary agreeing
    // is the point); only disagreements are errors.
    (Col::Mism, "errors", 6, 0),
    (Col::Bytes, "bytes", 9, 128),
    (Col::Bps, "B/s", 9, 110),
    (Col::TotP50, "p50", 7, 0),
    (Col::TotP99, "p99", 7, 100),
    (Col::TotMax, "max", 7, 118),
    (Col::PriP50, "pri p50", 7, 0),
    (Col::PriP99, "pri p99", 7, 92),
    (Col::SecP50, "sec p50", 7, 0),
    (Col::SecP99, "sec p99", 7, 92),
    (Col::Ratio, "sec/pri", 7, 0),
];

fn col_cell(c: Col, r: &OpRow) -> Cell<'static> {
    match c {
        Col::Op => Cell::from(Span::styled(r.kind.name(), BOLD)),
        Col::Count => num_cell(fmt_count(r.count)),
        Col::Rate => num_cell(fmt_rate(r.rate)),
        Col::Mism => {
            let st = if r.mismatches > 0 {
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                DIM
            };
            Cell::from(Line::styled(fmt_count(r.mismatches), st).alignment(Alignment::Right))
        }
        Col::Bytes => num_cell(if r.bytes == 0 {
            String::new()
        } else {
            fmt_bytes(r.bytes as f64)
        }),
        Col::Bps => num_cell(if r.bps < 1.0 {
            String::new()
        } else {
            fmt_bytes(r.bps)
        }),
        Col::TotP50 => lat_cell(r.total.p50, &r.total),
        Col::TotP99 => lat_cell(r.total.p99, &r.total),
        Col::TotMax => lat_cell(r.total.max, &r.total),
        Col::PriP50 => lat_cell(r.pri.p50, &r.pri),
        Col::PriP99 => lat_cell(r.pri.p99, &r.pri),
        Col::SecP50 => lat_cell(r.sec.p50, &r.sec),
        Col::SecP99 => lat_cell(r.sec.p99, &r.sec),
        Col::Ratio => match r.ratio() {
            None => Cell::from(Line::styled("-", DIM).alignment(Alignment::Right)),
            Some(x) => {
                let st = if x > 10.0 {
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else if x > 2.0 {
                    Style::new().fg(Color::Yellow)
                } else {
                    Style::new().fg(Color::Green)
                };
                Cell::from(Line::styled(format!("{x:.1}x"), st).alignment(Alignment::Right))
            }
        },
    }
}

fn col_header(c: Col, text: &'static str) -> Cell<'static> {
    let color = match c {
        Col::PriP50 | Col::PriP99 => COLOR_PRI,
        Col::SecP50 | Col::SecP99 => COLOR_SEC,
        Col::Ratio => Color::Yellow,
        _ => Color::Cyan,
    };
    let align = if matches!(c, Col::Op) {
        Alignment::Left
    } else {
        Alignment::Right
    };
    Cell::from(Line::styled(text, Style::new().fg(color).add_modifier(Modifier::BOLD)).alignment(align))
}

fn draw_stats(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Stats;
    let cap = area.height.saturating_sub(3) as usize;
    if focused {
        app.page = cap;
    }
    let max_off = app.rows.len().saturating_sub(cap);
    app.stats_scroll.off = app.stats_scroll.off.min(max_off);
    let off = app.stats_scroll.off;

    let cols: Vec<_> = COLS.iter().filter(|c| area.width >= c.3).collect();
    let header = Row::new(cols.iter().map(|c| col_header(c.0, c.1)));
    let rows = app
        .rows
        .iter()
        .skip(off)
        .take(cap)
        .map(|r| Row::new(cols.iter().map(|c| col_cell(c.0, r))));
    let widths = cols.iter().map(|c| Constraint::Length(c.2));

    let which = if app.windowed {
        format!("last {WINDOW_SECS:.0}s")
    } else {
        "since start".into()
    };
    let mut t = title(format!("Operations ({})", app.rows.len()), focused);
    t.push(Span::styled(format!("latency: {which} [w] "), DIM));
    if app.windowed {
        t.push(Span::styled("(max is all-time) ", DIM));
    }
    if max_off > 0 {
        t.push(Span::styled(
            format!(
                "{}-{}/{} ",
                off + 1,
                (off + cap).min(app.rows.len()),
                app.rows.len()
            ),
            DIM,
        ));
    }
    f.render_widget(
        Table::new(rows, widths)
            .header(header)
            .block(pane(t, focused))
            .column_spacing(1),
        area,
    );
}

// ---- throughput / counters ----

fn draw_throughput(f: &mut Frame, app: &App, area: Rect) {
    let block = pane(title("Throughput", false), false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = app.history.window(WINDOW_SECS);
    let snap = app.history.latest();
    let metrics: [(&str, Color, &Ring<u64>, String); 3] = [
        (
            "read",
            COLOR_PRI,
            &app.history.read_series,
            format!(
                "{:>11}/s  total {}",
                fmt_bytes(w.as_ref().map_or(0.0, |w| w.rate(|s| s.bytes_read))),
                fmt_bytes(app.stats.bytes_read.load(Relaxed) as f64)
            ),
        ),
        (
            "write",
            COLOR_SEC,
            &app.history.write_series,
            format!(
                "{:>11}/s  total {}",
                fmt_bytes(w.as_ref().map_or(0.0, |w| w.rate(|s| s.bytes_written))),
                fmt_bytes(app.stats.bytes_written.load(Relaxed) as f64)
            ),
        ),
        (
            "ops",
            Color::Cyan,
            &app.history.ops_series,
            format!(
                "{:>11}/s  total {}",
                fmt_rate(w.as_ref().map_or(0.0, |w| w.rate(|s| s.total_ops()))),
                fmt_count(snap.map_or(0, |s| s.total_ops()))
            ),
        ),
    ];
    let slots = Layout::vertical([Constraint::Length(2); 3]).split(inner);
    for ((name, color, series, text), slot) in metrics.into_iter().zip(slots.iter()) {
        let [label, spark] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(*slot);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{name:<6}"),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(text),
            ])),
            label,
        );
        let n = series.len().saturating_sub(spark.width as usize);
        let data: Vec<u64> = series.iter().skip(n).copied().collect();
        f.render_widget(
            Sparkline::default().data(data).style(Style::new().fg(color)),
            spark,
        );
    }
}

fn draw_counters(f: &mut Frame, app: &App, area: Rect) {
    let s = &app.stats;
    let l = |a: &std::sync::atomic::AtomicU64| a.load(Relaxed);
    let cell = |label: &str, v: u64, warn: bool| -> Vec<Span<'static>> {
        let st = if warn && v > 0 {
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        vec![
            Span::styled(format!("{label:<13}"), DIM),
            Span::styled(format!("{:>7}", fmt_count(v)), st),
        ]
    };
    let pairs = [
        (
            ("nodes", l(&s.nodes), false),
            ("verifications", l(&s.verifications), false),
        ),
        (
            ("open files", l(&s.open_files), false),
            ("sec skipped", l(&s.secondary_skipped), false),
        ),
        (
            ("open dirs", l(&s.open_dirs), false),
            ("events dropped", l(&s.events_dropped), true),
        ),
        (
            ("lock waiters", l(&s.lock_waiters), true),
            ("rules", app.rules as u64, false),
        ),
        (
            ("repaired", l(&s.resyncs), false),
            ("unrepaired", l(&s.resync_failures) + l(&s.resync_giveups), true),
        ),
        (
            ("quarantined", l(&s.quarantined), false),
            ("", 0, false),
        ),
    ];
    let lines: Vec<Line> = pairs
        .iter()
        .map(|(a, b)| {
            let mut v = cell(a.0, a.1, a.2);
            if !b.0.is_empty() {
                v.push(Span::raw("  "));
                v.extend(cell(b.0, b.1, b.2));
            }
            Line::from(v)
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(pane(title("Counters", false), false)),
        area,
    );
}

// ---- operations log ----

fn draw_oplog(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::OpLog;
    let cap = area.height.saturating_sub(3) as usize;
    if focused {
        app.page = cap;
    }

    // Newest-first walk: skip what is scrolled off at the bottom, collect
    // one screenful, and clamp the scroll position to the available rows.
    let want = app.log_scroll.off.saturating_add(cap);
    let hits: Vec<&OpEntry> = app
        .oplog
        .iter()
        .rev()
        .filter(|e| app.filter.matches(e))
        .take(want)
        .collect();
    let off = app.log_scroll.off.min(hits.len().saturating_sub(cap));
    let end = (off + cap).min(hits.len());
    let w = area.width;
    let show_sec = w >= 70;
    let show_bytes = w >= 90;

    let rows: Vec<Row> = hits[off..end]
        .iter()
        .rev()
        .map(|e| {
            let style = if e.mismatch {
                MISMATCH_ROW
            } else {
                Style::new()
            };
            let mut cells = vec![
                Cell::from(Span::styled(e.ts.clone(), DIM)),
                Cell::from(e.op),
                Cell::from(e.detail.clone()),
                Cell::from(errno_name(e.errno)),
            ];
            if show_sec {
                cells.push(match e.sec_diff() {
                    Some(s) => Cell::from(Span::styled(
                        errno_name(s),
                        Style::new().fg(Color::LightRed).add_modifier(Modifier::BOLD),
                    )),
                    None => Cell::from(""),
                });
            }
            cells.push(num_cell(fmt_ns(e.total_ns)));
            if show_bytes {
                cells.push(num_cell(if e.bytes == 0 {
                    String::new()
                } else {
                    fmt_bytes(e.bytes as f64)
                }));
            }
            Row::new(cells).style(style)
        })
        .collect();

    let hdr = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let mut head = vec![
        Cell::from("time"),
        Cell::from("op"),
        Cell::from("detail"),
        Cell::from(Span::styled(
            "primary",
            Style::new().fg(COLOR_PRI).add_modifier(Modifier::BOLD),
        )),
    ];
    let mut widths = vec![
        Constraint::Length(12),
        Constraint::Length(11),
        Constraint::Fill(1),
        Constraint::Length(9),
    ];
    if show_sec {
        head.push(Cell::from(Span::styled(
            "secondary",
            Style::new().fg(COLOR_SEC).add_modifier(Modifier::BOLD),
        )));
        widths.push(Constraint::Length(9));
    }
    head.push(Cell::from(Line::from("latency").alignment(Alignment::Right)));
    widths.push(Constraint::Length(9));
    if show_bytes {
        head.push(Cell::from(Line::from("bytes").alignment(Alignment::Right)));
        widths.push(Constraint::Length(9));
    }

    let mut t = title(format!("Operations log ({} kept)", app.oplog.len()), focused);
    if off > 0 || !app.follow {
        t.push(Span::styled(
            format!("[paused, {off} newer] "),
            Style::new().fg(Color::Yellow),
        ));
    } else {
        t.push(Span::styled("[following] ", Style::new().fg(Color::Green)));
    }
    if app.filter.errors_only {
        t.push(Span::styled(
            "[errors only] ",
            Style::new().fg(Color::Yellow),
        ));
    }
    if !app.filter.text.is_empty() {
        t.push(Span::styled(
            format!("[/{}] ", app.filter_input),
            Style::new().fg(Color::Yellow),
        ));
    }
    if app.filter.is_active() {
        t.push(Span::styled(format!("{} shown ", hits.len()), DIM));
    }

    app.log_scroll.off = off;
    f.render_widget(
        Table::new(rows, widths)
            .header(Row::new(head).style(hdr))
            .column_spacing(1)
            .block(pane(t, focused)),
        area,
    );
}

// ---- in-flight / mismatches / logs ----

fn draw_inflight(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Inflight;
    let cap = area.height.saturating_sub(3) as usize;
    if focused {
        app.page = cap;
    }
    let max_off = app.inflight.len().saturating_sub(cap);
    app.inflight_scroll.off = app.inflight_scroll.off.min(max_off);
    let oldest = app
        .inflight
        .first()
        .map_or(0.0, |o| o.started.elapsed().as_secs_f64());

    let rows = app
        .inflight
        .iter()
        .skip(app.inflight_scroll.off)
        .take(cap)
        .map(|o| {
            let age = o.started.elapsed().as_secs_f64();
            let style = if age >= 10.0 {
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else if age >= 1.0 {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new()
            };
            Row::new(vec![
                num_cell(fmt_ns((age * 1e9) as u64)),
                Cell::from(o.op.name()),
                Cell::from(super::model::sanitize(&o.detail)),
            ])
            .style(style)
        });
    let accent = if oldest >= 10.0 {
        Color::Red
    } else if oldest >= 1.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    let mut t = title(format!("In flight ({})", app.inflight.len()), focused);
    if app.inflight.is_empty() {
        t.push(Span::styled("idle ", Style::new().fg(accent)));
    } else {
        t.push(Span::styled(
            format!("oldest {} ", fmt_ns((oldest * 1e9) as u64)),
            Style::new().fg(accent),
        ));
    }
    let head = Row::new(vec![
        Cell::from(Line::from("age").alignment(Alignment::Right)),
        Cell::from("op"),
        Cell::from("detail"),
    ])
    .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    f.render_widget(
        Table::new(
            rows,
            [Constraint::Length(8), Constraint::Length(11), Constraint::Fill(1)],
        )
        .header(head)
        .column_spacing(1)
        .block(pane(t, focused)),
        area,
    );
}

fn draw_mismatches(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Mismatches;
    let n = app.mismatches.len();
    let mut t = title(format!("Mismatches ({n})"), focused);
    if n > 0 {
        t.push(Span::styled("newest first, Enter: details ", DIM));
    }
    let block = pane(t, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // List on top; a summary box below when there is room.
    let (list, details) = if inner.height >= 9 {
        let [a, b] = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).areas(inner);
        (a, Some(b))
    } else {
        (inner, None)
    };
    let cap = list.height as usize;
    if focused {
        app.page = cap;
    }
    if n == 0 {
        f.render_widget(
            Paragraph::new(Span::styled(
                "none \u{2014} the file systems agree",
                Style::new().fg(Color::Green),
            )),
            list,
        );
        return;
    }
    let sel = app.mm_index();
    let start = sel.saturating_sub(cap / 2).min(n.saturating_sub(cap));
    let lines: Vec<Line> = app
        .mismatches
        .iter()
        .enumerate()
        .skip(start)
        .take(cap)
        .map(|(i, m)| {
            let mut l = Line::from(vec![
                Span::styled(
                    format!("#{:<4}", m.id),
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{} ", fmt_clock(m.time)), DIM),
                Span::styled(format!("{:<9}", m.op.name()), BOLD),
                Span::styled(format!("{:<8}", m.kind.name()), Style::new().fg(Color::Yellow)),
                Span::raw(m.path.clone()),
            ]);
            if i == sel {
                l = l.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            l
        })
        .collect();
    f.render_widget(Paragraph::new(lines), list);
    if let (Some(d), Some(m)) = (details, app.mismatches.get(sel)) {
        f.render_widget(
            Paragraph::new(m.summary())
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(Color::Gray))
                .block(
                    Block::new()
                        .borders(ratatui::widgets::Borders::TOP)
                        .border_style(DIM),
                ),
            d,
        );
    }
}

fn draw_logs(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Logs;
    let cap = area.height.saturating_sub(2) as usize;
    if focused {
        app.page = cap;
    }
    let total = app.logs.len();
    app.logs_scroll.off = app.logs_scroll.off.min(total.saturating_sub(cap));
    let end = total - app.logs_scroll.off;
    let start = end.saturating_sub(cap);
    let lines: Vec<Line> = app
        .logs
        .iter()
        .skip(start)
        .take(end - start)
        .map(|e| {
            Line::from(vec![
                Span::styled(format!("{} ", &e.ts[..e.ts.len().min(8)]), DIM),
                Span::styled(format!("{:<5} ", e.level.as_str()), level_style(e.level)),
                Span::styled(
                    e.message.clone(),
                    if e.level == tracing::Level::TRACE {
                        DIM
                    } else {
                        Style::new()
                    },
                ),
            ])
        })
        .collect();
    let mut t = title(format!("Log ({total})"), focused);
    if app.logs_scroll.off > 0 {
        t.push(Span::styled(
            format!("[{} newer] ", app.logs_scroll.off),
            Style::new().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(lines).block(pane(t, focused)), area);
}

// ---- modals ----

/// Lines describing one mismatch in full.
fn mismatch_lines(m: &Mismatch) -> Vec<Line<'static>> {
    let mut v = vec![
        Line::from(vec![
            Span::styled(
                format!("#{} ", m.id),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{} ", m.op.name()), BOLD),
            Span::styled(
                format!("{} mismatch", m.kind.name()),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
        ]),
        kv(
            "path",
            if m.path.is_empty() {
                "?".into()
            } else {
                m.path.clone()
            },
            Style::new(),
        ),
        kv("inode", m.ino.to_string(), Style::new()),
        kv("time", fmt_clock(m.time), Style::new()),
    ];
    if let Some(field) = &m.field {
        v.push(kv("field", field.clone(), Style::new().fg(Color::Cyan)));
    }
    v.push(kv(
        "primary",
        super::model::sanitize(&m.primary),
        Style::new().fg(COLOR_PRI),
    ));
    v.push(kv(
        "secondary",
        super::model::sanitize(&m.secondary),
        Style::new().fg(COLOR_SEC),
    ));
    if !m.detail.is_empty() {
        v.push(kv("detail", super::model::sanitize(&m.detail), Style::new()));
    }
    v
}

fn action_span(key: &str, label: &str, enabled: bool) -> Vec<Span<'static>> {
    let (k, l) = if enabled {
        (
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Style::new(),
        )
    } else {
        (Style::new().fg(Color::DarkGray), DIM)
    };
    vec![
        Span::styled(format!(" {key} "), k),
        Span::styled(format!(" {label}   "), l),
    ]
}

fn draw_freeze(f: &mut Frame, app: &App, area: Rect) {
    let Some(m) = app.current_pending() else { return };
    let w = area.width.saturating_sub(4).min(104);
    let lines = mismatch_lines(m);
    // Text (with a spare row for word-wrap slack), blank, 3 action rows, borders.
    let h = (wrapped_height(&lines, w.saturating_sub(2)) + 1 + 3 + 2).min(area.height.saturating_sub(2));
    let r = centered(area, w, h);
    f.render_widget(Clear, r);
    let n = app.pending.len();
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(Color::Red))
        .title(Line::styled(
            format!(" FROZEN: operator decision required ({}/{n}) ", app.pend_sel + 1),
            Style::new()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::styled(
            " all file system operations are blocked until every mismatch is resolved ",
            Style::new().fg(Color::Red),
        ));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [content, actions] = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(inner);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), content);

    let mut line1 = Vec::new();
    line1.extend(action_span("c", "continue", true));
    line1.extend(action_span("a", "allow...", true));
    line1.extend(action_span("r", "retry", m.retryable));
    line1.extend(action_span("s", "resync from primary", m.resyncable));
    let mut line2 = Vec::new();
    line2.extend(action_span("e", "fail op with EIO", true));
    line2.extend(action_span("d", "detach secondary", true));
    if n > 1 {
        line2.extend(action_span("\u{2190}/\u{2192}", "other pending", true));
    }
    f.render_widget(
        Paragraph::new(vec![Line::from(line1), Line::default(), Line::from(line2)]),
        actions,
    );
}

fn draw_allow(f: &mut Frame, app: &App, area: Rect) {
    let Some(m) = &app.target else { return };
    let persist = if app.policy.can_persist() {
        let p = app
            .policy
            .rules_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Line::styled(
            format!("The rule is saved to {p} and kept for this session."),
            Style::new().fg(Color::Green),
        )
    } else if app.policy.rules_path().is_some() {
        Line::styled(
            "Session only: the rules file is inside a mirrored tree and cannot be written.",
            Style::new().fg(Color::Yellow),
        )
    } else {
        Line::styled(
            "Session only: no rules file configured (--rules).",
            Style::new().fg(Color::Yellow),
        )
    };
    let r1 = crate::policy::Rule::from_mismatch(m, false).describe();
    let r2 = crate::policy::Rule::from_mismatch(m, true).describe();
    let lines = vec![
        Line::from(format!(
            "Allow mismatch #{} ({} {}) from now on:",
            m.id,
            m.op.name(),
            m.kind.name()
        )),
        Line::default(),
        Line::from(vec![
            Span::styled("[1] ", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(format!("everywhere: {r1}")),
        ]),
        Line::from(vec![
            Span::styled("[2] ", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(format!("this path only: {r2}")),
        ]),
        Line::default(),
        persist,
        Line::styled("Esc or any other key cancels.", DIM),
    ];
    dialog(f, area, 96, "Allow rule", Color::Cyan, lines);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rows: &[(&str, &str)] = &[
        ("q / Ctrl-C", "quit (asks for confirmation)"),
        (
            "Tab / Shift-Tab",
            "cycle focus: log, stats, in flight, mismatches, log pane",
        ),
        (
            "Up Down PgUp PgDn Home End",
            "scroll the focused pane (also j k g G)",
        ),
        ("f", "follow the tail of the operations log"),
        ("e", "only errors (mismatches) in the operations log"),
        ("/", "filter the operations log (Esc clears)"),
        ("w", "latency columns: last 5s / since start"),
        ("m", "mismatch mode: resync > log > fail > freeze > resync"),
        ("D", "detach the secondary (asks for confirmation)"),
        ("Enter", "full details of the selected mismatch (mismatches pane)"),
        ("?", "this help"),
        ("", ""),
        ("when frozen:", ""),
        ("c", "continue with the primary's result"),
        ("a, then 1 / 2", "allow: rule everywhere / for this path"),
        ("r", "retry the operation (read-only ones)"),
        ("s", "resync the object from the primary"),
        ("e", "fail the operation with EIO"),
        ("d", "detach the secondary (asks for confirmation)"),
        ("Left / Right", "switch between pending mismatches"),
    ];
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                Line::styled(*k, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else {
                Line::from(vec![
                    Span::styled(format!("{k:<28}"), Style::new().fg(Color::Cyan)),
                    Span::raw(*v),
                ])
            }
        })
        .collect();
    lines.push(Line::default());
    lines.push(Line::styled(
        "Latency ratio sec/pri: yellow above 2x, red above 10x.",
        DIM,
    ));
    lines.push(Line::styled("Press any key to close.", DIM));
    dialog(f, area, 92, "Help", Color::Cyan, lines);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::SystemTime;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::config::{CheckLevel, EngineConfig};
    use crate::events::{EventSink, OpEvent, UiEvent};
    use crate::policy::{MismatchKind, Policy};
    use crate::stats::Stats;
    use crate::tui::{MountInfo, TuiContext};

    fn app() -> (App, crossbeam_channel::Sender<UiEvent>, Arc<Policy>) {
        let stats = Arc::new(Stats::default());
        let policy = Arc::new(
            Policy::new(
                MismatchMode::Log,
                vec![],
                None,
                false,
                EventSink::disabled(),
                stats.clone(),
            )
            .unwrap(),
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        let ctx = TuiContext {
            stats,
            policy: policy.clone(),
            events: rx,
            history: 100,
            info: MountInfo {
                mountpoint: "/mnt/x".into(),
                primary: "/data/a".into(),
                secondary: "/data/b".into(),
                check: CheckLevel::Basic,
                engine: EngineConfig::default(),
                rules_path: None,
                control_socket: None,
            },
            shutdown: Arc::new(Default::default()),
        };
        (App::new(ctx), tx, policy)
    }

    fn op(n: u64, errno: i32, mismatch: bool) -> UiEvent {
        UiEvent::Op(OpEvent {
            time: SystemTime::now(),
            op: OpKind::Read,
            ino: n,
            detail: format!("/dir/file{n}\t@{n}+4096"),
            errno,
            sec_errno: Some(if mismatch { libc::EIO } else { errno }),
            total_ns: 1000 * n,
            primary_ns: 400 * n,
            secondary_ns: 600 * n,
            bytes: 4096,
            mismatch,
        })
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_at_all_sizes_without_panicking() {
        let (mut app, tx, _policy) = app();
        let stats = app.stats.clone();
        for i in 1..=300 {
            stats.op(OpKind::Read).count.fetch_add(1, Relaxed);
            stats.op(OpKind::Read).total.record(1000 * i);
            tx.send(op(i, if i % 7 == 0 { libc::ENOENT } else { 0 }, i % 50 == 0))
                .unwrap();
        }
        tx.send(UiEvent::Log {
            level: tracing::Level::WARN,
            message: "hello\nworld".into(),
        })
        .unwrap();
        app.drain_events();
        app.tick();
        for (w, h) in [
            (200, 60),
            (140, 45),
            (120, 38),
            (100, 30),
            (80, 24),
            (70, 20),
            (60, 18),
            (40, 10),
            (20, 5),
            (1, 1),
            (0, 0),
        ] {
            for focus in [
                Focus::OpLog,
                Focus::Stats,
                Focus::Inflight,
                Focus::Mismatches,
                Focus::Logs,
            ] {
                app.focus = focus;
                render(&mut app, w, h);
            }
        }
        let s = render(&mut app, 80, 24);
        assert!(
            s.contains("RUNNING") && s.contains("read") && s.contains("/dir/file300"),
            "{s}"
        );
        // Filters and scrolling clamp instead of panicking.
        app.filter.errors_only = true;
        app.log_scroll.off = usize::MAX;
        app.focus = Focus::OpLog;
        render(&mut app, 100, 30);
        assert!(app.log_scroll.off < 300);
    }

    #[test]
    fn freeze_modal_and_overlays_render() {
        let (mut app, _tx, policy) = app();
        policy.set_mode(MismatchMode::Freeze);
        let p2 = policy.clone();
        std::thread::spawn(move || {
            p2.report(Mismatch {
                id: 0,
                time: SystemTime::now(),
                op: OpKind::Read,
                kind: MismatchKind::Data,
                ino: 7,
                path: "/a/b".into(),
                field: None,
                primary: "x".into(),
                secondary: "y".into(),
                detail: "bytes differ at offset 3".into(),
                retryable: true,
                resyncable: false,
            })
        });
        while policy.pending().is_empty() {
            std::thread::yield_now();
        }
        app.tick();
        let s = render(&mut app, 100, 30);
        assert!(s.contains("FROZEN") && s.contains("bytes differ"), "{s}");
        for o in [
            Overlay::Help,
            Overlay::ConfirmQuit,
            Overlay::ConfirmDetach,
            Overlay::Filter,
        ] {
            app.overlay = o;
            render(&mut app, 80, 24);
            render(&mut app, 30, 8);
        }
        // Allow dialog via the keys, then resolution through the policy.
        app.overlay = Overlay::None;
        let key = |c| ratatui::crossterm::event::KeyEvent::from(ratatui::crossterm::event::KeyCode::Char(c));
        app.on_key(key('a'));
        assert_eq!(app.overlay, Overlay::AllowChoice);
        render(&mut app, 80, 24);
        app.on_key(key('2'));
        // The frozen thread adds the rule after waking up.
        for _ in 0..200 {
            if !policy.rules().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(policy.rules().len(), 1);
        assert!(policy.rules()[0].path.is_some());
    }
}
