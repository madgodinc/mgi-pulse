//! Application loop: ratatui terminal, event dispatch, viewmodel.
//!
//! The app holds several **views** (tabs). Each view is an independent
//! filtered window over the same underlying engine: own predicate stack,
//! own cursor, own scroll, own detail-pane toggle, own cached histogram.
//! Tab / Shift-Tab switch between them; Ctrl-T opens a new one (always
//! starts as "All", no filters carried over — predictable); Ctrl-W closes
//! the current one (and the binary exits when the last one is closed).
//!
//! Cursor and scroll_top are always logical `line_id`s, never row indexes
//! in `filtered_view`. When a filter change evicts the cursor's line_id,
//! we snap to the closest surviving one — never reset to top.

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyModifiers, MouseEvent, MouseEventKind,
};
use mgi_pulse_core::engine::histogram::Histogram;
use mgi_pulse_core::engine::predicate::{
    AndPredicate, FieldEqualsPredicate, Predicate, RegexBytesPredicate,
    SeverityInPredicate,
};
use mgi_pulse_core::engine::record::severity;
use mgi_pulse_core::engine::{query, Engine};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;

use crate::panes::{detail, table, timeline};

/// Given a set of severity levels, return the same set plus every level
/// ranked above the highest one. Used by Min-mode: choosing `Warn` with
/// Min-mode on means "warn + everything above warn".
fn expand_min(levels: &[u8]) -> Vec<u8> {
    let Some(&min) = levels.iter().min() else {
        return Vec::new();
    };
    let mut out: Vec<u8> = (min..=severity::FATAL).collect();
    // Honor the original set too, in case someone passes a higher level by
    // mistake (e.g. {ERROR, FATAL} stays the same under Min mode).
    for &l in levels {
        if !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

/// Input mode for the prompt at the bottom of the screen.
#[derive(Debug, Clone)]
pub enum Input {
    Search(String),
    Filter(String),
}

/// One tab's worth of state. Everything that can differ between tabs lives
/// here; everything shared (the engine, the source label) stays on `App`.
pub struct View {
    pub title: String,
    pub filtered_view: Vec<u64>,
    pub cursor: u64,
    pub scroll_top: u64,
    pub regex_filter: Option<String>,
    pub field_filters: Vec<(String, String)>,
    /// Severity set to show; empty = no severity filter. Set at tab
    /// creation for the per-severity tabs, and modifiable on any tab via
    /// keys 1-5 / 0.
    ///
    /// When `severity_min_mode` is true, the set is expanded at filter time
    /// to "this and above" — useful for the lnav-style "show me warn+" flow.
    /// The stored set itself is unchanged, so flipping the mode back snaps to
    /// the original strict choice.
    pub severity_levels: Vec<u8>,
    /// Min-mode toggle: if true, every selected severity counts as itself
    /// AND everything ranking above it. Per-view, defaults to false (Strict).
    pub severity_min_mode: bool,
    /// Human-readable label for the current severity selection (e.g. "Error",
    /// "Warn"). Empty when no severity filter is active. Used by the title
    /// and the status bar.
    pub severity_label: String,
    pub detail_open: bool,
    pub status_msg: String,
    pub histogram_cache: Option<(usize, u16, Histogram)>,
}

impl View {
    fn new_all(engine: &Engine) -> Self {
        let total = engine.indexes.len() as u64;
        let filtered_view = (0..total).collect();
        let ps = engine.indexes.parse_stats;
        let status_msg = if total == 0 {
            "no records loaded".to_string()
        } else if ps.untimed > 0 {
            format!(
                "{} records · {} untimed",
                total, ps.untimed,
            )
        } else {
            format!("{} records", total)
        };
        Self {
            title: "All".to_string(),
            filtered_view,
            cursor: 0,
            scroll_top: 0,
            regex_filter: None,
            field_filters: Vec::new(),
            severity_levels: Vec::new(),
            severity_min_mode: false,
            severity_label: String::new(),
            detail_open: false,
            status_msg,
            histogram_cache: None,
        }
    }

    /// Create a view pre-filtered to a specific severity set (e.g. just
    /// `Error+Fatal`, or just `Warn`). Used for the per-severity tabs at
    /// startup.
    fn new_with_severity(engine: &Engine, label: &str, levels: &[u8]) -> Self {
        let mut v = Self::new_all(engine);
        v.title = label.to_string();
        v.severity_label = label.to_string();
        v.severity_levels = levels.to_vec();
        v.rebuild_view(engine);
        v
    }

    fn move_cursor(&mut self, delta: i64) {
        if self.filtered_view.is_empty() {
            return;
        }
        let i = table::lower_bound(&self.filtered_view, self.cursor) as i64;
        let new_i = (i + delta).clamp(0, self.filtered_view.len() as i64 - 1) as usize;
        self.cursor = self.filtered_view[new_i];
        self.ensure_cursor_visible(20);
    }

    fn cursor_to_start(&mut self) {
        if let Some(&first) = self.filtered_view.first() {
            self.cursor = first;
            self.scroll_top = first;
        }
    }

    fn cursor_to_end(&mut self) {
        if let Some(&last) = self.filtered_view.last() {
            self.cursor = last;
            let i = self.filtered_view.len().saturating_sub(20);
            self.scroll_top = self.filtered_view[i];
        }
    }

    fn ensure_cursor_visible(&mut self, visible: u16) {
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

    fn rebuild_view(&mut self, engine: &Engine) {
        let has_filter = self.regex_filter.is_some()
            || !self.field_filters.is_empty()
            || !self.severity_levels.is_empty();

        if !has_filter {
            self.filtered_view = (0..engine.indexes.len() as u64).collect();
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
            if !self.severity_levels.is_empty() {
                let effective = if self.severity_min_mode {
                    expand_min(&self.severity_levels)
                } else {
                    self.severity_levels.clone()
                };
                and.push(Box::new(SeverityInPredicate::new(&effective)));
            }
            let predicate: Box<dyn Predicate> = Box::new(and);
            let hits = query::scan(engine, predicate.as_ref());
            let mut parts: Vec<String> = Vec::new();
            if !self.severity_label.is_empty() {
                if self.severity_min_mode {
                    parts.push(format!("{}+", self.severity_label));
                } else {
                    parts.push(self.severity_label.clone());
                }
            }
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
                engine.indexes.len()
            );
            self.filtered_view = hits;
        }
        if let Some(snapped) = table::snap_cursor(&self.filtered_view, self.cursor) {
            self.cursor = snapped;
            self.scroll_top = snapped;
        }
        self.histogram_cache = None;
    }

    fn set_severity(&mut self, label: &str, levels: &[u8], engine: &Engine) {
        self.severity_label = label.to_string();
        self.severity_levels = levels.to_vec();
        self.rebuild_view(engine);
    }

    fn clear_severity(&mut self, engine: &Engine) {
        self.severity_label.clear();
        self.severity_levels.clear();
        self.rebuild_view(engine);
    }

    fn toggle_severity_mode(&mut self, engine: &Engine) {
        self.severity_min_mode = !self.severity_min_mode;
        self.rebuild_view(engine);
    }

    fn set_regex(&mut self, pattern: &str, engine: &Engine) {
        self.regex_filter = if pattern.is_empty() {
            None
        } else {
            Some(pattern.to_string())
        };
        self.rebuild_view(engine);
    }

    fn add_field_filter(&mut self, raw: &str, engine: &Engine) {
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
        self.rebuild_view(engine);
    }

    fn clear_filters(&mut self, engine: &Engine) {
        self.regex_filter = None;
        self.field_filters.clear();
        self.severity_levels.clear();
        self.severity_label.clear();
        self.rebuild_view(engine);
    }

    /// Get (and cache) the histogram for the timeline pane.
    fn histogram(&mut self, engine: &Engine, bars: u16) -> &Histogram {
        let view_len = self.filtered_view.len();
        let needs_build = match &self.histogram_cache {
            Some((cached_len, cached_w, _)) => *cached_len != view_len || *cached_w != bars,
            None => true,
        };
        if needs_build {
            let h = Histogram::build_over(engine, &self.filtered_view, bars as usize);
            self.histogram_cache = Some((view_len, bars, h));
        }
        &self.histogram_cache.as_ref().unwrap().2
    }
}

pub struct App {
    pub engine: Engine,
    pub source_label: String,
    pub views: Vec<View>,
    pub active_tab: usize,
    pub input: Option<Input>,
}

impl App {
    pub fn new(engine: Engine, source_label: String) -> Self {
        // Default tab set: All, then one tab per severity bucket. Each is a
        // SeverityMinPredicate, so "Error" means error+fatal, "Warn" means
        // warn+ above, etc. Keeps the temporal order intact within each tab.
        // Per-severity tabs only make sense when at least one record had a
        // parseable level. Plain-text logs (Clojure log4j defaults, raw
        // stdout) get only `All` — extra tabs would be empty and confusing.
        let views = if engine.has_severity() {
            vec![
                View::new_all(&engine),
                View::new_with_severity(
                    &engine,
                    "Error",
                    &[severity::ERROR, severity::FATAL],
                ),
                View::new_with_severity(&engine, "Warn", &[severity::WARN]),
                View::new_with_severity(&engine, "Info", &[severity::INFO]),
                View::new_with_severity(
                    &engine,
                    "Debug",
                    &[severity::DEBUG, severity::TRACE],
                ),
            ]
        } else {
            vec![View::new_all(&engine)]
        };
        Self {
            engine,
            source_label,
            views,
            active_tab: 0,
            input: None,
        }
    }

    fn active(&mut self) -> &mut View {
        &mut self.views[self.active_tab]
    }

    fn active_ref(&self) -> &View {
        &self.views[self.active_tab]
    }

    fn next_tab(&mut self) {
        if self.views.len() > 1 {
            self.active_tab = (self.active_tab + 1) % self.views.len();
        }
    }

    fn prev_tab(&mut self) {
        if self.views.len() > 1 {
            self.active_tab = if self.active_tab == 0 {
                self.views.len() - 1
            } else {
                self.active_tab - 1
            };
        }
    }

    fn open_tab(&mut self) {
        let view = View::new_all(&self.engine);
        self.views.push(view);
        self.active_tab = self.views.len() - 1;
    }

    /// Close the active tab. Returns true if the app should quit (last tab
    /// just closed).
    fn close_tab(&mut self) -> bool {
        if self.views.len() <= 1 {
            return true;
        }
        self.views.remove(self.active_tab);
        if self.active_tab >= self.views.len() {
            self.active_tab = self.views.len() - 1;
        }
        false
    }
}

pub fn run(mut app: App) -> Result<()> {
    let mut stdout = io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app);

    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
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
        // Ensure the active view's histogram is built before the closure
        // takes an immutable borrow of app. Borrow gymnastics: split the
        // engine reference out so the view can take a `&mut` independently.
        let term_w = terminal.size()?.width;
        let bars = term_w.saturating_sub(2);
        {
            let App { engine, views, active_tab, .. } = app;
            let _ = views[*active_tab].histogram(engine, bars);
        }

        terminal.draw(|f| {
            let area = f.area();

            // Layout adapts to what the data actually carries. Hide the
            // tab bar when there's only one tab (plain-text input → just
            // `All`); hide the timeline when no record had a parseable ts.
            let show_tabs = app.views.len() > 1;
            let show_timeline = app.engine.has_timestamps();
            let mut constraints: Vec<Constraint> = Vec::with_capacity(4);
            if show_tabs { constraints.push(Constraint::Length(1)); }
            if show_timeline { constraints.push(Constraint::Length(4)); }
            constraints.push(Constraint::Min(3));
            constraints.push(Constraint::Length(1));
            let outer = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);

            // Resolve which slot each pane occupies.
            let mut slot = 0usize;
            let tabs_slot = if show_tabs { let s = slot; slot += 1; Some(s) } else { None };
            let timeline_slot = if show_timeline { let s = slot; slot += 1; Some(s) } else { None };
            let table_slot = slot;
            let status_slot = slot + 1;

            // Tabs bar. Each tab shows `Title` (strict) or `Title+` (min mode
            // expanded — Warn+ means warn AND everything above).
            if let Some(slot) = tabs_slot {
                let mut tab_spans: Vec<Span> = Vec::with_capacity(app.views.len() * 3);
                for (i, v) in app.views.iter().enumerate() {
                    if i > 0 {
                        tab_spans.push(Span::raw(" "));
                    }
                    let suffix = if v.severity_min_mode && !v.severity_levels.is_empty() {
                        "+"
                    } else {
                        ""
                    };
                    let label = format!(" {}{} ", v.title, suffix);
                    let style = if i == app.active_tab {
                        Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    tab_spans.push(Span::styled(label, style));
                }
                let tabs_line = Paragraph::new(Line::from(tab_spans));
                f.render_widget(tabs_line, outer[slot]);
            }

            // Timeline.
            let v = &app.views[app.active_tab];
            if let Some(slot) = timeline_slot {
                if let Some((_, _, h)) = &v.histogram_cache {
                    timeline::render(f, outer[slot], h);
                }
            }

            // Table area (and detail when open).
            let title = format!(
                "mgi-pulse  {}  [{} rows]",
                app.source_label,
                v.filtered_view.len()
            );
            if v.detail_open {
                let split = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .split(outer[table_slot]);
                table::render(
                    f,
                    split[0],
                    &app.engine,
                    &v.filtered_view,
                    v.scroll_top,
                    v.cursor,
                    &title,
                );
                detail::render(f, split[1], &app.engine, v.cursor);
            } else {
                table::render(
                    f,
                    outer[table_slot],
                    &app.engine,
                    &v.filtered_view,
                    v.scroll_top,
                    v.cursor,
                    &title,
                );
            }

            // Status bar.
            let prompt = match &app.input {
                Some(Input::Search(s)) => format!("/ {}_", s),
                Some(Input::Filter(s)) => format!("f {}_", s),
                None => v.status_msg.clone(),
            };
            let status = Paragraph::new(Line::from(vec![
                Span::styled(prompt, Style::default().add_modifier(Modifier::DIM)),
                Span::raw("  "),
                Span::styled(
                    if show_tabs {
                        "q quit · / regex · f field=val · 1-4 severity · m min-mode · d detail · Tab next · Esc clear"
                    } else {
                        // Plain-text fallback: only the keys that actually do
                        // anything are advertised.
                        "q quit · / regex · ↑↓ PgUp PgDn g G · Esc clear"
                    },
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
            .block(Block::default().borders(Borders::NONE));
            f.render_widget(status, outer[status_slot]);
        })?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }

        let mut wheel_delta: i64 = 0;
        let mut should_break = false;
        loop {
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
                                let engine = &app.engine;
                                app.views[app.active_tab].set_regex(&pattern, engine);
                            }
                            (Input::Filter(buf), KeyCode::Enter) => {
                                let raw = buf.clone();
                                app.input = None;
                                let engine = &app.engine;
                                app.views[app.active_tab].add_field_filter(&raw, engine);
                            }
                            (
                                Input::Search(buf) | Input::Filter(buf),
                                KeyCode::Backspace,
                            ) => {
                                buf.pop();
                            }
                            (
                                Input::Search(buf) | Input::Filter(buf),
                                KeyCode::Char(c),
                            ) => {
                                buf.push(c);
                            }
                            _ => {}
                        }
                    } else {
                        match (k.code, k.modifiers) {
                            (KeyCode::Char('q'), _) => should_break = true,
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                should_break = true
                            }
                            (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                                app.open_tab();
                            }
                            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                                if app.close_tab() {
                                    should_break = true;
                                }
                            }
                            (KeyCode::Tab, m) if m.contains(KeyModifiers::SHIFT) => {
                                app.prev_tab();
                            }
                            (KeyCode::BackTab, _) => {
                                app.prev_tab();
                            }
                            (KeyCode::Tab, _) => {
                                app.next_tab();
                            }
                            (KeyCode::Char('/'), _) => {
                                app.input = Some(Input::Search(String::new()));
                            }
                            (KeyCode::Char('f'), _) => {
                                app.input = Some(Input::Filter(String::new()));
                            }
                            (KeyCode::Char('d'), _) => {
                                let v = app.active();
                                v.detail_open = !v.detail_open;
                            }
                            (KeyCode::Char('m'), _) => {
                                let engine = &app.engine;
                                app.views[app.active_tab].toggle_severity_mode(engine);
                            }
                            // Quick severity filters on the active tab.
                            // Strict by default; `m` toggles Min mode.
                            (KeyCode::Char('0'), _) => {
                                let engine = &app.engine;
                                app.views[app.active_tab].clear_severity(engine);
                            }
                            (KeyCode::Char('1'), _) => {
                                let engine = &app.engine;
                                app.views[app.active_tab].set_severity(
                                    "Error",
                                    &[severity::ERROR, severity::FATAL],
                                    engine,
                                );
                            }
                            (KeyCode::Char('2'), _) => {
                                let engine = &app.engine;
                                app.views[app.active_tab].set_severity(
                                    "Warn",
                                    &[severity::WARN],
                                    engine,
                                );
                            }
                            (KeyCode::Char('3'), _) => {
                                let engine = &app.engine;
                                app.views[app.active_tab].set_severity(
                                    "Info",
                                    &[severity::INFO],
                                    engine,
                                );
                            }
                            (KeyCode::Char('4'), _) => {
                                let engine = &app.engine;
                                app.views[app.active_tab].set_severity(
                                    "Debug",
                                    &[severity::DEBUG, severity::TRACE],
                                    engine,
                                );
                            }
                            (KeyCode::Esc, _) => {
                                let engine = &app.engine;
                                app.views[app.active_tab].clear_filters(engine);
                            }
                            (KeyCode::Up, _) => wheel_delta -= 1,
                            (KeyCode::Down, _) => wheel_delta += 1,
                            (KeyCode::PageUp, _) => wheel_delta -= 20,
                            (KeyCode::PageDown, _) => wheel_delta += 20,
                            (KeyCode::Char('g'), _) => {
                                app.active().cursor_to_start();
                                wheel_delta = 0;
                            }
                            (KeyCode::Char('G'), _) => {
                                app.active().cursor_to_end();
                                wheel_delta = 0;
                            }
                            _ => {}
                        }
                    }
                }
                Event::Mouse(MouseEvent { kind, .. }) => match kind {
                    MouseEventKind::ScrollUp => wheel_delta -= 1,
                    MouseEventKind::ScrollDown => wheel_delta += 1,
                    _ => {}
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
            if !event::poll(Duration::from_millis(0))? {
                break;
            }
        }
        if wheel_delta != 0 {
            app.active().move_cursor(wheel_delta);
        }
        if should_break {
            break;
        }
    }
    let _ = app.active_ref();
    Ok(())
}
