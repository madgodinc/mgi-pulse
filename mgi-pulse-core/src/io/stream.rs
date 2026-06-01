//! StreamProducer: BufRead-backed source for stdin and (in v0.2) growing tails.
//!
//! Emits records with `RecordBytes::Owned(Box<[u8]>)`. No mmap, no lifetimes
//! crossing thread boundaries. Parses `ts`/`level` at emit time, same as
//! FileProducer — so k-way merge downstream has timestamps.

use std::io::BufRead;

use crate::engine::parse::{ts_and_level, ts_and_level_named, FieldNames, ParseStats};
use crate::engine::record::{RawRecord, RecordBytes};
use crate::io::RecordProducer;

pub struct StreamProducer<R: BufRead> {
    source_id: u32,
    reader: R,
    line_id_counter: u64,
    scratch: Vec<u8>,
    closed: bool,
    stats: ParseStats,
    fields: Option<FieldNames>,
}

impl<R: BufRead> StreamProducer<R> {
    pub fn new(reader: R, source_id: u32) -> Self {
        Self {
            source_id,
            reader,
            line_id_counter: 0,
            scratch: Vec::with_capacity(4096),
            closed: false,
            stats: ParseStats::default(),
            fields: None,
        }
    }

    pub fn with_fields(mut self, fields: FieldNames) -> Self {
        self.fields = Some(fields);
        self
    }

    pub fn source_id(&self) -> u32 {
        self.source_id
    }

    pub fn stats(&self) -> ParseStats {
        self.stats
    }
}

impl<R: BufRead> RecordProducer for StreamProducer<R> {
    fn next(&mut self) -> Option<RawRecord> {
        if self.closed {
            return None;
        }
        loop {
            self.scratch.clear();
            match self.reader.read_until(b'\n', &mut self.scratch) {
                Ok(0) => {
                    self.closed = true;
                    return None;
                }
                Ok(_) => {
                    if let Some(&b) = self.scratch.last() {
                        if b == b'\n' {
                            self.scratch.pop();
                        }
                    }
                    if let Some(&b) = self.scratch.last() {
                        if b == b'\r' {
                            self.scratch.pop();
                        }
                    }
                    if self.scratch.is_empty() {
                        continue;
                    }
                    let (ts_micros, severity) = match &self.fields {
                        Some(f) => ts_and_level_named(&self.scratch, f, &mut self.stats),
                        None => ts_and_level(&self.scratch, &mut self.stats),
                    };
                    let line_id = self.line_id_counter;
                    self.line_id_counter += 1;
                    let owned = self.scratch.clone().into_boxed_slice();
                    return Some(RawRecord {
                        source_id: self.source_id,
                        line_id,
                        ts_micros,
                        severity,
                        bytes: RecordBytes::Owned(owned),
                    });
                }
                Err(_) => {
                    // IO error — close silently in v0.1. M2 surfaces this via
                    // tracing::warn! and a status-line hint.
                    self.closed = true;
                    return None;
                }
            }
        }
    }

    fn is_live(&self) -> bool {
        !self.closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::record::severity;
    use std::io::Cursor;

    #[test]
    fn emits_records_and_drops_trailing_blank() {
        let body = b"alpha\n\nbeta\r\ngamma".to_vec();
        let mut prod = StreamProducer::new(Cursor::new(body), 7);
        let r1 = prod.next().unwrap();
        let r2 = prod.next().unwrap();
        let r3 = prod.next().unwrap();
        assert!(prod.next().is_none());

        for (r, expected, expected_id) in [
            (r1, b"alpha".as_slice(), 0),
            (r2, b"beta", 1),
            (r3, b"gamma", 2),
        ] {
            assert_eq!(r.line_id, expected_id);
            assert_eq!(r.source_id, 7);
            match r.bytes {
                RecordBytes::Owned(b) => assert_eq!(&*b, expected),
                _ => panic!("expected Owned"),
            }
        }
    }

    #[test]
    fn ts_and_level_parsed_for_json_lines() {
        let body = b"{\"ts\":\"2026-06-01T12:00:00Z\",\"level\":\"warn\",\"msg\":\"x\"}\n";
        let mut prod = StreamProducer::new(Cursor::new(body.to_vec()), 0);
        let r = prod.next().unwrap();
        assert_eq!(r.severity, severity::WARN);
        assert!(r.ts_micros > 0);
    }
}
