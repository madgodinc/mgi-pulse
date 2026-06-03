//! Apache / nginx access log parser (Common Log Format + Combined).
//!
//! Reference: <https://httpd.apache.org/docs/current/logs.html#accesslog>.
//!
//! Common Log Format (CLF):
//!
//! ```text
//! 1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET /api HTTP/1.1" 200 1234
//! ```
//!
//! Combined adds the referer and user-agent fields:
//!
//! ```text
//! 1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET /api HTTP/1.1" 200 1234 "-" "curl/8.0"
//! ```
//!
//! Layout:
//!
//! - `remote_host` — bare token to SP.
//! - `remote_logname` — `-` or token.
//! - `remote_user` — `-` or token (no SP).
//! - `[timestamp]` — `[DD/MMM/YYYY:HH:MM:SS ±HHMM]`.
//! - `"request"` — quoted, `\"` allowed (Apache does emit this).
//! - `status` — integer.
//! - `bytes` — integer or `-` (when no body, mod_log_config writes `-`).
//! - Combined only: `"referer"`, `"user_agent"` — quoted.
//!
//! Severity is synthesised from status: 5xx → ERROR, 4xx → WARN,
//! 2xx/3xx → INFO, anything outside 100-599 → UNKNOWN. There is no
//! native "level" in the line.

use crate::engine::parse::{parse_rfc3339_micros, FieldNames, ParseStats};
use crate::engine::record::{severity, TS_UNTIMED};

/// Header layout broken out for field projection.
struct Parsed<'a> {
    host: &'a [u8],
    logname: &'a [u8],
    user: &'a [u8],
    /// Raw timestamp bytes inside the brackets, without the brackets.
    /// E.g. `01/Jun/2026:12:00:00 +0000`.
    ts_raw: &'a [u8],
    /// Request line, dequoted but not parsed further.
    request: Vec<u8>,
    status: &'a [u8],
    /// `-` (no body) or a numeric byte count, as a slice.
    bytes_field: &'a [u8],
    /// Combined-only. None on CLF.
    referer: Option<Vec<u8>>,
    /// Combined-only. None on CLF.
    user_agent: Option<Vec<u8>>,
}

fn parse_line(line: &[u8]) -> Option<Parsed<'_>> {
    let mut i = 0;
    let host = next_sp_token(line, &mut i)?;
    let logname = next_sp_token(line, &mut i)?;
    let user = next_sp_token(line, &mut i)?;

    // [timestamp ...] block.
    if i >= line.len() || line[i] != b'[' {
        return None;
    }
    i += 1;
    let ts_start = i;
    while i < line.len() && line[i] != b']' {
        i += 1;
    }
    if i >= line.len() {
        return None;
    }
    let ts_raw = &line[ts_start..i];
    i += 1; // past ]
    if i < line.len() && line[i] == b' ' {
        i += 1;
    }

    // "request" — quoted with `\"` escapes.
    let request = read_quoted(line, &mut i)?;
    if i < line.len() && line[i] == b' ' {
        i += 1;
    }

    let status = next_sp_token(line, &mut i)?;
    let bytes_field = next_sp_token_or_eof(line, &mut i);

    // Combined: optional "referer" "user_agent".
    let referer = if i < line.len() && line[i] == b'"' {
        Some(read_quoted(line, &mut i)?)
    } else {
        None
    };
    if i < line.len() && line[i] == b' ' {
        i += 1;
    }
    let user_agent = if i < line.len() && line[i] == b'"' {
        Some(read_quoted(line, &mut i)?)
    } else {
        None
    };

    Some(Parsed {
        host,
        logname,
        user,
        ts_raw,
        request,
        status,
        bytes_field,
        referer,
        user_agent,
    })
}

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

/// Same as `next_sp_token` but accepts EOF as a valid terminator and
/// returns an empty slice if there's nothing to read. Used for the
/// `bytes` field which can be the last token on a CLF line.
fn next_sp_token_or_eof<'a>(line: &'a [u8], i: &mut usize) -> &'a [u8] {
    let start = *i;
    while *i < line.len() && line[*i] != b' ' {
        *i += 1;
    }
    let tok = &line[start..*i];
    if *i < line.len() && line[*i] == b' ' {
        *i += 1;
    }
    tok
}

/// Read a `"..."` block from `*i`. Honours `\"` and `\\` escapes.
/// Advances `*i` past the closing quote. Returns the unescaped bytes
/// or `None` if the open quote isn't found / line ends mid-quote.
fn read_quoted(line: &[u8], i: &mut usize) -> Option<Vec<u8>> {
    if *i >= line.len() || line[*i] != b'"' {
        return None;
    }
    *i += 1;
    let mut out = Vec::new();
    while *i < line.len() {
        if line[*i] == b'\\' && *i + 1 < line.len() {
            out.push(line[*i + 1]);
            *i += 2;
            continue;
        }
        if line[*i] == b'"' {
            *i += 1;
            return Some(out);
        }
        out.push(line[*i]);
        *i += 1;
    }
    None
}

