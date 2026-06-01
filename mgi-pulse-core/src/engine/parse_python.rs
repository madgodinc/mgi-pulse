//! Python `logging` default-format parser.
//!
//! The canonical Python `logging.basicConfig()` line shape:
//!
//! ```text
//! 2026-06-01 12:00:00,123 - my.module - INFO - server started
//! 2026-06-01 12:00:01,456 - my.module - ERROR - DB timeout
//! Traceback (most recent call last):
//!   File "app.py", line 42, in main
//!     raise ValueError("bad")
//! ValueError: bad
//! ```
//!
//! Two quirks set this apart from logfmt / EDN:
//! - The fractional separator is a comma (`,`), per PEP 282. We
//!   normalize it to a dot before handing off to `parse_rfc3339_micros`,
//!   plus the timestamp uses a space instead of `T`.
//! - Continuation lines for tracebacks don't always start with
//!   whitespace — `Traceback (most recent call last):` doesn't, nor
//!   does the final `ValueError: bad`. We use a broader rule than
//!   logfmt: a continuation is any line whose first byte is **not** a
//!   digit (i.e. doesn't start a `YYYY-MM-DD ...` timestamp).

use crate::engine::parse::{parse_rfc3339_micros, FieldNames, ParseStats};
use crate::engine::record::{severity, TS_UNTIMED};

/// Normalize Python's `2026-06-01 12:00:00,123` format into something the
/// shared RFC3339 parser will accept, then call it. Returns `None` when
/// the prefix doesn't look like a Python timestamp.
fn parse_python_ts(prefix: &[u8]) -> Option<i64> {
    // We need at least `YYYY-MM-DD HH:MM:SS` = 19 bytes.
    if prefix.len() < 19 {
        return None;
    }
    let mut buf = [0u8; 32];
    if prefix.len() > buf.len() {
        return None;
    }
    buf[..prefix.len()].copy_from_slice(prefix);
    // Replace the space at index 10 with `T`.
    if buf[10] != b' ' {
        return None;
    }
    buf[10] = b'T';
    // Replace `,` with `.` if present (fractional seconds).
    for b in &mut buf[..prefix.len()] {
        if *b == b',' {
            *b = b'.';
        }
    }
    // Append `Z` so RFC3339 parser is happy.
    let len = prefix.len();
    if len >= buf.len() {
        return None;
    }
    buf[len] = b'Z';
    let s = std::str::from_utf8(&buf[..len + 1]).ok()?;
    parse_rfc3339_micros(s)
}

/// Parse one Python log line, extracting (ts, severity). Returns the
/// payload after the level when the caller asks for the `msg` field.
pub fn ts_and_level(
    line: &[u8],
    stats: &mut ParseStats,
    _fields: Option<&FieldNames>,
) -> (i64, u8) {
    // 1. Timestamp: bytes 0..N until the first " - " separator.
    let sep = match find_separator(line) {
        Some(i) => i,
        None => {
            stats.json_parse_errors += 1;
            stats.untimed += 1;
            return (TS_UNTIMED, severity::UNKNOWN);
        }
    };
    let ts_raw = &line[..sep];
    let ts = match parse_python_ts(ts_raw) {
        Some(v) => v,
        None => {
            // Not a Python-shaped timestamp — treat as untimed.
            stats.untimed += 1;
            stats.ts_parse_errors += 1;
            return (TS_UNTIMED, severity::UNKNOWN);
        }
    };

    // 2. Skip the " - " separator and find the logger name.
    let after_ts = &line[sep + 3..];
    let sep2 = match find_separator(after_ts) {
        Some(i) => i,
        None => {
            // No level field — keep the timestamp but mark severity unknown.
            return (ts, severity::UNKNOWN);
        }
    };

    // 3. Skip the logger and find the level.
    let after_logger = &after_ts[sep2 + 3..];
    let sep3 = find_separator(after_logger).unwrap_or(after_logger.len());
    let level_raw = trim_ascii(&after_logger[..sep3]);
    let sev = severity::from_bytes(level_raw);

    (ts, sev)
}

/// Find the first occurrence of " - " (the default Python `logging`
/// separator).
fn find_separator(line: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 2 < line.len() {
        if line[i] == b' ' && line[i + 1] == b'-' && line[i + 2] == b' ' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = b.len();
    while start < end && b[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && b[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &b[start..end]
}

/// True if `line` is a continuation of a previous Python log record.
/// Used by `LogFormat::is_continuation`. A line that doesn't start with
/// a digit can't open a new `YYYY-MM-DD` timestamp, so it must extend
/// the prior one.
pub fn is_continuation(line: &[u8]) -> bool {
    match line.first() {
        Some(&b) => !b.is_ascii_digit(),
        None => false,
    }
}

pub fn project_field(line: &[u8], key: &str) -> Option<String> {
    // Best-effort projection of a few well-known fields. Anything else
    // would require user-supplied field positions, which v0.1 doesn't
    // expose.
    let sep1 = find_separator(line)?;
    let after_ts = &line[sep1 + 3..];
    if key == "ts" {
        return std::str::from_utf8(&line[..sep1]).ok().map(String::from);
    }
    let sep2 = find_separator(after_ts)?;
    let logger = trim_ascii(&after_ts[..sep2]);
    if key == "logger" {
        return std::str::from_utf8(logger).ok().map(String::from);
    }
    let after_logger = &after_ts[sep2 + 3..];
    let sep3 = find_separator(after_logger).unwrap_or(after_logger.len());
    let level = trim_ascii(&after_logger[..sep3]);
    if key == "level" {
        return std::str::from_utf8(level).ok().map(String::from);
    }
    if key == "msg" {
        let rest = if sep3 < after_logger.len() {
            trim_ascii(&after_logger[sep3 + 3..])
        } else {
            &[]
        };
        return std::str::from_utf8(rest).ok().map(String::from);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_format() {
        let mut stats = ParseStats::default();
        let line = b"2026-06-01 12:00:00,123 - my.module - INFO - server started";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::INFO);
    }

    #[test]
    fn handles_millis_with_comma() {
        let mut stats = ParseStats::default();
        let line1 = b"2026-06-01 12:00:00,000 - x - DEBUG - a";
        let line2 = b"2026-06-01 12:00:01,500 - x - DEBUG - b";
        let (t1, _) = ts_and_level(line1, &mut stats, None);
        let (t2, _) = ts_and_level(line2, &mut stats, None);
        assert!(t2 > t1);
    }

    #[test]
    fn extracts_logger_and_message() {
        let line = b"2026-06-01 12:00:00,123 - my.module - INFO - server started";
        // logger/msg come back as bytes after the separator; leading
        // space is part of the field. Real callers (TUI) display them
        // verbatim, no trim.
        assert_eq!(
            project_field(line, "logger"),
            Some("my.module".to_string())
        );
        assert_eq!(project_field(line, "level"), Some("INFO".to_string()));
        assert_eq!(
            project_field(line, "msg"),
            Some("server started".to_string())
        );
    }

    #[test]
    fn rejects_non_python_line() {
        let mut stats = ParseStats::default();
        let line = b"this is not python at all";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
    }

    #[test]
    fn continuation_recognizes_traceback() {
        assert!(is_continuation(b"Traceback (most recent call last):"));
        assert!(is_continuation(b"  File \"app.py\", line 42, in main"));
        assert!(is_continuation(b"    raise ValueError(\"bad\")"));
        assert!(is_continuation(b"ValueError: bad"));
        // A new line that starts with a digit is a new record.
        assert!(!is_continuation(b"2026-06-01 12:00:00,123 - x - INFO - y"));
    }
}
