//! Syslog RFC 5424 parser.
//!
//! Reference: <https://datatracker.ietf.org/doc/html/rfc5424>.
//!
//! Line shape:
//!
//! ```text
//! <PRI>VERSION TIMESTAMP HOSTNAME APP-NAME PROCID MSGID STRUCTURED-DATA MSG
//! <134>1 2026-06-01T12:00:00.123Z host01 my.app 1234 - - server started
//! <131>1 2026-06-01T12:00:01.456Z host01 my.app 1234 audit [origin ip="10.0.0.1"] DB timeout
//! ```
//!
//! - PRI = facility*8 + severity. We extract the lower 3 bits as the
//!   syslog severity (0=emerg ... 7=debug) and map to the engine's
//!   `severity` enum. Facility is ignored.
//! - VERSION is always `1` in 5424. We don't validate it strictly —
//!   anything that parses as a digit run is accepted.
//! - TIMESTAMP is RFC3339 with optional fractional seconds and a
//!   timezone. NILVALUE (`-`) means "no timestamp" → untimed.
//! - The non-MSG header fields (HOSTNAME, APP-NAME, PROCID, MSGID) are
//!   space-separated bare tokens; `-` is the NILVALUE convention and
//!   exposed as `None` from `project_field`.
//! - STRUCTURED-DATA is `[SD-ID key="value" key="value"][SD-ID ...]` or
//!   the NILVALUE `-`. Multiple SD blocks may appear. Inside the
//!   brackets, values are quoted, and `\\`, `\"`, `\]` are the only
//!   escapes (per the RFC). We expose every `SD-ID.key` flat (e.g.
//!   `origin.ip`) plus the bare `SD-ID` membership via `project_field`.
//! - MSG is everything after STRUCTURED-DATA, including embedded
//!   whitespace. May be empty.
//!
//! Non-goals (deliberate):
//!
//! - RFC 3164 BSD syslog (no version, ambiguous timestamp, no SD). It's
//!   a different parser and has no real timezone; would need its own
//!   issue.
//! - BOM stripping. The RFC allows a UTF-8 BOM at the start of MSG to
//!   announce UTF-8 — virtually nobody emits it, and the MSG passes
//!   through as raw bytes so it doesn't matter for our pipeline.

use crate::engine::parse::{parse_rfc3339_micros, FieldNames, ParseStats};
use crate::engine::record::{severity, TS_UNTIMED};

/// Map a syslog severity (lower 3 bits of PRI) onto the engine's
/// severity enum. Notice (5) collapses into INFO — the distinction
/// isn't surfaced anywhere in the UI and `notice` doesn't carry a
/// useful operator meaning.
fn map_syslog_severity(sysv: u8) -> u8 {
    match sysv {
        0 | 1 | 2 => severity::FATAL, // emerg / alert / crit
        3 => severity::ERROR,         // err
        4 => severity::WARN,          // warning
        5 | 6 => severity::INFO,      // notice / info
        7 => severity::DEBUG,
        _ => severity::UNKNOWN,
    }
}

/// Header layout broken out for field projection. Borrows from the
/// input line; lifetimes are confined to one record's evaluation.
struct Header<'a> {
    severity_byte: u8,
    timestamp: &'a [u8],
    hostname: &'a [u8],
    app: &'a [u8],
    procid: &'a [u8],
    msgid: &'a [u8],
    /// Slice of STRUCTURED-DATA, including outer brackets. Empty if
    /// the field was the NILVALUE `-`.
    sd: &'a [u8],
    /// MSG (after STRUCTURED-DATA + optional separating space).
    msg: &'a [u8],
}

/// Parse the header into typed slices. Returns `None` on malformed
/// input; the caller treats this as a parse error and the record
/// lands in the untimed / unknown bucket.
fn parse_header(line: &[u8]) -> Option<Header<'_>> {
    let mut i = 0;
    if line.first() != Some(&b'<') {
        return None;
    }
    i += 1;
    // PRI digits, then `>`.
    let pri_start = i;
    while i < line.len() && line[i].is_ascii_digit() {
        i += 1;
    }
    if i == pri_start || i >= line.len() || line[i] != b'>' {
        return None;
    }
    let pri: u32 = std::str::from_utf8(&line[pri_start..i])
        .ok()
        .and_then(|s| s.parse().ok())?;
    let severity_byte = map_syslog_severity((pri & 0b111) as u8);
    i += 1; // past `>`

    // VERSION — one or more digits. Followed by SP.
    let v_start = i;
    while i < line.len() && line[i].is_ascii_digit() {
        i += 1;
    }
    if i == v_start {
        return None;
    }
    if i >= line.len() || line[i] != b' ' {
        return None;
    }
    i += 1;

    // Five space-separated header fields. The RFC bans SP inside
    // any of them, so a single `next_sp_token` works for all.
    let timestamp = next_sp_token(line, &mut i)?;
    let hostname = next_sp_token(line, &mut i)?;
    let app = next_sp_token(line, &mut i)?;
    let procid = next_sp_token(line, &mut i)?;
    let msgid = next_sp_token(line, &mut i)?;

    // STRUCTURED-DATA: either NILVALUE `-` or one-or-more `[...]`
    // blocks. We read up to the next SP outside the brackets to
    // delimit the whole SD region.
    let (sd, after_sd) = read_structured_data(line, i)?;
    i = after_sd;

    // Optional separating SP, then MSG (the rest).
    if i < line.len() && line[i] == b' ' {
        i += 1;
    }
    let msg = &line[i..];

    Some(Header {
        severity_byte,
        timestamp,
        hostname,
        app,
        procid,
        msgid,
        sd,
        msg,
    })
}

