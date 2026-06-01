//! Compact, parallel arrays indexed by `line_id`.
//!
//! - `LineIndex`: line locator + length, one entry per record.
//! - `TimeIndex`: `Vec<i64>` of `ts_micros`; `TS_UNTIMED` for the untimed bucket.
//! - `SeverityIndex`: `Vec<u8>` of severity enum bytes.
//!
//! On the hot path the indexer appends to all three in lockstep. The arrays
//! stay parallel: `time_index[i]`, `severity_index[i]`, and `line_index[i]`
//! always refer to the same record, where `i == line_id`.

use crate::engine::record::TS_UNTIMED;

/// A single record's location inside its source.
///
/// For files: `offset` is the byte offset inside the mmap'd source.
/// For streams: `offset` is meaningless and set to 0; bytes are carried inside
/// the `RawRecord` itself as `RecordBytes::Owned`.
///
/// `len` is u32 because per-line size beyond 4 GB is meaningless for logs.
/// If a single log "line" is bigger than 4 GB the user has bigger problems
/// than us truncating it.
#[derive(Debug, Clone, Copy)]
pub struct LineLoc {
    pub source_id: u32,
    pub offset: u64,
    pub len: u32,
}

#[derive(Debug, Default)]
pub struct LineIndex {
    pub locs: Vec<LineLoc>,
}

impl LineIndex {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            locs: Vec::with_capacity(cap),
        }
    }

    pub fn push(&mut self, loc: LineLoc) -> u64 {
        let id = self.locs.len() as u64;
        self.locs.push(loc);
        id
    }

    pub fn len(&self) -> usize {
        self.locs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.locs.is_empty()
    }

    pub fn get(&self, line_id: u64) -> Option<&LineLoc> {
        self.locs.get(line_id as usize)
    }
}

#[derive(Debug, Default)]
pub struct TimeIndex {
    pub ts: Vec<i64>,
}

impl TimeIndex {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            ts: Vec::with_capacity(cap),
        }
    }

    pub fn push(&mut self, ts_micros: i64) {
        self.ts.push(ts_micros);
    }

    pub fn len(&self) -> usize {
        self.ts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ts.is_empty()
    }

    pub fn get(&self, line_id: u64) -> Option<i64> {
        self.ts.get(line_id as usize).copied()
    }

    /// Number of records that landed in the untimed bucket. Useful for the
    /// status line ("12 lines have no timestamp").
    pub fn untimed_count(&self) -> usize {
        self.ts.iter().filter(|t| **t == TS_UNTIMED).count()
    }
}

#[derive(Debug, Default)]
pub struct SeverityIndex {
    pub levels: Vec<u8>,
}

impl SeverityIndex {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            levels: Vec::with_capacity(cap),
        }
    }

    pub fn push(&mut self, level: u8) {
        self.levels.push(level);
    }

    pub fn len(&self) -> usize {
        self.levels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    pub fn get(&self, line_id: u64) -> Option<u8> {
        self.levels.get(line_id as usize).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::record::severity;

    #[test]
    fn parallel_arrays_stay_in_lockstep() {
        let mut li = LineIndex::default();
        let mut ti = TimeIndex::default();
        let mut si = SeverityIndex::default();

        let id1 = li.push(LineLoc { source_id: 0, offset: 0, len: 10 });
        ti.push(1_700_000_000_000_000);
        si.push(severity::INFO);

        let id2 = li.push(LineLoc { source_id: 0, offset: 10, len: 12 });
        ti.push(1_700_000_001_000_000);
        si.push(severity::ERROR);

        assert_eq!(id1, 0);
        assert_eq!(id2, 1);
        assert_eq!(li.len(), 2);
        assert_eq!(ti.len(), 2);
        assert_eq!(si.len(), 2);
        assert_eq!(si.get(id2), Some(severity::ERROR));
        assert_eq!(ti.get(id1), Some(1_700_000_000_000_000));
    }
}
