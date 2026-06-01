//! Indexer thread.
//!
//! Drains a `RecordProducer`, parses ts/level via a borrowed serde struct
//! (not a hand-rolled byte scan, not full `Value`), updates LineIndex,
//! TimeIndex, SeverityIndex, and the per-predicate bitsets in one pass.
//!
//! Memory budget target: ~150 MB of indexes per 1 GB of NDJSON, comfortable
//! up to ~5–10 GB total. On-disk indexes are backlog.
//!
//! Parse cost is the dominant indexing cost and the measurable risk of M1.
//! Measure on a real 2 GB log in the first week of M1; be ready to swap to
//! simd-json / jiter if the gate slips.
//!
//! M1 task: implement.
