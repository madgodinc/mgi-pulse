//! One predicate machine for search, field-equals, and time-range.
//!
//! Search is not a separate engine. `/foo` becomes a `RegexBytesPredicate`,
//! `f` on a cell becomes a `FieldEqualsPredicate`, scrubbing time becomes a
//! `TimeRangePredicate`. They compose through `AndPredicate`.
//!
//! Bytes flow through `regex::bytes` everywhere. UTF-8 is only assumed at the
//! render boundary (lossy). Logs are not guaranteed to be valid UTF-8.

use regex::bytes::Regex;

use crate::engine::format::FieldCache;
use crate::engine::record::RawRecord;

/// Decision: does this record match? Predicates take raw bytes (the actual
/// log line, post-mmap-resolve) plus the parsed record header.
///
/// The trait is intentionally tiny. A predicate is a pure function of its
/// inputs; matching twice for the same record must return the same answer.
/// Predicate evaluation contract.
///
/// `cache` is a per-record cache of parsed field values. Field-aware
/// predicates (field-equals, future SQL-DSL field clauses) read through
/// `cache.get(key)` so the parse cost is paid at most once per field per
/// record, regardless of how many predicates need that field.
///
/// Predicates that don't need field-level access (regex over the whole
/// raw line) call `cache.raw()` to read the underlying bytes directly.
pub trait Predicate: Send + Sync {
    fn matches(&self, rec: &RawRecord, cache: &mut FieldCache<'_>) -> bool;
}

pub struct RegexBytesPredicate {
    re: Regex,
}

impl RegexBytesPredicate {
    pub fn new(pattern: &str) -> anyhow::Result<Self> {
        let re = Regex::new(pattern)?;
        Ok(Self { re })
    }
}

impl Predicate for RegexBytesPredicate {
    fn matches(&self, _rec: &RawRecord, cache: &mut FieldCache<'_>) -> bool {
        self.re.is_match(cache.raw())
    }
}

/// Predicate: a specific JSON field on the record equals (string-wise) the
/// given value. Backs the keyboard `f` filter. Comparison is on the unquoted
/// string form of the field's raw JSON value, so users can filter
/// `level=error` whether `error` is JSON-quoted or not.
pub struct FieldEqualsPredicate {
    field: String,
    value: String,
}

impl FieldEqualsPredicate {
    pub fn new(field: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            value: value.into(),
        }
    }
}

impl Predicate for FieldEqualsPredicate {
    fn matches(&self, _rec: &RawRecord, cache: &mut FieldCache<'_>) -> bool {
        match cache.get(&self.field) {
            Some(v) => v == self.value,
            None => false,
        }
    }
}

/// Field-targeted regex predicate. Reads the named field through the
/// FieldCache and matches its value against the supplied pattern.
/// Different from `RegexBytesPredicate` (which matches the whole raw
/// line) — useful for `msg~/timeout/` style DSL clauses.
pub struct FieldRegexPredicate {
    field: String,
    re: regex::Regex,
}

impl FieldRegexPredicate {
    pub fn new(field: String, pattern: &str) -> Result<Self, regex::Error> {
        let re = regex::Regex::new(pattern)?;
        Ok(Self { field, re })
    }
}

impl Predicate for FieldRegexPredicate {
    fn matches(&self, _rec: &RawRecord, cache: &mut FieldCache<'_>) -> bool {
        match cache.get(&self.field) {
            Some(v) => self.re.is_match(v),
            None => false,
        }
    }
}

/// Negation wrapper. Inverts the inner predicate's verdict. Used by the
/// DSL's `!=` operator.
pub struct NotPredicate {
    inner: Box<dyn Predicate>,
}

impl NotPredicate {
    pub fn new(inner: Box<dyn Predicate>) -> Self {
        Self { inner }
    }
}

impl Predicate for NotPredicate {
    fn matches(&self, rec: &RawRecord, cache: &mut FieldCache<'_>) -> bool {
        !self.inner.matches(rec, cache)
    }
}

/// Time-range predicate. Compares a record's `ts_micros` to a fixed
/// bound. Used by the DSL's `>`, `>=`, `<`, `<=` operators on `ts` and
/// (when wired) by the timeline-scrub interaction.
pub struct TimeRangePredicate {
    bound: i64,
    cmp: TimeCmp,
}

