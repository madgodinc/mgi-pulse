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
//!
//! M1 task: implement minimal run loop with TablePane only.
