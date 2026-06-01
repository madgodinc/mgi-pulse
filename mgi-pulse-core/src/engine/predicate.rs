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
}
