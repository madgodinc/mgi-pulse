//! Stats overlay pane.
//!
//! Renders the `Stats` summary as a small box overlay: total / matched,
//! per-severity bar, time span, and top values for the chosen field.

use mgi_pulse_core::engine::record::severity;
use mgi_pulse_core::engine::stats::Stats;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

pub fn render(
    f: &mut Frame,
    area: Rect,
    stats: &Stats,
    theme: crate::theme::Theme,
) {
    let block = Block::default().title(" stats ").borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 8 || inner.height < 3 {
        return;
    }

    let dim = theme.hint_dim();
    let bright = theme.hint_bright();

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("total: ", dim),
        Span::styled(stats.total.to_string(), bright),
    ]));

    // Severity row: only show levels with non-zero counts so a plain-
    // text source doesn't litter the pane with zeros.
    for (sev, name) in [
        (severity::FATAL, "fatal"),
        (severity::ERROR, "error"),
        (severity::WARN, "warn"),
        (severity::INFO, "info"),
        (severity::DEBUG, "debug"),
        (severity::TRACE, "trace"),
    ] {
        let n = stats.by_severity[sev as usize];
        if n == 0 {
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{:>6}: ", name), dim),
            Span::styled(n.to_string(), theme.severity_style(sev)),
        ]));
    }

    if stats.untimed > 0 {
        lines.push(Line::from(vec![
            Span::styled("untimed: ", dim),
            Span::raw(stats.untimed.to_string()),
        ]));
    }

    if let (Some(lo), Some(hi)) = (stats.t_min, stats.t_max) {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "time span:",
            Style::default().add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(Span::styled(
            crate::panes::timeline::format_micros_short(lo),
            dim,
        )));
        lines.push(Line::from(Span::raw(" →")));
        lines.push(Line::from(Span::styled(
            crate::panes::timeline::format_micros_short(hi),
            dim,
        )));
    }

    if !stats.top_values.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("top {}:", stats.top_field),
            Style::default().add_modifier(Modifier::BOLD),
        )]));
        let widest = stats
            .top_values
            .iter()
            .map(|(_, n)| n.to_string().len())
            .max()
            .unwrap_or(1);
        for (v, n) in &stats.top_values {
            // Truncate to the pane width minus the count and a gap.
            let cap = (inner.width as usize).saturating_sub(widest + 3);
            let label: String = if v.chars().count() > cap {
                let mut s: String = v.chars().take(cap.saturating_sub(1)).collect();
                s.push('…');
                s
            } else {
                v.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{:>w$} ", n, w = widest), bright),
                Span::raw(label),
            ]));
        }
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}
