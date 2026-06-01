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

use std::sync::Arc;

use memmap2::Mmap;

use crate::engine::indexer::Indexes;

/// Owns the indexed data plus the mmap snapshots needed to resolve bytes by
/// `line_id`. Single-source today; the dense `mmaps` vector is keyed by
/// `source_id` and is ready for the M1.5 k-way-merge step.
pub struct Engine {
    pub indexes: Indexes,
    /// `mmaps[source_id]` is the resolver for `RecordBytes::FileRef`. Streams
    /// don't need an entry — their bytes are owned. We still keep an entry
    /// (a zero-length Arc) per source to keep the index dense.
    pub mmaps: Vec<Arc<Mmap>>,
    /// For stream sources, the owned bytes per record are not stored in the
    /// engine — they would double the memory cost. The indexer keeps them
    /// alive in `owned_lines[line_id]` only for stream sources. File sources
    /// leave a `None` here and resolve through `mmaps`.
    pub owned_lines: Vec<Option<Box<[u8]>>>,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            indexes: Indexes::default(),
            mmaps: Vec::new(),
            owned_lines: Vec::new(),
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
        if let Some(Some(b)) = self.owned_lines.get(line_id as usize) {
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
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}
