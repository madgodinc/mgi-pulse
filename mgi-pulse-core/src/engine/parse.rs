//! Shared parse helpers: NDJSON `ts` + `level` extraction.
//!
//! Used by every `RecordProducer` so that records carry valid `ts_micros` and
//! `severity` at the moment they're handed to the engine. This is what makes
//! k-way merge by timestamp meaningful: the merge step has the timestamps it
//! needs without parsing the bytes a second time.
//!
//! Measured 2026-06-01 on synthetic 2 GB NDJSON: serde-borrow ≈ 905 MB/s
//! single-threaded, ~5× faster than `simd_json::to_borrowed_value`. The plan
//! is validated; producers use the serde borrow strategy.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::engine::record::{severity, TS_UNTIMED};

/// Field names to extract for ts and level. Defaults match the most common
/// NDJSON conventions; CLI flags override them for inputs with non-standard
/// schemas (e.g. `@t`, `@timestamp`, `eventTime`, `severity_text`).
#[derive(Debug, Clone)]
pub struct FieldNames {
    pub ts: String,
    pub level: String,
}

impl Default for FieldNames {
    fn default() -> Self {
        Self {
            ts: "ts".to_string(),
            level: "level".to_string(),
        }
    }
}

/// The shape we extract from every NDJSON record when both fields use the
/// default names. Generated derive code is a touch faster than the dynamic
/// HashMap path, so we keep it for the common case.
#[derive(Deserialize)]
struct TsLevel<'a> {
    #[serde(default, borrow)]
    ts: Option<&'a str>,
    #[serde(default, borrow)]
    level: Option<&'a str>,
}

/// Tally of parser outcomes accumulated by a producer. The engine collects
/// these per source and folds them into the aggregate `Indexes` stats.
#[derive(Debug, Default, Clone, Copy)]
pub struct ParseStats {
    pub untimed: u64,
    pub ts_parse_errors: u64,
    pub json_parse_errors: u64,
}

impl ParseStats {
    pub fn fold(&mut self, other: ParseStats) {
        self.untimed += other.untimed;
        self.ts_parse_errors += other.ts_parse_errors;
        self.json_parse_errors += other.json_parse_errors;
    }
}

/// Extract `(ts_micros, severity)` from a raw NDJSON line. Updates `stats`
/// with the outcome. Never allocates.
pub fn ts_and_level(bytes: &[u8], stats: &mut ParseStats) -> (i64, u8) {
    match serde_json::from_slice::<TsLevel>(bytes) {
        Ok(t) => {
            let sev = t
                .level
                .map(|l| severity::from_bytes(l.as_bytes()))
                .unwrap_or(severity::UNKNOWN);
            let ts = match t.ts {
                Some(s) => match parse_rfc3339_micros(s) {
                    Some(v) => v,
                    None => {
                        stats.ts_parse_errors += 1;
                        stats.untimed += 1;
                        TS_UNTIMED
                    }
                },
                None => {
                    stats.untimed += 1;
                    TS_UNTIMED
                }
            };
            (ts, sev)
        }
        Err(_) => {
            stats.json_parse_errors += 1;
            stats.untimed += 1;
            (TS_UNTIMED, severity::UNKNOWN)
        }
    }
}

/// Extract `(ts_micros, severity)` from a raw NDJSON line, looking up custom
/// field names instead of the hardcoded `ts` / `level`. Slower than
/// [`ts_and_level`] because it goes through a borrowed HashMap; use it only
/// when CLI override flags are set.
pub fn ts_and_level_named(bytes: &[u8], fields: &FieldNames, stats: &mut ParseStats) -> (i64, u8) {
    let map: HashMap<&str, &RawValue> = match serde_json::from_slice(bytes) {
        Ok(m) => m,
        Err(_) => {
            stats.json_parse_errors += 1;
            stats.untimed += 1;
            return (TS_UNTIMED, severity::UNKNOWN);
        }
    };
    let strip_quotes = |raw: &str| -> Option<String> {
        if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
            Some(raw[1..raw.len() - 1].to_string())
        } else {
            None
        }
    };
    let sev = map
        .get(fields.level.as_str())
        .and_then(|v| strip_quotes(v.get()))
        .map(|s| severity::from_bytes(s.as_bytes()))
        .unwrap_or(severity::UNKNOWN);
    let ts = match map
        .get(fields.ts.as_str())
        .and_then(|v| strip_quotes(v.get()))
    {
        Some(s) => match parse_rfc3339_micros(&s) {
            Some(v) => v,
            None => {
                stats.ts_parse_errors += 1;
                stats.untimed += 1;
                TS_UNTIMED
            }
        },
        None => {
            stats.untimed += 1;
            TS_UNTIMED
        }
    };
    (ts, sev)
}

