//! EDN parser — Clojure-style `{:k v :k v}` records.
//!
//! Reference: <https://github.com/edn-format/edn>. Real-world log lines
//! that this parser is built for come from `mulog`, `clojure.tools.logging`
//! with EDN appenders, and assorted hand-rolled emitters. The full EDN
//! grammar is large; we only need to walk the **top-level map** to find
//! `:ts` and `:level`, and to project named fields for predicates.
//!
//! What's supported:
//! - Top-level map: `{ ... }` with no surrounding noise.
//! - Keys: `:keyword`, `:namespaced/keyword`, `"string"`.
//! - Values:
//!   - String literals `"..."` with `\"` / `\\` escapes.
//!   - Keywords `:foo` (level → severity mapping below).
//!   - Numbers (integer/decimal — passed through as-is text).
//!   - Tagged literals: `#inst "..."`, `#uuid "..."` — we take the
//!     string inside the tag's argument as the value.
//!   - Nested maps `{...}` / vectors `[...]` / lists `(...)` are
//!     skipped without parsing their interior.
//!
//! Not supported in v0.1: sets `#{...}`, characters `\c`, rationals
//! `1/2`, custom tagged readers, multi-line records.

use crate::engine::parse::{parse_rfc3339_micros, FieldNames, ParseStats};
use crate::engine::record::{severity, TS_UNTIMED};

/// Iterator over the top-level key/value pairs of an EDN map. Yields
/// `(key, value)` where both have already been unwrapped (keyword sigils,
/// surrounding quotes, `#inst` wrappers stripped).
pub struct EdnPairs<'a> {
    rest: &'a [u8],
    /// True once we've entered the outer `{`; we stop when we hit the
    /// matching `}` or run out of input.
    started: bool,
    finished: bool,
}

impl<'a> EdnPairs<'a> {
    pub fn new(line: &'a [u8]) -> Self {
        Self {
            rest: line,
            started: false,
            finished: false,
        }
    }

    fn skip_whitespace_and_commas(&mut self) {
        while let Some(&b) = self.rest.first() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' || b == b',' {
                self.rest = &self.rest[1..];
            } else {
                break;
            }
        }
    }

    /// Read one EDN element starting at the current cursor. Returns the
    /// slice for the element's *content* (with sigil stripped where the
    /// caller expects it). Advances `self.rest` past the element.
    fn read_element(&mut self) -> Option<EdnValue<'a>> {
        self.skip_whitespace_and_commas();
        let first = *self.rest.first()?;
        match first {
            b':' => {
                // Keyword.
                self.rest = &self.rest[1..];
                let end = self
                    .rest
                    .iter()
                    .position(|&b| is_terminator(b))
                    .unwrap_or(self.rest.len());
                let kw = &self.rest[..end];
                self.rest = &self.rest[end..];
                Some(EdnValue::Keyword(kw))
            }
            b'"' => {
                // String literal — read until matching unescaped `"`.
                self.rest = &self.rest[1..];
                let mut owned: Option<Vec<u8>> = None;
                let mut i = 0;
                while i < self.rest.len() {
                    let b = self.rest[i];
                    if b == b'\\' && i + 1 < self.rest.len() {
                        let next = self.rest[i + 1];
                        if next == b'"' || next == b'\\' {
                            let buf = owned.get_or_insert_with(|| self.rest[..i].to_vec());
                            buf.push(next);
                            i += 2;
                            continue;
                        }
                    }
                    if b == b'"' {
                        let val = match owned {
                            Some(buf) => {
                                EdnValue::OwnedString(String::from_utf8_lossy(&buf).into_owned())
                            }
                            None => EdnValue::String(&self.rest[..i]),
                        };
                        self.rest = &self.rest[i + 1..];
                        return Some(val);
                    }
                    if let Some(buf) = owned.as_mut() {
                        buf.push(b);
                    }
                    i += 1;
                }
                // Unterminated string — consume the rest.
                let val = match owned {
                    Some(buf) => EdnValue::OwnedString(String::from_utf8_lossy(&buf).into_owned()),
                    None => EdnValue::String(self.rest),
                };
                self.rest = &[];
                Some(val)
            }
            b'#' => {
                // Tagged literal `#inst "..."` or `#uuid "..."`. We
                // peek the tag, skip whitespace, and recurse on the
                // wrapped value. The tag is discarded — the underlying
                // string is what matters for ts and field projection.
                self.rest = &self.rest[1..];
                let tag_end = self
                    .rest
                    .iter()
                    .position(|&b| is_terminator(b))
                    .unwrap_or(self.rest.len());
                let _tag = &self.rest[..tag_end];
                self.rest = &self.rest[tag_end..];
                self.read_element()
            }
            b'{' => {
                self.skip_balanced(b'{', b'}');
                // Nested maps don't surface as projectable values — return
                // a placeholder that the caller can choose to ignore.
                Some(EdnValue::Nested)
            }
            b'[' => {
                self.skip_balanced(b'[', b']');
                Some(EdnValue::Nested)
            }
            b'(' => {
                self.skip_balanced(b'(', b')');
                Some(EdnValue::Nested)
            }
            b'}' | b']' | b')' => {
                // Wrong level — caller (next) will handle the close.
                None
            }
            _ => {
                // Bare token: number, boolean, nil. Read until terminator.
                let end = self
                    .rest
                    .iter()
                    .position(|&b| is_terminator(b))
                    .unwrap_or(self.rest.len());
                let val = EdnValue::Bare(&self.rest[..end]);
                self.rest = &self.rest[end..];
                Some(val)
            }
        }
    }

    /// Skip over a balanced delimited form (e.g. `{...}` or `[...]`).
    /// Handles nesting of any of the three forms.
    fn skip_balanced(&mut self, open: u8, close: u8) {
        // Consume the opening byte.
        self.rest = &self.rest[1..];
        let mut depth = 1i32;
        let mut in_string = false;
        let mut escape = false;
        while !self.rest.is_empty() && depth > 0 {
            let b = self.rest[0];
            self.rest = &self.rest[1..];
            if in_string {
                if escape {
                    escape = false;
                } else if b == b'\\' {
                    escape = true;
                } else if b == b'"' {
                    in_string = false;
                }
                continue;
            }
            if b == b'"' {
                in_string = true;
                continue;
            }
            if b == open || b == b'{' || b == b'[' || b == b'(' {
                if b == open {
                    depth += 1;
                }
                continue;
            }
            if b == close || b == b'}' || b == b']' || b == b')' {
                if b == close {
                    depth -= 1;
                }
                continue;
            }
        }
    }
}

