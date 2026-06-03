//! CSV / TSV parser.
//!
//! Reference: RFC 4180. The shape:
//!
//! ```text
//! col1,col2,col3
//! a,b,c
//! "quoted, value","with ""escapes""",plain
//! ```
//!
//! - Delimiter is `,` for CSV, `\t` for TSV. The format variant
//!   carries the delimiter, not a runtime flag.
//! - Values may be unquoted or `"`-quoted. Inside quotes, `""` is a
//!   literal `"`. Anything else inside quotes is verbatim, including
//!   embedded newlines (which v0.x doesn't fold — we treat the file
//!   as one-record-per-line, so a quoted value containing `\n` will
//!   split the record. This is a documented limitation matching what
//!   most CSV consumers do without an explicit multi-line opt-in.)
//! - The first physical line of the source is the header — it names
//!   the columns. The header doesn't appear as a data row.
//!
//! Field projection works by header name: `project_field(line, "ts")`
//! finds the column whose header is `ts` and returns the cell value
//! for that record. Because the header is per-source and we don't
//! have a way to thread it through the per-line `project_field`
//! contract today, we fall back to a single global header captured at
//! parse-call time (the producer caches it). The producer integration
//! lands in a follow-up; this module exposes a stateless helper that
//! takes the header explicitly.
//!
//! Until the producer wiring carries the header, predicates against
//! CSV/TSV files can address columns positionally as `_1` ... `_N`
//! (1-based) — this works regardless of headers and is what the
//! status bar shows when no header match is found.

use crate::engine::parse::{parse_rfc3339_micros, FieldNames, ParseStats};
use crate::engine::record::{severity, TS_UNTIMED};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delim {
    Comma,
    Tab,
}

impl Delim {
    pub fn byte(self) -> u8 {
        match self {
            Delim::Comma => b',',
            Delim::Tab => b'\t',
        }
    }
}

/// Split one CSV/TSV record into owned field strings. RFC 4180
/// quoting: `"value"` strips the outer quotes, `""` inside becomes a
/// literal `"`. Outside quotes the delimiter splits.
pub fn split_record(line: &[u8], delim: Delim) -> Vec<String> {
    let d = delim.byte();
    let mut out = Vec::new();
    let mut i = 0;
    while i <= line.len() {
        // Special case: trailing delimiter → emit one more empty field.
        if i == line.len() {
            // If we reached EOL after a delimiter, push the empty
            // tail field; otherwise we've already pushed it.
            if !line.is_empty() && line[line.len() - 1] == d {
                out.push(String::new());
            } else if line.is_empty() {
                out.push(String::new());
            }
            break;
        }
        if line[i] == b'"' {
            // Quoted field. Walk to the closing quote, honouring `""`.
            i += 1;
            let mut value = Vec::new();
            while i < line.len() {
                if line[i] == b'"' {
                    if i + 1 < line.len() && line[i + 1] == b'"' {
                        // Escaped quote.
                        value.push(b'"');
                        i += 2;
                        continue;
                    }
                    // Closing quote.
                    i += 1;
                    break;
                }
                value.push(line[i]);
                i += 1;
            }
            out.push(String::from_utf8_lossy(&value).into_owned());
            // Skip past one delimiter, if any.
            if i < line.len() && line[i] == d {
                i += 1;
            } else if i >= line.len() {
                break;
            }
            continue;
        }
        // Unquoted field — runs until the next delimiter or EOL.
        let start = i;
        while i < line.len() && line[i] != d {
            i += 1;
        }
        out.push(String::from_utf8_lossy(&line[start..i]).into_owned());
        if i < line.len() && line[i] == d {
            i += 1;
        }
    }
    out
}

/// Find the column index of a named header. Case-sensitive. Returns
/// `None` if no header matches.
pub fn column_index(headers: &[String], name: &str) -> Option<usize> {
    headers.iter().position(|h| h == name)
}

/// Stateless `ts_and_level` for CSV/TSV that takes the header up
/// front. Producer holds the header and forwards it on each call.
pub fn ts_and_level_with_header(
    line: &[u8],
    delim: Delim,
    headers: &[String],
    stats: &mut ParseStats,
    fields: Option<&FieldNames>,
) -> (i64, u8) {
    let row = split_record(line, delim);
    let ts_key = fields.map(|f| f.ts.as_str()).unwrap_or("ts");
    let level_key = fields.map(|f| f.level.as_str()).unwrap_or("level");

    let ts_idx = column_index(headers, ts_key);
    let level_idx = column_index(headers, level_key);

    let sev = level_idx
        .and_then(|i| row.get(i))
        .map(|s| severity::from_bytes(s.as_bytes()))
        .unwrap_or(severity::UNKNOWN);

    let ts_raw = ts_idx.and_then(|i| row.get(i));
    let ts_micros = match ts_raw {
        Some(s) if !s.is_empty() => match parse_rfc3339_micros(s) {
            Some(t) => t,
            None => {
                stats.ts_parse_errors += 1;
                stats.untimed += 1;
                TS_UNTIMED
            }
        },
        _ => {
            stats.untimed += 1;
            TS_UNTIMED
        }
    };
    (ts_micros, sev)
}

