//! Engine layer: ingest, indexes, queries, histogram.
//!
//! Threading:
//! - indexer thread: consumes `RawRecord` from a `RecordProducer`, fills
//!   line/time/severity indexes, updates active predicate bitsets in lockstep.
//! - query thread: backfills new predicates against the indexed prefix `[0, K)`,
//!   honors cancellation.
//! - main thread: UI loop (in pulse-tui), pulls snapshots.
//!
//! Channels: `crossbeam-channel`. No async runtime.

pub mod record;

pub mod histogram;
pub mod indexer;
pub mod indexes;
pub mod parse;
pub mod predicate;
pub mod query;

use std::collections::HashMap;
use std::sync::Arc;

use memmap2::Mmap;

use crate::engine::indexer::Indexes;
use crate::schema::{LockedSchema, SchemaBuilder, FILE_WARMUP_LINES};

/// Owns the indexed data plus the mmap snapshots needed to resolve bytes by
/// `line_id`. Single-source today; the dense `mmaps` vector is keyed by
/// `source_id` and is ready for the M1.5 k-way-merge step.
pub struct Engine {
    pub indexes: Indexes,
    /// `mmaps[source_id]` is the resolver for `RecordBytes::FileRef`. Streams
    /// don't need an entry — their bytes are owned. We still keep an entry
    /// (a zero-length Arc) per source to keep the index dense.
    pub mmaps: Vec<Arc<Mmap>>,
    /// For stream sources, the owned bytes per record are stored sparsely —
    /// only stream records take space. Previously this was a parallel
    /// `Vec<Option<Box<[u8]>>>`, which spent ~16 B per file record on empty
    /// `None` slots (a fat pointer + Option discriminant, ~176 MB on the
    /// 11M-record fixture). The HashMap is empty for pure-file pipelines.
    pub owned_lines: HashMap<u64, Box<[u8]>>,
    /// Frozen-after-warmup schema. None until `scan_schema` runs.
    pub schema: Option<LockedSchema>,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            indexes: Indexes::default(),
            mmaps: Vec::new(),
            owned_lines: HashMap::new(),
            schema: None,
        }
    }

    /// Resolve a single line's bytes. Returns the slice or `&[]` if the
    /// `line_id` is out of range. Cheap and synchronous — UI calls this in
    /// the render path.
    pub fn line_bytes(&self, line_id: u64) -> &[u8] {
        let loc = match self.indexes.line.get(line_id) {
            Some(l) => l,
            None => return &[],
        };
        if let Some(b) = self.owned_lines.get(&line_id) {
            return b;
        }
        let mmap = match self.mmaps.get(loc.source_id as usize) {
            Some(m) => m,
            None => return &[],
        };
        let start = loc.offset as usize;
        let end = start + loc.len as usize;
        if end > mmap.len() {
            return &[];
        }
        &mmap[start..end]
    }

    /// Build the locked schema by scanning the first `min(FILE_WARMUP_LINES,
    /// indexed)` records. Call once after `indexer::drain` is done.
    pub fn scan_schema(&mut self) {
        let total = self.indexes.len() as u64;
        let window = (total.min(FILE_WARMUP_LINES as u64)) as u64;
        let mut sb = SchemaBuilder::new();
        for line_id in 0..window {
            let bytes = self.line_bytes(line_id);
            sb.scan(bytes);
        }
        self.schema = Some(sb.lock());
    }

    /// True when more than half the records carry a parsed timestamp. We
    /// use a majority threshold (not "at least one") so a single
    /// accidentally-timestamp-shaped line in a plain-text log doesn't
    /// flip the whole UI into structured mode and render a useless
    /// single-bar histogram. Mirror logic in less-mode rules.
    pub fn has_timestamps(&self) -> bool {
        let total = self.indexes.len() as u64;
        if total == 0 {
            return false;
        }
        let timed = total - self.indexes.parse_stats.untimed;
        timed * 2 > total
    }

    /// True when more than half the records carry a recognizable severity
    /// level. Same majority logic as `has_timestamps` — a stray ERROR-shaped
    /// word in plain text shouldn't unlock severity tabs.
    pub fn has_severity(&self) -> bool {
        let total = self.indexes.len();
        if total == 0 {
            return false;
        }
        let known = self
            .indexes
            .severity
            .levels
            .iter()
            .filter(|s| **s != crate::engine::record::severity::UNKNOWN)
            .count();
        known * 2 > total
    }

    /// True if schema warmup found at least one structured field. False on
    /// plain-text inputs — the table should hide column headers and give
    /// the whole row width to the raw payload.
    pub fn has_structured_fields(&self) -> bool {
        self.schema
            .as_ref()
            .map(|s| !s.ordered_fields.is_empty())
            .unwrap_or(false)
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}
