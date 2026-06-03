//! Time histogram for the Timeline pane.
//!
//! v0.1 ships a fixed-overview histogram: bins span the indexed time range,
//! bin count is whatever the renderer asks for, and per-bin payload is
//! `count` + severity tally. No keyboard scrub, no zoom — those are v0.2
//! once a week of personal use settles the keyboard model.
//!
//! Records with `ts_micros == TS_UNTIMED` are deliberately NOT placed in any
//! bin and the renderer is told how many were dropped so it can display an
//! honest "N untimed" hint instead of pretending they don't exist.

use crate::engine::record::{severity, TS_UNTIMED};
use crate::engine::Engine;

/// Per-bin counters.
#[derive(Debug, Default, Clone, Copy)]
pub struct Bin {
    pub count: u64,
    pub trace: u64,
    pub debug: u64,
    pub info: u64,
    pub warn: u64,
    pub error: u64,
    pub fatal: u64,
    pub unknown: u64,
}

impl Bin {
    fn add(&mut self, sev: u8) {
        self.count += 1;
        match sev {
            severity::TRACE => self.trace += 1,
            severity::DEBUG => self.debug += 1,
            severity::INFO => self.info += 1,
            severity::WARN => self.warn += 1,
            severity::ERROR => self.error += 1,
            severity::FATAL => self.fatal += 1,
            _ => self.unknown += 1,
        }
    }

    /// Severity that "dominates" this bin, for color picking. Error/fatal
    /// beat warn beats info beats anything else — even one error in a sea of
    /// info paints the bin red. That is the point: the eye should find
    /// outliers without scanning.
    pub fn dominant_severity(&self) -> u8 {
        if self.fatal > 0 {
            return severity::FATAL;
        }
        if self.error > 0 {
            return severity::ERROR;
        }
        if self.warn > 0 {
            return severity::WARN;
        }
        if self.info > 0 {
            return severity::INFO;
        }
        if self.debug > 0 {
            return severity::DEBUG;
        }
        if self.trace > 0 {
            return severity::TRACE;
        }
        severity::UNKNOWN
    }
}

#[derive(Debug, Default)]
pub struct Histogram {
    pub bins: Vec<Bin>,
    /// Inclusive lower bound of the histogram's time range, in microseconds.
    pub t_min: i64,
    /// Exclusive upper bound. `t_max - t_min` is the total covered span.
    pub t_max: i64,
    /// Records that fell outside the range (shouldn't happen in v0.1 since
    /// we build the range from the data) plus all the untimed ones.
    pub untimed: u64,
}

impl Histogram {
    /// Build a histogram over every indexed record. Convenience for callers
    /// without a filtered view.
    pub fn build(engine: &Engine, bins: usize) -> Self {
        let view: Vec<u64> = (0..engine.indexes.len() as u64).collect();
        Self::build_over(engine, &view, bins)
    }

    /// Build a histogram over a specific filtered view (a sorted list of
    /// surviving line_ids). This is what the UI calls — the cached version
    /// in App reuses the result until the view or width changes.
    pub fn build_over(engine: &Engine, view: &[u64], bins: usize) -> Self {
        if bins == 0 || view.is_empty() {
            return Self::default();
        }
        let ts_index = &engine.indexes.time.ts;
        let sevs = &engine.indexes.severity.levels;

        let mut t_min = i64::MAX;
        let mut t_max = i64::MIN;
        let mut untimed: u64 = 0;
        for &line_id in view {
            let t = match ts_index.get(line_id as usize) {
                Some(&t) => t,
                None => continue,
            };
            if t == TS_UNTIMED {
                untimed += 1;
                continue;
            }
            if t < t_min {
                t_min = t;
            }
            if t > t_max {
                t_max = t;
            }
        }
        if t_min == i64::MAX {
            return Self {
                bins: Vec::new(),
                t_min: 0,
                t_max: 0,
                untimed,
            };
        }
        let upper = t_max.saturating_add(1);
        let span = (upper - t_min).max(1);
        let mut out = Self {
            bins: vec![Bin::default(); bins],
            t_min,
            t_max: upper,
            untimed,
        };
        for &line_id in view {
            let idx = line_id as usize;
            let t = match ts_index.get(idx) {
                Some(&t) => t,
                None => continue,
            };
            if t == TS_UNTIMED {
                continue;
            }
            let pos = (((t - t_min) as i128 * bins as i128) / span as i128) as usize;
            let pos = pos.min(bins - 1);
            let sev = sevs.get(idx).copied().unwrap_or(severity::UNKNOWN);
            out.bins[pos].add(sev);
        }
        out
    }

