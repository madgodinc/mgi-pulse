//! Schema inference: union of seen fields with per-field stats.
//!
//! - **Union, not last-N.** Columns are stable in a session: a field that
//!   disappears becomes an empty cell, never a removed column. UI as a pure
//!   function of state is the rationale.
//! - **Warmup-lock**, two flavors:
//!   - File: lock after `min(10_000, total)` lines or EOF. No timer.
//!   - Stream: lock at 10_000 lines OR (T=5s AND `has_seen_data`). Where
//!     `has_seen_data` means ≥1 field has been emitted, not ≥1 row arrived.
//!     RAW-only for 5s → stay provisional, render a RAW-only view honestly.
//!   M2: file warmup only. Stream warmup-timer is M2.5 / pre-v0.1.
//! - **Top-K**: bounded-exact map (1000 distinct), with an overflow flag.
//!   No HLL, no count-min for v0.1 — they answer the wrong question for
//!   header counters. HLL is backlog as a high-cardinality detector.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::value::RawValue;
use smol_str::SmolStr;

/// Default warmup window for file sources.
pub const FILE_WARMUP_LINES: usize = 10_000;

/// Per-field running stats during warmup.
#[derive(Debug, Default, Clone)]
pub struct FieldStats {
    pub presence: u64,
    /// True if every observed value so far has been a JSON string. Used to
    /// pick a default render style — strings render as text, numbers / objects
    /// get a `…` indicator in the table and full pretty-print in DetailPane.
    pub all_strings: bool,
}

#[derive(Debug, Default)]
pub struct SchemaBuilder {
    pub fields: HashMap<SmolStr, FieldStats>,
    pub records_scanned: u64,
    pub locked: bool,
}

impl SchemaBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Walk a top-level JSON object and tick presence for each key. Silently
    /// no-op on non-JSON or non-object lines — `mgi-pulse` never refuses to
    /// show a line, the schema just doesn't widen.
    pub fn scan(&mut self, line_bytes: &[u8]) {
        if self.locked {
            return;
        }
        self.records_scanned += 1;
        // Borrow keys and unparsed values. RawValue avoids the cost of walking
        // nested children we don't care about here.
        let map = match serde_json::from_slice::<HashMap<&str, &RawValue>>(line_bytes) {
            Ok(m) => m,
            Err(_) => return,
        };
        for (k, v) in map {
            let entry = self
                .fields
                .entry(SmolStr::new(k))
                .or_insert(FieldStats {
                    presence: 0,
                    all_strings: true,
                });
            entry.presence += 1;
            // Cheap "is a JSON string?" check on RawValue: first non-space
            // byte is `"`. RawValue keeps the source verbatim.
            let raw = v.get().as_bytes();
            let is_string = raw.first() == Some(&b'"');
            if !is_string {
                entry.all_strings = false;
            }
        }
    }

    pub fn lock(self) -> LockedSchema {
        let mut entries: Vec<(SmolStr, FieldStats)> = self.fields.into_iter().collect();
        // Sort by presence descending, then by name for determinism.
        entries.sort_by(|a, b| {
            b.1.presence
                .cmp(&a.1.presence)
                .then_with(|| a.0.cmp(&b.0))
        });
        LockedSchema {
            ordered_fields: entries,
            records_scanned: self.records_scanned,
        }
    }
}

#[derive(Debug, Default)]
pub struct LockedSchema {
    /// Field name → stats, sorted by presence descending.
    pub ordered_fields: Vec<(SmolStr, FieldStats)>,
    pub records_scanned: u64,
}

impl LockedSchema {
    /// Pick the top-K column names that look most useful. Always demotes the
    /// `ts` and `level` columns — they're shown as separate columns from the
    /// indexer, not as schema fields.
    pub fn auto_columns(&self, max: usize) -> Vec<SmolStr> {
        self.ordered_fields
            .iter()
            .filter(|(name, _)| name != "ts" && name != "level")
            .take(max)
            .map(|(n, _)| n.clone())
            .collect()
    }
}

