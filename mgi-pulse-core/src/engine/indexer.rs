//! Indexer thread.
//!
//! Drains a `RecordProducer`, parses ts/level via a borrowed serde struct
//! (not a hand-rolled byte scan, not full `Value`), updates LineIndex,
//! TimeIndex, SeverityIndex, and the per-predicate bitsets in one pass.
//!
//! Memory budget target: ~150 MB of indexes per 1 GB of NDJSON, comfortable
//! up to ~5–10 GB total. On-disk indexes are backlog.
//!
//! Parse cost is the dominant indexing cost and the measurable risk of M1.
//! Measured 2026-06-01 on synthetic 2 GB NDJSON: serde-borrow ≈ 905 MB/s
//! single-threaded, ~5× faster than `simd_json::to_borrowed_value`. The plan
//! is validated; the indexer uses the serde borrow strategy.

use serde::Deserialize;

use crate::engine::indexes::{LineIndex, LineLoc, SeverityIndex, TimeIndex};
use crate::engine::record::{severity, RecordBytes, TS_UNTIMED};

/// The shape we extract from every NDJSON record. Only `ts` and `level` are
/// parsed by the indexer; everything else stays as raw bytes for the renderer
/// to project on demand.
///
/// Both fields are optional — a record that omits one (or both) is still
/// indexed, just with `TS_UNTIMED` / `severity::UNKNOWN` placeholders.
#[derive(Deserialize)]
struct TsLevel<'a> {
    #[serde(default, borrow)]
    ts: Option<&'a str>,
    #[serde(default, borrow)]
    level: Option<&'a str>,
}

/// Three parallel arrays grown together. `LineIndex[i]`, `TimeIndex[i]`,
/// `SeverityIndex[i]` always refer to the same record.
#[derive(Debug, Default)]
pub struct Indexes {
    pub line: LineIndex,
    pub time: TimeIndex,
    pub severity: SeverityIndex,
    /// Tally of how many records had to fall back to the untimed bucket on
    /// file sources. Useful for the status line.
    pub untimed_on_file: u64,
    /// Tally of records where ts was present but did not parse as RFC3339.
    /// Distinct from "ts absent" — this is "ts present but malformed".
    pub ts_parse_errors: u64,
    /// Tally of records that failed the JSON pre-pass entirely (the line is
    /// not JSON at all, or had a syntax error). They're still indexed (so the
    /// table never silently loses them), just with severity UNKNOWN and the
    /// untimed bucket.
    pub json_parse_errors: u64,
}

impl Indexes {
    pub fn len(&self) -> usize {
        self.line.len()
    }

    pub fn is_empty(&self) -> bool {
        self.line.is_empty()
    }
}

/// Drain a producer into the engine. Files use the engine's mmap snapshot to
/// resolve `FileRef` records to bytes without copying. Stream producers hand
/// over `Owned` bytes; the engine keeps them alive in `owned_lines` so the
/// renderer can find them by `line_id` later.
pub fn drain<P: crate::io::RecordProducer>(
    producer: &mut P,
    engine: &mut crate::engine::Engine,
) {
    while let Some(rec) = producer.next() {
        // First resolve the bytes for parsing, then decide where they live.
        match rec.bytes {
            RecordBytes::FileRef { source_id, offset, len } => {
                let mmap_opt = engine.mmaps.get(source_id as usize).cloned();
                let bytes_opt = mmap_opt.as_ref().and_then(|m| {
                    let start = offset as usize;
                    let end = start + len as usize;
                    if end <= m.len() {
                        Some(&m[start..end])
                    } else {
                        None
                    }
                });
                let (ts_micros, sev) = match bytes_opt {
                    Some(b) => parse_ts_level(b, &mut engine.indexes),
                    None => {
                        // FileRef but the mmap is missing or the slice is out
                        // of bounds. Surface as a JSON error and push a
                        // placeholder so parallel arrays stay aligned.
                        engine.indexes.json_parse_errors += 1;
                        (TS_UNTIMED, severity::UNKNOWN)
                    }
                };
                engine.indexes.line.locs.push(LineLoc { source_id, offset, len });
                engine.indexes.time.ts.push(ts_micros);
                engine.indexes.severity.levels.push(sev);
                engine.owned_lines.push(None);
            }
            RecordBytes::Owned(boxed) => {
                let (ts_micros, sev) = parse_ts_level(&boxed, &mut engine.indexes);
                let len = boxed.len() as u32;
                engine.indexes.line.locs.push(LineLoc {
                    source_id: rec.source_id,
                    offset: 0,
                    len,
                });
                engine.indexes.time.ts.push(ts_micros);
                engine.indexes.severity.levels.push(sev);
                engine.owned_lines.push(Some(boxed));
            }
        }
    }
}

