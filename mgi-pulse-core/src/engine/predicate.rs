//! One predicate machine for search, field-equals, and time-range.
//!
//! Search is not a separate engine. `/foo` becomes a `RegexBytesPredicate`,
//! `f` on a cell becomes a `FieldEqualsPredicate`, scrubbing time becomes a
//! `TimeRangePredicate`. They compose through `AndPredicate`.
//!
//! Bytes flow through `regex::bytes` everywhere. UTF-8 is only assumed at the
//! render boundary (lossy). Logs are not guaranteed to be valid UTF-8.
//!
//! M1 task: trait + RegexBytesPredicate (drives `/search`).
//! M2 task: FieldEqualsPredicate (drives `f` filter).
//! M3 task: TimeRangePredicate (drives timeline scrub).