#[derive(Debug, Clone, Copy)]
enum TimeCmp {
    Greater,
    GreaterEq,
    Less,
    LessEq,
}

impl TimeRangePredicate {
    pub fn greater_than(bound: i64) -> Self {
        Self {
            bound,
            cmp: TimeCmp::Greater,
        }
    }
    pub fn at_or_after(bound: i64) -> Self {
        Self {
            bound,
            cmp: TimeCmp::GreaterEq,
        }
    }
    pub fn less_than(bound: i64) -> Self {
        Self {
            bound,
            cmp: TimeCmp::Less,
        }
    }
    pub fn at_or_before(bound: i64) -> Self {
        Self {
            bound,
            cmp: TimeCmp::LessEq,
        }
    }
}

impl Predicate for TimeRangePredicate {
    fn matches(&self, rec: &RawRecord, _cache: &mut FieldCache<'_>) -> bool {
        // Untimed records (TS_UNTIMED = i64::MIN) never satisfy a
        // time-range filter — they're outside the time axis by design.
        if rec.ts_micros == crate::engine::record::TS_UNTIMED {
            return false;
        }
        match self.cmp {
            TimeCmp::Greater => rec.ts_micros > self.bound,
            TimeCmp::GreaterEq => rec.ts_micros >= self.bound,
            TimeCmp::Less => rec.ts_micros < self.bound,
            TimeCmp::LessEq => rec.ts_micros <= self.bound,
        }
    }
}

/// Severity filter: keep records whose severity is exactly one of the
/// allowed levels.
///
/// Bit-mask over the severity enum (8 entries fit in a u8 mask). The
/// "Error" tab uses `{ERROR, FATAL}` so fatal records do show up alongside
/// errors (the same row would never reach a user as "warn-only" anyway).
/// Plain `{INFO}` / `{WARN}` keep the tab strictly to one level.
pub struct SeverityInPredicate {
    /// Bit `1 << sev` is set for each allowed severity.
    mask: u8,
}

impl SeverityInPredicate {
    pub fn new(levels: &[u8]) -> Self {
        let mut mask = 0u8;
        for &lv in levels {
            mask |= 1 << lv;
        }
        Self { mask }
    }
}

impl Predicate for SeverityInPredicate {
    fn matches(&self, rec: &RawRecord, _cache: &mut FieldCache<'_>) -> bool {
        (self.mask >> rec.severity) & 1 == 1
    }
}

/// AND-composition of any predicates. Empty composition matches everything;
/// `f` then `f` again produces a two-element And and only rows passing both
/// survive.
pub struct AndPredicate {
    pub parts: Vec<Box<dyn Predicate>>,
}

impl AndPredicate {
    pub fn new() -> Self {
        Self { parts: Vec::new() }
    }
    pub fn push(&mut self, p: Box<dyn Predicate>) {
        self.parts.push(p);
    }
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
    pub fn len(&self) -> usize {
        self.parts.len()
    }
}

impl Default for AndPredicate {
    fn default() -> Self {
        Self::new()
    }
}

impl Predicate for AndPredicate {
    fn matches(&self, rec: &RawRecord, cache: &mut FieldCache<'_>) -> bool {
        self.parts.iter().all(|p| p.matches(rec, cache))
    }
}

/// OR-composition. Mirror of `AndPredicate` — empty matches *nothing*
/// (vacuous OR), `f OR g` survives if either side does. Built by the
/// DSL parser; UI-level filter stacks still use AND.
pub struct OrPredicate {
    pub parts: Vec<Box<dyn Predicate>>,
}

impl OrPredicate {
    pub fn new() -> Self {
        Self { parts: Vec::new() }
    }
    pub fn push(&mut self, p: Box<dyn Predicate>) {
        self.parts.push(p);
    }
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
    pub fn len(&self) -> usize {
        self.parts.len()
    }
}

impl Default for OrPredicate {
    fn default() -> Self {
        Self::new()
    }
}

