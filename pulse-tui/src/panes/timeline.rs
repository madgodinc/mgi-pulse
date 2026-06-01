//! TimelinePane — overview histogram of record arrival over time.
//!
//! v0.1 ships a non-interactive overview: severity-colored bars spanning the
//! full indexed time range, painted across the available width. Keyboard
//! scrub (`<`, `>`, zoom) is v0.2 — held back until a week of personal use
//! settles the keyboard model.

use mgi_pulse_core::engine::histogram::Histogram;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

const BAR_CHARS: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];

// Bar colours now come from `crate::theme::Theme::histogram_bar`.

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
///
/// Layout: each visible bar gets one column. The bar is split into up to
/// three vertically-stacked bands so that error / warn / info+debug all
/// stay visible even when one severity dwarfs the others on the same bin.
/// Each band is normalised against its own peak across the row, not the
/// overall total — otherwise on a 1.2M-error / 9.8M-info dataset the warn
/// band would always render as one pixel.
pub fn render(f: &mut Frame, area: Rect, h: &Histogram, theme: crate::theme::Theme) {
    let block = Block::default().title(" timeline ").borders(Borders::ALL);
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

    use mgi_pulse_core::engine::record::severity;

    let bar_steps = (BAR_CHARS.len() - 1) as u64;

    // One peak per severity band. Independent normalisation lets less-frequent
    // severities still show outlines on a dataset dominated by another level.
    // Bands (top → bottom): error+fatal, warn, info, debug+trace.
    let mut peak_err: u64 = 0;
    let mut peak_warn: u64 = 0;
    let mut peak_info: u64 = 0;
    let mut peak_dbg: u64 = 0;
    for bin in &h.bins {
        let err = bin.error + bin.fatal;
        let warn = bin.warn;
        let info = bin.info;
        let dbg = bin.debug + bin.trace;
        if err > peak_err {
            peak_err = err;
        }
        if warn > peak_warn {
            peak_warn = warn;
        }
        if info > peak_info {
            peak_info = info;
        }
        if dbg > peak_dbg {
            peak_dbg = dbg;
        }
    }
    let peak_err = peak_err.max(1);
    let peak_warn = peak_warn.max(1);
    let peak_info = peak_info.max(1);
    let peak_dbg = peak_dbg.max(1);

    // Reserve one row for the time labels at the bottom; the rest are
    // band rows. Bands assigned top → bottom in worst-attention order
    // (err on top so the eye finds it first), with graceful collapse
    // when the timeline is squeezed vertically.
    let total_h = inner.height as usize;
    let label_rows = 1usize;
    let band_rows = total_h.saturating_sub(label_rows).max(1);

    // Assignment: top band always = err; if there's room, the rest fill in
    // descending priority warn / info / debug. With 4 band rows each
    // severity gets exactly one row.
    let (err_row, warn_row, info_row, dbg_row): (
        Option<usize>,
        Option<usize>,
        Option<usize>,
        Option<usize>,
    ) = match band_rows {
        1 => (Some(0), None, None, None),
        2 => (Some(0), Some(1), None, None),
        3 => (Some(0), Some(1), Some(2), None),
        _ => (Some(0), Some(1), Some(2), Some(band_rows - 1)),
    };

    let style_err = theme.histogram_bar(severity::ERROR);
    let style_warn = theme.histogram_bar(severity::WARN);
    let style_info = theme.histogram_bar(severity::INFO);
    let style_dbg = theme.histogram_bar(severity::DEBUG);

    let mut rows: Vec<Vec<Span>> = vec![Vec::with_capacity(h.bins.len()); band_rows];
    for bin in &h.bins {
        let err = bin.error + bin.fatal;
        let warn = bin.warn;
        let info = bin.info;
        let dbg = bin.debug + bin.trace;

        let frac_err = (err * bar_steps) / peak_err;
        let frac_warn = (warn * bar_steps) / peak_warn;
        let frac_info = (info * bar_steps) / peak_info;
        let frac_dbg = (dbg * bar_steps) / peak_dbg;

        let ch_err = BAR_CHARS[frac_err.min(bar_steps) as usize];
        let ch_warn = BAR_CHARS[frac_warn.min(bar_steps) as usize];
        let ch_info = BAR_CHARS[frac_info.min(bar_steps) as usize];
        let ch_dbg = BAR_CHARS[frac_dbg.min(bar_steps) as usize];

        for (row_idx, row) in rows.iter_mut().enumerate().take(band_rows) {
            let (ch, style) = if Some(row_idx) == err_row {
                (ch_err, style_err)
            } else if Some(row_idx) == warn_row {
                (ch_warn, style_warn)
            } else if Some(row_idx) == info_row {
                (ch_info, style_info)
            } else if Some(row_idx) == dbg_row {
                (ch_dbg, style_dbg)
            } else {
                (' ', Style::default())
            };
            row.push(Span::styled(ch.to_string(), style));
        }
    }

    let label_left = format_micros_short(h.t_min);
    let label_right = format_micros_short(h.t_max);
    let mut center_summary = String::new();
    if h.untimed > 0 {
        center_summary = format!(" · {} untimed", h.untimed);
    }

    let mut lines: Vec<Line> = rows.into_iter().map(Line::from).collect();
    lines.push(Line::from(vec![
        Span::styled(label_left, Style::default().add_modifier(Modifier::DIM)),
        Span::raw(" "),
        Span::styled(center_summary, Style::default().add_modifier(Modifier::DIM)),
        Span::raw(" "),
        Span::styled(label_right, Style::default().add_modifier(Modifier::DIM)),
    ]));

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}
