//! DetailPane — pretty-printed JSON of the currently focused row.
//!
//! Single-line dump in M1 was too cramped to inspect a structured payload.
//! DetailPane is opt-in via Tab; renders side-by-side with the table.
//!
//! Pretty-print uses `serde_json::Value` — a full parse. This is OK because
//! it runs on exactly one record at a time, never on the index hot path.

use mgi_pulse_core::engine::Engine;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub fn render(f: &mut Frame, area: Rect, engine: &Engine, line_id: u64) {
    let block = Block::default()
        .title(format!(" detail · line {} ", line_id))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let bytes = engine.line_bytes(line_id);
    let body = match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(v) => match serde_json::to_string_pretty(&v) {
            Ok(s) => s,
            Err(_) => String::from_utf8_lossy(bytes).into_owned(),
        },
        Err(_) => {
            // Non-JSON line: just show the raw bytes verbatim.
            format!("(non-JSON line)\n{}", String::from_utf8_lossy(bytes))
        }
    };

    let lines: Vec<Line> = body
        .lines()
        .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(Color::Gray))))
        .collect();
    let hint = Line::from(Span::styled(
        "Tab to close, Esc to clear filter",
        Style::default().add_modifier(Modifier::DIM),
    ));
    let mut all = lines;
    all.push(Line::from(""));
    all.push(hint);

    let p = Paragraph::new(all).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}