/// Consume one SP-delimited token starting at `*i`. Advances `*i` past
/// the trailing space. Returns the token slice (may equal `b"-"` for
/// NILVALUE).
fn next_sp_token<'a>(line: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
    let start = *i;
    while *i < line.len() && line[*i] != b' ' {
        *i += 1;
    }
    if *i == start {
        return None;
    }
    let tok = &line[start..*i];
    if *i < line.len() && line[*i] == b' ' {
        *i += 1;
    }
    Some(tok)
}

/// Read the STRUCTURED-DATA region. Either:
///
/// - `-` (NILVALUE) followed by SP-or-EOL; returns `(b"", new_pos)`.
/// - One or more `[...]` blocks back-to-back; returns the slice from
///   the opening `[` up to (but not including) the trailing SP or EOL.
///
/// Returns `None` if neither pattern matches.
fn read_structured_data(line: &[u8], start: usize) -> Option<(&[u8], usize)> {
    if start >= line.len() {
        return Some((&[], start));
    }
    if line[start] == b'-' {
        // NILVALUE — must be followed by SP or end of line.
        let next = start + 1;
        if next >= line.len() || line[next] == b' ' {
            return Some((&[], next));
        }
        return None;
    }
    if line[start] != b'[' {
        return None;
    }
    let mut i = start;
    while i < line.len() && line[i] == b'[' {
        // Skip past the closing `]`, honouring `\]` escapes.
        i += 1;
        while i < line.len() && line[i] != b']' {
            if line[i] == b'\\' && i + 1 < line.len() {
                i += 2;
                continue;
            }
            i += 1;
        }
        if i >= line.len() {
            return None;
        }
        i += 1; // past `]`
    }
    Some((&line[start..i], i))
}

/// Hot path. Mirrors the other parsers' contract: returns `(ts, sev)`
/// and updates `stats` for untimed / parse-error counters. Field-name
/// overrides have no effect — RFC 5424 has fixed field positions, not
/// names.
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
    if h.timestamp == b"-" {
        stats.untimed += 1;
        return (TS_UNTIMED, h.severity_byte);
    }
    let ts_str = match std::str::from_utf8(h.timestamp) {
        Ok(s) => s,
        Err(_) => {
            stats.ts_parse_errors += 1;
            stats.untimed += 1;
            return (TS_UNTIMED, h.severity_byte);
        }
    };
    match parse_rfc3339_micros(ts_str) {
        Some(t) => (t, h.severity_byte),
        None => {
            stats.ts_parse_errors += 1;
            stats.untimed += 1;
            (TS_UNTIMED, h.severity_byte)
        }
    }
}

/// Field projection for predicates. Recognised keys:
///
/// - `host`, `app`, `procid`, `msgid` — header fields. NILVALUE `-`
///   returns `None`.
/// - `msg` — the MSG portion (may be empty).
/// - `ts` — the RFC3339 timestamp string (for `ts>=...` DSL queries
///   that compare lexicographically; the engine's `ts_micros` is the
///   authoritative form).
/// - `level` — the human name of the severity, lowercase.
/// - `<SD-ID>.<key>` — a structured-data parameter. Multiple SD-ID
///   blocks may exist; the first match wins.
/// - `<SD-ID>` (bare) — empty string if the block is present, else
///   `None`. Lets predicates test for "did this record carry an
///   `audit` block?" with `audit=""`.
pub fn project_field(line: &[u8], key: &str) -> Option<String> {
    let h = parse_header(line)?;
    match key {
        "host" => nil_to_none(h.hostname).map(|b| String::from_utf8_lossy(b).into_owned()),
        "app" => nil_to_none(h.app).map(|b| String::from_utf8_lossy(b).into_owned()),
        "procid" => nil_to_none(h.procid).map(|b| String::from_utf8_lossy(b).into_owned()),
        "msgid" => nil_to_none(h.msgid).map(|b| String::from_utf8_lossy(b).into_owned()),
        "msg" => Some(String::from_utf8_lossy(h.msg).into_owned()),
        "ts" => {
            if h.timestamp == b"-" {
                None
            } else {
                Some(String::from_utf8_lossy(h.timestamp).into_owned())
            }
        }
        "level" => Some(severity::name(h.severity_byte).to_lowercase()),
        _ => project_structured_data(h.sd, key),
    }
}