impl Predicate for OrPredicate {
    fn matches(&self, rec: &RawRecord, cache: &mut FieldCache<'_>) -> bool {
        self.parts.iter().any(|p| p.matches(rec, cache))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::format::LogFormat;
    use crate::engine::record::{severity, RecordBytes};

    fn fake_rec() -> RawRecord {
        RawRecord {
            source_id: 0,
            line_id: 0,
            ts_micros: 0,
            severity: severity::INFO,
            bytes: RecordBytes::Owned(Box::from([])),
        }
    }

    fn check(p: &dyn Predicate, line: &[u8]) -> bool {
        let r = fake_rec();
        let mut cache = FieldCache::new(LogFormat::Ndjson, line);
        p.matches(&r, &mut cache)
    }

    fn check_with(p: &dyn Predicate, rec: &RawRecord, line: &[u8]) -> bool {
        let mut cache = FieldCache::new(LogFormat::Ndjson, line);
        p.matches(rec, &mut cache)
    }

    #[test]
    fn regex_matches_substring() {
        let p = RegexBytesPredicate::new("erro").unwrap();
        assert!(check(&p, br#"{"level":"error","msg":"boom"}"#));
        assert!(!check(&p, br#"{"level":"info","msg":"ok"}"#));
    }

    #[test]
    fn regex_handles_non_utf8() {
        let p = RegexBytesPredicate::new(r"(?-u)\xff{3}").unwrap();
        assert!(check(&p, b"\xff\xff\xff payload"));
        assert!(!check(&p, b"\xff payload"));
    }

    #[test]
    fn field_equals_unquotes_string_values() {
        let p = FieldEqualsPredicate::new("level", "error");
        assert!(check(&p, br#"{"level":"error","msg":"x"}"#));
        assert!(!check(&p, br#"{"level":"info","msg":"x"}"#));
        assert!(!check(&p, br#"{"msg":"no level here"}"#));
    }

    #[test]
    fn field_equals_works_on_numeric() {
        let p = FieldEqualsPredicate::new("status", "200");
        assert!(check(&p, br#"{"status":200}"#));
        assert!(!check(&p, br#"{"status":404}"#));
    }

    #[test]
    fn and_predicate_requires_all_to_match() {
        let mut and = AndPredicate::new();
        and.push(Box::new(FieldEqualsPredicate::new("level", "error")));
        and.push(Box::new(RegexBytesPredicate::new("boom").unwrap()));
        assert!(check(&and, br#"{"level":"error","msg":"boom"}"#));
        assert!(!check(&and, br#"{"level":"error","msg":"ok"}"#));
        assert!(!check(&and, br#"{"level":"info","msg":"boom"}"#));
    }

    #[test]
    fn empty_and_matches_everything() {
        let and = AndPredicate::new();
        assert!(check(&and, b"anything"));
    }

    #[test]
    fn severity_in_set_membership() {
        let p = SeverityInPredicate::new(&[severity::ERROR, severity::FATAL]);
        let mut r = fake_rec();
        r.severity = severity::ERROR;
        assert!(check_with(&p, &r, b""));
        r.severity = severity::FATAL;
        assert!(check_with(&p, &r, b""));
        r.severity = severity::WARN;
        assert!(!check_with(&p, &r, b""));
        r.severity = severity::INFO;
        assert!(!check_with(&p, &r, b""));
        r.severity = severity::TRACE;
        assert!(!check_with(&p, &r, b""));
    }

    #[test]
    fn severity_in_single_level() {
        let p = SeverityInPredicate::new(&[severity::WARN]);
        let mut r = fake_rec();
        r.severity = severity::WARN;
        assert!(check_with(&p, &r, b""));
        r.severity = severity::ERROR;
        assert!(!check_with(&p, &r, b""));
        r.severity = severity::INFO;
        assert!(!check_with(&p, &r, b""));
    }

    #[test]
    fn and_predicate_shares_field_cache_across_predicates() {
        // Two field-equals on the same key should hit the cache the
        // second time. The test runs without instrumentation; the
        // intent is that this works at all under the new contract.
        let mut and = AndPredicate::new();
        and.push(Box::new(FieldEqualsPredicate::new("level", "error")));
        and.push(Box::new(FieldEqualsPredicate::new("level", "error")));
        assert!(check(&and, br#"{"level":"error"}"#));
    }
}
