//! Application loop: ratatui terminal, event dispatch, viewmodel.
//!
//! ViewModel owns:
//! - filter set (`Vec<Box<dyn Predicate>>` mirrored from engine)
//! - cursor as a `line_id` (logical), never as a row index in `filtered_view`
//! - scroll_top as a `line_id`
//! - follow-mode flag (stick-to-bottom; separate from one-shot `G`)
//!
//! When the cursor's `line_id` is evicted by a filter change, snap to the
//! closest surviving line_id (largest `<=` old, else smallest `>=`). Never
//! reset to top — that would feel like a glitch.

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use mgi_pulse_core::engine::predicate::{
    AndPredicate, FieldEqualsPredicate, Predicate, RegexBytesPredicate,
};
use mgi_pulse_core::engine::{query, Engine};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;

use crate::panes::{detail, table};

/// Input mode for the prompt at the bottom of the screen.
#[derive(Debug, Clone)]
pub enum Input {
    Search(String),
    /// `f` field=value filter. The buffer is the in-progress text.
    Filter(String),
}

pub struct App {
    pub engine: Engine,
    /// Sorted list of line_ids that survive the current filter set. M1 has
    /// no filters; this is `0..len()` materialized.
    pub filtered_view: Vec<u64>,
    pub cursor: u64,
    pub scroll_top: u64,
    pub input: Option<Input>,
    pub status_msg: String,
    pub source_label: String,
    pub detail_open: bool,
    /// Active filters. Search predicate (regex) sits at index 0 when present;
    /// field-equals filters accumulate. AND-composition.
    pub regex_filter: Option<String>,
    pub field_filters: Vec<(String, String)>,
}

impl App {
    pub fn new(engine: Engine, source_label: String) -> Self {
        let total = engine.indexes.len() as u64;
        let filtered_view = (0..total).collect();
        let ps = engine.indexes.parse_stats;
        let status_msg = if total == 0 {
            "no records loaded".to_string()
        } else if ps.untimed > 0 {
            format!(
                "{} records loaded · {} untimed (ts missing/bad: {}/{})",
                total,
                ps.untimed,
                ps.untimed - ps.ts_parse_errors,
                ps.ts_parse_errors
            )
        } else {
            format!("{} records loaded", total)
        };
        Self {
            engine,
            filtered_view,
            cursor: 0,
            scroll_top: 0,
            input: None,
            status_msg,
            source_label,
            detail_open: false,
            regex_filter: None,
            field_filters: Vec::new(),
        }
    }

    pub fn move_cursor(&mut self, delta: i64) {
        if self.filtered_view.is_empty() {
            return;
        }
        let i = table::lower_bound(&self.filtered_view, self.cursor) as i64;
        let new_i = (i + delta).clamp(0, self.filtered_view.len() as i64 - 1) as usize;
        self.cursor = self.filtered_view[new_i];
        self.ensure_cursor_visible(20);
    }

    pub fn cursor_to_start(&mut self) {
        if let Some(&first) = self.filtered_view.first() {
            self.cursor = first;
            self.scroll_top = first;
        }
    }

    pub fn cursor_to_end(&mut self) {
        if let Some(&last) = self.filtered_view.last() {
            self.cursor = last;
            // Aim for the cursor to sit a few rows from the bottom by setting
            // scroll_top a window up. Use 20 as a placeholder since we don't
            // have the actual height here.
            let i = self.filtered_view.len().saturating_sub(20);
            self.scroll_top = self.filtered_view[i];
        }
    }

    /// Keep cursor inside the visible window, where the window height was
    /// passed in. Approximation: assumes the height; UI re-aligns on next draw.
    pub fn ensure_cursor_visible(&mut self, visible: u16) {
        if self.filtered_view.is_empty() {
            return;
        }
        let top_i = table::lower_bound(&self.filtered_view, self.scroll_top);
        let cur_i = table::lower_bound(&self.filtered_view, self.cursor);
        let window = visible.max(1) as usize;
        if cur_i < top_i {
            self.scroll_top = self.cursor;
        } else if cur_i >= top_i + window {
            let new_top_i = cur_i + 1 - window;
            self.scroll_top = self.filtered_view[new_top_i];
        }
    }

    /// Rebuild `filtered_view` from the current filter set. The set is the
    /// AND of `regex_filter` (if any) and every entry in `field_filters`.
    pub fn rebuild_view(&mut self) {
        if self.regex_filter.is_none() && self.field_filters.is_empty() {
            self.filtered_view = (0..self.engine.indexes.len() as u64).collect();
            self.status_msg = format!("{} records (no filter)", self.filtered_view.len());
        } else {
            let mut and = AndPredicate::new();
            if let Some(re) = &self.regex_filter {
                match RegexBytesPredicate::new(re) {
                    Ok(p) => and.push(Box::new(p)),
                    Err(e) => {
                        self.status_msg = format!("regex error: {}", e);
                        return;
                    }
                }
            }
            for (k, v) in &self.field_filters {
                and.push(Box::new(FieldEqualsPredicate::new(k.clone(), v.clone())));
            }
            let predicate: Box<dyn Predicate> = Box::new(and);
            let hits = query::scan(&self.engine, predicate.as_ref());
            let mut parts: Vec<String> = Vec::new();
            if let Some(re) = &self.regex_filter {
                parts.push(format!("/{}/", re));
            }
            for (k, v) in &self.field_filters {
                parts.push(format!("{}={}", k, v));
            }
            self.status_msg = format!(
                "{}  {} matches of {}",
                parts.join(" & "),
                hits.len(),
                self.engine.indexes.len()
            );
            self.filtered_view = hits;
        }
        if let Some(snapped) = table::snap_cursor(&self.filtered_view, self.cursor) {
            self.cursor = snapped;
            self.scroll_top = snapped;
        }
    }