/// Minimal RFC3339 parser for the timestamps `mgi-pulse` actually sees:
/// `YYYY-MM-DDTHH:MM:SS(.ffffff)?(Z|±HH:MM)`. Returns microseconds since
/// epoch. Reject anything weird; the bench fixture validates the happy path.
///
/// We don't pull `chrono`/`time` for v0.1: this code is on the hot indexing
/// path, and the format is well-defined and narrow. A wider parser is M2 if
/// real-world fixtures show drift from RFC3339.
pub fn parse_rfc3339_micros(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let year = ascii_u32(&b[0..4])? as i64;
    if b[4] != b'-' {
        return None;
    }
    let month = ascii_u32(&b[5..7])? as i64;
    if b[7] != b'-' {
        return None;
    }
    let day = ascii_u32(&b[8..10])? as i64;
    if b[10] != b'T' && b[10] != b' ' {
        return None;
    }
    let hour = ascii_u32(&b[11..13])? as i64;
    if b[13] != b':' {
        return None;
    }
    let minute = ascii_u32(&b[14..16])? as i64;
    if b[16] != b':' {
        return None;
    }
    let second = ascii_u32(&b[17..19])? as i64;

    let mut cursor = 19;
    let mut micros: i64 = 0;
    if b.get(cursor) == Some(&b'.') {
        cursor += 1;
        let frac_start = cursor;
        while cursor < b.len() && b[cursor].is_ascii_digit() {
            cursor += 1;
        }
        let mut frac: i64 = ascii_u32(&b[frac_start..cursor])? as i64;
        let mut digits = cursor - frac_start;
        while digits < 6 {
            frac *= 10;
            digits += 1;
        }
        while digits > 6 {
            frac /= 10;
            digits -= 1;
        }
        micros = frac;
    }

    let offset_secs: i64 = match b.get(cursor) {
        Some(&b'Z') => {
            cursor += 1;
            0
        }
        Some(&b'+') | Some(&b'-') => {
            let sign: i64 = if b[cursor] == b'+' { 1 } else { -1 };
            cursor += 1;
            if cursor + 5 > b.len() {
                return None;
            }
            let oh = ascii_u32(&b[cursor..cursor + 2])? as i64;
            if b[cursor + 2] != b':' {
                return None;
            }
            let om = ascii_u32(&b[cursor + 3..cursor + 5])? as i64;
            cursor += 5;
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };

    if cursor != b.len() {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_secs;
    Some(secs * 1_000_000 + micros)
}

fn ascii_u32(b: &[u8]) -> Option<u32> {
    let mut acc: u32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        acc = acc * 10 + (c - b'0') as u32;
    }
    Some(acc)
}

/// Howard Hinnant's days_from_civil. Days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (m + (if m > 2 { -3 } else { 9 })) + 2) / 5 + d - 1;
    let doe = yoe * 365 + (yoe / 4) - (yoe / 100) + doy as u64;
    era * 146_097 + (doe as i64) - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_basic() {
        assert_eq!(parse_rfc3339_micros("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_micros("1970-01-01T00:00:01Z"),
            Some(1_000_000)
        );
    }

    #[test]
    fn rfc3339_fractional() {
        let micros = parse_rfc3339_micros("2026-06-01T12:00:00.123456789Z").unwrap();
        let base = parse_rfc3339_micros("2026-06-01T12:00:00Z").unwrap();
        assert_eq!(micros - base, 123_456);
        let micros = parse_rfc3339_micros("2026-06-01T12:00:00.5Z").unwrap();
        assert_eq!(micros - base, 500_000);
    }

    #[test]
    fn rfc3339_offset() {
        let utc = parse_rfc3339_micros("2026-06-01T12:00:00Z").unwrap();
        let east = parse_rfc3339_micros("2026-06-01T15:00:00+03:00").unwrap();
        assert_eq!(utc, east);
        let west = parse_rfc3339_micros("2026-06-01T07:00:00-05:00").unwrap();
        assert_eq!(utc, west);
    }

    #[test]
    fn rfc3339_rejects_garbage() {
        assert!(parse_rfc3339_micros("").is_none());
        assert!(parse_rfc3339_micros("2026-06-01").is_none());
        assert!(parse_rfc3339_micros("not a date").is_none());
        assert!(parse_rfc3339_micros("2026-06-01T12:00:00Zextra").is_none());
        assert!(parse_rfc3339_micros("2026/06/01T12:00:00Z").is_none());
    }

    #[test]
    fn ts_and_level_missing_fields() {
        let mut s = ParseStats::default();
        let (ts, sev) = ts_and_level(b"{}", &mut s);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
        assert_eq!(s.untimed, 1);
        assert_eq!(s.json_parse_errors, 0);
    }

    #[test]
    fn ts_and_level_bad_ts() {
        let mut s = ParseStats::default();
        let (ts, sev) = ts_and_level(br#"{"ts":"not a date","level":"warn"}"#, &mut s);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::WARN);
        assert_eq!(s.ts_parse_errors, 1);
    }

    #[test]
    fn ts_and_level_happy_path() {
        let mut s = ParseStats::default();
        let (ts, sev) = ts_and_level(
            br#"{"ts":"2026-06-01T12:00:00Z","level":"error","other":"x"}"#,
            &mut s,
        );
        assert_eq!(sev, severity::ERROR);
        assert!(ts > 0);
        assert_eq!(s.untimed, 0);
        assert_eq!(s.ts_parse_errors, 0);
        assert_eq!(s.json_parse_errors, 0);
    }

    #[test]
    fn ts_and_level_non_json() {
        let mut s = ParseStats::default();
        let (ts, sev) = ts_and_level(b"just a raw line", &mut s);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
        assert_eq!(s.json_parse_errors, 1);
    }
}