/// Stateless `project_field` for CSV/TSV that takes the header up
/// front. Recognises positional `_N` (1-based) as a fallback when the
/// header doesn't have the requested name — handy for headerless or
/// poorly-named exports.
pub fn project_field_with_header(
    line: &[u8],
    delim: Delim,
    headers: &[String],
    key: &str,
) -> Option<String> {
    let row = split_record(line, delim);
    if let Some(idx) = column_index(headers, key) {
        return row.get(idx).cloned();
    }
    if let Some(rest) = key.strip_prefix('_') {
        if let Ok(n) = rest.parse::<usize>() {
            if n >= 1 {
                return row.get(n - 1).cloned();
            }
        }
    }
    None
}

/// Heuristic: a line looks like CSV/TSV when it has at least one
/// delimiter outside of quoted regions and isn't already covered by
/// the other detectors. Used by `LogFormat::detect` only after the
/// more-specific syslog / NDJSON / EDN / logfmt votes failed.
pub fn delim_vote(line: &[u8]) -> Option<Delim> {
    let mut commas = 0;
    let mut tabs = 0;
    let mut in_quote = false;
    let mut i = 0;
    while i < line.len() {
        let b = line[i];
        if b == b'"' {
            if in_quote && i + 1 < line.len() && line[i + 1] == b'"' {
                i += 2;
                continue;
            }
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if !in_quote {
            if b == b',' {
                commas += 1;
            } else if b == b'\t' {
                tabs += 1;
            }
        }
        i += 1;
    }
    if tabs >= 2 && tabs > commas {
        Some(Delim::Tab)
    } else if commas >= 2 {
        Some(Delim::Comma)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_simple_csv() {
        let row = split_record(b"a,b,c", Delim::Comma);
        assert_eq!(row, vec!["a", "b", "c"]);
    }

    #[test]
    fn split_simple_tsv() {
        let row = split_record(b"a\tb\tc", Delim::Tab);
        assert_eq!(row, vec!["a", "b", "c"]);
    }

    #[test]
    fn quoted_field_strips_outer_quotes() {
        let row = split_record(br#""ab","cd"#, Delim::Comma);
        assert_eq!(row, vec!["ab", "cd"]);
    }

    #[test]
    fn escaped_quote_inside_quoted_field() {
        let row = split_record(br#""he said ""hi""","next"#, Delim::Comma);
        assert_eq!(row, vec![r#"he said "hi""#, "next"]);
    }

    #[test]
    fn delimiter_inside_quoted_field_does_not_split() {
        let row = split_record(br#""a, b, c",x"#, Delim::Comma);
        assert_eq!(row, vec!["a, b, c", "x"]);
    }

    #[test]
    fn trailing_empty_field() {
        let row = split_record(b"a,b,", Delim::Comma);
        assert_eq!(row, vec!["a", "b", ""]);
    }

    #[test]
    fn leading_empty_field() {
        let row = split_record(b",b,c", Delim::Comma);
        assert_eq!(row, vec!["", "b", "c"]);
    }

    #[test]
    fn ts_level_from_named_columns() {
        let headers = vec!["id".to_string(), "ts".to_string(), "level".to_string()];
        let mut stats = ParseStats::default();
        let (ts, sev) = ts_and_level_with_header(
            b"1,2026-06-01T12:00:00Z,error",
            Delim::Comma,
            &headers,
            &mut stats,
            None,
        );
        assert!(ts > 0);
        assert_eq!(sev, severity::ERROR);
    }

    #[test]
    fn missing_ts_column_lands_in_untimed() {
        let headers = vec!["id".to_string(), "level".to_string()];
        let mut stats = ParseStats::default();
        let (ts, _) = ts_and_level_with_header(
            b"1,info",
            Delim::Comma,
            &headers,
            &mut stats,
            None,
        );
        assert_eq!(ts, TS_UNTIMED);
        assert_eq!(stats.untimed, 1);
    }

    #[test]
    fn project_named_column() {
        let headers = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(
            project_field_with_header(b"1,2,3", Delim::Comma, &headers, "b").as_deref(),
            Some("2")
        );
    }

    #[test]
    fn project_positional_fallback_when_header_missing() {
        let headers = vec!["a".to_string(), "b".to_string()];
        // `_2` → second column.
        assert_eq!(
            project_field_with_header(b"x,y,z", Delim::Comma, &headers, "_2").as_deref(),
            Some("y")
        );
        // `_99` out of range → None.
        assert!(project_field_with_header(b"x,y", Delim::Comma, &headers, "_99").is_none());
    }

    #[test]
    fn delim_vote_picks_csv() {
        // Need at least 2 commas to vote.
        assert_eq!(delim_vote(b"a,b,c"), Some(Delim::Comma));
        assert_eq!(delim_vote(b"a,b"), None);
    }

    #[test]
    fn delim_vote_picks_tsv() {
        assert_eq!(delim_vote(b"a\tb\tc"), Some(Delim::Tab));
    }

    #[test]
    fn delim_vote_ignores_delim_inside_quotes() {
        // 4 commas total, 3 of them inside quotes — only 1 outside.
        assert_eq!(delim_vote(br#""a,b,c,d",x"#), None);
    }
}
