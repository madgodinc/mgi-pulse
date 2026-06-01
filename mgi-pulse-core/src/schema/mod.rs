//! Schema inference: union of seen fields with per-field stats.
//!
//! - **Union, not last-N.** Columns are stable in a session: a field that
//!   disappears becomes an empty cell, never a removed column. UI as a pure
//!   function of state is the rationale.
//! - **Warmup-lock**, two flavors:
//!   - File: lock after `min(10_000, total)` lines or EOF. No timer.
//!   - Stream: lock at 10_000 lines OR (T=5s AND `has_seen_data`). Where
//!     `has_seen_data` means ≥1 field has been emitted, not ≥1 row arrived.
//!     RAW-only for 5s → stay provisional, render a RAW-only view honestly.
//! - **Top-K**: bounded-exact map (1000 distinct), with an overflow flag.
//!   No HLL, no count-min for v0.1 — they answer the wrong question for
//!   header counters. HLL is backlog as a high-cardinality detector.
//!
//! M2 task: implement.
