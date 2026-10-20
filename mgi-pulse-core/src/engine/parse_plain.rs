//! Plain-text fallback: the common "timestamp LEVEL message" shapes
//! that have no dedicated parser.
//!
//! ```text
//! 2026-10-21T02:30:00+03:00 ERROR pulse: plain line
//! 2026-10-21 02:30:01,123 ERROR [main] pulse: log4j style
//! [2026-10-21 02:30:02.5] WARN worker: slow
//! 2026/10/21 02:30:03 [error] 1234#0: *5 connect() failed      (nginx)
//! 2026/10/21 02:30:04 listening on :8080                      (Go log)
//! ERROR:root:message                                           (Python basicConfig)
//! [WARN] disk almost full
//! ```
//!
//! The timestamp is optional and read as UTC when it has no zone. The
//! level is the first token after the timestamp in any case, or an
//! upper-case or bracketed level word among the next few tokens, so a
//! message that merely contains "error" in lower case is not promoted.

use crate::engine::parse::{FieldNames, ParseStats};
use crate::engine::record::{severity, TS_UNTIMED};

/// Tokens after the timestamp that may hold the level.
const LEVEL_WINDOW: usize = 5;

#[derive(Debug, Default, PartialEq)]
pub struct Parsed<'a> {
    pub ts: Option<i64>,
    pub ts_raw: Option<&'a str>,
    pub level: Option<&'a str>,
    pub severity: u8,
    pub thread: Option<&'a str>,
    pub logger: Option<&'a str>,
    pub msg: &'a str,
}

/// Map a level word to the engine's severity; `None` if it isn't one.
pub fn level_of(word: &str) -> Option<u8> {
    // Lower-case on the stack: this runs for several tokens per line.
    let b = word.as_bytes();
    if b.is_empty() || b.len() > 11 {
        return None;
    }
    let mut buf = [0u8; 11];
    for (d, s) in buf.iter_mut().zip(b) {
        *d = s.to_ascii_lowercase();
    }
    Some(match &buf[..b.len()] {
        b"trace" | b"finest" | b"finer" => severity::TRACE,
        b"debug" | b"dbg" | b"fine" => severity::DEBUG,
        b"info" | b"information" | b"notice" => severity::INFO,
        b"warn" | b"warning" => severity::WARN,
        b"error" | b"err" | b"eror" => severity::ERROR,
        b"fatal" | b"critical" | b"crit" | b"alert" | b"emerg" | b"emergency" | b"panic"
        | b"severe" => severity::FATAL,
        _ => return None,
    })
}

fn num(b: &[u8]) -> i64 {
    b.iter().fold(0, |acc, d| acc * 10 + (d - b'0') as i64)
}

/// Length of a leading timestamp: `YYYY-MM-DD` or `YYYY/MM/DD`, then
/// `T` or a space, `HH:MM:SS`, optional `.`/`,` fraction, optional `Z`
/// or numeric offset (`+03:00`, `+0300`). No zone means UTC. Returns
/// (bytes consumed, micros).
fn leading_ts(s: &str) -> Option<(usize, i64)> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    let sep = b[4];
    if !(digits(0..4)
        && (sep == b'-' || sep == b'/')
        && b[7] == sep
        && digits(5..7)
        && digits(8..10)
        && (b[10] == b'T' || b[10] == b' ')
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19))
    {
        return None;
    }
    let (year, month, day) = (num(&b[0..4]), num(&b[5..7]), num(&b[8..10]));
    let (hour, minute, second) = (num(&b[11..13]), num(&b[14..16]), num(&b[17..19]));
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut end = 19;
    let mut micros = 0i64;
    if end < b.len() && (b[end] == b'.' || b[end] == b',') {
        let start = end + 1;
        let mut j = start;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > start {
            let frac = &b[start..j.min(start + 6)];
            micros = num(frac) * 10i64.pow(6 - frac.len() as u32);
            end = j;
        }
    }
    let mut offset = 0i64;
    if end < b.len() && b[end] == b'Z' {
        end += 1;
    } else if end + 5 <= b.len() && (b[end] == b'+' || b[end] == b'-') {
        let sign = if b[end] == b'+' { 1 } else { -1 };
        let colon = end + 6 <= b.len()
            && b[end + 3] == b':'
            && digits(end + 1..end + 3)
            && digits(end + 4..end + 6);
        if colon {
            offset = sign * (num(&b[end + 1..end + 3]) * 3600 + num(&b[end + 4..end + 6]) * 60);
            end += 6;
        } else if digits(end + 1..end + 5) {
            offset = sign * (num(&b[end + 1..end + 3]) * 3600 + num(&b[end + 3..end + 5]) * 60);
            end += 5;
        }
    }
    let days = crate::engine::parse::days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some((end, secs * 1_000_000 + micros))
}