fn parse_ts_level(bytes: &[u8], out: &mut Indexes) -> (i64, u8) {
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
                        out.ts_parse_errors += 1;
                        out.untimed_on_file += 1;
                        TS_UNTIMED
                    }
                },
                None => {
                    out.untimed_on_file += 1;
                    TS_UNTIMED
                }
            };
            (ts, sev)
        }
        Err(_) => {
            out.json_parse_errors += 1;
            out.untimed_on_file += 1;
            (TS_UNTIMED, severity::UNKNOWN)
        }
    }
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
    // Date.
    let year = ascii_u32(&b[0..4])? as i64;
    if b[4] != b'-' { return None; }
    let month = ascii_u32(&b[5..7])? as i64;
    if b[7] != b'-' { return None; }
    let day = ascii_u32(&b[8..10])? as i64;
    if b[10] != b'T' && b[10] != b' ' { return None; }
    let hour = ascii_u32(&b[11..13])? as i64;
    if b[13] != b':' { return None; }
    let minute = ascii_u32(&b[14..16])? as i64;
    if b[16] != b':' { return None; }
    let second = ascii_u32(&b[17..19])? as i64;

    // Optional fractional seconds and timezone.
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
        // Normalize to microseconds.
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

    // Timezone.
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
            if b[cursor + 2] != b':' { return None; }
            let om = ascii_u32(&b[cursor + 3..cursor + 5])? as i64;
            cursor += 5;
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };

    // Trailing characters are not allowed for v0.1.
    if cursor != b.len() {
        return None;
    }

    // Compose UTC microseconds via days_from_civil.
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

/// Howard Hinnant's days_from_civil. Returns days since 1970-01-01.
/// Handles dates from year ±32k inclusive — way past anything we'll see.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let doy = (153 * ((m + (if m > 2 { -3 } else { 9 })) as i64) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + (yoe / 4) - (yoe / 100) + doy as u64;
    era * 146_097 + (doe as i64) - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_basic() {
        let micros = parse_rfc3339_micros("1970-01-01T00:00:00Z").unwrap();
        assert_eq!(micros, 0);
        let micros = parse_rfc3339_micros("1970-01-01T00:00:01Z").unwrap();
        assert_eq!(micros, 1_000_000);
    }

    #[test]
    fn rfc3339_fractional() {
        // Nanosecond precision is truncated to micros (not rounded).
        let micros = parse_rfc3339_micros("2026-06-01T12:00:00.123456789Z").unwrap();
        // 2026-06-01T12:00:00Z = ?
        let base = parse_rfc3339_micros("2026-06-01T12:00:00Z").unwrap();
        assert_eq!(micros - base, 123_456);

        // Sub-microsecond fractional pads with zeros.
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
        // Trailing junk.
        assert!(parse_rfc3339_micros("2026-06-01T12:00:00Zextra").is_none());
        // Wrong separator.
        assert!(parse_rfc3339_micros("2026/06/01T12:00:00Z").is_none());
    }

    #[test]
    fn ts_level_parser_handles_missing_fields() {
        let mut idx = Indexes::default();
        let (ts, sev) = parse_ts_level(b"{}", &mut idx);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
        // Missing-ts counts as untimed_on_file.
        assert_eq!(idx.untimed_on_file, 1);
        assert_eq!(idx.json_parse_errors, 0);
    }

    #[test]
    fn ts_level_parser_handles_bad_ts() {
        let mut idx = Indexes::default();
        let (ts, sev) = parse_ts_level(
            br#"{"ts":"not a date","level":"warn"}"#,
            &mut idx,
        );
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::WARN);
        assert_eq!(idx.ts_parse_errors, 1);
    }

    #[test]
    fn ts_level_parser_happy_path() {
        let mut idx = Indexes::default();
        let (ts, sev) = parse_ts_level(
            br#"{"ts":"2026-06-01T12:00:00Z","level":"error","other":"ignored"}"#,
            &mut idx,
        );
        assert_eq!(sev, severity::ERROR);
        assert!(ts > 0);
        assert_eq!(idx.untimed_on_file, 0);
        assert_eq!(idx.ts_parse_errors, 0);
        assert_eq!(idx.json_parse_errors, 0);
    }

    #[test]
    fn ts_level_parser_handles_non_json() {
        let mut idx = Indexes::default();
        let (ts, sev) = parse_ts_level(b"just a raw line", &mut idx);
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(sev, severity::UNKNOWN);
        assert_eq!(idx.json_parse_errors, 1);
    }
}