    /// Build a histogram restricted to a fixed time window. `t_lo` /
    /// `t_hi` are inclusive bounds in micros. Records outside the
    /// window are skipped, untimed records count toward the
    /// `untimed` field same as in `build_over`. Used by the timeline
    /// scrub zoom — the cursor stays anchored at a logical bin
    /// position while the visible range narrows.
    pub fn build_over_range(
        engine: &Engine,
        view: &[u64],
        bins: usize,
        t_lo: i64,
        t_hi: i64,
    ) -> Self {
        if bins == 0 || view.is_empty() || t_hi < t_lo {
            return Self::default();
        }
        let ts_index = &engine.indexes.time.ts;
        let sevs = &engine.indexes.severity.levels;
        let upper = t_hi.saturating_add(1);
        let span = (upper - t_lo).max(1);
        let mut out = Self {
            bins: vec![Bin::default(); bins],
            t_min: t_lo,
            t_max: upper,
            untimed: 0,
        };
        for &line_id in view {
            let idx = line_id as usize;
            let t = match ts_index.get(idx) {
                Some(&t) => t,
                None => continue,
            };
            if t == TS_UNTIMED {
                out.untimed += 1;
                continue;
            }
            if t < t_lo || t > t_hi {
                continue;
            }
            let pos = (((t - t_lo) as i128 * bins as i128) / span as i128) as usize;
            let pos = pos.min(bins - 1);
            let sev = sevs.get(idx).copied().unwrap_or(severity::UNKNOWN);
            out.bins[pos].add(sev);
        }
        out
    }

    /// Largest `count` across all bins. Used by the renderer to normalize
    /// bar heights.
    pub fn peak(&self) -> u64 {
        self.bins.iter().map(|b| b.count).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::indexes::LineLoc;
    use crate::engine::Engine;

    fn make_engine(records: &[(i64, u8)]) -> Engine {
        let mut e = Engine::new();
        for &(t, sev) in records {
            e.indexes.line.locs.push(LineLoc {
                source_id: 0,
                offset: 0,
                len: 0,
            });
            e.indexes.time.ts.push(t);
            e.indexes.severity.levels.push(sev);
            // owned_lines is now sparse — file-style fixtures leave it empty.
        }
        e
    }

    #[test]
    fn build_distributes_records_across_bins() {
        // 10 records evenly spaced 0..10, 5 bins → 2 each.
        let recs: Vec<(i64, u8)> = (0..10).map(|i| (i * 1_000_000, severity::INFO)).collect();
        let e = make_engine(&recs);
        let h = Histogram::build(&e, 5);
        assert_eq!(h.bins.len(), 5);
        let total: u64 = h.bins.iter().map(|b| b.count).sum();
        assert_eq!(total, 10);
        for b in &h.bins {
            assert_eq!(b.count, 2);
            assert_eq!(b.info, 2);
        }
    }

    #[test]
    fn dominant_severity_picks_error_over_info() {
        let mut b = Bin::default();
        for _ in 0..1000 {
            b.add(severity::INFO);
        }
        b.add(severity::ERROR);
        assert_eq!(b.dominant_severity(), severity::ERROR);
    }

    #[test]
    fn untimed_records_are_counted_separately() {
        let recs = vec![
            (1_000_000, severity::INFO),
            (TS_UNTIMED, severity::WARN),
            (2_000_000, severity::INFO),
            (TS_UNTIMED, severity::ERROR),
        ];
        let e = make_engine(&recs);
        let h = Histogram::build(&e, 4);
        assert_eq!(h.untimed, 2);
        let total: u64 = h.bins.iter().map(|b| b.count).sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn empty_input_returns_empty_histogram() {
        let e = Engine::new();
        let h = Histogram::build(&e, 10);
        assert!(h.bins.is_empty());
    }

    #[test]
    fn build_over_range_restricts_to_window() {
        // 10 records at t=0,1,...,9 seconds. Restrict to [3s,6s].
        let recs: Vec<(i64, u8)> = (0..10).map(|i| (i * 1_000_000, severity::INFO)).collect();
        let e = make_engine(&recs);
        let view: Vec<u64> = (0..10).collect();
        let h = Histogram::build_over_range(&e, &view, 4, 3_000_000, 6_000_000);
        // Window covers t=3,4,5,6 → 4 records distributed across 4 bins.
        let total: u64 = h.bins.iter().map(|b| b.count).sum();
        assert_eq!(total, 4);
        // t_min / t_max reflect the window, not the engine.
        assert_eq!(h.t_min, 3_000_000);
        assert_eq!(h.t_max, 6_000_001);
    }

    #[test]
    fn build_over_range_counts_untimed_separately() {
        let recs = vec![
            (1_000_000, severity::INFO),
            (TS_UNTIMED, severity::WARN),
            (5_000_000, severity::INFO),
        ];
        let e = make_engine(&recs);
        let view: Vec<u64> = vec![0, 1, 2];
        let h = Histogram::build_over_range(&e, &view, 2, 0, 10_000_000);
        assert_eq!(h.untimed, 1);
        let total: u64 = h.bins.iter().map(|b| b.count).sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn all_untimed_returns_empty_bins_but_counts_untimed() {
        let recs = vec![(TS_UNTIMED, severity::INFO); 5];
        let e = make_engine(&recs);
        let h = Histogram::build(&e, 4);
        assert!(h.bins.is_empty());
        assert_eq!(h.untimed, 5);
    }

    #[test]
    fn last_record_lands_in_last_bin() {
        let recs = vec![(0_i64, severity::INFO), (100, severity::ERROR)];
        let e = make_engine(&recs);
        let h = Histogram::build(&e, 5);
        assert_eq!(h.bins.last().unwrap().error, 1);
        assert_eq!(h.bins[0].info, 1);
    }
}