fn nil_to_none(b: &[u8]) -> Option<&[u8]> {
    if b == b"-" {
        None
    } else {
        Some(b)
    }
}

/// Walk the SD blocks looking for either `<id>.<param>` or a bare
/// `<id>` membership probe.
fn project_structured_data(sd: &[u8], key: &str) -> Option<String> {
    if sd.is_empty() {
        return None;
    }
    let dot = key.find('.');
    let (want_id, want_key) = match dot {
        Some(i) => (&key[..i], Some(&key[i + 1..])),
        None => (key, None),
    };
    let mut i = 0;
    while i < sd.len() {
        if sd[i] != b'[' {
            return None;
        }
        i += 1;
        // Read SD-ID up to SP or `]`.
        let id_start = i;
        while i < sd.len() && sd[i] != b' ' && sd[i] != b']' {
            i += 1;
        }
        let id = &sd[id_start..i];
        if id == want_id.as_bytes() {
            // Hit. Either return "" for membership probe or find the
            // requested key.
            let Some(wk) = want_key else {
                return Some(String::new());
            };
            // Walk params until `]`.
            while i < sd.len() && sd[i] != b']' {
                // Skip SP.
                while i < sd.len() && sd[i] == b' ' {
                    i += 1;
                }
                if i >= sd.len() || sd[i] == b']' {
                    break;
                }
                let pk_start = i;
                while i < sd.len() && sd[i] != b'=' && sd[i] != b']' && sd[i] != b' ' {
                    i += 1;
                }
                let pk = &sd[pk_start..i];
                if i >= sd.len() || sd[i] != b'=' {
                    return None;
                }
                i += 1; // past `=`
                if i >= sd.len() || sd[i] != b'"' {
                    return None;
                }
                i += 1; // past `"`
                let mut value = Vec::new();
                while i < sd.len() && sd[i] != b'"' {
                    if sd[i] == b'\\' && i + 1 < sd.len() {
                        // RFC 5424 §6.3.3: only `"`, `\`, `]` are escapable.
                        value.push(sd[i + 1]);
                        i += 2;
                    } else {
                        value.push(sd[i]);
                        i += 1;
                    }
                }
                if i < sd.len() {
                    i += 1; // past closing `"`
                }
                if pk == wk.as_bytes() {
                    return Some(String::from_utf8_lossy(&value).into_owned());
                }
            }
            // Block matched the SD-ID but didn't contain the key.
            return None;
        }
        // Different SD-ID — skip to its closing `]`.
        while i < sd.len() && sd[i] != b']' {
            if sd[i] == b'\\' && i + 1 < sd.len() {
                i += 2;
                continue;
            }
            i += 1;
        }
        if i < sd.len() {
            i += 1; // past `]`
        }
    }
    None
}