    pub fn set_regex(&mut self, pattern: &str) {
        self.regex_filter = if pattern.is_empty() {
            None
        } else {
            Some(pattern.to_string())
        };
        self.rebuild_view();
    }

    pub fn add_field_filter(&mut self, raw: &str) {
        // Parse `field=value`. Anything else → status error, no filter.
        let Some((field, value)) = raw.split_once('=') else {
            self.status_msg = format!("filter must be 'field=value', got '{}'", raw);
            return;
        };
        let field = field.trim();
        let value = value.trim();
        if field.is_empty() {
            self.status_msg = "filter field is empty".to_string();
            return;
        }
        self.field_filters
            .push((field.to_string(), value.to_string()));
        self.rebuild_view();
    }

    pub fn clear_filters(&mut self) {
        self.regex_filter = None;
        self.field_filters.clear();
        self.rebuild_view();
    }
}

pub fn run(mut app: App) -> Result<()> {
    let mut stdout = io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app);

    // Always restore the terminal, even on error.
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    );
    let _ = terminal.show_cursor();

    result
}

fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|f| {
            let area = f.area();
            let outer = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(1)])
                .split(area);

            let title = format!(
                "mgi-pulse  {}  [{} rows]",
                app.source_label,
                app.filtered_view.len()
            );

            if app.detail_open {
                let split = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .split(outer[0]);
                table::render(
                    f,
                    split[0],
                    &app.engine,
                    &app.filtered_view,
                    app.scroll_top,
                    app.cursor,
                    &title,
                );
                detail::render(f, split[1], &app.engine, app.cursor);
            } else {
                table::render(
                    f,
                    outer[0],
                    &app.engine,
                    &app.filtered_view,
                    app.scroll_top,
                    app.cursor,
                    &title,
                );
            }

            let prompt = match &app.input {
                Some(Input::Search(s)) => format!("/ {}_", s),
                Some(Input::Filter(s)) => format!("f {}_", s),
                None => app.status_msg.clone(),
            };
            let status = Paragraph::new(Line::from(vec![
                Span::styled(prompt, Style::default().add_modifier(Modifier::DIM)),
                Span::raw("  "),
                Span::styled(
                    "q quit  / regex  f field=val  Tab detail  Esc clear  ↑↓ PgUp PgDn g G",
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
            .block(Block::default().borders(Borders::NONE));
            f.render_widget(status, outer[1]);
        })?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let evt = event::read()?;
        match evt {
            Event::Key(k) => {
                if let Some(input) = app.input.as_mut() {
                    match (input, k.code) {
                        (_, KeyCode::Esc) => {
                            app.input = None;
                        }
                        (Input::Search(buf), KeyCode::Enter) => {
                            let pattern = buf.clone();
                            app.input = None;
                            app.set_regex(&pattern);
                        }
                        (Input::Filter(buf), KeyCode::Enter) => {
                            let raw = buf.clone();
                            app.input = None;
                            app.add_field_filter(&raw);
                        }
                        (Input::Search(buf) | Input::Filter(buf), KeyCode::Backspace) => {
                            buf.pop();
                        }
                        (Input::Search(buf) | Input::Filter(buf), KeyCode::Char(c)) => {
                            buf.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Char('q'), _) => break,
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                    (KeyCode::Char('/'), _) => {
                        app.input = Some(Input::Search(String::new()));
                    }
                    (KeyCode::Char('f'), _) => {
                        app.input = Some(Input::Filter(String::new()));
                    }
                    (KeyCode::Tab, _) => {
                        app.detail_open = !app.detail_open;
                    }
                    (KeyCode::Esc, _) => {
                        app.clear_filters();
                    }
                    (KeyCode::Up, _) => app.move_cursor(-1),
                    (KeyCode::Down, _) => app.move_cursor(1),
                    (KeyCode::PageUp, _) => app.move_cursor(-20),
                    (KeyCode::PageDown, _) => app.move_cursor(20),
                    (KeyCode::Char('g'), _) => app.cursor_to_start(),
                    (KeyCode::Char('G'), _) => app.cursor_to_end(),
                    _ => {}
                }
            }
            Event::Resize(_, _) => {
                // Next draw will pick up the new size.
            }
            _ => {}
        }
    }
    Ok(())
}