/// One element extracted from EDN. Strings come out unwrapped; tagged
/// literals delegate to whatever they wrap.
#[derive(Debug)]
pub enum EdnValue<'a> {
    Keyword(&'a [u8]),
    String(&'a [u8]),
    OwnedString(String),
    Bare(&'a [u8]),
    Nested,
}

impl<'a> EdnValue<'a> {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            EdnValue::Keyword(b) | EdnValue::String(b) | EdnValue::Bare(b) => b,
            EdnValue::OwnedString(s) => s.as_bytes(),
            EdnValue::Nested => &[],
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            EdnValue::Keyword(b) | EdnValue::String(b) | EdnValue::Bare(b) => {
                std::str::from_utf8(b).ok()
            }
            EdnValue::OwnedString(s) => Some(s.as_str()),
            EdnValue::Nested => None,
        }
    }
}

impl<'a> Iterator for EdnPairs<'a> {
    type Item = (EdnValue<'a>, EdnValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if !self.started {
            self.skip_whitespace_and_commas();
            if self.rest.first() != Some(&b'{') {
                self.finished = true;
                return None;
            }
            self.rest = &self.rest[1..];
            self.started = true;
        }
        self.skip_whitespace_and_commas();
        if self.rest.first() == Some(&b'}') || self.rest.is_empty() {
            self.finished = true;
            return None;
        }
        let key = self.read_element()?;
        let value = self.read_element()?;
        Some((key, value))
    }
}

fn is_terminator(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\n' | b'\r' | b',' | b'{' | b'}' | b'[' | b']' | b'(' | b')'
    )
}

/// Match `key` (an EdnValue) against an expected field name. Strips the
/// keyword sigil and any namespace before comparing. So both `:ts` and
/// `:log/ts` match `"ts"`, and `"ts"` (quoted-string key) matches too.
fn key_matches(key: &EdnValue, expected: &str) -> bool {
    let bytes = match key {
        EdnValue::Keyword(b) => b,
        EdnValue::String(b) => b,
        _ => return false,
    };
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let local = match s.rsplit_once('/') {
        Some((_, local)) => local,
        None => s,
    };
    local == expected
}

