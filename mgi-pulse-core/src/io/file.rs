//! FileProducer: mmap-backed source.
//!
//! Owns `Arc<Mmap>`. Lines are found via `memchr::memchr_iter(b'\n', ...)`.
//! Emits records that reference the mmap via offset/length, not a borrowed
//! slice with a lifetime. The engine resolves bytes against a per-pass
//! snapshot of `Arc<Mmap>` keyed by `source_id`.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use memmap2::Mmap;

use crate::engine::record::{RawRecord, RecordBytes, TS_UNTIMED};
use crate::io::RecordProducer;

pub struct FileProducer {
    source_id: u32,
    mmap: Arc<Mmap>,
    // Cursor in bytes into the mmap.
    cursor: usize,
    line_id_counter: u64,
}

impl FileProducer {
    /// Open `path` and mmap it. The file is held open via the Mmap (the
    /// underlying File is dropped — mmap owns the kernel handle).
    pub fn open<P: AsRef<Path>>(path: P, source_id: u32) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        // SAFETY: we don't write to the mmap, and Mmap::map's contract about
        // file mutation under us is well-understood: a process truncating the
        // file from underneath would be a bug we don't promise to survive in
        // v0.1 — that's the static-file path. Live tails go through stream.
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Self {
            source_id,
            mmap: Arc::new(mmap),
            cursor: 0,
            line_id_counter: 0,
        })
    }

    /// Hand a shared snapshot of the mmap. The engine keeps these per
    /// `source_id` to resolve `RecordBytes::FileRef` in the hot read path.
    pub fn mmap(&self) -> Arc<Mmap> {
        Arc::clone(&self.mmap)
    }

    pub fn source_id(&self) -> u32 {
        self.source_id
    }

    pub fn total_bytes(&self) -> u64 {
        self.mmap.len() as u64
    }
}

impl RecordProducer for FileProducer {
    fn next(&mut self) -> Option<RawRecord> {
        let buf: &[u8] = &self.mmap[..];
        if self.cursor >= buf.len() {
            return None;
        }
        // Locate the next newline, or take to EOF.
        let rest = &buf[self.cursor..];
        let nl = memchr::memchr(b'\n', rest);
        let (line_len, advance) = match nl {
            Some(pos) => (pos, pos + 1),
            None => (rest.len(), rest.len()),
        };
        // Skip empty lines (blank "" between two \n) — they're not records.
        if line_len == 0 {
            self.cursor += advance;
            return self.next();
        }
        let offset = self.cursor as u64;
        let len = line_len as u32;
        let line_id = self.line_id_counter;
        self.line_id_counter += 1;
        self.cursor += advance;
        Some(RawRecord {
            source_id: self.source_id,
            line_id,
            // ts and severity are filled in by the indexer — the producer
            // hands raw bytes only; the engine owns parsing.
            ts_micros: TS_UNTIMED,
            severity: 0,
            bytes: RecordBytes::FileRef {
                source_id: self.source_id,
                offset,
                len,
            },
        })
    }

    fn is_live(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