fn strip_wrap(tok: &str) -> (&str, bool) {
    let t = tok.trim_end_matches(':');
    for (open, close) in [('[', ']'), ('<', '>'), ('(', ')')] {
        if let Some(inner) = t.strip_prefix(open).and_then(|r| r.strip_suffix(close)) {
            return (inner, true);
        }
    }
    (t, false)
}

/// Split off the next whitespace-delimited token.
fn next_token(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    Some((&s[..end], &s[end..]))
}

pub fn parse(line: &str) -> Parsed<'_> {
    let mut out = Parsed {
        msg: line.trim(),
        ..Default::default()
    };
    let mut rest = line.trim_start();

    // Timestamp, possibly in brackets.
    let bracketed = rest.starts_with('[');
    let body = if bracketed { &rest[1..] } else { rest };
    // A comma or tab right after the time means CSV/TSV, not a log line.
    let ts = leading_ts(body)
        .filter(|(n, _)| matches!(body.as_bytes().get(*n), None | Some(b' ') | Some(b']')));
    if let Some((n, micros)) = ts {
        let mut after = &body[n..];
        if bracketed {
            match after.strip_prefix(']') {
                Some(a) => after = a,
                None => after = after.split_once(']').map_or(after, |(_, a)| a),
            }
        }
        out.ts = Some(micros);
        out.ts_raw = Some(&body[..n]);
        rest = after;
    }

    // Python basicConfig: `LEVEL:logger:message`.
    if out.ts.is_none() {
        if let Some((head, tail)) = rest.split_once(':') {
            if let Some(sev) = level_of(head) {
                if head.chars().all(|c| c.is_ascii_alphabetic()) && !tail.starts_with(' ') {
                    out.level = Some(head);
                    out.severity = sev;
                    match tail.split_once(':') {
                        Some((logger, msg)) if !logger.contains(' ') => {
                            out.logger = Some(logger);
                            out.msg = msg.trim();
                        }
                        _ => out.msg = tail.trim(),
                    }
                    return out;
                }
            }
        }
    }

    // Level: the first token in any case, or a loud/bracketed one soon after.
    let mut cursor = rest;
    for i in 0..LEVEL_WINDOW {
        let Some((tok, after)) = next_token(cursor) else {
            break;
        };
        let (word, wrapped) = strip_wrap(tok);
        let loud = word.len() > 1 && word.chars().all(|c| c.is_ascii_uppercase());
        if let Some(sev) = level_of(word) {
            if i == 0 || wrapped || loud {
                out.level = Some(word);
                out.severity = sev;
                cursor = after;
                break;
            }
        }
        cursor = after;
    }
    if out.level.is_none() {
        cursor = rest;
    }

    // `[thread]`, then `logger:` or `logger -`.
    let mut msg = cursor
        .trim_start()
        .trim_start_matches(['-', '|', ':'])
        .trim_start();
    if out.level.is_some() {
        if let Some((tok, after)) = next_token(msg) {
            if tok.starts_with('[') && tok.ends_with(']') && tok.len() > 2 {
                out.thread = Some(&tok[1..tok.len() - 1]);
                msg = after.trim_start();
            }
        }
        if let Some((tok, after)) = next_token(msg) {
            let after_t = after.trim_start();
            // `pulse:`, `com.example.App -`; not nginx's `1234#0:`.
            let name = tok.trim_end_matches(':');
            let named = name.len() > 1
                && name.starts_with(|c: char| c.is_ascii_alphabetic())
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '$'));
            if named && tok.ends_with(':') {
                out.logger = Some(tok.trim_end_matches(':'));
                msg = after_t;
            } else if named && after_t.starts_with("- ") {
                out.logger = Some(tok);
                msg = after_t[2..].trim_start();
            }
        }
    }
    out.msg = msg.trim_end();
    out
}

