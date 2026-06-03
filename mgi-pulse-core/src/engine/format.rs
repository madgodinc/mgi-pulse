//! Log format dispatch.
//!
//! A format is a property of the **source**, not the line: one file is
//! always one format (NDJSON, logfmt, EDN, …), inside one file they don't
//! mix. The engine keeps a `LogFormat` per `source_id` and dispatches
//! through a small `match` for every record — no `Box<dyn>` per line, no
//! lifetime gymnastics through trait objects, no vtable in the hot path.
//!
//! v0.1 ships exactly one variant (`Ndjson`); shape the API now so the
//! later format adds are pure additions, not refactors.

use crate::engine::parse::{
    parse_rfc3339_micros, ts_and_level, ts_and_level_named, FieldNames, ParseStats,
};
use crate::engine::record::severity;

/// Closed set of supported log formats. Adding a new format means
/// extending this enum and the four `match` arms below.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogFormat {
    #[default]
    Ndjson,
    /// Go / Heroku-style `key=value key="quoted value"` lines. Reference:
    /// <https://brandur.org/logfmt>. Common in Go ecosystems via
    /// `kr/logfmt` and Heroku Logplex.
    Logfmt,
    /// Clojure EDN-style `{:k v :k v}` maps. Reference:
    /// <https://github.com/edn-format/edn>. Common in Clojure logging
    /// through `mulog`, `clojure.tools.logging` with EDN appenders, etc.
    Edn,
    /// Python `logging.basicConfig()` default format:
    /// `2026-06-01 12:00:00,123 - logger - LEVEL - message`. Reference:
    /// <https://docs.python.org/3/library/logging.html#logrecord-attributes>.
    Python,
    /// Syslog RFC 5424. Reference:
    /// <https://datatracker.ietf.org/doc/html/rfc5424>. The PRI digits
    /// at the start carry both facility (ignored) and severity (lower
    /// 3 bits → engine severity). Structured-data blocks are exposed
    /// as flat `SD-ID.key` fields by `project_field`.
    Syslog,
}

impl LogFormat {
    /// Extract `(ts_micros, severity)` from a line. Called by producers on
    /// every record, so this is on the hot indexing path. Default field
    /// names live in `FieldNames`; the `--time-field` / `--level-field`
    /// flags override them.
    pub fn parse_ts_level(
        self,
        line: &[u8],
        stats: &mut ParseStats,
        fields: Option<&FieldNames>,
    ) -> (i64, u8) {
        match self {
            LogFormat::Ndjson => match fields {
                Some(f) => ts_and_level_named(line, f, stats),
                None => ts_and_level(line, stats),
            },
            LogFormat::Logfmt => crate::engine::parse_logfmt::ts_and_level(line, stats, fields),
            LogFormat::Edn => crate::engine::parse_edn::ts_and_level(line, stats, fields),
            LogFormat::Python => crate::engine::parse_python::ts_and_level(line, stats, fields),
            LogFormat::Syslog => crate::engine::parse_syslog::ts_and_level(line, stats, fields),
        }
    }

