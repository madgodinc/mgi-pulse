//! Generic regex-extraction parser.
//!
//! Lets users open any plain-text log by handing in a named-capture
//! regex on the command line. Each line is matched against the
//! pattern; named captures become typed fields the predicate /
//! schema layer can use just like a real format's keys.
//!
//! Example invocation:
//!
//! ```sh
//! mgi-pulse --pattern='(?P<ts>\d{4}-\d{2}-\d{2}\s\d{2}:\d{2}:\d{2})\s+(?P<level>\w+)\s+(?P<msg>.*)' app.log
//! ```
//!
//! Recognised capture group names:
//!
//! - `ts` — timestamp. We try to parse the matched text as
//!   RFC3339 (with the same prefix-padding the DSL uses for `ts>2026`).
//! - `level` — severity name (`info`, `warn`, ...). Falls through to
//!   `severity::from_bytes`.
//! - any other name — exposed as a field for `field=value` predicates,
//!   the DSL, and the auto-columns table.
//!
//! Lines that don't match the pattern land in the untimed bucket with
//! `severity::UNKNOWN` and a `json_parse_errors` increment (the same
//! "unparseable" channel other formats use).

use crate::engine::parse::{parse_rfc3339_micros, FieldNames, ParseStats};
use crate::engine::record::{severity, TS_UNTIMED};

/// Same prefix-padding as the DSL's `parse_partial_rfc3339`. Lets a
/// user supply `(?P<ts>\d{4}-\d{2}-\d{2}\s\d{2}:\d{2})` and still
/// get a usable timestamp out of a log that lacks seconds.
fn parse_padded_ts(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    // Replace a space-separated date-time into a `T`-separated one
    // before further padding so `2026-06-01 12:00:00` works the same
    // as the RFC3339 form `2026-06-01T12:00:00Z`.
    let normalized: String = trimmed
        .char_indices()
        .map(|(i, c)| if i == 10 && c == ' ' { 'T' } else { c })
        .collect();
    let padded: String = match normalized.len() {
        4 => format!("{}-01-01T00:00:00Z", normalized),
        7 => format!("{}-01T00:00:00Z", normalized),
        10 => format!("{}T00:00:00Z", normalized),
        13 => format!("{}:00:00Z", normalized),
        16 => format!("{}:00Z", normalized),
        19 => format!("{}Z", normalized),
        _ => normalized,
    };
    parse_rfc3339_micros(&padded)
}

/// Apply `pattern` to `line` and pull out (ts, severity). `None` for
/// either when the capture is absent or unparseable.
pub fn ts_and_level(
    line: &[u8],
    pattern: &regex::bytes::Regex,
    stats: &mut ParseStats,
    _fields: Option<&FieldNames>,
) -> (i64, u8) {
    let Some(caps) = pattern.captures(line) else {
        stats.json_parse_errors += 1;
        stats.untimed += 1;
        return (TS_UNTIMED, severity::UNKNOWN);
    };

    let sev = caps
        .name("level")
        .map(|m| severity::from_bytes(m.as_bytes()))
        .unwrap_or(severity::UNKNOWN);

    let ts = match caps.name("ts") {
        Some(m) => {
            // Best-effort UTF-8 decode for timestamps. Logs that
            // capture binary bytes into a `ts` field have bigger
            // problems than us.
            let s = match std::str::from_utf8(m.as_bytes()) {
                Ok(s) => s,
                Err(_) => {
                    stats.ts_parse_errors += 1;
                    stats.untimed += 1;
                    return (TS_UNTIMED, sev);
                }
            };
            match parse_padded_ts(s) {
                Some(t) => t,
                None => {
                    stats.ts_parse_errors += 1;
                    stats.untimed += 1;
                    return (TS_UNTIMED, sev);
                }
            }
        }
        None => {
            stats.untimed += 1;
            return (TS_UNTIMED, sev);
        }
    };

    (ts, sev)
}

