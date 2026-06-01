//! TablePane — M1 renderer.
//!
//! Shows one line per record, ordered by `line_id`. Severity tints the time
//! column. Cursor is a `line_id`, not a row index.
//!
//! The filtered view is a `&[u64]` of surviving `line_id`s. To render rows in
//! window `[scroll_top, scroll_top + visible_rows)`, the pane binary-searches
//! `filtered_view` for the lower bound and walks forward.

use mgi_pulse_core::engine::record::{severity, TS_UNTIMED};
use mgi_pulse_core::engine::Engine;
use mgi_pulse_core::schema::{project_field, unquote_if_string};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

const COL_LINE_W: usize = 9;
const COL_TS_W: usize = 24;
const COL_LV_W: usize = 6;
/// Default width for each auto-column. Real adaptive sizing is backlog;
/// this picks a comfortable default for level/logger/msg shapes.
const COL_FIELD_W: usize = 18;

/// Locate the index inside `filtered_view` at or after `cursor`. Returns
/// `filtered_view.len()` if every survivor is below `cursor`.
pub fn lower_bound(filtered_view: &[u64], cursor: u64) -> usize {
    filtered_view.partition_point(|&v| v < cursor)
}

/// Snap a cursor onto `filtered_view`. Used after filter changes: the old
/// cursor's `line_id` may not survive, so we land on the closest survivor
/// (largest `<= cursor`, else smallest `>= cursor`).
pub fn snap_cursor(filtered_view: &[u64], cursor: u64) -> Option<u64> {
    if filtered_view.is_empty() {
        return None;
    }
    let i = lower_bound(filtered_view, cursor);
    if i < filtered_view.len() && filtered_view[i] == cursor {
        return Some(cursor);
    }
    // Prefer the largest survivor <= cursor.
    if i > 0 {
        return Some(filtered_view[i - 1]);
    }
    Some(filtered_view[0])
}

fn severity_style(sev: u8) -> Style {
    let color = match sev {
        severity::ERROR | severity::FATAL => Color::Red,
        severity::WARN => Color::Yellow,
        severity::INFO => Color::Reset,
        severity::DEBUG | severity::TRACE => Color::DarkGray,
        _ => Color::Reset,
    };
    Style::default().fg(color)
}

/// Convert microseconds since epoch to `YYYY-MM-DDTHH:MM:SS.ffffff` (UTC).
/// Mirror of the indexer's parser. Returns an owned `String`; this is only
/// called on visible rows, not in the index hot path.
fn format_ts_utc(ts_micros: i64) -> String {
    if ts_micros == TS_UNTIMED {
        return "—".repeat(COL_TS_W - 1);
    }
    let secs = ts_micros.div_euclid(1_000_000);
    let micros = ts_micros.rem_euclid(1_000_000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let day_secs = secs.rem_euclid(86_400);
    let hh = day_secs / 3600;
    let mm = (day_secs % 3600) / 60;
    let ss = day_secs % 60;
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}", y, m, d, hh, mm, ss, micros)
}

/// Inverse of indexer's `days_from_civil`.
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

