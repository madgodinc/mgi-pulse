//! Colour themes.
//!
//! Three presets: `dark` (default), `light`, `nocolor`. The third uses
//! only modifiers (bold / dim) so the table stays usable when piped
//! through `script` to a file, or when the terminal has no ANSI colour.
//!
//! Selection precedence: `NO_COLOR` / `TERM=dumb` / non-tty stdout >
//! `--theme` flag > `MGI_PULSE_THEME` env var > `dark`.
//!
//! The first tier is an override (per <https://no-color.org/>): if the
//! environment asks for no colour, the user's explicit `--theme=dark`
//! still loses, because the environment knows things the user didn't
//! type (piped output, dumb terminal, accessibility setting).

use mgi_pulse_core::engine::record::severity;
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    NoColor,
}

impl Theme {
    pub fn parse(name: &str) -> Option<Theme> {
        match name {
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            "nocolor" | "mono" => Some(Theme::NoColor),
            _ => None,
        }
    }

    pub fn from_env_or_default() -> Theme {
        if let Ok(s) = std::env::var("MGI_PULSE_THEME") {
            if let Some(t) = Theme::parse(&s) {
                return t;
            }
        }
        Theme::Dark
    }

    /// Override that wins over `--theme` and `MGI_PULSE_THEME`. Returns
    /// `Some(NoColor)` if the environment signals "no colour, please"
    /// in any of the conventional ways. The TUI itself always renders
    /// to a tty (alt-screen + raw mode), so we read the stdout-tty
    /// signal at startup before entering raw mode — by the time we're
    /// in the UI it's too late to ask.
    pub fn env_override(stdout_is_tty: bool) -> Option<Theme> {
        // <https://no-color.org/> — any value (including empty) disables.
        if std::env::var_os("NO_COLOR").is_some() {
            return Some(Theme::NoColor);
        }
        if let Ok(term) = std::env::var("TERM") {
            if term == "dumb" {
                return Some(Theme::NoColor);
            }
        }
        if !stdout_is_tty {
            return Some(Theme::NoColor);
        }
        None
    }

    /// Style for a severity-coloured cell (the timestamp column inherits
    /// this, as do level cells).
    pub fn severity_style(self, sev: u8) -> Style {
        match self {
            Theme::Dark => match sev {
                severity::ERROR | severity::FATAL => Style::default().fg(Color::Red),
                severity::WARN => Style::default().fg(Color::Yellow),
                severity::INFO => Style::default().fg(Color::Reset),
                severity::DEBUG | severity::TRACE => Style::default().fg(Color::DarkGray),
                _ => Style::default().fg(Color::Reset),
            },
            Theme::Light => match sev {
                severity::ERROR | severity::FATAL => {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                }
                severity::WARN => Style::default().fg(Color::Yellow),
                severity::INFO => Style::default().fg(Color::Black),
                severity::DEBUG | severity::TRACE => Style::default().fg(Color::Gray),
                _ => Style::default().fg(Color::Black),
            },
            Theme::NoColor => match sev {
                severity::ERROR | severity::FATAL => Style::default().add_modifier(Modifier::BOLD),
                severity::WARN => Style::default().add_modifier(Modifier::BOLD),
                severity::DEBUG | severity::TRACE => Style::default().add_modifier(Modifier::DIM),
                _ => Style::default(),
            },
        }
    }

    /// Background style for the row the cursor is on.
    pub fn cursor_row_style(self) -> Style {
        match self {
            Theme::Dark => Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
            Theme::Light => Style::default()
                .bg(Color::Gray)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            Theme::NoColor => Style::default().add_modifier(Modifier::REVERSED),
        }
    }

    /// Style for the active tab in the tab bar.
    ///
    /// History: previous versions used `bg(Blue) + fg(White)`. On terminals
    /// that interpret `Color::Blue` as a saturated bright blue (WezTerm with
    /// the default palette is a common case), white-on-blue can drop below
    /// useful contrast and the label visually disappears. Switching to a
    /// foreground-only accent (yellow + bold + underlined) sidesteps the
    /// palette-dependence and matches the convention most other TUIs use
    /// (htop, lazygit, helix).
    pub fn active_tab_style(self) -> Style {
        match self {
            Theme::Dark => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            Theme::Light => Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            Theme::NoColor => Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        }
    }

