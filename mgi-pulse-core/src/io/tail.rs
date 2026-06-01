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
}

impl TailReader {
    /// Open `path` and seek to the **end**, like `tail -F`. Only new
    /// writes from now on are visible.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        file.seek(SeekFrom::End(0))?;
        let inode = inode_of(&file)?;
        Ok(Self {
            path,
            reader: BufReader::with_capacity(64 * 1024, file),
            inode,
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
        })
    }

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
        if new_inode != self.inode {
            let new_file = File::open(&self.path)?;
            self.inode = inode_of(&new_file)?;
            self.reader = BufReader::with_capacity(64 * 1024, new_file);
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
}