/// Render one frame of the table. `area` is the rectangle assigned by the
/// caller; `scroll_top` is the topmost visible `line_id`; `cursor` is the
/// focused `line_id`. The pane truncates raw line bytes to fit the row width.
pub fn render(
    f: &mut Frame,
    area: Rect,
    engine: &Engine,
    filtered_view: &[u64],
    scroll_top: u64,
    cursor: u64,
    title: &str,
) {
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if filtered_view.is_empty() || inner.height == 0 {
        let p = Paragraph::new(Line::from(Span::styled(
            "no rows",
            Style::default().add_modifier(Modifier::DIM),
        )));
        f.render_widget(p, inner);
        return;
    }

    // Less-mode adapt: each column is only shown when there's data behind
    // it. Plain-text inputs (no JSON ts / level / fields) collapse the
    // whole row to a wide raw column — like `less`, but with line numbers
    // and the cursor we already have.
    let show_ts = engine.has_timestamps();
    let show_level = engine.has_severity();
    let show_columns = engine.has_structured_fields();

    let fixed_w = COL_LINE_W as u16
        + 1
        + if show_ts { COL_TS_W as u16 + 1 } else { 0 }
        + if show_level { COL_LV_W as u16 + 1 } else { 0 };
    let inner_w = inner.width.saturating_sub(fixed_w);
    let auto_cols = if show_columns {
        let max_auto = (inner_w / (COL_FIELD_W as u16 + 1)) as usize;
        engine
            .schema
            .as_ref()
            .map(|s| s.auto_columns(max_auto.saturating_sub(1).max(0)))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let raw_w = inner_w
        .saturating_sub((auto_cols.len() * (COL_FIELD_W + 1)) as u16)
        as usize;

    let start = lower_bound(filtered_view, scroll_top);
    let visible = inner.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(visible.saturating_add(1));

    // Header row.
    let mut header_spans = vec![Span::styled(
        format!("{:>1$}", "line", COL_LINE_W),
        Style::default().add_modifier(Modifier::DIM),
    )];
    if show_ts {
        header_spans.push(Span::raw(" "));
        header_spans.push(Span::styled(
            format!("{:<1$}", "timestamp", COL_TS_W),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if show_level {
        header_spans.push(Span::raw(" "));
        header_spans.push(Span::styled(
            format!("{:<1$}", "level", COL_LV_W),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    for col in &auto_cols {
        header_spans.push(Span::raw(" "));
        header_spans.push(Span::styled(
            truncate_padded(col.as_str(), COL_FIELD_W),
            Style::default().add_modifier(Modifier::DIM | Modifier::BOLD),
        ));
    }
    if raw_w > 0 {
        header_spans.push(Span::raw(" "));
        let raw_label = if !show_ts && !show_level && auto_cols.is_empty() {
            "(plain text)"
        } else {
            "raw"
        };
        header_spans.push(Span::styled(
            truncate_padded(raw_label, raw_w),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    lines.push(Line::from(header_spans));

    let body_visible = visible.saturating_sub(1);
    for i in start..start.saturating_add(body_visible).min(filtered_view.len()) {
        let line_id = filtered_view[i];
        let sev = engine.indexes.severity.get(line_id).unwrap_or(0);
        let ts = engine.indexes.time.get(line_id).unwrap_or(TS_UNTIMED);
        let bytes = engine.line_bytes(line_id);

        let row_style = if line_id == cursor {
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let mut spans = vec![Span::styled(
            format!("{:>1$}", line_id, COL_LINE_W),
            Style::default().fg(Color::DarkGray),
        )];
        if show_ts {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format_ts_utc(ts), severity_style(sev)));
        }
        if show_level {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{:<1$}", severity::name(sev), COL_LV_W),
                severity_style(sev).add_modifier(Modifier::BOLD),
            ));
        }
        for col in &auto_cols {
            spans.push(Span::raw(" "));
            let value = project_field(bytes, col.as_str())
                .map(unquote_if_string)
                .unwrap_or("");
            spans.push(Span::raw(truncate_padded(value, COL_FIELD_W)));
        }
        if raw_w > 0 {
            let raw = String::from_utf8_lossy(bytes);
            let raw_trunc = if raw.len() > raw_w { &raw[..raw_w] } else { &raw[..] };
            spans.push(Span::raw(" "));
            spans.push(Span::raw(raw_trunc.to_string()));
        }
        lines.push(Line::from(spans).style(row_style));
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

/// Truncate or right-pad a string to exactly `width` columns. Inputs are
/// assumed to be ASCII / single-width; multi-byte handling is M3 polish.
fn truncate_padded(s: &str, width: usize) -> String {
    if s.len() >= width {
        // Truncate on a char boundary to avoid panicking on multi-byte input.
        let mut end = width;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        s[..end].to_string()
    } else {
        let mut out = String::with_capacity(width);
        out.push_str(s);
        for _ in s.len()..width {
            out.push(' ');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_bound_finds_lower_bound() {
        let v = vec![0u64, 2, 5, 10, 11];
        assert_eq!(lower_bound(&v, 0), 0);
        assert_eq!(lower_bound(&v, 1), 1);
        assert_eq!(lower_bound(&v, 5), 2);
        assert_eq!(lower_bound(&v, 12), 5);
    }

    #[test]
    fn snap_stays_on_survivor_when_possible() {
        let v = vec![1u64, 4, 7, 9];
        assert_eq!(snap_cursor(&v, 4), Some(4));
        // 5 evicted, snap to nearest <=
        assert_eq!(snap_cursor(&v, 5), Some(4));
        // Below everyone, snap to smallest >=
        assert_eq!(snap_cursor(&v, 0), Some(1));
        // Above everyone, snap to largest <=
        assert_eq!(snap_cursor(&v, 99), Some(9));
        assert_eq!(snap_cursor(&[], 5), None);
    }
}