    /// Style for inactive tabs.
    pub fn inactive_tab_style(self) -> Style {
        match self {
            Theme::Dark => Style::default().fg(Color::DarkGray),
            Theme::Light => Style::default().fg(Color::Gray),
            Theme::NoColor => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// Bright style for primary status-bar entry points (/ f t etc).
    pub fn hint_bright(self) -> Style {
        match self {
            Theme::Dark => Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            Theme::Light => Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            Theme::NoColor => Style::default().add_modifier(Modifier::BOLD),
        }
    }

    /// Dim style for surrounding labels.
    pub fn hint_dim(self) -> Style {
        match self {
            Theme::Dark => Style::default().fg(Color::DarkGray),
            Theme::Light => Style::default().fg(Color::Gray),
            Theme::NoColor => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// Style for the histogram bar colour given the dominant severity.
    pub fn histogram_bar(self, sev: u8) -> Style {
        match self {
            Theme::Dark | Theme::Light => match sev {
                severity::FATAL | severity::ERROR => Style::default().fg(Color::Red),
                severity::WARN => Style::default().fg(Color::Yellow),
                severity::INFO => Style::default().fg(Color::Green),
                severity::DEBUG | severity::TRACE => Style::default().fg(Color::DarkGray),
                _ => Style::default().fg(Color::Gray),
            },
            Theme::NoColor => match sev {
                severity::FATAL | severity::ERROR => Style::default().add_modifier(Modifier::BOLD),
                _ => Style::default(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_names() {
        assert_eq!(Theme::parse("dark"), Some(Theme::Dark));
        assert_eq!(Theme::parse("light"), Some(Theme::Light));
        assert_eq!(Theme::parse("nocolor"), Some(Theme::NoColor));
        assert_eq!(Theme::parse("mono"), Some(Theme::NoColor));
        assert_eq!(Theme::parse("blue"), None);
    }

    #[test]
    fn severity_style_distinguishes_levels() {
        let dark = Theme::Dark;
        // Error and info shouldn't be the same style.
        assert_ne!(
            dark.severity_style(severity::ERROR),
            dark.severity_style(severity::INFO)
        );
    }

    #[test]
    fn nocolor_uses_modifiers_not_colours() {
        let nc = Theme::NoColor;
        let error_style = nc.severity_style(severity::ERROR);
        // No fg colour, only modifiers.
        assert_eq!(error_style.fg, None);
        assert!(error_style.add_modifier.contains(Modifier::BOLD));
    }

    // env_override reads process-wide env vars (NO_COLOR, TERM). Tests
    // that mutate them must not run in parallel, hence the shared mutex
    // and serial section per test.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var_os(key);
            // SAFETY: tests serialise via ENV_LOCK; no other threads
            // read these vars while we hold the lock.
            unsafe {
                std::env::set_var(key, val);
            }
            EnvGuard { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            unsafe {
                std::env::remove_var(key);
            }
            EnvGuard { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn no_color_env_forces_nocolor() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g1 = EnvGuard::set("NO_COLOR", "1");
        let _g2 = EnvGuard::unset("TERM");
        // stdout_is_tty=true so only NO_COLOR can trigger.
        assert_eq!(Theme::env_override(true), Some(Theme::NoColor));
    }

    #[test]
    fn no_color_empty_value_still_triggers() {
        // Per <https://no-color.org/> any value including empty disables colour.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g1 = EnvGuard::set("NO_COLOR", "");
        let _g2 = EnvGuard::unset("TERM");
        assert_eq!(Theme::env_override(true), Some(Theme::NoColor));
    }

    #[test]
    fn term_dumb_forces_nocolor() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g1 = EnvGuard::unset("NO_COLOR");
        let _g2 = EnvGuard::set("TERM", "dumb");
        assert_eq!(Theme::env_override(true), Some(Theme::NoColor));
    }

    #[test]
    fn non_tty_stdout_forces_nocolor() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g1 = EnvGuard::unset("NO_COLOR");
        let _g2 = EnvGuard::set("TERM", "xterm-256color");
        assert_eq!(Theme::env_override(false), Some(Theme::NoColor));
    }

    #[test]
    fn normal_tty_no_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g1 = EnvGuard::unset("NO_COLOR");
        let _g2 = EnvGuard::set("TERM", "xterm-256color");
        assert_eq!(Theme::env_override(true), None);
    }
}
