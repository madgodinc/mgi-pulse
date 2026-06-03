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

pub mod dsl;
pub mod format;
pub mod histogram;
pub mod indexer;
pub mod indexes;
pub mod parse;
pub mod parse_access;
pub mod parse_csv;
pub mod parse_edn;
pub mod parse_logfmt;
pub mod parse_python;
pub mod parse_syslog;
pub mod predicate;
pub mod query;

use std::sync::Arc;

use memmap2::Mmap;

use crate::engine::format::LogFormat;
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
    /// Stream-source owned bytes, indexed by `line_id - stream_base`. Stream
    /// records are append-only with dense, monotonic line_ids; a Vec by
    /// offset-from-base resolves in O(1) without the hash overhead a HashMap
    /// pays on every render and predicate evaluation. Empty for pure-file
    /// pipelines.
    pub owned_lines: Vec<Box<[u8]>>,
    /// First `line_id` that belongs to the stream source. `None` until the
    /// first owned record arrives. Set once at the start of streaming and
    /// never changed; the indexer asserts `line_id - stream_base ==
    /// owned_lines.len()` before each push.
    pub stream_base: Option<u64>,
    /// Per-source format. Mirrors `mmaps` for file sources and grows
    /// alongside it when streams join. Predicates look up
    /// `source_formats[source_id]` to dispatch parsing.
    pub source_formats: Vec<LogFormat>,
    /// Per-source column headers for CSV/TSV sources. Stays `None` for
    /// stateless formats (NDJSON, logfmt, EDN, Python, syslog).
    /// Captured at indexing time from the first record of each
    /// CSV/TSV source and never mutated again.
    pub source_headers: Vec<Option<Vec<String>>>,
    /// Frozen-after-warmup schema. None until `scan_schema` runs.
    pub schema: Option<LockedSchema>,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            indexes: Indexes::default(),
            mmaps: Vec::new(),
            owned_lines: Vec::new(),
            stream_base: None,
            source_formats: Vec::new(),
            source_headers: Vec::new(),
            schema: None,
        }
    }

    /// Lookup the format of one source. Falls back to NDJSON if the
    /// index is empty (single-source pipelines populate slot 0 only).
    pub fn format_of(&self, source_id: u32) -> LogFormat {
        self.source_formats
            .get(source_id as usize)
            .copied()
            .unwrap_or(LogFormat::Ndjson)
    }

    /// Look up the captured column headers for one source. Returns
    /// `None` for stateless formats and for CSV/TSV sources whose
    /// first row hasn't been parsed yet (i.e. before
    /// `capture_csv_headers`).
    pub fn headers_of(&self, source_id: u32) -> Option<&[String]> {
        self.source_headers
            .get(source_id as usize)
            .and_then(|h| h.as_deref())
    }

    /// For every CSV/TSV source, parse the first record as the column
    /// header and stash it in `source_headers`. Call once after
    /// `indexer::drain` completes. The header line stays in the index
    /// as record 0 — hiding it from the table view is a separate
    /// concern (it would let predicates positional-address `_N`
    /// shift their meaning otherwise).
    pub fn capture_csv_headers(&mut self) {
        // Resize the headers vec to match formats — fill missing slots
        // with None so indexing by source_id is always safe.
        if self.source_headers.len() < self.source_formats.len() {
            self.source_headers
                .resize(self.source_formats.len(), None);
        }
        // Find the first line_id from each CSV/TSV source. For
        // single-source pipelines this is line_id=0; for merge we'd
        // need a per-source scan, but k-way merge across CSV sources
        // isn't a supported configuration today.
        for (sid, fmt) in self.source_formats.iter().enumerate() {
            let delim = match fmt {
                LogFormat::Csv => crate::engine::parse_csv::Delim::Comma,
                LogFormat::Tsv => crate::engine::parse_csv::Delim::Tab,
                _ => continue,
            };
            // Take the first record whose loc.source_id == sid. Linear
            // scan; single-source files exit on the first iteration.
            let mut header_line: Option<Vec<u8>> = None;
            for (line_id, loc) in self.indexes.line.locs.iter().enumerate() {
                if loc.source_id as usize == sid {
                    header_line = Some(self.line_bytes(line_id as u64).to_vec());
                    break;
                }
            }
            if let Some(bytes) = header_line {
                let headers = crate::engine::parse_csv::split_record(&bytes, delim);
                self.source_headers[sid] = Some(headers);
            }
        }
        self.recompute_csv_ts_level();
    }

    /// Walk every record whose source is CSV/TSV and re-parse its
    /// `(ts, severity)` using the now-known header. The indexer's
    /// initial pass marked them all untimed because it ran before the
    /// header was captured. Header row itself stays untimed/unknown.
    fn recompute_csv_ts_level(&mut self) {
        for line_id in 0..self.indexes.line.locs.len() as u64 {
            let loc = self.indexes.line.locs[line_id as usize];
            let fmt = self.format_of(loc.source_id);
            let delim = match fmt {
                LogFormat::Csv => crate::engine::parse_csv::Delim::Comma,
                LogFormat::Tsv => crate::engine::parse_csv::Delim::Tab,
                _ => continue,
            };
            // The header line itself stays untimed — it isn't data.
            // For single-source it's line_id=0; for k-way merge (not
            // yet supported on CSV) it would be the first record per
            // source.
            let is_header = self
                .indexes
                .line
                .locs
                .iter()
                .position(|l| l.source_id == loc.source_id)
                .map(|p| p as u64 == line_id)
                .unwrap_or(false);
            if is_header {
                continue;
            }
            let Some(headers) = self.headers_of(loc.source_id) else {
                continue;
            };
            let headers_owned: Vec<String> = headers.to_vec();
            let bytes = self.line_bytes(line_id).to_vec();
            let mut stats = crate::engine::parse::ParseStats::default();
            let (ts, sev) = crate::engine::parse_csv::ts_and_level_with_header(
                &bytes,
                delim,
                &headers_owned,
                &mut stats,
                None,
            );
            self.indexes.time.ts[line_id as usize] = ts;
            self.indexes.severity.levels[line_id as usize] = sev;
            // Fold the per-record stats into the global aggregate so
            // the dry-run summary reflects post-CSV-wire numbers.
            // Subtract the indexer's earlier "untimed" mark first to
            // avoid double-counting (every CSV row got incremented in
            // the first pass).
            self.indexes.parse_stats.untimed -= 1;
            self.indexes.parse_stats.untimed += stats.untimed;
            self.indexes.parse_stats.ts_parse_errors += stats.ts_parse_errors;
        }
    }

    /// Resolve a single line's bytes. Returns the slice or `&[]` if the
    /// `line_id` is out of range. Cheap and synchronous — UI calls this in
    /// the render path. Stream rows go through a dense Vec by offset; file
    /// rows resolve to the per-source mmap.
    pub fn line_bytes(&self, line_id: u64) -> &[u8] {
        let loc = match self.indexes.line.get(line_id) {
            Some(l) => l,
            None => return &[],
        };
        if let Some(base) = self.stream_base {
            if line_id >= base {
                let idx = (line_id - base) as usize;
                if let Some(b) = self.owned_lines.get(idx) {
                    return b;
                }
            }
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

    /// Push an owned (stream) record into dense storage. The indexer calls
    /// this; sets `stream_base` on first owned record so subsequent records
    /// land at `line_id - base`.
    pub fn push_owned(&mut self, line_id: u64, bytes: Box<[u8]>) {
        if self.stream_base.is_none() {
            self.stream_base = Some(line_id);
        }
        let base = self.stream_base.unwrap();
        debug_assert_eq!(
            (line_id - base) as usize,
            self.owned_lines.len(),
            "stream records must arrive in monotonic line_id order"
        );
        self.owned_lines.push(bytes);
    }

    /// Ingest one `RawRecord` directly into the engine, the same way
    /// `indexer::drain` would but for a single record. Used by the
    /// background follow worker that streams in records after the
    /// initial synchronous indexing finishes. Returns the new
    /// `line_id` of the inserted record.
    ///
    /// The caller is responsible for line ordering — owned records
    /// must arrive in monotonic order per the `push_owned` invariant.
    /// For `--follow` the worker is the single producer so this holds
    /// trivially.
    pub fn ingest_one(&mut self, rec: crate::engine::record::RawRecord) -> u64 {
        use crate::engine::indexes::LineLoc;
        use crate::engine::record::RecordBytes;
        let line_id = self.indexes.line.locs.len() as u64;
        match rec.bytes {
            RecordBytes::FileRef {
                source_id,
                offset,
                len,
            } => {
                self.indexes.line.locs.push(LineLoc {
                    source_id,
                    offset,
                    len,
                });
            }
            RecordBytes::Owned(boxed) => {
                self.indexes.line.locs.push(LineLoc {
                    source_id: rec.source_id,
                    offset: 0,
                    len: boxed.len() as u32,
                });
                self.push_owned(line_id, boxed);
            }
            RecordBytes::FileRefMulti { source_id, spans } => {
                let mut joined: Vec<u8> = Vec::new();
                if let Some(mmap) = self.mmaps.get(source_id as usize) {
                    for (offset, len) in &spans {
                        let start = *offset as usize;
                        let end = start + *len as usize;
                        if end <= mmap.len() {
                            joined.extend_from_slice(&mmap[start..end]);
                            joined.push(b'\n');
                        }
                    }
                }
                let total_len = joined.len() as u32;
                self.indexes.line.locs.push(LineLoc {
                    source_id,
                    offset: 0,
                    len: total_len,
                });
                self.push_owned(line_id, joined.into_boxed_slice());
            }
        }
        self.indexes.time.ts.push(rec.ts_micros);
        self.indexes.severity.levels.push(rec.severity);
        line_id
    }

    /// Build the locked schema by scanning the first `min(FILE_WARMUP_LINES,
    /// indexed)` records. Call once after `indexer::drain` is done.
    pub fn scan_schema(&mut self) {
        let total = self.indexes.len() as u64;
        let window = total.min(FILE_WARMUP_LINES as u64);
        let mut sb = SchemaBuilder::new();
        for line_id in 0..window {
            let bytes = self.line_bytes(line_id);
            sb.scan(bytes);
        }
        self.schema = Some(sb.lock());
    }

    /// Rescan the schema across an arbitrary set of records — useful when the
    /// initial warmup landed on a boot banner or other unrepresentative
    /// prefix and the user hits `R` to retry over the filtered view.
    ///
    /// Samples a window **centered on the middle** of `line_ids` rather than
    /// the first N. Sampling the first N would degenerate to a no-op when
    /// `line_ids = [0..len)` (the same prefix the original warmup saw) — the
    /// whole point of `R` is to escape that prefix.
    pub fn rescan_schema(&mut self, line_ids: &[u64]) {
        let n = line_ids.len();
        if n == 0 {
            return;
        }
        let window = n.min(FILE_WARMUP_LINES);
        let start = n.saturating_sub(window) / 2;
        let end = start + window;
        let mut sb = SchemaBuilder::new();
        for &line_id in &line_ids[start..end] {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::record::{severity, RawRecord, RecordBytes};

    fn owned_rec(line_id: u64, ts: i64, sev: u8, bytes: &[u8]) -> RawRecord {
        RawRecord {
            source_id: 0,
            line_id,
            ts_micros: ts,
            severity: sev,
            bytes: RecordBytes::Owned(Box::from(bytes)),
        }
    }

    #[test]
    fn ingest_one_appends_and_returns_line_id() {
        let mut e = Engine::new();
        let id1 = e.ingest_one(owned_rec(0, 1_000_000, severity::INFO, b"a"));
        let id2 = e.ingest_one(owned_rec(0, 2_000_000, severity::WARN, b"b"));
        assert_eq!(id1, 0);
        assert_eq!(id2, 1);
        assert_eq!(e.indexes.len(), 2);
        assert_eq!(e.line_bytes(0), b"a");
        assert_eq!(e.line_bytes(1), b"b");
        assert_eq!(e.indexes.time.ts, vec![1_000_000, 2_000_000]);
        assert_eq!(
            e.indexes.severity.levels,
            vec![severity::INFO, severity::WARN]
        );
    }

    #[test]
    fn ingest_one_grows_existing_engine() {
        // Seed with one record, then live-ingest a second.
        let mut e = Engine::new();
        e.ingest_one(owned_rec(0, 1_000_000, severity::INFO, b"seed"));
        assert_eq!(e.indexes.len(), 1);
        e.ingest_one(owned_rec(0, 2_000_000, severity::ERROR, b"live"));
        assert_eq!(e.indexes.len(), 2);
        assert_eq!(e.line_bytes(1), b"live");
    }
}