/// Fast path for the indexer: walk the map once, extract ts and level.
pub fn ts_and_level(line: &[u8], stats: &mut ParseStats, fields: Option<&FieldNames>) -> (i64, u8) {
    let ts_key = fields.map(|f| f.ts.as_str()).unwrap_or("ts");
    let level_key = fields.map(|f| f.level.as_str()).unwrap_or("level");

    let mut ts_micros: Option<i64> = None;
    let mut sev = severity::UNKNOWN;
    let mut ts_seen_bad = false;
    let mut seen_pair = false;

    for (key, value) in EdnPairs::new(line) {
        seen_pair = true;
        if key_matches(&key, ts_key) {
            if let Some(s) = value.as_str() {
                match parse_rfc3339_micros(s) {
                    Some(v) => ts_micros = Some(v),
                    None => ts_seen_bad = true,
                }
            }
        } else if key_matches(&key, level_key) {
            // `:error` keyword: skip the `:` prefix already done by the
            // Keyword variant — its bytes are the bare name.
            sev = match &value {
                EdnValue::Keyword(b) | EdnValue::String(b) | EdnValue::Bare(b) => {
                    severity::from_bytes(b)
                }
                EdnValue::OwnedString(s) => severity::from_bytes(s.as_bytes()),
                EdnValue::Nested => severity::UNKNOWN,
            };
        }
    }

    if !seen_pair {
        stats.json_parse_errors += 1;
        stats.untimed += 1;
        return (TS_UNTIMED, severity::UNKNOWN);
    }

    match ts_micros {
        Some(v) => (v, sev),
        None => {
            if ts_seen_bad {
                stats.ts_parse_errors += 1;
            }
            stats.untimed += 1;
            (TS_UNTIMED, sev)
        }
    }
}

pub fn project_field(line: &[u8], key: &str) -> Option<String> {
    for (k, v) in EdnPairs::new(line) {
        if key_matches(&k, key) {
            return v.as_str().map(String::from);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_pairs(line: &[u8]) -> Vec<(String, String)> {
        EdnPairs::new(line)
            .map(|(k, v)| {
                let kk = match &k {
                    EdnValue::Keyword(b) => format!(":{}", String::from_utf8_lossy(b)),
                    EdnValue::String(b) => format!("\"{}\"", String::from_utf8_lossy(b)),
                    _ => "?".to_string(),
                };
                let vv = String::from_utf8_lossy(v.as_bytes()).into_owned();
                (kk, vv)
            })
            .collect()
    }

    #[test]
    fn parses_simple_map() {
        let pairs = collect_pairs(br#"{:level :info :msg "hello"}"#);
        assert_eq!(
            pairs,
            vec![
                (":level".to_string(), "info".to_string()),
                (":msg".to_string(), "hello".to_string()),
            ]
        );
    }

    #[test]
    fn keyword_namespaces_are_stripped_for_match() {
        // :log/ts and :ts both project as "ts".
        assert_eq!(
            project_field(br#"{:log/ts "2026" :level :info}"#, "ts"),
            Some("2026".to_string())
        );
    }

    #[test]
    fn quoted_string_keys_work() {
        assert_eq!(
            project_field(br#"{"ts" "2026" "level" "error"}"#, "ts"),
            Some("2026".to_string())
        );
    }

    #[test]
    fn ts_and_level_extracts_ts() {
        let mut stats = ParseStats::default();
        let line = br#"{:ts "2026-06-01T12:00:00Z" :level :error :msg "boom"}"#;
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::ERROR);
    }

    #[test]
    fn tagged_inst_is_unwrapped() {
        let mut stats = ParseStats::default();
        let line = br#"{:ts #inst "2026-06-01T12:00:00Z" :level :warn}"#;
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::WARN);
    }

    #[test]
    fn nested_maps_are_skipped() {
        let mut stats = ParseStats::default();
        let line =
            br#"{:ts "2026-06-01T12:00:00Z" :context {:user "alice" :ip "1.2.3.4"} :level :info}"#;
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::INFO);
    }

    #[test]
    fn missing_ts_is_marked_untimed() {
        let mut stats = ParseStats::default();
        let line = br#"{:level :info :msg "no ts"}"#;
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::INFO);
        assert_eq!(stats.untimed, 1);
    }

    #[test]
    fn non_edn_line_marks_json_parse_error() {
        let mut stats = ParseStats::default();
        let line = b"this is not edn at all";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
        assert_eq!(stats.json_parse_errors, 1);
    }

    #[test]
    fn project_arbitrary_field() {
        let line = br#"{:ts "x" :level :info :user "alice" :payload {:n 5}}"#;
        assert_eq!(project_field(line, "user"), Some("alice".to_string()));
        assert_eq!(project_field(line, "missing"), None);
        // Nested map value can't be projected as a string.
        assert_eq!(project_field(line, "payload"), None);
    }

    #[test]
    fn override_field_names_honored() {
        let mut stats = ParseStats::default();
        let fields = FieldNames {
            ts: "time".to_string(),
            level: "severity".to_string(),
        };
        let line = br#"{:time "2026-06-01T12:00:00Z" :severity :warn}"#;
        let (ts, sev) = ts_and_level(line, &mut stats, Some(&fields));
        assert!(ts > 0);
        assert_eq!(sev, severity::WARN);
    }
}
