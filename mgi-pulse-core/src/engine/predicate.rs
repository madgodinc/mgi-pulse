//! One predicate machine for search, field-equals, and time-range.
//!
//! Search is not a separate engine. `/foo` becomes a `RegexBytesPredicate`,
//! `f` on a cell becomes a `FieldEqualsPredicate`, scrubbing time becomes a
//! `TimeRangePredicate`. They compose through `AndPredicate`.
//!
//! Bytes flow through `regex::bytes` everywhere. UTF-8 is only assumed at the
//! render boundary (lossy). Logs are not guaranteed to be valid UTF-8.

use regex::bytes::Regex;

use crate::engine::record::RawRecord;
use crate::schema::{project_field, unquote_if_string};

/// Decision: does this record match? Predicates take raw bytes (the actual
/// log line, post-mmap-resolve) plus the parsed record header.
///
/// The trait is intentionally tiny. A predicate is a pure function of its
/// inputs; matching twice for the same record must return the same answer.
pub trait Predicate: Send + Sync {
    fn matches(&self, rec: &RawRecord, line_bytes: &[u8]) -> bool;
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
    fn matches(&self, _rec: &RawRecord, line_bytes: &[u8]) -> bool {
        self.re.is_match(line_bytes)
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
    fn matches(&self, _rec: &RawRecord, line_bytes: &[u8]) -> bool {
        match project_field(line_bytes, &self.field) {
            Some(raw) => unquote_if_string(raw) == self.value,
            None => false,
        }
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
    fn matches(&self, rec: &RawRecord, line_bytes: &[u8]) -> bool {
        self.parts.iter().all(|p| p.matches(rec, line_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn regex_matches_substring() {
        let p = RegexBytesPredicate::new("erro").unwrap();
        let r = fake_rec();
        assert!(p.matches(&r, br#"{"level":"error","msg":"boom"}"#));
        assert!(!p.matches(&r, br#"{"level":"info","msg":"ok"}"#));
    }

    #[test]
    fn regex_handles_non_utf8() {
        // (?-u) disables Unicode mode so \xff matches the literal byte rather
        // than the Unicode codepoint U+00FF (which would be encoded as two
        // bytes in UTF-8).
        let p = RegexBytesPredicate::new(r"(?-u)\xff{3}").unwrap();
        let r = fake_rec();
        assert!(p.matches(&r, b"\xff\xff\xff payload"));
        // Negative case: a single 0xff is not three in a row.
        assert!(!p.matches(&r, b"\xff payload"));
    }

    #[test]
    fn field_equals_unquotes_string_values() {
        let p = FieldEqualsPredicate::new("level", "error");
        let r = fake_rec();
        assert!(p.matches(&r, br#"{"level":"error","msg":"x"}"#));
        assert!(!p.matches(&r, br#"{"level":"info","msg":"x"}"#));
        assert!(!p.matches(&r, br#"{"msg":"no level here"}"#));
    }

    #[test]
    fn field_equals_works_on_numeric() {
        let p = FieldEqualsPredicate::new("status", "200");
        let r = fake_rec();
        // Numeric value: project_field returns "200", unquote_if_string
        // leaves it unchanged.
        assert!(p.matches(&r, br#"{"status":200}"#));
        assert!(!p.matches(&r, br#"{"status":404}"#));
    }

    #[test]
    fn and_predicate_requires_all_to_match() {
        let mut and = AndPredicate::new();
        and.push(Box::new(FieldEqualsPredicate::new("level", "error")));
        and.push(Box::new(RegexBytesPredicate::new("boom").unwrap()));
        let r = fake_rec();
        assert!(and.matches(&r, br#"{"level":"error","msg":"boom"}"#));
        assert!(!and.matches(&r, br#"{"level":"error","msg":"ok"}"#));
        assert!(!and.matches(&r, br#"{"level":"info","msg":"boom"}"#));
    }

    #[test]
    fn empty_and_matches_everything() {
        let and = AndPredicate::new();
        let r = fake_rec();
        assert!(and.matches(&r, b"anything"));
    }
}
