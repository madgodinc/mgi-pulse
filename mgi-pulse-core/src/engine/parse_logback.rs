//! Java logback / log4j2 default-format parser.
//!
//! Canonical shapes covered:
//!
//! ```text
//! 2026-06-01 12:00:00.123 INFO  [main] com.example.App - server started
//! 2026-06-01 12:00:01,456 ERROR [http-nio-8080-exec-1] c.e.Controller - DB timeout
//! ```
//!
//! - Logback default conversion pattern is
//!   `%d{HH:mm:ss.SSS} [%thread] %-5level %logger{36} - %msg%n` but the
//!   most common Spring Boot variant is the one above (date + space +
//!   `LEVEL`-padded + `[thread]` + ` - logger - msg`).
//! - log4j2 default ConsoleAppender: same shape, comma or dot before
//!   millis depending on locale. We accept both.
//! - Continuation lines for stack traces (`\tat com.example...`,
//!   `Caused by: ...`) do **not** start with a digit, so the same
//!   "first byte is not a digit" heuristic Python uses folds them in.
//!
//! Quirks vs Python:
//! - The first space-separated token after the timestamp is the
//!   level (no ` - ` separator like Python).
//! - The thread name is wrapped in `[brackets]` and may contain
//!   spaces (e.g. `[http-nio-8080-exec-1]`). We read it as a balanced
//!   bracket block.
//! - Logger name is everything between the closing `]` and the next
//!   ` - ` separator.
//! - Msg is everything after that ` - `.

use crate::engine::parse::{parse_rfc3339_micros, FieldNames, ParseStats};
use crate::engine::record::{severity, TS_UNTIMED};

struct Header<'a> {
    ts_raw: &'a [u8],
    level: &'a [u8],
    thread: &'a [u8],
    logger: &'a [u8],
    msg: &'a [u8],
}

/// Parse the header into typed slices. Returns `None` on malformed
/// input; callers treat that as a parse error.
fn parse_header(line: &[u8]) -> Option<Header<'_>> {
    // Timestamp: bytes 0..19 minimum (YYYY-MM-DD HH:MM:SS), optional
    // fractional ms after a `.` or `,`. Find the end by scanning to
    // the first SP after the seconds-or-millis.
    if line.len() < 20 {
        return None;
    }
    if line[10] != b' ' || line[4] != b'-' || line[7] != b'-' {
        return None;
    }
    // Find end of timestamp — first SP after position 19.
    let mut ts_end = 19;
    if ts_end < line.len() && (line[ts_end] == b'.' || line[ts_end] == b',') {
        ts_end += 1;
        while ts_end < line.len() && line[ts_end].is_ascii_digit() {
            ts_end += 1;
        }
    }
    if ts_end >= line.len() || line[ts_end] != b' ' {
        return None;
    }
    let ts_raw = &line[..ts_end];
    let mut i = ts_end + 1;

    // Level — alphabetic run, possibly padded with trailing spaces
    // when the format uses `%-5level`.
    let lvl_start = i;
    while i < line.len() && line[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == lvl_start {
        return None;
    }
    let level = &line[lvl_start..i];
    // Skip whitespace after the level.
    while i < line.len() && line[i] == b' ' {
        i += 1;
    }

    // Thread `[...]`. Logback emits it; if absent, fall back to
    // log4j2-without-thread shape (level then logger directly).
    let (thread, after_thread) = if i < line.len() && line[i] == b'[' {
        let bracket_end = match line[i..].iter().position(|&b| b == b']') {
            Some(p) => i + p,
            None => return None,
        };
        let thread = &line[i + 1..bracket_end];
        let mut j = bracket_end + 1;
        while j < line.len() && line[j] == b' ' {
            j += 1;
        }
        (thread, j)
    } else {
        (&[][..], i)
    };
    i = after_thread;

    // Logger + msg: everything up to the next ` - ` separator, then
    // the rest. If no separator, treat the whole tail as the msg.
    let sep = find_dash_separator(&line[i..]).map(|p| p + i);
    let (logger, msg) = match sep {
        Some(p) => (trim_ascii(&line[i..p]), &line[p + 3..]),
        None => (&[][..], &line[i..]),
    };

    Some(Header {
        ts_raw,
        level,
        thread,
        logger,
        msg,
    })
}

