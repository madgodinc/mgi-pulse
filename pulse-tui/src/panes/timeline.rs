//! TimelinePane — overview histogram of record arrival over time.
//!
//! v0.1 ships a non-interactive overview: severity-colored bars spanning the
//! full indexed time range, painted across the available width. Keyboard
//! scrub (`<`, `>`, zoom) is v0.2 — held back until a week of personal use
//! settles the keyboard model.

use mgi_pulse_core::engine::histogram::Histogram;
use mgi_pulse_core::engine::record::severity;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

const BAR_CHARS: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];

fn severity_color(sev: u8) -> Color {
    match sev {
        severity::FATAL | severity::ERROR => Color::Red,
        severity::WARN => Color::Yellow,
        severity::INFO => Color::Green,
        severity::DEBUG | severity::TRACE => Color::DarkGray,
        _ => Color::Gray,
    }
}

fn format_micros_short(micros: i64) -> String {
    if micros == i64::MIN || micros == 0 {
        return "—".to_string();
    }
    let secs = micros.div_euclid(1_000_000);
    let day_secs = secs.rem_euclid(86_400);
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = day_secs / 3600;
    let mm = (day_secs % 3600) / 60;
    let ss = day_secs % 60;
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, hh, mm, ss)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Render the cached histogram. The caller (App) owns the cache so this pane
/// stays free of work on every redraw.
pub fn render(f: &mut Frame, area: Rect, h: &Histogram) {
    let block = Block::default()
        .title(" timeline ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width < 4 || inner.height < 2 {
        return;
    }

    if h.bins.is_empty() {
        let msg = if h.untimed > 0 {
            format!(" all {} records untimed (no ts field) ", h.untimed)
        } else {
            " no records ".to_string()
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().add_modifier(Modifier::DIM),
        )));
        f.render_widget(p, inner);
        return;
    }

    let peak = h.peak().max(1);
    let bar_steps = (BAR_CHARS.len() - 1) as u64;

    let mut bar_line_spans: Vec<Span> = Vec::with_capacity(h.bins.len());
    for bin in &h.bins {
        let frac = (bin.count * bar_steps) / peak;
        let ch = BAR_CHARS[frac.min(bar_steps) as usize];
        let style = Style::default().fg(severity_color(bin.dominant_severity()));
        bar_line_spans.push(Span::styled(ch.to_string(), style));
    }

    let label_left = format_micros_short(h.t_min);
    let label_right = format_micros_short(h.t_max);
    let mut center_summary = String::new();
    if h.untimed > 0 {
        center_summary = format!(" · {} untimed", h.untimed);
    }

    let lines = vec![
        Line::from(bar_line_spans),
        Line::from(vec![
            Span::styled(label_left, Style::default().add_modifier(Modifier::DIM)),
            Span::raw(" "),
            Span::styled(
                center_summary,
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::raw(" "),
            Span::styled(label_right, Style::default().add_modifier(Modifier::DIM)),
        ]),
    ];

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}
