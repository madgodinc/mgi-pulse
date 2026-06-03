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
use crate::engine::record::{severity, TS_UNTIMED};

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
    /// Comma-separated values, RFC 4180. Header is captured once per
    /// source and projection is by column name; `_N` (1-based)
    /// projects the Nth column for headerless or oddly-named files.
    /// Predicate-side access goes through `FieldCache::with_headers`.
    Csv,
    /// Tab-separated values. Same parser as `Csv`, different
    /// delimiter.
    Tsv,
    /// Apache / nginx access log: Common Log Format (CLF) and the
    /// Combined extension with referer + user-agent. The line has no
    /// native severity — we synthesize one from the HTTP status code
    /// (5xx → error, 4xx → warn, 2xx/3xx → info). Time format is the
    /// Apache `[DD/MMM/YYYY:HH:MM:SS ±HHMM]` shape, not RFC3339.
    Access,
    /// Java logback / log4j2 default console pattern: `YYYY-MM-DD
    /// HH:MM:SS[.,]mmm LEVEL [thread] logger - msg`. Stack-trace
    /// continuations (`\tat ...`, `Caused by: ...`) fold via the
    /// "first byte is not a digit" rule.
    Logback,
    /// systemd `journalctl -o json` output. NDJSON under the hood,
    /// but `__REALTIME_TIMESTAMP` (micros since epoch as a string)
    /// and `PRIORITY` (syslog 0-7) replace the standard `ts` / `level`.
    Journalctl,
    /// Generic regex-extraction format. Per-source pattern is held in
    /// `Engine::source_regex`; named captures `ts`, `level`, and any
    /// other group become projectable fields. Lets users open any
    /// plain-text log by supplying `--pattern='...'`.
    Regex,
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
            // CSV/TSV need a header to resolve `ts` / `level` columns
            // by name. The indexer's parse path doesn't carry that
            // here, so we leave records untimed unless field overrides
            // happen to match positional column names like `_2`. The
            // proper wire is via `Engine::source_headers` and an
            // alternate entry point; until that's stitched in (see
            // CSV-wire issue) hot-path indexing of CSV/TSV is purely
            // arrival-ordered.
            LogFormat::Csv | LogFormat::Tsv => {
                stats.untimed += 1;
                (TS_UNTIMED, severity::UNKNOWN)
            }
            LogFormat::Access => crate::engine::parse_access::ts_and_level(line, stats, fields),
            LogFormat::Logback => crate::engine::parse_logback::ts_and_level(line, stats, fields),
            LogFormat::Journalctl => {
                crate::engine::parse_journalctl::ts_and_level(line, stats, fields)
            }
            // Regex needs a per-source pattern that doesn't reach this
            // stateless entry point. Mark untimed/unknown on the
            // indexer pass; `Engine::recompute_regex_ts_level` walks
            // back over the records once the pattern is attached.
            LogFormat::Regex => {
                stats.untimed += 1;
                (TS_UNTIMED, severity::UNKNOWN)
            }
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
            // CSV/TSV need the per-source header which isn't reachable
            // from this stateless entry point. Callers that have the
            // header should go through `FieldCache::with_headers`
            // (which knows the source) — falling through here returns
            // None so unwired predicates fail closed rather than
            // silently using positional guesses.
            LogFormat::Csv | LogFormat::Tsv => None,
            LogFormat::Access => {
                crate::engine::parse_access::project_field(line, key).map(std::borrow::Cow::Owned)
            }
            LogFormat::Logback => {
                crate::engine::parse_logback::project_field(line, key).map(std::borrow::Cow::Owned)
            }
            LogFormat::Journalctl => crate::engine::parse_journalctl::project_field_journal(
                line, key,
            )
            .map(std::borrow::Cow::Owned),
            // Regex projection needs the pattern; FieldCache::with_regex
            // is the proper entry point. Returning None here lets
            // unwired predicates fail closed.
            LogFormat::Regex => None,
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
            // CSV/TSV are strictly one record per physical line. RFC
            // 4180 allows newlines inside quoted values, but supporting
            // that requires a stateful line splitter — out of scope
            // for v0.x. Documented in `parse_csv` module header.
            LogFormat::Csv | LogFormat::Tsv => false,
            // Access log records are strictly one line each. The
            // request URL never contains a literal newline (mod_log
            // emits `\n` as an escape).
            LogFormat::Access => false,
            LogFormat::Logback => crate::engine::parse_logback::is_continuation(line),
            // Journalctl is one record per JSON line — same as NDJSON.
            LogFormat::Journalctl => false,
            // Regex format: any line that doesn't match the pattern
            // could plausibly be a continuation (stack trace), but
            // without the per-source pattern at this entry point we
            // can't tell. Default to "no continuation"; future work
            // could thread the pattern in.
            LogFormat::Regex => false,
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
            | LogFormat::Syslog
            | LogFormat::Csv
            | LogFormat::Tsv
            | LogFormat::Access
            | LogFormat::Logback
            | LogFormat::Journalctl
            | LogFormat::Regex => severity::from_bytes(level.as_bytes()),
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
            | LogFormat::Syslog
            | LogFormat::Csv
            | LogFormat::Tsv
            | LogFormat::Access
            | LogFormat::Logback
            | LogFormat::Journalctl
            | LogFormat::Regex => parse_rfc3339_micros(s),
        }
    }

    /// Cheap heuristic to guess a format from the first records of an
    /// input. Auto-detect is opt-in: producers default to whatever the
    /// CLI says.
    ///
    /// ## Behaviour summary
    ///
    /// - Per-line votes accumulate; a format needs at least 2 votes
    ///   AND a strict majority over rival formats to win.
    /// - Single-line files and ambiguous samples (no format reaches
    ///   the 2-vote threshold) fall back to `LogFormat::Ndjson`. This
    ///   is intentional: NDJSON sources that happen to have only one
    ///   line of probe content land on the correct parser, and
    ///   genuinely plain-text content fails to parse as NDJSON in a
    ///   way that's reported in the dry-run summary (`json errors: N`)
    ///   — so the user has a clear signal to pass `--format` or
    ///   `--pattern`.
    ///
    /// ## Probe window vs full file
    ///
    /// Only the head of the file is sampled (~16 KiB / 64 lines in
    /// the CLI wrapper). A file that opens with a banner of a
    /// different shape than the body (e.g. plain-text header before
    /// the NDJSON records) can fool the detector. The mitigations:
    ///
    /// - The user can force the format with `--format=...`.
    /// - The `R` key rescans the **schema** (column derivation) over
    ///   the middle of the current filtered view, but does NOT
    ///   re-run format detection — the format is a property of the
    ///   source decided at ingest time and isn't revisited. If the
    ///   banner misled detect, restart with `--format`.
    ///
    /// Per-line format dispatch is intentionally out of scope; see
    /// ADR 0004.
    ///
    /// ## Precedence order
    ///
    /// More specific signatures win over less specific ones:
    /// syslog > access > logback > journalctl > NDJSON / EDN >
    /// logfmt > TSV > CSV. CSV / TSV go last because "≥2 delimiters
    /// outside quotes" is the loosest signature and would otherwise
    /// claim free-form prose with commas.
    pub fn detect(first_lines: &[&[u8]]) -> LogFormat {
        let mut ndjson_votes = 0;
        let mut logfmt_votes = 0;
        let mut edn_votes = 0;
        let mut syslog_votes = 0;
        let mut csv_votes = 0;
        let mut tsv_votes = 0;
        let mut access_votes = 0;
        let mut logback_votes = 0;
        let mut journalctl_votes = 0;
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
            // Access log signature next — `[DD/MMM/YYYY...]` after a
            // few space-separated tokens. Specific enough not to
            // collide with anything else, and we want it ahead of
            // logfmt because access lines occasionally contain `=`
            // in the user-agent.
            if crate::engine::parse_access::looks_like_access(trimmed) {
                access_votes += 1;
                continue;
            }
            // Logback before NDJSON/logfmt because its signature is
            // specific (`YYYY-MM-DD HH:MM:SS[.,]mmm LEVEL ...`) and
            // there's no other digit-prefix format we collide with.
            if crate::engine::parse_logback::looks_like_logback(trimmed) {
                logback_votes += 1;
                continue;
            }
            // journalctl before generic NDJSON — it IS JSON, but the
            // `__REALTIME_TIMESTAMP` / `PRIORITY` signature is
            // specific enough to claim before falling into the
            // braces-counter for NDJSON.
            if crate::engine::parse_journalctl::looks_like_journalctl(trimmed) {
                journalctl_votes += 1;
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
                continue;
            }
            // CSV/TSV last because their signature (≥2 delimiters
            // outside quotes) is the loosest and would otherwise eat
            // free-form text like "a, b, c is a list" as CSV.
            match crate::engine::parse_csv::delim_vote(trimmed) {
                Some(crate::engine::parse_csv::Delim::Comma) => csv_votes += 1,
                Some(crate::engine::parse_csv::Delim::Tab) => tsv_votes += 1,
                None => {}
            }
        }
        if syslog_votes >= 2
            && syslog_votes > ndjson_votes
            && syslog_votes > logfmt_votes
            && syslog_votes > edn_votes
            && syslog_votes > access_votes
            && syslog_votes > logback_votes
        {
            LogFormat::Syslog
        } else if access_votes >= 2
            && access_votes > ndjson_votes
            && access_votes > logfmt_votes
            && access_votes > edn_votes
            && access_votes > logback_votes
        {
            LogFormat::Access
        } else if logback_votes >= 2
            && logback_votes > ndjson_votes
            && logback_votes > logfmt_votes
            && logback_votes > edn_votes
        {
            LogFormat::Logback
        } else if journalctl_votes >= 2
            && journalctl_votes > ndjson_votes
            && journalctl_votes > edn_votes
        {
            LogFormat::Journalctl
        } else if edn_votes > ndjson_votes && edn_votes > logfmt_votes && edn_votes >= 2 {
            LogFormat::Edn
        } else if logfmt_votes > ndjson_votes && logfmt_votes >= 2 {
            LogFormat::Logfmt
        } else if ndjson_votes >= 2 {
            LogFormat::Ndjson
        } else if tsv_votes >= 2 && tsv_votes > csv_votes {
            LogFormat::Tsv
        } else if csv_votes >= 2 {
            LogFormat::Csv
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
    /// Optional column headers for CSV/TSV sources. Stateless formats
    /// (NDJSON, logfmt, EDN, Python, syslog) leave this `None`; they
    /// don't need it.
    headers: Option<&'a [String]>,
    /// Optional regex pattern for `LogFormat::Regex` sources.
    regex: Option<&'a regex::bytes::Regex>,
    cache: std::collections::HashMap<smol_str::SmolStr, Option<String>>,
}

