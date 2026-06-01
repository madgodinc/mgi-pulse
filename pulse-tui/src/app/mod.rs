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
use mgi_pulse_core::engine::predicate::RegexBytesPredicate;
use mgi_pulse_core::engine::{query, Engine};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;

use crate::panes::table;

pub struct App {
    pub engine: Engine,
    /// Sorted list of line_ids that survive the current filter set. M1 has
    /// no filters; this is `0..len()` materialized.
    pub filtered_view: Vec<u64>,
    pub cursor: u64,
    pub scroll_top: u64,
    pub search_input: Option<String>,
    pub status_msg: String,
    pub source_label: String,
}

impl App {
    pub fn new(engine: Engine, source_label: String) -> Self {
        let total = engine.indexes.len() as u64;
        let filtered_view = (0..total).collect();
        let status_msg = if total == 0 {
            "no records loaded".to_string()
        } else {
            format!("{} records loaded", total)
        };
        Self {
            engine,
            filtered_view,
            cursor: 0,
            scroll_top: 0,
            search_input: None,
            status_msg,
            source_label,
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

    pub fn apply_search(&mut self, pattern: &str) {
        if pattern.is_empty() {
            self.filtered_view = (0..self.engine.indexes.len() as u64).collect();
            self.status_msg = format!("{} records (no filter)", self.filtered_view.len());
        } else {
            match RegexBytesPredicate::new(pattern) {
                Ok(p) => {
                    let hits = query::scan(&self.engine, &p);
                    self.status_msg = format!(
                        "/{}/  {} matches of {}",
                        pattern,
                        hits.len(),
                        self.engine.indexes.len()
                    );
                    self.filtered_view = hits;
                }
                Err(e) => {
                    self.status_msg = format!("regex error: {}", e);
                    return;
                }
            }
        }
        // Snap cursor onto the new view.
        if let Some(snapped) = table::snap_cursor(&self.filtered_view, self.cursor) {
            self.cursor = snapped;
            self.scroll_top = snapped;
        }
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
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(1)])
                .split(area);

            let title = format!(
                "mgi-pulse  {}  [{} rows]",
                app.source_label,
                app.filtered_view.len()
            );
            table::render(
                f,
                chunks[0],
                &app.engine,
                &app.filtered_view,
                app.scroll_top,
                app.cursor,
                &title,
            );

            let prompt = match &app.search_input {
                Some(s) => format!("/ {}_", s),
                None => app.status_msg.clone(),
            };
            let status = Paragraph::new(Line::from(vec![
                Span::styled(prompt, Style::default().add_modifier(Modifier::DIM)),
                Span::raw("  "),
                Span::styled(
                    "q quit  / search  Esc clear  ↑↓ PgUp PgDn g G",
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
            .block(Block::default().borders(Borders::NONE));
            f.render_widget(status, chunks[1]);
        })?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let evt = event::read()?;
        match evt {
            Event::Key(k) => {
                if let Some(ref mut buf) = app.search_input {
                    match k.code {
                        KeyCode::Esc => {
                            app.search_input = None;
                        }
                        KeyCode::Enter => {
                            let pattern = buf.clone();
                            app.search_input = None;
                            app.apply_search(&pattern);
                        }
                        KeyCode::Backspace => {
                            buf.pop();
                        }
                        KeyCode::Char(c) => {
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
                        app.search_input = Some(String::new());
                    }
                    (KeyCode::Esc, _) => {
                        app.apply_search("");
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