/// Detector vote: a leading timestamp, or a leading level word.
pub fn looks_like_plain(line: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(line) else {
        return false;
    };
    let p = parse(s);
    if p.ts.is_some() {
        return true;
    }
    // Without a timestamp, demand the level up front.
    p.level.is_some_and(|l| {
        let t = s.trim_start();
        t.starts_with(l) || t.strip_prefix(['[', '<']).is_some_and(|r| r.starts_with(l))
    })
}

pub fn ts_and_level(
    line: &[u8],
    stats: &mut ParseStats,
    _fields: Option<&FieldNames>,
) -> (i64, u8) {
    let s = String::from_utf8_lossy(line);
    let p = parse(&s);
    let ts = match p.ts {
        Some(t) => t,
        None => {
            stats.untimed += 1;
            TS_UNTIMED
        }
    };
    (ts, p.severity)
}

pub fn project_field(line: &[u8], key: &str) -> Option<String> {
    let s = String::from_utf8_lossy(line);
    let p = parse(&s);
    let v = match key {
        "msg" | "message" => Some(p.msg),
        "level" => return p.level.map(|_| severity::name(p.severity).to_string()),
        "ts" | "time" => p.ts_raw,
        "logger" => p.logger,
        "thread" => p.thread,
        _ => None,
    };
    v.filter(|v| !v.is_empty()).map(str::to_string)
}

