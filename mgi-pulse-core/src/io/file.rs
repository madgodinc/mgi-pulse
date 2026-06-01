//! FileProducer: mmap-backed source.
//!
//! Owns `Arc<Mmap>`. Lines are found via `memchr::memchr_iter(b'\n', ...)`.
//! Each emitted record carries the line's mmap location AND its parsed
//! `ts_micros`/`severity` — the producer is the single place where NDJSON
//! parse happens, so k-way merge downstream has timestamps to sort on.
//!
//! # mmap safety
//!
//! `Mmap::map` is `unsafe` because the OS gives us a memory region that
//! reflects the current file. If a *different* process truncates or
//! replaces the file underneath us, reading past the truncated region
//! delivers `SIGBUS` to the whole process — not a Rust error, not
//! catchable as `Result`. The process just dies.
//!
//! For mgi-pulse this means: **FileProducer is for static snapshots**
//! (rotated logs, archived files). Active log files that the running
//! application keeps writing to should be opened through the stream
//! path (`tail -F file | mgi-pulse -`), which uses owned buffers and
//! is safe under rotation. README documents this for end users.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use memmap2::Mmap;

use crate::engine::parse::{ts_and_level, ts_and_level_named, FieldNames, ParseStats};
use crate::engine::record::{RawRecord, RecordBytes};
use crate::io::RecordProducer;

pub struct FileProducer {
    source_id: u32,
    mmap: Arc<Mmap>,
    cursor: usize,
    line_id_counter: u64,
    stats: ParseStats,
    /// When set, overrides the default `ts` / `level` field names.
    fields: Option<FieldNames>,
}

impl FileProducer {
    pub fn open<P: AsRef<Path>>(path: P, source_id: u32) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        // SAFETY: read-only access; we don't promise to survive concurrent
        // truncation of static files in v0.1. Live tails go through stream.
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Self {
            source_id,
            mmap: Arc::new(mmap),
            cursor: 0,
            line_id_counter: 0,
            stats: ParseStats::default(),
            fields: None,
        })
    }

    /// Override the JSON field names used to extract ts / level. Pass this
    /// before draining if the source's schema differs from the defaults.
    pub fn with_fields(mut self, fields: FieldNames) -> Self {
        self.fields = Some(fields);
        self
    }

    pub fn mmap(&self) -> Arc<Mmap> {
        Arc::clone(&self.mmap)
    }

    pub fn source_id(&self) -> u32 {
        self.source_id
    }

    pub fn total_bytes(&self) -> u64 {
        self.mmap.len() as u64
    }

    /// Cumulative parse stats since `open`. The engine collects these per
    /// source and folds them into the aggregate counters.
    pub fn stats(&self) -> ParseStats {
        self.stats
    }
}

impl RecordProducer for FileProducer {
    fn next(&mut self) -> Option<RawRecord> {
        let buf: &[u8] = &self.mmap[..];
        loop {
            if self.cursor >= buf.len() {
                return None;
            }
            let rest = &buf[self.cursor..];
            let (line_len, advance) = match memchr::memchr(b'\n', rest) {
                Some(pos) => (pos, pos + 1),
                None => (rest.len(), rest.len()),
            };
            if line_len == 0 {
                // Blank line — skip, do not emit. Stay in the loop.
                self.cursor += advance;
                continue;
            }
            let offset = self.cursor as u64;
            let len = line_len as u32;
            let line_bytes = &buf[self.cursor..self.cursor + line_len];
            let (ts_micros, severity) = match &self.fields {
                Some(f) => ts_and_level_named(line_bytes, f, &mut self.stats),
                None => ts_and_level(line_bytes, &mut self.stats),
            };

            let line_id = self.line_id_counter;
            self.line_id_counter += 1;
            self.cursor += advance;
            return Some(RawRecord {
                source_id: self.source_id,
                line_id,
                ts_micros,
                severity,
                bytes: RecordBytes::FileRef {
                    source_id: self.source_id,
                    offset,
                    len,
                },
            });
        }
    }

    fn is_live(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::record::severity;
    use std::io::Write;

    fn write_tmp(name: &str, body: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mgi-pulse-fileproducer-{}.ndjson", name));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body).unwrap();
        p
    }

    #[test]
    fn emits_one_record_per_line() {
        let p = write_tmp(
            "basic",
            b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n",
        );
        let mut prod = FileProducer::open(&p, 0).unwrap();
        let r1 = prod.next().unwrap();
        let r2 = prod.next().unwrap();
        let r3 = prod.next().unwrap();
        assert!(prod.next().is_none());
        match (r1.bytes, r2.bytes, r3.bytes) {
            (
                RecordBytes::FileRef { offset: o1, len: l1, .. },
                RecordBytes::FileRef { offset: o2, len: l2, .. },
                RecordBytes::FileRef { offset: o3, len: l3, .. },
            ) => {
                assert_eq!((o1, l1), (0, 7));
                assert_eq!((o2, l2), (8, 7));
                assert_eq!((o3, l3), (16, 7));
            }
            _ => panic!("expected FileRef"),
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn last_line_without_trailing_newline_is_emitted() {
        let p = write_tmp("no_trailing_nl", b"{\"a\":1}\n{\"a\":2}");
        let mut prod = FileProducer::open(&p, 0).unwrap();
        let _r1 = prod.next().unwrap();
        let r2 = prod.next().unwrap();
        assert!(prod.next().is_none());
        match r2.bytes {
            RecordBytes::FileRef { offset, len, .. } => {
                assert_eq!((offset, len), (8, 7));
            }
            _ => panic!(),
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn empty_lines_are_skipped() {
        let p = write_tmp("blanks", b"{\"a\":1}\n\n{\"a\":2}\n\n");
        let mut prod = FileProducer::open(&p, 0).unwrap();
        let r1 = prod.next().unwrap();
        let r2 = prod.next().unwrap();
        assert!(prod.next().is_none());
        assert_eq!(r1.line_id, 0);
        assert_eq!(r2.line_id, 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn ts_and_level_are_parsed_at_emit_time() {
        let p = write_tmp(
            "parsed",
            b"{\"ts\":\"2026-06-01T12:00:00Z\",\"level\":\"error\",\"msg\":\"x\"}\n",
        );
        let mut prod = FileProducer::open(&p, 0).unwrap();
        let r1 = prod.next().unwrap();
        assert_eq!(r1.severity, severity::ERROR);
        assert!(r1.ts_micros > 0);
        let _ = std::fs::remove_file(&p);
    }
}