    /// Project one field as a borrowed string. Used by predicates that
    /// need to read a specific JSON / logfmt / EDN field per record.
    /// Returns `None` if the field is absent or the line doesn't parse.
    ///
    /// The borrow lifetime is tied to `line`: callers either render the
    /// value immediately or cache an owned copy via `FieldCache`.
    pub fn project_field<'a>(self, line: &'a [u8], key: &str) -> Option<std::borrow::Cow<'a, str>> {
        match self {
            LogFormat::Ndjson => crate::schema::project_field(line, key)
                .map(crate::schema::unquote_if_string)
                .map(std::borrow::Cow::Borrowed),
            LogFormat::Logfmt => {
                crate::engine::parse_logfmt::project_field(line, key).map(std::borrow::Cow::Owned)
            }
            LogFormat::Edn => {
                crate::engine::parse_edn::project_field(line, key).map(std::borrow::Cow::Owned)
            }
            LogFormat::Python => {
                crate::engine::parse_python::project_field(line, key).map(std::borrow::Cow::Owned)
            }
            LogFormat::Syslog => {
                crate::engine::parse_syslog::project_field(line, key).map(std::borrow::Cow::Owned)
            }
        }
    }

    /// True if `line` is a continuation of the previous record (e.g.
    /// stack-trace `^\s+at ...` for Java, `^\s+File "..."` for Python).
    /// v0.1 NDJSON treats every newline as a record boundary, so this is
    /// always false; later formats override.
    pub fn is_continuation(self, line: &[u8]) -> bool {
        match self {
            // NDJSON records are always one valid JSON per line; nothing
            // legitimately continues onto the next line.
            LogFormat::Ndjson => false,
            // For logfmt and EDN we use the `^\s+` heuristic: a line that
            // starts with whitespace is treated as a continuation of the
            // record above. This is the common shape of Java / Python /
            // Ruby stack traces and most exception serialisations.
            LogFormat::Logfmt | LogFormat::Edn => {
                matches!(line.first(), Some(&b' ') | Some(&b'\t'))
            }
            // Python's heuristic is broader: any line that doesn't start
            // with a digit can't open a new `YYYY-MM-DD` timestamp, so
            // `Traceback (...)` and `ValueError: bad` count as
            // continuations of the previous record.
            LogFormat::Python => crate::engine::parse_python::is_continuation(line),
            // Syslog records always begin with `<PRI>` — a line that
            // doesn't start with `<` can't open a new record, so it
            // folds into the previous one (covers multi-line MSG
            // payloads like embedded stack traces).
            LogFormat::Syslog => line.first() != Some(&b'<'),
        }
    }

    /// Severity rank used by the indexer when the level field is present
    /// but doesn't map to a known severity name. Same byte rank across
    /// formats; here so format-specific aliases (e.g. EDN keywords) can
    /// override without touching the predicate machinery.
    pub fn severity_from_level(self, level: &str) -> u8 {
        match self {
            LogFormat::Ndjson
            | LogFormat::Logfmt
            | LogFormat::Edn
            | LogFormat::Python
            | LogFormat::Syslog => severity::from_bytes(level.as_bytes()),
        }
    }

    /// Used by future formats whose timestamp encoding isn't RFC3339.
    /// v0.1 NDJSON expects RFC3339 strings; this delegates to the shared
    /// parser.
    pub fn parse_timestamp(self, s: &str) -> Option<i64> {
        match self {
            LogFormat::Ndjson
            | LogFormat::Logfmt
            | LogFormat::Edn
            | LogFormat::Python
            | LogFormat::Syslog => parse_rfc3339_micros(s),
        }
    }

    /// Cheap heuristic to guess a format from the first records of an
    /// input. Auto-detect is opt-in: producers default to whatever the
    /// CLI says. Returns `Ndjson` when in doubt.
    ///
    /// Heuristic: a line that starts with `{` and ends with `}` is
    /// treated as NDJSON; a line that has at least two `key=value` pairs
    /// (no leading `{`) is logfmt. Everything else defaults to NDJSON
    /// so plain-text falls into the less-mode path the way the user
    /// already expects.
    pub fn detect(first_lines: &[&[u8]]) -> LogFormat {
        let mut ndjson_votes = 0;
        let mut logfmt_votes = 0;
        let mut edn_votes = 0;
        let mut syslog_votes = 0;
        for line in first_lines {
            let trimmed = trim_ascii(line);
            if trimmed.is_empty() {
                continue;
            }
            // Syslog 5424 signature first — `<DIGITS>1 ` is unambiguous
            // and a syslog line never starts with `{` or contains the
            // `key=value` shape that would fool logfmt.
            if crate::engine::parse_syslog::looks_like_syslog(trimmed) {
                syslog_votes += 1;
                continue;
            }
            if trimmed.first() == Some(&b'{') && trimmed.last() == Some(&b'}') {
                // EDN signature: first non-whitespace inside the braces is
                // `:` (keyword key) or `#` (tagged). JSON uses `"` for keys.
                if edn_signature(trimmed) {
                    edn_votes += 1;
                    continue;
                }
                ndjson_votes += 1;
                continue;
            }
            if logfmt_signature(trimmed) {
                logfmt_votes += 1;
            }
        }
        if syslog_votes >= 2
            && syslog_votes > ndjson_votes
            && syslog_votes > logfmt_votes
            && syslog_votes > edn_votes
        {
            LogFormat::Syslog
        } else if edn_votes > ndjson_votes && edn_votes > logfmt_votes && edn_votes >= 2 {
            LogFormat::Edn
        } else if logfmt_votes > ndjson_votes && logfmt_votes >= 2 {
            LogFormat::Logfmt
        } else {
            LogFormat::Ndjson
        }
    }
}

