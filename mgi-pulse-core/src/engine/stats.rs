//! Quick-summary stats over a filtered view.
//!
//! Cheap to compute (single pass over the filtered_view's line_ids)
//! and small enough to render in a sidebar — used by the `?` /
//! `Stats` overlay in the TUI.

use crate::engine::record::{severity, TS_UNTIMED};
use crate::engine::Engine;

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub total: u64,
    /// Index 0..7 by severity::*. Index 0 is `UNKNOWN`.
    pub by_severity: [u64; 8],
    pub timed: u64,
    pub untimed: u64,
    /// First and last timed record's `ts_micros`, or `None` if no
    /// record in the view carried a timestamp.
    pub t_min: Option<i64>,
    pub t_max: Option<i64>,
    /// Top-N values for one chosen field, with their counts. Empty
    /// when no field was specified or it didn't project on any
    /// record in the view.
    pub top_values: Vec<(String, u64)>,
    /// The field whose top values are above. Empty when no field
    /// was selected.
    pub top_field: String,
}

impl Stats {
    /// Build stats over the given filtered view. `top_field` is the
    /// field name to bucket top-N for; pass `""` to skip the per-
    /// field bucketing pass (saves the second walk over the lines).
    /// `top_n` caps the returned list. The bucketing uses a bounded
    /// HashMap with `MAX_BUCKETS` keys — high-cardinality fields
    /// stop counting past that to keep stats cheap.
    pub fn build(
        engine: &Engine,
        view: &[u64],
        top_field: &str,
        top_n: usize,
    ) -> Self {
        const MAX_BUCKETS: usize = 1024;

        let mut out = Stats::default();
        out.total = view.len() as u64;

        let ts_index = &engine.indexes.time.ts;
        let sevs = &engine.indexes.severity.levels;

        for &line_id in view {
            let idx = line_id as usize;
            let sev = sevs.get(idx).copied().unwrap_or(severity::UNKNOWN);
            if (sev as usize) < out.by_severity.len() {
                out.by_severity[sev as usize] += 1;
            }
            let t = ts_index.get(idx).copied().unwrap_or(TS_UNTIMED);
            if t == TS_UNTIMED {
                out.untimed += 1;
            } else {
                out.timed += 1;
                out.t_min = Some(match out.t_min {
                    Some(v) => v.min(t),
                    None => t,
                });
                out.t_max = Some(match out.t_max {
                    Some(v) => v.max(t),
                    None => t,
                });
            }
        }

        if !top_field.is_empty() && top_n > 0 {
            use std::collections::HashMap;
            let mut counts: HashMap<String, u64> = HashMap::new();
            let mut overflow = false;
            for &line_id in view {
                let bytes = engine.line_bytes(line_id);
                let fmt = engine.format_of(engine.indexes.line.locs[line_id as usize].source_id);
                // Stateless project_field is enough for the formats we
                // currently surface from the UI; stateful formats
                // (CSV, regex) need FieldCache-side projection that's
                // out of scope for the summary pane.
                let val = match fmt.project_field(bytes, top_field) {
                    Some(v) => v.into_owned(),
                    None => continue,
                };
                if counts.contains_key(&val) {
                    *counts.get_mut(&val).unwrap() += 1;
                } else if counts.len() < MAX_BUCKETS {
                    counts.insert(val, 1);
                } else {
                    overflow = true;
                }
            }
            let mut sorted: Vec<(String, u64)> = counts.into_iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            sorted.truncate(top_n);
            out.top_values = sorted;
            out.top_field = top_field.to_string();
            if overflow {
                out.top_field.push_str(" (high-card)");
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::indexes::LineLoc;

    fn make_engine(records: &[(i64, u8, &[u8])]) -> Engine {
        let mut e = Engine::new();
        for (i, &(t, sev, body)) in records.iter().enumerate() {
            e.indexes.line.locs.push(LineLoc {
                source_id: 0,
                offset: 0,
                len: body.len() as u32,
            });
            e.indexes.time.ts.push(t);
            e.indexes.severity.levels.push(sev);
            e.push_owned(i as u64, Box::from(body));
        }
        e.source_formats.push(crate::engine::format::LogFormat::Ndjson);
        e
    }

    #[test]
    fn counts_total_and_severity() {
        let recs = vec![
            (1_000_000, severity::ERROR, br#"{"a":1}"# as &[u8]),
            (2_000_000, severity::ERROR, br#"{"a":2}"#),
            (3_000_000, severity::INFO, br#"{"a":3}"#),
        ];
        let e = make_engine(&recs);
        let view: Vec<u64> = (0..3).collect();
        let s = Stats::build(&e, &view, "", 0);
        assert_eq!(s.total, 3);
        assert_eq!(s.by_severity[severity::ERROR as usize], 2);
        assert_eq!(s.by_severity[severity::INFO as usize], 1);
    }

    #[test]
    fn tracks_time_span_and_untimed() {
        let recs = vec![
            (1_000_000, severity::INFO, b"a" as &[u8]),
            (TS_UNTIMED, severity::INFO, b"b"),
            (5_000_000, severity::INFO, b"c"),
        ];
        let e = make_engine(&recs);
        let view: Vec<u64> = (0..3).collect();
        let s = Stats::build(&e, &view, "", 0);
        assert_eq!(s.timed, 2);
        assert_eq!(s.untimed, 1);
        assert_eq!(s.t_min, Some(1_000_000));
        assert_eq!(s.t_max, Some(5_000_000));
    }

    #[test]
    fn top_values_buckets_field() {
        let recs = vec![
            (1, severity::INFO, br#"{"user":"alice"}"# as &[u8]),
            (2, severity::INFO, br#"{"user":"alice"}"#),
            (3, severity::INFO, br#"{"user":"bob"}"#),
        ];
        let e = make_engine(&recs);
        let view: Vec<u64> = (0..3).collect();
        let s = Stats::build(&e, &view, "user", 5);
        assert_eq!(s.top_field, "user");
        assert_eq!(s.top_values, vec![
            ("alice".to_string(), 2),
            ("bob".to_string(), 1),
        ]);
    }

    #[test]
    fn empty_view_returns_zeros() {
        let e = Engine::new();
        let s = Stats::build(&e, &[], "", 0);
        assert_eq!(s.total, 0);
        assert!(s.t_min.is_none());
        assert!(s.t_max.is_none());
    }
}
