//! Native `tail -F` follow mode.
//!
//! `TailReader` implements `BufRead` over a file but blocks on EOF
//! instead of returning `Ok(0)`. When the file grows, the next `read`
//! call sees the new bytes. When the file is replaced (rotation —
//! inode changes), we reopen it and continue from the start of the new
//! file.
//!
//! Detection is poll-based: every `POLL_INTERVAL` we check the file's
//! inode against the last seen value. This avoids a `notify`
//! dependency (a heavy crate with platform-specific build deps) at
//! the cost of up to `POLL_INTERVAL` latency on new lines.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct TailReader {
    path: PathBuf,
    reader: BufReader<File>,
    inode: u64,
    /// Logical position in the underlying file: the offset we expect
    /// the next read to come from. Maintained by Read / BufRead impls
    /// (incrementing by the count of consumed bytes). Used by
    /// `check_rotation` to detect copytruncate: when the file's
    /// current size drops below this position, the file was truncated
    /// in place and we have to re-open and seek to 0.
    read_pos: u64,
}

impl TailReader {
    /// Open `path` and seek to the **end**, like `tail -F`. Only new
    /// writes from now on are visible.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let end = file.seek(SeekFrom::End(0))?;
        let inode = inode_of(&file)?;
        Ok(Self {
            path,
            reader: BufReader::with_capacity(64 * 1024, file),
            inode,
            read_pos: end,
        })
    }

    /// Open `path` at the **start**, so the reader sees existing
    /// content before blocking for new writes. Useful when the caller
    /// wants to backfill the index before going live.
    pub fn open_from_start<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let inode = inode_of(&file)?;
        Ok(Self {
            path,
            reader: BufReader::with_capacity(64 * 1024, file),
            inode,
            read_pos: 0,
        })
    }

    /// Check whether the underlying file has been rotated since the
    /// last read. Two distinct modes are handled:
    ///
    /// - **rename/create rotation** (logrotate's default `create`
    ///   mode, also what `mv app.log app.log.1 && touch app.log`
    ///   produces). The inode changes; we re-open the new file and
    ///   read from the start.
    /// - **copytruncate** (logrotate's `copytruncate` mode, common
    ///   for daemons that can't be signalled to reopen their log).
    ///   The file is copied elsewhere, then truncated to 0 bytes in
    ///   place — the inode is unchanged but the file's size drops
    ///   below our `read_pos`. We re-open and seek to 0 to start
    ///   reading the fresh content. Without this branch the reader
    ///   would silently hang on EOF forever while new writes
    ///   accumulate at offsets below the stale cursor.
    ///
    /// ## Known edge case: rotation mid-line
    ///
    /// If a caller is using `BufRead::read_until` and a rotation
    /// happens while a partial line sits in the caller's own scratch
    /// buffer (no `\n` seen yet), the next record will be
    /// `partial_old + first_line_new` glued together. The TailReader
    /// itself drops its internal `BufReader` buffer on swap, but it
    /// can't reach into the caller's scratch.
    ///
    /// In practice this requires the rotation to land within the
    /// ~milliseconds between a writer flushing a partial line and
    /// flushing the rest with `\n` — most rotation tools sync the
    /// writer first, and `read_until` only returns at a line
    /// boundary, so during the 500 ms poll interval the caller is
    /// almost always either fully consumed or fully waiting. Living
    /// with it.
    fn check_rotation(&mut self) -> io::Result<bool> {
        let new_meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => return Ok(false),
        };
        #[cfg(unix)]
        let new_inode = {
            use std::os::unix::fs::MetadataExt;
            new_meta.ino()
        };
        #[cfg(not(unix))]
        let new_inode = new_meta.len();
        let new_size = new_meta.len();

        // rename/create: inode changes → fresh file, start from 0.
        if new_inode != self.inode {
            let new_file = File::open(&self.path)?;
            self.inode = inode_of(&new_file)?;
            self.reader = BufReader::with_capacity(64 * 1024, new_file);
            self.read_pos = 0;
            return Ok(true);
        }

        // copytruncate: same inode but file shrank below our cursor.
        // We have to drop the cached buffered reader (its position is
        // past EOF now) and re-open. seek(0) is implicit on a fresh
        // BufReader<File>.
        if new_size < self.read_pos {
            let new_file = File::open(&self.path)?;
            self.reader = BufReader::with_capacity(64 * 1024, new_file);
            self.read_pos = 0;
            return Ok(true);
        }

        Ok(false)
    }
}

#[cfg(unix)]
fn inode_of(file: &File) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.ino())
}

