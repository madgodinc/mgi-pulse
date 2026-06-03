//! systemd journal `journalctl -o json` parser.
//!
//! Reference: <https://systemd.io/JOURNAL_EXPORT_FORMATS/>.
//!
//! Each line is a JSON object, like:
//!
//! ```json
//! {
//!   "__REALTIME_TIMESTAMP": "1717235400123456",
//!   "PRIORITY": "6",
//!   "MESSAGE": "Started session.",
//!   "SYSLOG_IDENTIFIER": "systemd",
//!   "_HOSTNAME": "host01"
//! }
//! ```
//!
//! Differences from generic NDJSON:
//!
//! - Timestamp lives in `__REALTIME_TIMESTAMP` as **microseconds**
//!   since the Unix epoch encoded as a string. No RFC3339, no
//!   fractional dot — just the raw number.
//! - Severity is `PRIORITY` as a string-encoded syslog level (0-7,
//!   same scheme as the syslog RFC 5424 parser).
//! - Field names are uppercase, often start with `_` or `__`.
//!
//! We piggy-back on the NDJSON parser for everything else (the line
//! is real JSON), only overriding the timestamp/severity extraction.

use crate::engine::parse::{FieldNames, ParseStats};
use crate::engine::record::{severity, TS_UNTIMED};
use crate::schema::project_field;

/// Map a syslog priority string ("0".."7") to the engine's severity.
/// Same mapping as `parse_syslog::map_syslog_severity`.
fn map_priority(s: &[u8]) -> u8 {
    match s {
        b"0" | b"1" | b"2" => severity::FATAL,
        b"3" => severity::ERROR,
        b"4" => severity::WARN,
        b"5" | b"6" => severity::INFO,
        b"7" => severity::DEBUG,
        _ => severity::UNKNOWN,
    }
}

pub fn ts_and_level(
    line: &[u8],
    stats: &mut ParseStats,
    _fields: Option<&FieldNames>,
) -> (i64, u8) {
    // PRIORITY first — it's cheap and works even if the timestamp is
    // somehow absent.
    let sev = match project_field(line, "PRIORITY") {
        Some(raw) => {
            // The raw value comes back including its JSON quotes when
            // it's a string. Strip them; numeric values come back
            // unquoted.
            let trimmed = trim_quotes(raw);
            map_priority(trimmed.as_bytes())
        }
        None => severity::UNKNOWN,
    };

    let ts_raw = match project_field(line, "__REALTIME_TIMESTAMP") {
        Some(raw) => raw,
        None => {
            stats.untimed += 1;
            return (TS_UNTIMED, sev);
        }
    };
    let trimmed = trim_quotes(ts_raw);
    // Micros since epoch.
    let micros: i64 = match trimmed.parse() {
        Ok(v) => v,
        Err(_) => {
            stats.ts_parse_errors += 1;
            stats.untimed += 1;
            return (TS_UNTIMED, sev);
        }
    };
    (micros, sev)
}

fn trim_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Field projection. Falls back to the NDJSON schema project for
/// every key. Special-cases `level` → mapped severity name and
/// `msg` → MESSAGE (the canonical journal payload field).
pub fn project_field_journal(line: &[u8], key: &str) -> Option<String> {
    match key {
        "msg" => project_field(line, "MESSAGE").map(strip_quotes_owned),
        "level" => {
            let raw = project_field(line, "PRIORITY")?;
            let trimmed = trim_quotes(&raw);
            let sev = map_priority(trimmed.as_bytes());
            Some(severity::name(sev).to_lowercase())
        }
        // Aliases for the common journal fields so users don't have to
        // remember the `_HOSTNAME` shape.
        "host" | "_HOSTNAME" => project_field(line, "_HOSTNAME").map(strip_quotes_owned),
        "unit" | "_SYSTEMD_UNIT" => {
            project_field(line, "_SYSTEMD_UNIT").map(strip_quotes_owned)
        }
        "ident" | "SYSLOG_IDENTIFIER" => {
            project_field(line, "SYSLOG_IDENTIFIER").map(strip_quotes_owned)
        }
        _ => project_field(line, key).map(strip_quotes_owned),
    }
}

fn strip_quotes_owned<S: AsRef<str>>(s: S) -> String {
    trim_quotes(s.as_ref()).to_string()
}

/// Heuristic: lines look like journalctl JSON when they're JSON with
/// `__REALTIME_TIMESTAMP` or `PRIORITY` near the start. Cheap byte
/// search — no full JSON parse for the detector.
pub fn looks_like_journalctl(line: &[u8]) -> bool {
    if line.first() != Some(&b'{') {
        return false;
    }
    // Bound the search so a giant JSON blob doesn't sweep the whole
    // line on every detect tick.
    let window = &line[..line.len().min(512)];
    contains_subslice(window, b"__REALTIME_TIMESTAMP")
        || contains_subslice(window, b"\"PRIORITY\"")
}

fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_realtime_timestamp_and_priority() {
        let mut stats = ParseStats::default();
        let line =
            br#"{"__REALTIME_TIMESTAMP":"1717235400123456","PRIORITY":"6","MESSAGE":"hello"}"#;
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(ts, 1717235400123456);
        assert_eq!(sev, severity::INFO);
        assert_eq!(stats.untimed, 0);
    }

    #[test]
    fn priority_3_maps_to_error() {
        let mut stats = ParseStats::default();
        let line = br#"{"__REALTIME_TIMESTAMP":"1000","PRIORITY":"3","MESSAGE":"x"}"#;
        let (_, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(sev, severity::ERROR);
    }

    #[test]
    fn missing_priority_uses_unknown() {
        let mut stats = ParseStats::default();
        let line = br#"{"__REALTIME_TIMESTAMP":"1000","MESSAGE":"x"}"#;
        let (_, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(sev, severity::UNKNOWN);
    }

    #[test]
    fn projects_msg_and_level() {
        let line =
            br#"{"__REALTIME_TIMESTAMP":"1000","PRIORITY":"4","MESSAGE":"slow query"}"#;
        assert_eq!(
            project_field_journal(line, "msg").as_deref(),
            Some("slow query")
        );
        assert_eq!(
            project_field_journal(line, "level").as_deref(),
            Some("warn")
        );
    }

    #[test]
    fn projects_host_alias() {
        let line =
            br#"{"__REALTIME_TIMESTAMP":"1000","PRIORITY":"6","_HOSTNAME":"host01","MESSAGE":"x"}"#;
        assert_eq!(
            project_field_journal(line, "host").as_deref(),
            Some("host01")
        );
        assert_eq!(
            project_field_journal(line, "_HOSTNAME").as_deref(),
            Some("host01")
        );
    }

    #[test]
    fn looks_like_journalctl_matches() {
        assert!(looks_like_journalctl(
            br#"{"__REALTIME_TIMESTAMP":"1000","MESSAGE":"x"}"#
        ));
        assert!(looks_like_journalctl(
            br#"{"PRIORITY":"6","MESSAGE":"x"}"#
        ));
    }

    #[test]
    fn looks_like_journalctl_rejects_other_json() {
        assert!(!looks_like_journalctl(br#"{"ts":"2026-06-01","msg":"x"}"#));
        assert!(!looks_like_journalctl(b"plain text"));
    }
}
