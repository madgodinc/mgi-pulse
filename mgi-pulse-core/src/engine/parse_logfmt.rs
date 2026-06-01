//! logfmt parser — Go/Heroku-style `key=value key="value with spaces"` lines.
//!
//! Reference: <https://brandur.org/logfmt> and the Go `kr/logfmt` package.
//! Pairs are whitespace-separated. Values are either bare (no spaces, no
//! quotes) or quoted with `"`; inside quotes, `\"` escapes a literal quote
//! and `\\` escapes a backslash. Anything else inside quotes is verbatim.
//!
//! The hot path is the indexer's `parse_ts_level`: it pulls exactly two
//! known keys and skips everything else. We avoid the full HashMap path
//! by scanning once for `ts` / `level` directly. Field projection for
//! predicates does pay the HashMap-build cost — same trade-off as
//! NDJSON's `project_field`.

use crate::engine::parse::{parse_rfc3339_micros, FieldNames, ParseStats};
use crate::engine::record::{severity, TS_UNTIMED};

/// Iterator-style scan of a logfmt line. Yields `(key, value)` pairs, with
/// `value` already unescaped if it was quoted. Allocates one `String` per
/// quoted value that actually contained an escape; bare values are
/// returned as borrowed slices of the input.
pub struct LogfmtPairs<'a> {
    rest: &'a [u8],
}

impl<'a> LogfmtPairs<'a> {
    pub fn new(line: &'a [u8]) -> Self {
        Self { rest: line }
    }
}

/// A value produced by the scanner. Borrowed slice when the original input
/// already encoded the value without escapes; owned `String` only when a
/// `\"` or `\\` had to be expanded.
pub enum LogfmtValue<'a> {
    Borrowed(&'a [u8]),
    Owned(String),
}

impl<'a> LogfmtValue<'a> {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            LogfmtValue::Borrowed(b) => b,
            LogfmtValue::Owned(s) => s.as_bytes(),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            LogfmtValue::Borrowed(b) => std::str::from_utf8(b).ok(),
            LogfmtValue::Owned(s) => Some(s.as_str()),
        }
    }
}

impl<'a> Iterator for LogfmtPairs<'a> {
    type Item = (&'a [u8], LogfmtValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        // Skip leading whitespace.
        while let Some(&b) = self.rest.first() {
            if b == b' ' || b == b'\t' {
                self.rest = &self.rest[1..];
            } else {
                break;
            }
        }
        if self.rest.is_empty() {
            return None;
        }

        // Read the key — runs until `=` or whitespace.
        let key_end = self
            .rest
            .iter()
            .position(|&b| b == b'=' || b == b' ' || b == b'\t')
            .unwrap_or(self.rest.len());
        let key = &self.rest[..key_end];
        if key.is_empty() {
            // Malformed leading `=` — skip the byte and try again.
            self.rest = &self.rest[1..];
            return self.next();
        }
        if key_end == self.rest.len() {
            // Bare key with no value: treat as `key=""`.
            self.rest = &self.rest[key_end..];
            return Some((key, LogfmtValue::Borrowed(&[])));
        }
        if self.rest[key_end] != b'=' {
            // Bare key followed by space: same.
            self.rest = &self.rest[key_end..];
            return Some((key, LogfmtValue::Borrowed(&[])));
        }
        // Skip the `=`.
        self.rest = &self.rest[key_end + 1..];

        // Read the value.
        if self.rest.first() == Some(&b'"') {
            // Quoted value.
            self.rest = &self.rest[1..]; // skip opening quote
            let mut owned: Option<Vec<u8>> = None;
            let mut i = 0;
            while i < self.rest.len() {
                let b = self.rest[i];
                if b == b'\\' && i + 1 < self.rest.len() {
                    // Escape: \" or \\.
                    let next = self.rest[i + 1];
                    if next == b'"' || next == b'\\' {
                        let buf = owned.get_or_insert_with(|| self.rest[..i].to_vec());
                        buf.push(next);
                        i += 2;
                        continue;
                    }
                }
                if b == b'"' {
                    let value = match owned {
                        Some(buf) => LogfmtValue::Owned(String::from_utf8_lossy(&buf).into_owned()),
                        None => LogfmtValue::Borrowed(&self.rest[..i]),
                    };
                    self.rest = &self.rest[i + 1..];
                    return Some((key, value));
                }
                if let Some(buf) = owned.as_mut() {
                    buf.push(b);
                }
                i += 1;
            }
            // Unterminated quote — consume the rest as the value.
            let value = match owned {
                Some(buf) => LogfmtValue::Owned(String::from_utf8_lossy(&buf).into_owned()),
                None => LogfmtValue::Borrowed(self.rest),
            };
            self.rest = &[];
            Some((key, value))
        } else {
            // Bare value — runs until whitespace.
            let end = self
                .rest
                .iter()
                .position(|&b| b == b' ' || b == b'\t')
                .unwrap_or(self.rest.len());
            let value = LogfmtValue::Borrowed(&self.rest[..end]);
            self.rest = &self.rest[end..];
            Some((key, value))
        }
    }
}