/// Heuristic for `LogFormat::detect`. A line matches syslog 5424 if it
/// starts with `<DIGITS>1 ` (PRI followed by version 1 + SP).
pub fn looks_like_syslog(line: &[u8]) -> bool {
    if line.first() != Some(&b'<') {
        return false;
    }
    let mut i = 1;
    while i < line.len() && line[i].is_ascii_digit() {
        i += 1;
    }
    if i == 1 || i >= line.len() || line[i] != b'>' {
        return false;
    }
    i += 1;
    // Version digits + SP.
    let v_start = i;
    while i < line.len() && line[i].is_ascii_digit() {
        i += 1;
    }
    if i == v_start || i >= line.len() || line[i] != b' ' {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_5424_line() {
        let mut stats = ParseStats::default();
        // PRI=134 → facility=16 (local0), severity=6 (info)
        let line = b"<134>1 2026-06-01T12:00:00.123Z host01 my.app 1234 - - server started";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::INFO);
        assert_eq!(stats.untimed, 0);
    }

    #[test]
    fn pri_severity_3_maps_to_error() {
        let mut stats = ParseStats::default();
        // PRI=131 → severity=3 (err)
        let line = b"<131>1 2026-06-01T12:00:00Z host app 1 - - DB timeout";
        let (_, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(sev, severity::ERROR);
    }

    #[test]
    fn pri_severity_2_maps_to_fatal() {
        let mut stats = ParseStats::default();
        // PRI=2 → severity=2 (crit)
        let line = b"<2>1 2026-06-01T12:00:00Z host app 1 - - heap exhausted";
        let (_, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(sev, severity::FATAL);
    }

    #[test]
    fn pri_severity_7_maps_to_debug() {
        let mut stats = ParseStats::default();
        let line = b"<15>1 2026-06-01T12:00:00Z host app 1 - - verbose";
        let (_, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(sev, severity::DEBUG);
    }

    #[test]
    fn nilvalue_timestamp_lands_in_untimed() {
        let mut stats = ParseStats::default();
        let line = b"<134>1 - host app 1 - - bootstrap";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::INFO);
        assert_eq!(stats.untimed, 1);
        assert_eq!(stats.ts_parse_errors, 0);
    }

    #[test]
    fn malformed_increments_parse_errors() {
        let mut stats = ParseStats::default();
        // No `<` at start — not even a syslog frame.
        let line = b"this is not syslog";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
        assert_eq!(stats.json_parse_errors, 1);
    }

    #[test]
    fn projects_header_fields() {
        let line = b"<134>1 2026-06-01T12:00:00Z host01 my.app 1234 audit - server started";
        assert_eq!(project_field(line, "host").as_deref(), Some("host01"));
        assert_eq!(project_field(line, "app").as_deref(), Some("my.app"));
        assert_eq!(project_field(line, "procid").as_deref(), Some("1234"));
        assert_eq!(project_field(line, "msgid").as_deref(), Some("audit"));
    }

    #[test]
    fn nilvalue_header_field_is_none() {
        let line = b"<134>1 2026-06-01T12:00:00Z - - - - - bare msg";
        assert!(project_field(line, "host").is_none());
        assert!(project_field(line, "app").is_none());
        assert!(project_field(line, "procid").is_none());
        assert!(project_field(line, "msgid").is_none());
    }

    #[test]
    fn projects_msg() {
        let line = b"<134>1 2026-06-01T12:00:00Z host app 1 - - hello world";
        assert_eq!(project_field(line, "msg").as_deref(), Some("hello world"));
    }

    #[test]
    fn projects_level_name() {
        let line = b"<131>1 2026-06-01T12:00:00Z host app 1 - - boom";
        assert_eq!(project_field(line, "level").as_deref(), Some("error"));
    }

    #[test]
    fn projects_structured_data_value() {
        // PRI=134, SD = [origin ip="10.0.0.1" port="443"]
        let line =
            b"<134>1 2026-06-01T12:00:00Z host app 1 - [origin ip=\"10.0.0.1\" port=\"443\"] hi";
        assert_eq!(
            project_field(line, "origin.ip").as_deref(),
            Some("10.0.0.1")
        );
        assert_eq!(
            project_field(line, "origin.port").as_deref(),
            Some("443")
        );
    }

    #[test]
    fn structured_data_membership_probe_returns_empty_string() {
        let line = b"<134>1 2026-06-01T12:00:00Z host app 1 - [audit src=\"x\"] hi";
        // Bare `audit` exists → empty string (truthy for `audit=""` probes).
        assert_eq!(project_field(line, "audit").as_deref(), Some(""));
        // Missing block.
        assert!(project_field(line, "telemetry").is_none());
    }

    #[test]
    fn structured_data_escape_inside_value() {
        let line = b"<134>1 2026-06-01T12:00:00Z host app 1 - [exo msg=\"with \\\"quote\\\"\"] hi";
        assert_eq!(
            project_field(line, "exo.msg").as_deref(),
            Some(r#"with "quote""#)
        );
    }

    #[test]
    fn multiple_sd_blocks_first_match_wins() {
        let line = b"<134>1 2026-06-01T12:00:00Z host app 1 - [a x=\"1\"][b y=\"2\"] hi";
        assert_eq!(project_field(line, "a.x").as_deref(), Some("1"));
        assert_eq!(project_field(line, "b.y").as_deref(), Some("2"));
    }

    #[test]
    fn empty_msg_is_supported() {
        let line = b"<134>1 2026-06-01T12:00:00Z host app 1 - -";
        let mut stats = ParseStats::default();
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::INFO);
        assert_eq!(project_field(line, "msg").as_deref(), Some(""));
    }

    #[test]
    fn looks_like_syslog_matches_5424_frame() {
        assert!(looks_like_syslog(b"<134>1 2026-06-01T12:00:00Z host app 1 - - x"));
        assert!(looks_like_syslog(b"<0>1 - host app 1 - - x"));
    }

    #[test]
    fn looks_like_syslog_rejects_non_frames() {
        assert!(!looks_like_syslog(b"{\"a\":1}"));
        assert!(!looks_like_syslog(b"foo=bar"));
        assert!(!looks_like_syslog(b"<no-pri-digits>"));
        assert!(!looks_like_syslog(b"<134> missing version"));
    }
}
