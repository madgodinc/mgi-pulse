//! DetailPane — full-context view of the focused record.
//!
//! Two modes, chosen by what the data carries:
//!
//! - **JSON record:** parses the focused line and pretty-prints it. This is
//!   the original M2 behavior — useful for nested payloads that the table's
//!   single-line cell can't show.
//! - **Plain-text:** shows N lines of context above and below the focused
//!   line, so multi-line records (Java stack traces, formatted exceptions,
//!   continuation lines) read as a connected block. This is what `less`
//!   does when you center the cursor; without it the user is stuck reading
//!   one row at a time.
//!
//! The renderer runs on exactly one focused record at a time, never on the
//! indexing hot path, so a full `serde_json::Value` parse is cheap here.

use mgi_pulse_core::engine::Engine;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

const CONTEXT_BEFORE: u64 = 5;
const CONTEXT_AFTER: u64 = 5;

pub fn render(f: &mut Frame, area: Rect, engine: &Engine, line_id: u64) {
    let block = Block::default()
        .title(format!(" detail · line {} ", line_id))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let bytes = engine.line_bytes(line_id);

    // JSON path: pretty-print the focused record. Fall through to the
    // context view if parsing fails.
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
        if let Ok(pretty) = serde_json::to_string_pretty(&v) {
            let lines: Vec<Line> = pretty
                .lines()
                .map(|l| {
                    Line::from(Span::styled(
                        l.to_string(),
                        Style::default().fg(Color::Gray),
                    ))
                })
                .collect();
            let p = Paragraph::new(lines).wrap(Wrap { trim: false });
            f.render_widget(p, inner);
            return;
        }
    }

    // Plain-text path: show context around the focused line so multi-line
    // records (Java stack traces, exception continuations) read as one
    // block. Surrounding lines are dimmed to keep the focus obvious.
    let total = engine.indexes.len() as u64;
    let from = line_id.saturating_sub(CONTEXT_BEFORE);
    let to = (line_id + CONTEXT_AFTER + 1).min(total);

    let mut lines: Vec<Line> = Vec::with_capacity((to - from) as usize);
    for lid in from..to {
        let raw = engine.line_bytes(lid);
        let text = String::from_utf8_lossy(raw);
        let style = if lid == line_id {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let marker = if lid == line_id { "▶ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(format!("{}{:>6}  ", marker, lid), Style::default().fg(Color::DarkGray)),
            Span::styled(text.to_string(), style),
        ]));
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}