impl<'a> FieldCache<'a> {
    pub fn new(format: LogFormat, bytes: &'a [u8]) -> Self {
        Self {
            format,
            bytes,
            headers: None,
            regex: None,
            cache: std::collections::HashMap::new(),
        }
    }

    /// Attach a CSV/TSV header list. Stateful formats look this up to
    /// resolve named columns; other formats ignore it.
    pub fn with_headers(mut self, headers: &'a [String]) -> Self {
        self.headers = Some(headers);
        self
    }

    /// Attach the per-source regex pattern. Required for
    /// `LogFormat::Regex` field projection; ignored otherwise.
    pub fn with_regex(mut self, regex: &'a regex::bytes::Regex) -> Self {
        self.regex = Some(regex);
        self
    }

    /// Look up one field. The first call parses; subsequent calls hit the
    /// in-record cache.
    pub fn get(&mut self, key: &str) -> Option<&str> {
        let smol = smol_str::SmolStr::new(key);
        if !self.cache.contains_key(&smol) {
            let parsed = match self.format {
                LogFormat::Csv => self.headers.and_then(|h| {
                    crate::engine::parse_csv::project_field_with_header(
                        self.bytes,
                        crate::engine::parse_csv::Delim::Comma,
                        h,
                        key,
                    )
                }),
                LogFormat::Tsv => self.headers.and_then(|h| {
                    crate::engine::parse_csv::project_field_with_header(
                        self.bytes,
                        crate::engine::parse_csv::Delim::Tab,
                        h,
                        key,
                    )
                }),
                LogFormat::Regex => self
                    .regex
                    .and_then(|re| crate::engine::parse_regex::project_field(self.bytes, re, key)),
                _ => self.format.project_field(self.bytes, key).map(String::from),
            };
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
    fn detect_picks_csv_when_lines_have_commas() {
        let lines: Vec<&[u8]> = vec![
            b"ts,level,host,msg",
            b"2026-06-01T12:00:00Z,info,host01,hi",
            b"2026-06-01T12:00:01Z,error,host02,oops",
        ];
        assert_eq!(LogFormat::detect(&lines), LogFormat::Csv);
    }

    #[test]
    fn detect_picks_journalctl_when_realtime_timestamp_present() {
        let lines: Vec<&[u8]> = vec![
            br#"{"__REALTIME_TIMESTAMP":"1717235400123456","PRIORITY":"6","MESSAGE":"a"}"#,
            br#"{"__REALTIME_TIMESTAMP":"1717235401123456","PRIORITY":"3","MESSAGE":"b"}"#,
        ];
        assert_eq!(LogFormat::detect(&lines), LogFormat::Journalctl);
    }

    #[test]
    fn detect_picks_logback_when_pattern_matches() {
        let lines: Vec<&[u8]> = vec![
            b"2026-06-01 12:00:00.123 INFO  [main] x.y.Z - hi",
            b"2026-06-01 12:00:01.456 ERROR [main] x.y.Z - boom",
        ];
        assert_eq!(LogFormat::detect(&lines), LogFormat::Logback);
    }

    #[test]
    fn detect_picks_access_when_lines_have_clf_signature() {
        let lines: Vec<&[u8]> = vec![
            br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET / HTTP/1.1" 200 5"#,
            br#"5.6.7.8 - - [01/Jun/2026:12:00:01 +0000] "POST /a HTTP/1.1" 201 0"#,
        ];
        assert_eq!(LogFormat::detect(&lines), LogFormat::Access);
    }

    #[test]
    fn detect_picks_tsv_when_lines_have_tabs() {
        let lines: Vec<&[u8]> = vec![
            b"ts\tlevel\tmsg",
            b"2026-06-01T12:00:00Z\tinfo\thello",
            b"2026-06-01T12:00:01Z\terror\toops",
        ];
        assert_eq!(LogFormat::detect(&lines), LogFormat::Tsv);
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