/// Fast path for the indexer: scan once, extract ts and level, skip
/// everything else. Names come from FieldNames (CLI override-friendly).
pub fn ts_and_level(line: &[u8], stats: &mut ParseStats, fields: Option<&FieldNames>) -> (i64, u8) {
    let ts_key = fields.map(|f| f.ts.as_str()).unwrap_or("ts");
    let level_key = fields.map(|f| f.level.as_str()).unwrap_or("level");

    let mut ts_micros = None;
    let mut sev = severity::UNKNOWN;
    let mut ts_seen_bad = false;
    let mut seen_any_pair = false;

    for (key, value) in LogfmtPairs::new(line) {
        seen_any_pair = true;
        if key == ts_key.as_bytes() {
            if let Some(s) = value.as_str() {
                match parse_rfc3339_micros(s) {
                    Some(v) => ts_micros = Some(v),
                    None => {
                        ts_seen_bad = true;
                    }
                }
            } else {
                ts_seen_bad = true;
            }
        } else if key == level_key.as_bytes() {
            sev = severity::from_bytes(value.as_bytes());
        }
    }

    if !seen_any_pair {
        // Not logfmt at all — let the caller treat as untimed/unknown.
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

/// Look up a single field by name. Slower than `ts_and_level` because
/// callers may target arbitrary keys, so we have to walk every pair until
/// the key matches. Same trade-off as NDJSON's `project_field`.
pub fn project_field(line: &[u8], key: &str) -> Option<String> {
    let key_b = key.as_bytes();
    for (k, v) in LogfmtPairs::new(line) {
        if k == key_b {
            return v.as_str().map(String::from);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_pairs(line: &[u8]) -> Vec<(String, String)> {
        LogfmtPairs::new(line)
            .map(|(k, v)| {
                (
                    String::from_utf8_lossy(k).into_owned(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn bare_pairs() {
        let pairs = collect_pairs(b"level=info logger=app");
        assert_eq!(
            pairs,
            vec![
                ("level".to_string(), "info".to_string()),
                ("logger".to_string(), "app".to_string()),
            ]
        );
    }

    #[test]
    fn quoted_values_keep_spaces() {
        let pairs = collect_pairs(br#"level=info msg="hello world""#);
        assert_eq!(
            pairs,
            vec![
                ("level".to_string(), "info".to_string()),
                ("msg".to_string(), "hello world".to_string()),
            ]
        );
    }

    #[test]
    fn quoted_values_unescape() {
        let pairs = collect_pairs(br#"msg="hello \"world\"""#);
        assert_eq!(
            pairs,
            vec![("msg".to_string(), r#"hello "world""#.to_string())]
        );
        let pairs = collect_pairs(br#"path="C:\\Users\\foo""#);
        assert_eq!(
            pairs,
            vec![("path".to_string(), r"C:\Users\foo".to_string())]
        );
    }

    #[test]
    fn extra_whitespace_is_tolerated() {
        let pairs = collect_pairs(b"  level=info   logger=app  ");
        assert_eq!(
            pairs,
            vec![
                ("level".to_string(), "info".to_string()),
                ("logger".to_string(), "app".to_string()),
            ]
        );
    }

    #[test]
    fn bare_key_without_value_yields_empty() {
        let pairs = collect_pairs(b"warn level=info");
        assert_eq!(
            pairs,
            vec![
                ("warn".to_string(), "".to_string()),
                ("level".to_string(), "info".to_string()),
            ]
        );
    }

    #[test]
    fn ts_and_level_picks_known_keys() {
        let mut stats = ParseStats::default();
        let line = br#"ts=2026-06-01T12:00:00Z level=error msg="boom""#;
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::ERROR);
        assert_eq!(stats.untimed, 0);
    }

    #[test]
    fn ts_and_level_honors_override_fields() {
        let mut stats = ParseStats::default();
        let fields = FieldNames {
            ts: "time".to_string(),
            level: "severity".to_string(),
        };
        let line = b"time=2026-06-01T12:00:00Z severity=warn msg=hello";
        let (ts, sev) = ts_and_level(line, &mut stats, Some(&fields));
        assert!(ts > 0);
        assert_eq!(sev, severity::WARN);
    }

    #[test]
    fn ts_and_level_marks_untimed_when_ts_missing() {
        let mut stats = ParseStats::default();
        let line = b"level=info msg=hello";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::INFO);
        assert_eq!(stats.untimed, 1);
        assert_eq!(stats.ts_parse_errors, 0);
    }

    #[test]
    fn ts_and_level_handles_garbage_line() {
        let mut stats = ParseStats::default();
        let line = b"not a logfmt line at all";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        // First "not" parses as bare key with empty value; that's "any pair",
        // so we don't flag json_parse_errors here. Treat as untimed/unknown.
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
        assert_eq!(stats.untimed, 1);
    }

    #[test]
    fn project_field_finds_value() {
        let line = br#"ts=2026 level=info msg="hello world" user=admin"#;
        assert_eq!(project_field(line, "user"), Some("admin".to_string()));
        assert_eq!(project_field(line, "msg"), Some("hello world".to_string()));
        assert_eq!(project_field(line, "missing"), None);
    }
}
