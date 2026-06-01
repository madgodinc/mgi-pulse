//! Query: apply a predicate to the indexed prefix `[0, K)`.
//!
//! The full plan calls for a background query thread with cancellation, a
//! K-snapshot handoff, and a shared FieldCache. M1 ships only what `/search`
//! actually needs: a synchronous scan that walks every line in `[0, K)`,
//! evaluates the predicate, and returns matching `line_id`s in ascending
//! order. The result feeds the filtered table view directly.
//!
//! On 2 GB of NDJSON the parse-bench measured ~905 MB/s for parsing; raw
//! byte access via mmap is ~12 GB/s. A regex scan over 11M lines lands well
//! below the 100 ms first-paint gate (M1.a) and is comfortable as the M1
//! search implementation.
//!
//! Per-record `FieldCache` (engine::format) is built and reset between
//! records so multi-field predicates pay the parse cost at most once per
//! field per record, regardless of how the AndPredicate composes them.

use crate::engine::format::{FieldCache, LogFormat};
use crate::engine::predicate::Predicate;
use crate::engine::Engine;

pub fn scan(engine: &Engine, predicate: &dyn Predicate) -> Vec<u64> {
    let mut matches = Vec::new();
    let total = engine.indexes.len() as u64;
    if total == 0 {
        return matches;
    }
    // Build the cache once, reset between records.
    let first_bytes = engine.line_bytes(0);
    let first_fmt = engine
        .indexes
        .line
        .get(0)
        .map(|loc| engine.format_of(loc.source_id))
        .unwrap_or(LogFormat::Ndjson);
    let mut cache = FieldCache::new(first_fmt, first_bytes);
    for line_id in 0..total {
        let bytes = engine.line_bytes(line_id);
        let rec = synth_record(engine, line_id);
        let fmt = engine.format_of(rec.source_id);
        if fmt != cache.format() {
            cache = FieldCache::new(fmt, bytes);
        } else {
            cache.reset(bytes);
        }
        if predicate.matches(&rec, &mut cache) {
            matches.push(line_id);
        }
    }
    matches
}

fn synth_record(engine: &Engine, line_id: u64) -> crate::engine::record::RawRecord {
    let loc = engine.indexes.line.get(line_id).unwrap();
    crate::engine::record::RawRecord {
        source_id: loc.source_id,
        line_id,
        ts_micros: engine
            .indexes
            .time
            .get(line_id)
            .unwrap_or(crate::engine::record::TS_UNTIMED),
        severity: engine.indexes.severity.get(line_id).unwrap_or(0),
        bytes: crate::engine::record::RecordBytes::FileRef {
            source_id: loc.source_id,
            offset: loc.offset,
            len: loc.len,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::predicate::RegexBytesPredicate;
    use crate::engine::Engine;
    use crate::io::stream::StreamProducer;
    use std::io::Cursor;

    #[test]
    fn scan_returns_matching_line_ids() {
        let body = b"\
            {\"ts\":\"2026-06-01T12:00:00Z\",\"level\":\"info\",\"msg\":\"hello\"}\n\
            {\"ts\":\"2026-06-01T12:00:01Z\",\"level\":\"error\",\"msg\":\"boom\"}\n\
            {\"ts\":\"2026-06-01T12:00:02Z\",\"level\":\"warn\",\"msg\":\"hmm\"}\n\
            {\"ts\":\"2026-06-01T12:00:03Z\",\"level\":\"error\",\"msg\":\"again\"}\n\
        ";
        let mut prod = StreamProducer::new(Cursor::new(body.to_vec()), 0);
        let mut engine = Engine::new();
        crate::engine::indexer::drain(&mut prod, &mut engine);

        let p = RegexBytesPredicate::new("error").unwrap();
        let hits = scan(&engine, &p);
        assert_eq!(hits, vec![1, 3]);
    }
}
