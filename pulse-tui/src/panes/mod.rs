//! Render panes.
//!
//! - `TablePane`: M1 raw lines, M2 auto-columns + click-filter, raw passthrough
//!   for non-JSON rows (rendered colspan with a `▶` marker).
//! - `DetailPane`: full JSON pretty-print for the focused row only.
//! - `TimelinePane`: M3 histogram + keyboard scrub (`<>+-`, Shift expands range).
//! - `StatusBar`: progress, mode, count, follow indicator.
//!
//! Mouse is opt-in via `--mouse`. Keyboard is primary: terminal text selection
//! must not be broken in the default mode.