/// Apache time format: `01/Jun/2026:12:00:00 +0000`. Convert to RFC3339
/// (`2026-06-01T12:00:00+00:00`) and let the shared parser take it.
/// Returns `None` on any malformed component.
fn parse_apache_time(b: &[u8]) -> Option<i64> {
    // Need at least DD/MMM/YYYY:HH:MM:SS ±HHMM (26 bytes).
    if b.len() < 26 {
        return None;
    }
    if b[2] != b'/' || b[6] != b'/' || b[11] != b':' || b[14] != b':' || b[17] != b':' {
        return None;
    }
    let dd = ascii_to_str(&b[0..2])?;
    let mmm = ascii_to_str(&b[3..6])?;
    let yyyy = ascii_to_str(&b[7..11])?;
    let hh = ascii_to_str(&b[12..14])?;
    let mm = ascii_to_str(&b[15..17])?;
    let ss = ascii_to_str(&b[18..20])?;
    // After the seconds we expect ` +HHMM` or ` -HHMM`.
    if b[20] != b' ' {
        return None;
    }
    let sign = b[21];
    if sign != b'+' && sign != b'-' {
        return None;
    }
    let tz_hh = ascii_to_str(&b[22..24])?;
    let tz_mm = ascii_to_str(&b[24..26])?;
    let mon = month_num(mmm)?;
    let rfc = format!(
        "{}-{:02}-{}T{}:{}:{}{}{}:{}",
        yyyy,
        mon,
        dd,
        hh,
        mm,
        ss,
        sign as char,
        tz_hh,
        tz_mm,
    );
    parse_rfc3339_micros(&rfc)
}

fn ascii_to_str(b: &[u8]) -> Option<&str> {
    std::str::from_utf8(b).ok()
}