/// Field projection. Returns the captured string for `key` if the
/// pattern matched and contains a group named `key`. Special-cases
/// `level` → mapped severity name (lowercase) so a numeric or oddly-
/// shaped level still surfaces as a readable label.
pub fn project_field(line: &[u8], pattern: &regex::bytes::Regex, key: &str) -> Option<String> {
    let caps = pattern.captures(line)?;
    if key == "level" {
        let m = caps.name("level")?;
        let sev = severity::from_bytes(m.as_bytes());
        if sev == severity::UNKNOWN {
            // Pattern matched but the value wasn't a known level —
            // fall through to the raw capture.
            return Some(String::from_utf8_lossy(m.as_bytes()).into_owned());
        }
        return Some(severity::name(sev).to_lowercase());
    }
    let m = caps.name(key)?;
    Some(String::from_utf8_lossy(m.as_bytes()).into_owned())
}

/// Compile a user-supplied pattern, returning a `regex::bytes::Regex`.
/// Errors come back as Strings for the CLI to surface in `--help`-
/// adjacent failure mode.
pub fn compile_pattern(src: &str) -> Result<regex::bytes::Regex, String> {
    regex::bytes::Regex::new(src).map_err(|e| format!("regex compile error: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ts_level_msg_named_captures() {
        let re = compile_pattern(
            r"(?P<ts>\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2})\s+(?P<level>\w+)\s+(?P<msg>.*)",
        )
        .unwrap();
        let mut stats = ParseStats::default();
        let line = b"2026-06-01 12:00:00 INFO server started";
        let (ts, sev) = ts_and_level(line, &re, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::INFO);
        assert_eq!(
            project_field(line, &re, "msg").as_deref(),
            Some("server started")
        );
    }

    #[test]
    fn captures_arbitrary_field() {
        // `[^\]]+` for thread because real thread names contain `-`
        // and `:` which `\w` excludes.
        let re = compile_pattern(
            r"\[(?P<thread>[^\]]+)\]\s+(?P<level>\w+)\s+(?P<msg>.*)",
        )
        .unwrap();
        let line = b"[worker-3] INFO hello";
        assert_eq!(
            project_field(line, &re, "thread").as_deref(),
            Some("worker-3")
        );
    }

    #[test]
    fn padded_ts_handles_short_prefix() {
        let re = compile_pattern(r"(?P<ts>\d{4})\s+(?P<msg>.*)").unwrap();
        let mut stats = ParseStats::default();
        let line = b"2026 boot";
        let (ts, _) = ts_and_level(line, &re, &mut stats, None);
        assert!(ts > 0);
    }

    #[test]
    fn unmatched_line_increments_parse_errors() {
        let re = compile_pattern(r"^DOES_NOT_MATCH").unwrap();
        let mut stats = ParseStats::default();
        let (ts, sev) = ts_and_level(b"random text", &re, &mut stats, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
        assert_eq!(stats.json_parse_errors, 1);
    }

    #[test]
    fn matched_without_ts_capture_is_untimed_but_severity_kept() {
        let re = compile_pattern(r"^(?P<level>\w+):\s+(?P<msg>.*)").unwrap();
        let mut stats = ParseStats::default();
        let (ts, sev) = ts_and_level(b"WARN: queue full", &re, &mut stats, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::WARN);
        assert_eq!(stats.untimed, 1);
    }

    #[test]
    fn level_alias_returns_lowercase_known_name() {
        let re = compile_pattern(r"(?P<level>\w+)").unwrap();
        assert_eq!(project_field(b"ERROR", &re, "level").as_deref(), Some("error"));
        assert_eq!(project_field(b"info", &re, "level").as_deref(), Some("info"));
    }

    #[test]
    fn level_alias_falls_through_when_value_is_unknown() {
        let re = compile_pattern(r"(?P<level>\w+)").unwrap();
        assert_eq!(
            project_field(b"NOTICE", &re, "level").as_deref(),
            Some("NOTICE")
        );
    }

    #[test]
    fn compile_pattern_surfaces_errors() {
        assert!(compile_pattern("[unbalanced").is_err());
    }
}
