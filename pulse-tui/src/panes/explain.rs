//! Explain pane: the model's answer about the focused record, filled
//! in as it streams. Long answers keep their tail in view.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::explain::Explain;

pub fn render(f: &mut Frame, area: Rect, ex: &Explain, theme: crate::theme::Theme) {
    let state = if ex.done { "" } else { " ..." };
    let block = Block::default()
        .title(format!(
            " explain line {}: {}{state} ",
            ex.line_id + 1,
            ex.model
        ))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 4 || inner.height < 2 {
        return;
    }
    let body = if ex.text.trim().is_empty() {
        "waiting for the model...".to_string()
    } else {
        ex.text.trim_start().to_string()
    };
    let mut lines: Vec<Line> = body.lines().map(|l| Line::from(l.to_string())).collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "e or Esc closes",
        theme.hint_dim(),
    )));
    // Approximate wrapped height so the newest text stays visible.
    let width = inner.width as usize;
    let rows: usize = lines
        .iter()
        .map(|l| (l.width().max(1)).div_ceil(width))
        .sum();
    let scroll = rows.saturating_sub(inner.height as usize) as u16;
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(p, inner);
}
