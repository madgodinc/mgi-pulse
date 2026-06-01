//! Compact, parallel arrays indexed by `line_id`.
//!
//! - `LineIndex`: `Vec<u64>` offsets; length is implied by `offsets[i+1] - offsets[i]`.
//! - `TimeIndex`: `Vec<i64>` of `ts_micros`; `TS_UNTIMED` for the untimed bucket.
//! - `SeverityIndex`: `Vec<u8>` of severity enum bytes.
//!
//! Not held in sorted order on the hot path. For static files: sort once
//! after indexing. For live streams: append-only, histogram bins are tolerant
//! to mild out-of-order; periodic re-sort of the tail window only.
//!
//! M1 task: implement.