/// A line that cannot open a record folds into the previous one
/// (stack traces, wrapped messages).
pub fn is_continuation(line: &[u8]) -> bool {
    !line.is_empty() && !looks_like_plain(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parse::parse_rfc3339_micros;

    fn p(s: &str) -> Parsed<'_> {
        parse(s)
    }

    #[test]
    fn iso_with_offset_and_logger() {
        let x = p("2026-10-21T02:30:00+03:00 ERROR pulse: plain line");
        assert_eq!(x.ts, parse_rfc3339_micros("2026-10-20T23:30:00Z"));
        assert_eq!(x.severity, severity::ERROR);
        assert_eq!(x.logger, Some("pulse"));
        assert_eq!(x.msg, "plain line");
    }

    #[test]
    fn log4j_style_with_comma_millis_and_thread() {
        let x = p("2026-10-21 02:30:01,123 ERROR [main] pulse: log4j style");
        assert_eq!(x.ts, parse_rfc3339_micros("2026-10-21T02:30:01.123Z"));
        assert_eq!(x.severity, severity::ERROR);
        assert_eq!(x.thread, Some("main"));
        assert_eq!(x.logger, Some("pulse"));
        assert_eq!(x.msg, "log4j style");
    }

    #[test]
    fn levels_in_any_case_and_aliases() {
        for (line, sev) in [
            ("2026-10-21 02:30:00 warning disk", severity::WARN),
            ("2026-10-21 02:30:00 Warn disk", severity::WARN),
            ("2026-10-21 02:30:00.5Z CRITICAL disk", severity::FATAL),
            ("2026-10-21 02:30:00 trace x", severity::TRACE),
            ("2026-10-21 02:30:00 debug x", severity::DEBUG),
            ("2026-10-21 02:30:00 FATAL x", severity::FATAL),
            ("2026-10-21 02:30:00 info x", severity::INFO),
        ] {
            assert_eq!(p(line).severity, sev, "{line}");
        }
    }

    #[test]
    fn bracketed_timestamp_and_level() {
        let x = p("[2026-10-21 02:30:02.5] [WARN] worker: slow");
        assert!(x.ts.is_some());
        assert_eq!(x.severity, severity::WARN);
        assert_eq!(x.logger, Some("worker"));
        assert_eq!(x.msg, "slow");
    }

    #[test]
    fn nginx_error_log() {
        let x =
            p("2026/10/21 02:30:03 [error] 1234#0: *5 connect() failed (111: Connection refused)");
        assert_eq!(x.ts, parse_rfc3339_micros("2026-10-21T02:30:03Z"));
        assert_eq!(x.severity, severity::ERROR);
        assert!(
            x.msg.starts_with("1234#0: *5 connect() failed"),
            "{}",
            x.msg
        );
        let c = p("2026/10/21 02:30:03 [crit] 1#0: worker died");
        assert_eq!(c.severity, severity::FATAL);
    }

    #[test]
    fn go_log_has_time_but_no_level() {
        let x = p("2026/10/21 02:30:04 listening on :8080");
        assert!(x.ts.is_some());
        assert_eq!(x.severity, severity::UNKNOWN);
        assert_eq!(x.msg, "listening on :8080");
        // A lower-case "error" deep in the message is not a level.
        let y = p("2026/10/21 02:30:04 retrying after error");
        assert_eq!(y.severity, severity::UNKNOWN);
    }

    #[test]
    fn python_basic_config() {
        let x = p("ERROR:root:something broke: details");
        assert_eq!(x.severity, severity::ERROR);
        assert_eq!(x.logger, Some("root"));
        assert_eq!(x.msg, "something broke: details");
        let w = p("WARNING:app.db:slow query");
        assert_eq!((w.severity, w.logger), (severity::WARN, Some("app.db")));
        assert!(looks_like_plain(b"INFO:root:started"));
    }

    #[test]
    fn python_default_formatter_dashes() {
        let x = p("2026-06-01 12:00:00,123 - app.db - ERROR - connection lost");
        assert_eq!(x.severity, severity::ERROR);
        assert_eq!(x.msg, "connection lost");
    }

    #[test]
    fn level_first_without_time() {
        let x = p("[WARN] disk almost full");
        assert_eq!(x.severity, severity::WARN);
        assert_eq!(x.ts, None);
        assert!(looks_like_plain(b"[WARN] disk almost full"));
        assert!(looks_like_plain(b"ERROR something"));
    }

    #[test]
    fn rejects_prose_and_json() {
        assert!(!looks_like_plain(b"just some text here"));
        assert!(!looks_like_plain(br#"{"level":"error"}"#));
        assert!(!looks_like_plain(b"   at com.example.Foo(Foo.java:12)"));
        assert!(is_continuation(b"   at com.example.Foo(Foo.java:12)"));
        assert!(!is_continuation(b"2026/10/21 02:30:04 next record"));
    }

    #[test]
    fn projections() {
        let line = b"2026-10-21 02:30:01,123 ERROR [main] pulse: log4j style";
        assert_eq!(project_field(line, "level").as_deref(), Some("error"));
        assert_eq!(project_field(line, "msg").as_deref(), Some("log4j style"));
        assert_eq!(project_field(line, "logger").as_deref(), Some("pulse"));
        assert_eq!(project_field(line, "thread").as_deref(), Some("main"));
        let mut st = ParseStats::default();
        let (ts, sev) = ts_and_level(line, &mut st, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::ERROR);
        let (ts, _) = ts_and_level(b"[WARN] x", &mut st, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(st.untimed, 1);
    }
}
