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
pub fn drain<P: crate::io::RecordProducer>(
    producer: &mut P,
    engine: &mut crate::engine::Engine,
) {
    while let Some(rec) = producer.next() {
        let (loc, owned): (LineLoc, Option<Box<[u8]>>) = match rec.bytes {
            RecordBytes::FileRef { source_id, offset, len } => (
                LineLoc { source_id, offset, len },
                None,
            ),
            RecordBytes::Owned(boxed) => (
                LineLoc {
                    source_id: rec.source_id,
                    offset: 0,
                    len: boxed.len() as u32,
                },
                Some(boxed),
            ),
        };
        engine.indexes.line.locs.push(loc);
        engine.indexes.time.ts.push(rec.ts_micros);
        engine.indexes.severity.levels.push(rec.severity);
        engine.owned_lines.push(owned);
    }
}