fn trim_ascii(line: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = line.len();
    while start < end && line[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && line[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &line[start..end]
}

/// True when `line` looks like EDN: starts with `{`, the first
/// non-whitespace inside the braces is a keyword `:` or tag `#`.
fn edn_signature(line: &[u8]) -> bool {
    if line.first() != Some(&b'{') {
        return false;
    }
    for &b in &line[1..] {
        if b == b' ' || b == b'\t' {
            continue;
        }
        return b == b':' || b == b'#';
    }
    false
}

/// True when `line` looks like logfmt: at least two `key=value` pairs
/// whose keys are runs of alphanumeric/_/. characters.
fn logfmt_signature(line: &[u8]) -> bool {
    let mut pairs = 0;
    let mut i = 0;
    while i < line.len() {
        // Skip spaces.
        while i < line.len() && line[i] == b' ' {
            i += 1;
        }
        let key_start = i;
        while i < line.len()
            && (line[i].is_ascii_alphanumeric() || line[i] == b'_' || line[i] == b'.')
        {
            i += 1;
        }
        let key_len = i - key_start;
        if key_len == 0 || i >= line.len() || line[i] != b'=' {
            return pairs >= 2;
        }
        pairs += 1;
        if pairs >= 2 {
            return true;
        }
        // Skip past the value.
        i += 1; // past `=`
        if i < line.len() && line[i] == b'"' {
            i += 1;
            while i < line.len() {
                if line[i] == b'\\' && i + 1 < line.len() {
                    i += 2;
                    continue;
                }
                if line[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else {
            while i < line.len() && line[i] != b' ' {
                i += 1;
            }
        }
    }
    pairs >= 2
}

/// Per-record field cache. Reused across predicate evaluations on a single
/// record so the parse cost is paid at most once per field per record,
/// even when several predicates (regex + field-equals + future SQL DSL)
/// read overlapping fields.
///
/// Owned strings are deliberate: predicate evaluation may borrow the
/// record bytes (FileRef → mmap region) immutably, and a borrowed cache
/// would force every cache miss to extend the same borrow. Owning the
/// values lets us mutate the cache freely. The allocation pressure is
/// bounded — one owned `String` per (field, record) pair, and `scan`
/// drops the cache between records.
pub struct FieldCache<'a> {
    format: LogFormat,
    bytes: &'a [u8],
    cache: std::collections::HashMap<smol_str::SmolStr, Option<String>>,
}

impl<'a> FieldCache<'a> {
    pub fn new(format: LogFormat, bytes: &'a [u8]) -> Self {
        Self {
            format,
            bytes,
            cache: std::collections::HashMap::new(),
        }
    }

    /// Look up one field. The first call parses; subsequent calls hit the
    /// in-record cache.
    pub fn get(&mut self, key: &str) -> Option<&str> {
        let smol = smol_str::SmolStr::new(key);
        if !self.cache.contains_key(&smol) {
            let parsed = self.format.project_field(self.bytes, key).map(String::from);
            self.cache.insert(smol.clone(), parsed);
        }
        self.cache.get(&smol).unwrap().as_deref()
    }

    /// Raw bytes, for predicates that work without parsing (regex over
    /// the whole line).
    pub fn raw(&self) -> &[u8] {
        self.bytes
    }

    /// Format of the record under evaluation. Predicates can switch
    /// behaviour per format if they ever need to.
    pub fn format(&self) -> LogFormat {
        self.format
    }

    /// Drop all parsed values. Called by `query::scan` between records to
    /// keep the cache from accumulating across the whole index.
    pub fn reset(&mut self, bytes: &'a [u8]) {
        self.bytes = bytes;
        self.cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndjson_parse_ts_level_uses_defaults() {
        let mut stats = ParseStats::default();
        let line = br#"{"ts":"2026-06-01T12:00:00Z","level":"error","msg":"boom"}"#;
        let (ts, sev) = LogFormat::Ndjson.parse_ts_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::ERROR);
        assert_eq!(stats.untimed, 0);
    }

    #[test]
    fn ndjson_parse_ts_level_honors_override() {
        let mut stats = ParseStats::default();
        let fields = FieldNames {
            ts: "@timestamp".to_string(),
            level: "severity_text".to_string(),
        };
        let line = br#"{"@timestamp":"2026-06-01T12:00:00Z","severity_text":"warn","msg":"x"}"#;
        let (ts, sev) = LogFormat::Ndjson.parse_ts_level(line, &mut stats, Some(&fields));
        assert!(ts > 0);
        assert_eq!(sev, severity::WARN);
    }

    #[test]
    fn ndjson_project_field_handles_string_and_number() {
        let line = br#"{"logger":"app","n":5}"#;
        assert_eq!(
            LogFormat::Ndjson.project_field(line, "logger").as_deref(),
            Some("app")
        );
        assert_eq!(
            LogFormat::Ndjson.project_field(line, "n").as_deref(),
            Some("5")
        );
        assert!(LogFormat::Ndjson
            .project_field(br#"{"logger":"app"}"#, "missing")
            .is_none());
    }

    #[test]
    fn logfmt_project_field_finds_quoted_value() {
        let line = br#"level=info msg="hello world" user=admin"#;
        assert_eq!(
            LogFormat::Logfmt.project_field(line, "msg").as_deref(),
            Some("hello world")
        );
        assert_eq!(
            LogFormat::Logfmt.project_field(line, "user").as_deref(),
            Some("admin")
        );
    }

    #[test]
    fn logfmt_parse_ts_level_round_trip() {
        let mut stats = ParseStats::default();
        let line = b"ts=2026-06-01T12:00:00Z level=error msg=boom";
        let (ts, sev) = LogFormat::Logfmt.parse_ts_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::ERROR);
    }

    #[test]
    fn detect_picks_logfmt_when_multiple_kv_pairs() {
        let lines: Vec<&[u8]> = vec![
            b"level=info ts=2026 msg=start",
            b"level=warn ts=2027 msg=slow",
        ];
        assert_eq!(LogFormat::detect(&lines), LogFormat::Logfmt);
    }

    #[test]
    fn detect_picks_ndjson_when_lines_look_like_json() {
        let lines: Vec<&[u8]> = vec![br#"{"a":1}"#, br#"{"b":2}"#];
        assert_eq!(LogFormat::detect(&lines), LogFormat::Ndjson);
    }

    #[test]
    fn detect_defaults_to_ndjson_for_plain_text() {
        let lines: Vec<&[u8]> = vec![b"plain", b"text without kv"];
        assert_eq!(LogFormat::detect(&lines), LogFormat::Ndjson);
    }

    #[test]
    fn detect_picks_syslog_when_lines_have_pri_version() {
        let lines: Vec<&[u8]> = vec![
            b"<134>1 2026-06-01T12:00:00Z host app 1 - - hi",
            b"<131>1 2026-06-01T12:00:01Z host app 1 - - oops",
        ];
        assert_eq!(LogFormat::detect(&lines), LogFormat::Syslog);
    }

    #[test]
    fn ndjson_is_never_continuation() {
        assert!(!LogFormat::Ndjson.is_continuation(b"    at foo()"));
        assert!(!LogFormat::Ndjson.is_continuation(b"random"));
    }

    #[test]
    fn field_cache_parses_once_per_field() {
        let line = br#"{"logger":"app","level":"info"}"#;
        let mut cache = FieldCache::new(LogFormat::Ndjson, line);
        let a = cache.get("logger");
        assert_eq!(a, Some("app"));
        // Same key, hits the cache.
        let b = cache.get("logger");
        assert_eq!(b, Some("app"));
        // Different key.
        assert_eq!(cache.get("level"), Some("info"));
        // Missing.
        assert_eq!(cache.get("missing"), None);
    }

    #[test]
    fn field_cache_reset_clears_state() {
        let line1 = br#"{"a":"x"}"#;
        let line2 = br#"{"a":"y"}"#;
        let mut cache = FieldCache::new(LogFormat::Ndjson, line1);
        assert_eq!(cache.get("a"), Some("x"));
        cache.reset(line2);
        assert_eq!(cache.get("a"), Some("y"));
    }
}