fn find_dash_separator(s: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 2 < s.len() {
        if s[i] == b' ' && s[i + 1] == b'-' && s[i + 2] == b' ' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let mut s = 0;
    let mut e = b.len();
    while s < e && b[s].is_ascii_whitespace() {
        s += 1;
    }
    while e > s && b[e - 1].is_ascii_whitespace() {
        e -= 1;
    }
    &b[s..e]
}

/// Normalize the logback timestamp (`YYYY-MM-DD HH:MM:SS[.,]mmm`) into
/// RFC3339 (`YYYY-MM-DDTHH:MM:SS.mmmZ`) and parse.
fn parse_logback_ts(prefix: &[u8]) -> Option<i64> {
    if prefix.len() < 19 {
        return None;
    }
    let mut buf = [0u8; 32];
    if prefix.len() >= buf.len() {
        return None;
    }
    buf[..prefix.len()].copy_from_slice(prefix);
    if buf[10] != b' ' {
        return None;
    }
    buf[10] = b'T';
    for b in &mut buf[..prefix.len()] {
        if *b == b',' {
            *b = b'.';
        }
    }
    buf[prefix.len()] = b'Z';
    let s = std::str::from_utf8(&buf[..prefix.len() + 1]).ok()?;
    parse_rfc3339_micros(s)
}

pub fn ts_and_level(
    line: &[u8],
    stats: &mut ParseStats,
    _fields: Option<&FieldNames>,
) -> (i64, u8) {
    let Some(h) = parse_header(line) else {
        stats.json_parse_errors += 1;
        stats.untimed += 1;
        return (TS_UNTIMED, severity::UNKNOWN);
    };
    let sev = severity::from_bytes(h.level);
    let ts = match parse_logback_ts(h.ts_raw) {
        Some(t) => t,
        None => {
            stats.ts_parse_errors += 1;
            stats.untimed += 1;
            return (TS_UNTIMED, sev);
        }
    };
    (ts, sev)
}

/// Field projection. Recognised keys: `level`, `thread`, `logger`,
/// `msg`, `ts`.
pub fn project_field(line: &[u8], key: &str) -> Option<String> {
    let h = parse_header(line)?;
    let out = match key {
        "level" => h.level,
        "thread" => h.thread,
        "logger" => h.logger,
        "msg" => h.msg,
        "ts" => h.ts_raw,
        _ => return None,
    };
    if out.is_empty() && key != "msg" {
        return None;
    }
    Some(String::from_utf8_lossy(out).into_owned())
}

/// Stack-trace continuation rule. Anything that doesn't open with a
/// digit can't start a fresh `YYYY-MM-DD` header, so it folds into
/// the previous record. Covers `\tat com.example...`,
/// `Caused by: ...`, `\t... 12 more`, etc.
pub fn is_continuation(line: &[u8]) -> bool {
    match line.first() {
        Some(&b) => !b.is_ascii_digit(),
        None => false,
    }
}

/// Heuristic for `LogFormat::detect`. Matches `YYYY-MM-DD HH:MM:SS`
/// followed by an alphabetic level token (INFO/WARN/ERROR/...). Goes
/// before the Python detector since Python lines have ` - ` after the
/// timestamp where logback has a level directly.
pub fn looks_like_logback(line: &[u8]) -> bool {
    if line.len() < 25 {
        return false;
    }
    if line[4] != b'-' || line[7] != b'-' || line[10] != b' ' || line[13] != b':' || line[16] != b':' {
        return false;
    }
    // Walk past optional fractional seconds.
    let mut i = 19;
    if i < line.len() && (line[i] == b'.' || line[i] == b',') {
        i += 1;
        while i < line.len() && line[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i >= line.len() || line[i] != b' ' {
        return false;
    }
    i += 1;
    // Then an alphabetic level token, not a logger-name `.`-shape
    // (which would be Python's ` - my.module - ` next).
    let lvl_start = i;
    while i < line.len() && line[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == lvl_start {
        return false;
    }
    let lvl = &line[lvl_start..i];
    matches!(
        lvl,
        b"INFO"
            | b"WARN"
            | b"WARNING"
            | b"ERROR"
            | b"DEBUG"
            | b"TRACE"
            | b"FATAL"
            | b"SEVERE"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_logback_line() {
        let mut stats = ParseStats::default();
        let line = b"2026-06-01 12:00:00.123 INFO  [main] com.example.App - server started";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::INFO);
        assert_eq!(stats.untimed, 0);
    }

    #[test]
    fn parses_log4j2_comma_millis() {
        let mut stats = ParseStats::default();
        let line = b"2026-06-01 12:00:00,123 ERROR [http-exec-1] c.e.Ctrl - boom";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::ERROR);
    }

    #[test]
    fn parses_line_without_thread_block() {
        // Some log4j2 patterns omit the thread.
        let mut stats = ParseStats::default();
        let line = b"2026-06-01 12:00:00.123 WARN com.example - slow query";
        let (_, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(sev, severity::WARN);
    }

    #[test]
    fn projects_thread_and_logger() {
        let line = b"2026-06-01 12:00:00.123 INFO  [http-nio-8080-exec-1] c.e.Controller - hi";
        assert_eq!(
            project_field(line, "thread").as_deref(),
            Some("http-nio-8080-exec-1")
        );
        assert_eq!(
            project_field(line, "logger").as_deref(),
            Some("c.e.Controller")
        );
        assert_eq!(project_field(line, "msg").as_deref(), Some("hi"));
        assert_eq!(project_field(line, "level").as_deref(), Some("INFO"));
    }

    #[test]
    fn stack_trace_is_continuation() {
        assert!(is_continuation(b"\tat com.example.App.main(App.java:42)"));
        assert!(is_continuation(b"Caused by: java.lang.NullPointerException"));
        assert!(is_continuation(b"  ... 12 more"));
        // A line that starts a fresh record (with a digit) is not a continuation.
        assert!(!is_continuation(
            b"2026-06-01 12:00:01.000 INFO [main] x - y"
        ));
    }

    #[test]
    fn malformed_increments_parse_errors() {
        let mut stats = ParseStats::default();
        let (ts, sev) = ts_and_level(b"this is not logback", &mut stats, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
        assert_eq!(stats.json_parse_errors, 1);
    }

    #[test]
    fn looks_like_logback_matches() {
        assert!(looks_like_logback(
            b"2026-06-01 12:00:00.123 INFO [main] x - y"
        ));
        assert!(looks_like_logback(
            b"2026-06-01 12:00:00,123 ERROR [t] x - y"
        ));
        assert!(looks_like_logback(b"2026-06-01 12:00:00 WARN x - y"));
    }

    #[test]
    fn looks_like_logback_rejects_python_and_other() {
        // Python has ` - module - ` not ` LEVEL `.
        assert!(!looks_like_logback(
            b"2026-06-01 12:00:00,123 - my.module - INFO - boom"
        ));
        assert!(!looks_like_logback(br#"{"a":1}"#));
        assert!(!looks_like_logback(
            b"<134>1 2026-06-01T12:00:00Z host app 1 - - hi"
        ));
    }
}
