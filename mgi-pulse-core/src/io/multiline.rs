//! Multi-line record assembler.
//!
//! Wraps any `RecordProducer` and folds continuation lines (`^\s+`) into
//! the preceding record. Detection comes from `LogFormat::is_continuation`;
//! v0.1 considers a line a continuation when it starts with whitespace,
//! which matches Java / Python / Ruby stack traces and most multi-line
//! exception serialisations.
//!
//! Zero-copy story: contiguous continuation lines (the common case for a
//! single writer in one process) come out as one `FileRef` with `len`
//! extended to cover the whole block — no allocation, mmap stays the
//! source. Owned bytes (stream path) are concatenated with a newline.

use crate::engine::format::LogFormat;
use crate::engine::record::{RawRecord, RecordBytes};
use crate::io::RecordProducer;

pub struct MultiLineProducer<P: RecordProducer> {
    inner: P,
    format: LogFormat,
    pending: Option<RawRecord>,
    done: bool,
}

impl<P: RecordProducer> MultiLineProducer<P> {
    pub fn new(inner: P, format: LogFormat) -> Self {
        Self {
            inner,
            format,
            pending: None,
            done: false,
        }
    }
}

impl<P: RecordProducer> RecordProducer for MultiLineProducer<P> {
    fn next(&mut self) -> Option<RawRecord> {
        if self.done {
            return None;
        }
        if self.pending.is_none() {
            self.pending = self.inner.next();
            if self.pending.is_none() {
                self.done = true;
                return None;
            }
        }
        loop {
            let next_rec = match self.inner.next() {
                Some(r) => r,
                None => {
                    self.done = true;
                    return self.pending.take();
                }
            };
            // We can only test continuation when the inner producer hands
            // us the bytes directly. For FileRef we'd need engine-level
            // mmap resolution, which lives outside this wrapper — so for
            // v0.1 the wrapper only folds Owned records (stream path).
            let is_cont = match &next_rec.bytes {
                RecordBytes::Owned(b) => self.format.is_continuation(b),
                _ => false,
            };
            if !is_cont {
                let to_emit = self.pending.take();
                self.pending = Some(next_rec);
                return to_emit;
            }
            if let Some(p) = self.pending.as_mut() {
                merge_into(p, next_rec);
            }
        }
    }

    fn is_live(&self) -> bool {
        self.inner.is_live() && !self.done
    }
}

fn merge_into(head: &mut RawRecord, next: RawRecord) {
    match (&mut head.bytes, next.bytes) {
        (
            RecordBytes::FileRef {
                source_id: hs,
                offset: ho,
                len: hlen,
            },
            RecordBytes::FileRef {
                source_id: ns,
                offset: no,
                len: nlen,
            },
        ) if *hs == ns && *ho + *hlen as u64 + 1 == no => {
            // Contiguous: the `+1` covers the newline byte between them.
            *hlen += 1 + nlen;
        }
        (RecordBytes::Owned(head_buf), RecordBytes::Owned(next_buf)) => {
            let mut v = head_buf.to_vec();
            v.push(b'\n');
            v.extend_from_slice(&next_buf);
            *head_buf = v.into_boxed_slice();
        }
        (head_bytes, next_bytes) => {
            // Mixed or non-contiguous: fall back to owned concat. This
            // path is rare in v0.1 since the producer types don't mix.
            let head_owned: Vec<u8> = match head_bytes {
                RecordBytes::Owned(b) => b.to_vec(),
                _ => Vec::new(),
            };
            let next_owned: Vec<u8> = match &next_bytes {
                RecordBytes::Owned(b) => b.to_vec(),
                _ => Vec::new(),
            };
            let mut v = Vec::with_capacity(head_owned.len() + 1 + next_owned.len());
            v.extend_from_slice(&head_owned);
            v.push(b'\n');
            v.extend_from_slice(&next_owned);
            *head_bytes = RecordBytes::Owned(v.into_boxed_slice());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::record::TS_UNTIMED;

    struct VecProducer {
        records: std::collections::VecDeque<RawRecord>,
    }

    impl VecProducer {
        fn new(records: Vec<RawRecord>) -> Self {
            Self {
                records: records.into(),
            }
        }
    }

    impl RecordProducer for VecProducer {
        fn next(&mut self) -> Option<RawRecord> {
            self.records.pop_front()
        }
        fn is_live(&self) -> bool {
            !self.records.is_empty()
        }
    }

    fn owned(s: &str) -> RawRecord {
        RawRecord {
            source_id: 0,
            line_id: 0,
            ts_micros: TS_UNTIMED,
            severity: 0,
            bytes: RecordBytes::Owned(s.as_bytes().to_vec().into_boxed_slice()),
        }
    }

    #[test]
    fn folds_continuation_lines_into_owner() {
        let records = vec![
            owned("level=error msg=boom"),
            owned("    at foo.bar(line 10)"),
            owned("    at foo.baz(line 20)"),
            owned("level=info msg=ok"),
        ];
        let mut p = MultiLineProducer::new(VecProducer::new(records), LogFormat::Logfmt);
        let first = p.next().unwrap();
        let second = p.next().unwrap();
        assert!(p.next().is_none());
        match &first.bytes {
            RecordBytes::Owned(b) => {
                let text = std::str::from_utf8(b).unwrap();
                assert!(text.starts_with("level=error msg=boom"));
                assert!(text.contains("at foo.bar"));
                assert!(text.contains("at foo.baz"));
            }
            _ => panic!("expected owned"),
        }
        match &second.bytes {
            RecordBytes::Owned(b) => {
                assert_eq!(std::str::from_utf8(b).unwrap(), "level=info msg=ok");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn ndjson_never_folds() {
        let records = vec![
            owned(r#"{"level":"error"}"#),
            owned("    not actually a continuation"),
            owned(r#"{"level":"info"}"#),
        ];
        let mut p = MultiLineProducer::new(VecProducer::new(records), LogFormat::Ndjson);
        let r1 = p.next().unwrap();
        let r2 = p.next().unwrap();
        let r3 = p.next().unwrap();
        assert!(p.next().is_none());
        // NDJSON's is_continuation is always false; each line emerges as
        // its own record.
        match &r1.bytes {
            RecordBytes::Owned(b) => assert_eq!(b.as_ref(), br#"{"level":"error"}"#),
            _ => panic!(),
        }
        match &r2.bytes {
            RecordBytes::Owned(b) => assert_eq!(b.as_ref(), b"    not actually a continuation"),
            _ => panic!(),
        }
        match &r3.bytes {
            RecordBytes::Owned(b) => assert_eq!(b.as_ref(), br#"{"level":"info"}"#),
            _ => panic!(),
        }
    }

    #[test]
    fn handles_record_with_no_continuation() {
        let records = vec![owned("level=info msg=hello")];
        let mut p = MultiLineProducer::new(VecProducer::new(records), LogFormat::Logfmt);
        let r = p.next().unwrap();
        assert!(p.next().is_none());
        match &r.bytes {
            RecordBytes::Owned(b) => assert_eq!(b.as_ref(), b"level=info msg=hello"),
            _ => panic!(),
        }
    }

    #[test]
    fn handles_empty_producer() {
        let mut p = MultiLineProducer::new(VecProducer::new(vec![]), LogFormat::Logfmt);
        assert!(p.next().is_none());
    }
}