#[cfg(not(unix))]
fn inode_of(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.len())
}

impl Read for TailReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.reader.read(buf)?;
            if n > 0 {
                // NOTE: we intentionally do NOT update `read_pos` here
                // even though bytes were just consumed. The BufRead
                // path (fill_buf + consume) is the authoritative
                // source of position tracking; mirroring it here would
                // double-count when both paths interleave (`read_until`
                // calls fill_buf+consume, then a stray `Read::read`).
                // All producers in this crate route through BufRead,
                // so this is safe. If you ever drive a TailReader
                // through plain `Read::read` (no BufRead consume),
                // copytruncate detection will lag — replace this
                // type's Read impl with an explicit position tracker.
                return Ok(n);
            }
            if self.check_rotation()? {
                continue;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

impl BufRead for TailReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        loop {
            let len = self.reader.fill_buf()?.len();
            if len > 0 {
                return self.reader.fill_buf();
            }
            if self.check_rotation()? {
                continue;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn consume(&mut self, amt: usize) {
        // BufRead::consume is the actual "we read these bytes" signal
        // for the buffered path — track the offset here.
        self.read_pos = self.read_pos.saturating_add(amt as u64);
        self.reader.consume(amt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mgi-pulse-tail-{}", name));
        p
    }

    #[test]
    fn open_from_start_reads_existing_content() {
        let p = tmp_path("from-start.log");
        std::fs::write(&p, b"line1\nline2\n").unwrap();
        let mut tail = TailReader::open_from_start(&p).unwrap();
        let mut buf = [0u8; 32];
        // Reading directly from the inner BufReader bypasses the
        // blocking impl on Self::read; we just want to confirm the
        // cursor starts at position 0.
        let n = tail.reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"line1\nline2\n");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn open_seeks_to_end() {
        let p = tmp_path("seek-end.log");
        std::fs::write(&p, b"history\n").unwrap();
        let mut tail = TailReader::open(&p).unwrap();
        let mut buf = [0u8; 32];
        let n = tail.reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn copytruncate_is_detected_and_reopens() {
        // Simulate logrotate's copytruncate: same inode, file is
        // truncated in place. The reader must spot this in
        // check_rotation and re-open from offset 0 so subsequent
        // writes are visible.
        let p = tmp_path("copytruncate.log");
        std::fs::write(&p, b"old line 1\nold line 2\n").unwrap();
        let mut tail = TailReader::open_from_start(&p).unwrap();

        // Read everything via the BufRead path (this is the same
        // path StreamProducer uses).
        let mut buf = Vec::new();
        let line_count = consume_all_available(&mut tail, &mut buf);
        assert!(line_count >= 2, "expected at least 2 lines, got {}", line_count);
        // read_pos is now == file size; rotation check should report
        // "no rotation" while everything matches.
        assert!(!tail.check_rotation().unwrap());

        // Simulate copytruncate: open the file with truncate flag,
        // then write a fresh, shorter line. Same inode, smaller size.
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&p)
            .unwrap();
        std::fs::write(&p, b"fresh\n").unwrap();

        // Now check_rotation should fire (file size 6 < read_pos ~21).
        let rotated = tail.check_rotation().unwrap();
        assert!(rotated, "copytruncate should be detected");
        assert_eq!(tail.read_pos, 0, "read_pos must reset after truncate");

        // After the rotation handler swaps in the new BufReader, we
        // should be able to read the fresh content.
        buf.clear();
        consume_all_available(&mut tail, &mut buf);
        assert_eq!(&buf, b"fresh\n");

        let _ = std::fs::remove_file(&p);
    }

    /// Helper: drain whatever the inner BufReader has buffered, plus
    /// what's available on disk right now. Does NOT engage TailReader's
    /// blocking poll loop — we want to test the rotation handler in
    /// isolation. Returns the count of newline-terminated lines seen.
    fn consume_all_available(tail: &mut TailReader, sink: &mut Vec<u8>) -> usize {
        let mut lines = 0;
        // Drive only the inner reader to avoid the blocking poll.
        loop {
            let available = match tail.reader.fill_buf() {
                Ok(b) if !b.is_empty() => b.to_vec(),
                _ => break,
            };
            sink.extend_from_slice(&available);
            // Mirror the same consume() call the BufRead impl would
            // do; this is what advances tail.read_pos.
            let n = available.len();
            tail.read_pos = tail.read_pos.saturating_add(n as u64);
            tail.reader.consume(n);
            lines += available.iter().filter(|&&b| b == b'\n').count();
        }
        lines
    }
}
