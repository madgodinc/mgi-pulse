//! Colour themes.
//!
//! Three presets: `dark` (default), `light`, `nocolor`. The third uses
//! only modifiers (bold / dim) so the table stays usable when piped
//! through `script` to a file, or when the terminal has no ANSI colour.
//!
//! Selection precedence: `--theme` flag > `MGI_PULSE_THEME` env var >
//! `dark`.

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
}
