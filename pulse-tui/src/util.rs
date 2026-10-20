//! Small helpers shared by the headless modes: durations, time bounds,
//! timestamp rendering, message normalisation.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mgi_pulse_core::engine::parse::parse_rfc3339_micros;
use mgi_pulse_core::engine::record::TS_UNTIMED;

/// `90s`, `10m`, `2h`, `1d`, `1w`. A bare number is seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("bad duration `{s}`; use e.g. 30s, 10m, 2h, 1d"))?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(format!("bad duration unit in `{s}`; use s, m, h, d or w")),
    };
    n.checked_mul(mult)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("duration `{s}` is too large"))
}

pub fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// A point in time given either as an age (`15m` = fifteen minutes ago)
/// or as an RFC3339 timestamp. Returns microseconds since the epoch.
pub fn parse_time_bound(s: &str, now: i64) -> Result<i64, String> {
    if let Some(m) = parse_rfc3339_micros(s.trim()) {
        return Ok(m);
    }
    // Date only: midnight UTC.
    if s.len() == 10 {
        if let Some(m) = parse_rfc3339_micros(&format!("{s}T00:00:00Z")) {
            return Ok(m);
        }
    }
    let d = parse_duration(s)
        .map_err(|_| format!("bad time `{s}`; use an age like 15m / 2h or RFC3339"))?;
    Ok(now - d.as_micros() as i64)
}

/// `2026-06-01T12:00:00.123Z`, or `-` for an untimed record.
pub fn format_ts(ts_micros: i64) -> String {
    if ts_micros == TS_UNTIMED {
        return "-".into();
    }
    let secs = ts_micros.div_euclid(1_000_000);
    let millis = ts_micros.rem_euclid(1_000_000) / 1000;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let day = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        day / 3600,
        (day % 3600) / 60,
        day % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Render a duration back in the unit it was most likely written in.
pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s == 0 {
        return "0s".into();
    }
    for (unit, size) in [("d", 86_400), ("h", 3600), ("m", 60)] {
        if s % size == 0 {
            return format!("{}{unit}", s / size);
        }
    }
    format!("{s}s")
}

/// Cut `s` to at most `max` bytes on a char boundary. The flag says
/// whether anything was dropped.
pub fn truncate(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// Collapse key for "the same message": digit runs become `#`, so
/// `retry 3 of 5 for id 8812` and `retry 4 of 5 for id 9001` match.
pub fn normalize_message(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len().min(256));
    let mut in_digits = false;
    for c in msg.chars().take(256) {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("m").is_err());
        assert!(parse_duration("").is_err());
        assert_eq!(format_duration(Duration::from_secs(600)), "10m");
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h");
    }

    #[test]
    fn time_bounds() {
        let now = 1_000_000_000_000;
        assert_eq!(parse_time_bound("1m", now).unwrap(), now - 60_000_000);
        let abs = parse_time_bound("2026-06-01T12:00:00Z", now).unwrap();
        assert_eq!(format_ts(abs), "2026-06-01T12:00:00.000Z");
        let day = parse_time_bound("2026-06-01", now).unwrap();
        assert_eq!(format_ts(day), "2026-06-01T00:00:00.000Z");
        assert!(parse_time_bound("yesterday-ish", now).is_err());
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("hello", 10), ("hello", false));
        assert_eq!(truncate("hello", 3), ("hel", true));
        assert_eq!(truncate("h\u{e9}llo", 2), ("h", true));
    }

    #[test]
    fn normalisation_folds_numbers() {
        assert_eq!(
            normalize_message("retry 3 of 5 for id 8812"),
            normalize_message("retry 4 of 5 for id 9001")
        );
        assert_ne!(normalize_message("disk full"), normalize_message("disk ok"));
    }
}
