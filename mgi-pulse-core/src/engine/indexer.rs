//! Indexer thread.
//!
//! Drains a `RecordProducer` into the three parallel arrays
//! (LineIndex/TimeIndex/SeverityIndex). The producer is responsible for the
//! NDJSON parse of `ts`/`level` (see `engine::parse`); the indexer is purely
//! mechanical — append three fields per record, plus owned bytes for stream
//! sources.
//!
//! Memory budget target: ~150 MB of indexes per 1 GB of NDJSON, comfortable
//! up to ~5–10 GB total. On-disk indexes are backlog.

use crate::engine::indexes::{LineIndex, LineLoc, SeverityIndex, TimeIndex};
use crate::engine::parse::ParseStats;
use crate::engine::record::RecordBytes;

/// Three parallel arrays grown together. `LineIndex[i]`, `TimeIndex[i]`,
/// `SeverityIndex[i]` always refer to the same record.
#[derive(Debug, Default)]
pub struct Indexes {
    pub line: LineIndex,
    pub time: TimeIndex,
    pub severity: SeverityIndex,
    /// Aggregate parser outcomes, folded from per-producer stats.
    pub parse_stats: ParseStats,
}

impl Indexes {
    pub fn len(&self) -> usize {
        self.line.len()
    }

    pub fn is_empty(&self) -> bool {
        self.line.is_empty()
    }
}

/// Drain a producer into the engine. After the call, `engine.indexes` has
/// every record from the producer, and `engine.owned_lines` holds the bytes
/// of any stream-sourced records.
///
/// The producer must have already parsed `ts` and `level` into the
/// `RawRecord`; the indexer trusts those fields and does not re-parse.
pub fn drain<P: crate::io::RecordProducer>(producer: &mut P, engine: &mut crate::engine::Engine) {
    while let Some(rec) = producer.next() {
        match rec.bytes {
            RecordBytes::FileRef {
                source_id,
                offset,
                len,
            } => {
                engine.indexes.line.locs.push(LineLoc {
                    source_id,
                    offset,
                    len,
                });
            }
            RecordBytes::Owned(boxed) => {
                let line_id = engine.indexes.line.locs.len() as u64;
                engine.indexes.line.locs.push(LineLoc {
                    source_id: rec.source_id,
                    offset: 0,
                    len: boxed.len() as u32,
                });
                engine.push_owned(line_id, boxed);
            }
            RecordBytes::FileRefMulti { source_id, spans } => {
                // Producers don't emit this variant in v0.1 — the type is
                // reserved for the multi-line story in the format dispatch
                // phase. If we ever see it, concatenate the spans into an
                // Owned buffer and index as a stream record.
                let line_id = engine.indexes.line.locs.len() as u64;
                let mut joined: Vec<u8> = Vec::new();
                // Resolve the mmap once and copy each span. Spans within a
                // single source share a mmap; deeper resolution lives in
                // engine::line_bytes.
                if let Some(mmap) = engine.mmaps.get(source_id as usize) {
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
                engine.indexes.line.locs.push(LineLoc {
                    source_id,
                    offset: 0,
                    len: total_len,
                });
                engine.push_owned(line_id, joined.into_boxed_slice());
            }
        }
        engine.indexes.time.ts.push(rec.ts_micros);
        engine.indexes.severity.levels.push(rec.severity);
    }
}