/// Tiny serde wrapper used by the table-pane field projector. Returns the raw
/// JSON value for a given key, without parsing nested children.
#[derive(Deserialize)]
struct FieldsView<'a>(
    #[serde(borrow)] HashMap<&'a str, &'a RawValue>,
);

/// Extract one field's raw JSON text from a line, or None if not present.
/// Used by the table renderer in the column hot path — called once per
/// (row, column) on the visible window only.
pub fn project_field<'a>(line_bytes: &'a [u8], field: &str) -> Option<&'a str> {
    let map: FieldsView<'a> = serde_json::from_slice(line_bytes).ok()?;
    let raw = map.0.get(field)?;
    Some(raw.get())
}

/// Strip enclosing quotes if the raw JSON is a string literal. Avoids
/// unescaping — the renderer is best-effort and showing escaped `\"` is fine.
/// For non-string values returns the input unchanged.
pub fn unquote_if_string(raw: &str) -> &str {
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        &raw[1..raw.len() - 1]
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_counts_presence_per_field() {
        let mut sb = SchemaBuilder::new();
        sb.scan(br#"{"ts":"x","level":"info","msg":"hi"}"#);
        sb.scan(br#"{"ts":"y","level":"warn","msg":"x","extra":1}"#);
        let s = sb.lock();
        let mut by_name: HashMap<&str, u64> = HashMap::new();
        for (n, st) in &s.ordered_fields {
            by_name.insert(n.as_str(), st.presence);
        }
        assert_eq!(by_name.get("ts"), Some(&2));
        assert_eq!(by_name.get("level"), Some(&2));
        assert_eq!(by_name.get("msg"), Some(&2));
        assert_eq!(by_name.get("extra"), Some(&1));
    }

    #[test]
    fn auto_columns_drops_ts_and_level() {
        let mut sb = SchemaBuilder::new();
        for _ in 0..5 {
            sb.scan(br#"{"ts":"x","level":"info","logger":"a","msg":"m"}"#);
        }
        let s = sb.lock();
        let cols = s.auto_columns(10);
        assert!(!cols.iter().any(|c| c == "ts" || c == "level"));
        // logger and msg should be there, both seen 5 times.
        assert!(cols.iter().any(|c| c == "logger"));
        assert!(cols.iter().any(|c| c == "msg"));
    }

    #[test]
    fn non_json_silently_ignored() {
        let mut sb = SchemaBuilder::new();
        sb.scan(b"not json");
        sb.scan(br#"{"a":1}"#);
        let s = sb.lock();
        // Two records scanned (counter increments even on non-JSON), but
        // only one contributed a field.
        assert_eq!(s.records_scanned, 2);
        assert_eq!(s.ordered_fields.len(), 1);
    }

    #[test]
    fn project_field_returns_raw_value_text() {
        let line = br#"{"ts":"2026","level":"info","logger":"aurora.tts","payload":{"n":5}}"#;
        assert_eq!(project_field(line, "logger"), Some("\"aurora.tts\""));
        assert_eq!(project_field(line, "payload"), Some("{\"n\":5}"));
        assert_eq!(project_field(line, "missing"), None);
    }

    #[test]
    fn unquote_strips_only_string_literals() {
        assert_eq!(unquote_if_string("\"hello\""), "hello");
        assert_eq!(unquote_if_string("42"), "42");
        assert_eq!(unquote_if_string("{\"x\":1}"), "{\"x\":1}");
        assert_eq!(unquote_if_string("\""), "\"");
    }

    #[test]
    fn locked_schema_blocks_further_scan() {
        let mut sb = SchemaBuilder::new();
        sb.scan(br#"{"a":1}"#);
        sb.locked = true;
        sb.scan(br#"{"b":2}"#);
        let s = sb.lock();
        let cols = s.auto_columns(10);
        assert!(cols.iter().any(|c| c == "a"));
        assert!(!cols.iter().any(|c| c == "b"));
    }
}