fn month_num(name: &str) -> Option<u8> {
    Some(match name {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

/// Synthesize severity from an HTTP status code byte slice.
fn severity_from_status(s: &[u8]) -> u8 {
    if s.is_empty() {
        return severity::UNKNOWN;
    }
    match s[0] {
        b'5' if s.len() == 3 => severity::ERROR,
        b'4' if s.len() == 3 => severity::WARN,
        b'2' | b'3' if s.len() == 3 => severity::INFO,
        _ => severity::UNKNOWN,
    }
}

pub fn ts_and_level(
    line: &[u8],
    stats: &mut ParseStats,
    _fields: Option<&FieldNames>,
) -> (i64, u8) {
    let Some(p) = parse_line(line) else {
        stats.json_parse_errors += 1;
        stats.untimed += 1;
        return (TS_UNTIMED, severity::UNKNOWN);
    };
    let sev = severity_from_status(p.status);
    match parse_apache_time(p.ts_raw) {
        Some(t) => (t, sev),
        None => {
            stats.ts_parse_errors += 1;
            stats.untimed += 1;
            (TS_UNTIMED, sev)
        }
    }
}

/// Field projection. Recognised keys:
///
/// - `ip` / `host` — the remote IP (`%h`).
/// - `logname` / `user` — `%l` and `%u`. NILVALUE (`-`) → None.
/// - `request` — the dequoted request line (method + URI + protocol).
/// - `method`, `uri`, `protocol` — split of `request` on the first
///   two spaces. Useful for `method=POST` predicates.
/// - `status` — 3-digit HTTP status as a string.
/// - `bytes` — response size, or None when the line had `-`.
/// - `referer`, `user_agent` — combined-only fields. None on CLF.
/// - `level` — synthesized severity name (`error`, `warn`, `info`,
///   `unknown`) from the status.
pub fn project_field(line: &[u8], key: &str) -> Option<String> {
    let p = parse_line(line)?;
    match key {
        "ip" | "host" => Some(String::from_utf8_lossy(p.host).into_owned()),
        "logname" => nil_to_str(p.logname),
        "user" => nil_to_str(p.user),
        "request" => Some(String::from_utf8_lossy(&p.request).into_owned()),
        "method" | "uri" | "protocol" => split_request(&p.request, key),
        "status" => Some(String::from_utf8_lossy(p.status).into_owned()),
        "bytes" => nil_to_str(p.bytes_field),
        "referer" => p
            .referer
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned()),
        "user_agent" => p
            .user_agent
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned()),
        "level" => Some(severity::name(severity_from_status(p.status)).to_lowercase()),
        _ => None,
    }
}

fn nil_to_str(b: &[u8]) -> Option<String> {
    if b == b"-" {
        None
    } else {
        Some(String::from_utf8_lossy(b).into_owned())
    }
}

fn split_request(req: &[u8], key: &str) -> Option<String> {
    let s = std::str::from_utf8(req).ok()?;
    let mut it = s.splitn(3, ' ');
    let method = it.next()?;
    let uri = it.next()?;
    let protocol = it.next()?;
    Some(
        match key {
            "method" => method,
            "uri" => uri,
            "protocol" => protocol,
            _ => return None,
        }
        .to_string(),
    )
}

/// Heuristic for `LogFormat::detect`. A line looks like access log if
/// it starts with what looks like an IP / hostname, has two `-` or
/// short tokens, and an `[...]` block early on.
pub fn looks_like_access(line: &[u8]) -> bool {
    // Find the first `[`; needs to appear in roughly the right spot
    // (after at least three space-separated tokens).
    let mut spaces = 0;
    let mut i = 0;
    while i < line.len() && i < 80 {
        if line[i] == b' ' {
            spaces += 1;
        }
        if line[i] == b'[' && spaces >= 3 {
            // Look ahead for `]` within ~30 bytes — Apache timestamp
            // is exactly 26 chars.
            let end = (i + 32).min(line.len());
            if line[i..end].iter().any(|&b| b == b']') {
                return true;
            }
            return false;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_log_format() {
        let mut stats = ParseStats::default();
        let line = br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET /api HTTP/1.1" 200 1234"#;
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::INFO);
        assert_eq!(stats.untimed, 0);
    }

    #[test]
    fn parses_combined_log_format() {
        let mut stats = ParseStats::default();
        let line = br#"1.2.3.4 - - [01/Jun/2026:12:00:01 +0000] "POST /login HTTP/1.1" 401 89 "-" "curl/8.0""#;
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert!(ts > 0);
        assert_eq!(sev, severity::WARN);
        assert_eq!(stats.untimed, 0);
    }

    #[test]
    fn status_5xx_maps_to_error() {
        let mut stats = ParseStats::default();
        let line = br#"10.0.0.1 - - [01/Jun/2026:12:00:00 +0000] "GET / HTTP/1.1" 502 0"#;
        let (_, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(sev, severity::ERROR);
    }

    #[test]
    fn malformed_increments_parse_errors() {
        let mut stats = ParseStats::default();
        let line = b"this is not access log";
        let (ts, sev) = ts_and_level(line, &mut stats, None);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
        assert_eq!(stats.json_parse_errors, 1);
    }

    #[test]
    fn projects_ip_status_bytes() {
        let line = br#"1.2.3.4 - admin [01/Jun/2026:12:00:00 +0000] "GET /api HTTP/1.1" 200 1234"#;
        assert_eq!(project_field(line, "ip").as_deref(), Some("1.2.3.4"));
        assert_eq!(project_field(line, "host").as_deref(), Some("1.2.3.4"));
        assert_eq!(project_field(line, "user").as_deref(), Some("admin"));
        assert_eq!(project_field(line, "status").as_deref(), Some("200"));
        assert_eq!(project_field(line, "bytes").as_deref(), Some("1234"));
    }

    #[test]
    fn nilvalue_bytes_becomes_none() {
        let line = br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET / HTTP/1.1" 304 -"#;
        assert!(project_field(line, "bytes").is_none());
    }

    #[test]
    fn projects_method_uri_protocol() {
        let line = br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "POST /api/v1/users HTTP/2" 201 0"#;
        assert_eq!(project_field(line, "method").as_deref(), Some("POST"));
        assert_eq!(
            project_field(line, "uri").as_deref(),
            Some("/api/v1/users")
        );
        assert_eq!(project_field(line, "protocol").as_deref(), Some("HTTP/2"));
    }

    #[test]
    fn projects_referer_and_user_agent_on_combined() {
        let line = br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET / HTTP/1.1" 200 5 "https://e.x/" "curl/8.0""#;
        assert_eq!(
            project_field(line, "referer").as_deref(),
            Some("https://e.x/")
        );
        assert_eq!(
            project_field(line, "user_agent").as_deref(),
            Some("curl/8.0")
        );
    }

    #[test]
    fn referer_user_agent_absent_on_clf() {
        let line = br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET / HTTP/1.1" 200 5"#;
        assert!(project_field(line, "referer").is_none());
        assert!(project_field(line, "user_agent").is_none());
    }

    #[test]
    fn projects_synthetic_level() {
        let line = br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET / HTTP/1.1" 500 0"#;
        assert_eq!(project_field(line, "level").as_deref(), Some("error"));
        let warn = br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET / HTTP/1.1" 404 0"#;
        assert_eq!(project_field(warn, "level").as_deref(), Some("warn"));
        let info = br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET / HTTP/1.1" 301 0"#;
        assert_eq!(project_field(info, "level").as_deref(), Some("info"));
    }

    #[test]
    fn looks_like_access_matches_clf() {
        assert!(looks_like_access(
            br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET / HTTP/1.1" 200 5"#
        ));
    }

    #[test]
    fn looks_like_access_rejects_json() {
        assert!(!looks_like_access(br#"{"a":1,"b":2}"#));
    }

    #[test]
    fn looks_like_access_rejects_syslog() {
        assert!(!looks_like_access(
            b"<134>1 2026-06-01T12:00:00Z host app 1 - - hi"
        ));
    }

    #[test]
    fn escaped_quote_inside_request() {
        // mod_log_config emits \" inside the request when the URL
        // contains a literal quote. We dequote correctly.
        let line = br#"1.2.3.4 - - [01/Jun/2026:12:00:00 +0000] "GET /a\"b HTTP/1.1" 200 5"#;
        assert_eq!(
            project_field(line, "request").as_deref(),
            Some(r#"GET /a"b HTTP/1.1"#)
        );
    }
}
