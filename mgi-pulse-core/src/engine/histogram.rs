//! Time histogram for the Timeline pane.
//!
//! Adaptive bin width based on the visible range (1s / 1min / 1h / ...).
//! Bin payload: total count + severity distribution.
//!
//! Records with `ts_micros == TS_UNTIMED` are NOT placed in any bin and are
//! excluded from time-range filters by default. On the table they remain
//! visible (no silent dropping); under a time filter they render grayed-out
//! or hidden, depending on the mode.
//!
//! M3 task: implement.
