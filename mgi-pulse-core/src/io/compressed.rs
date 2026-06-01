//! Compressed file input.
//!
//! Open `app.log.gz` or `app.log.zst` and get a `BufRead` over the
//! decompressed contents. Auto-detection is by magic bytes (the first
//! 2-4 bytes of the file) so misnamed extensions and double extensions
//! (`.gz.gz`) work as long as the *outer* layer is one we understand.
//!
//! Decompression is stream-mode, not in-memory: the file is read and
//! decompressed lazily as the producer pulls bytes. We never hold the
//! full uncompressed payload in RAM — a 2 GB gzip file can expand to
//! 6-8 GB of NDJSON, and that wouldn't fit in the mmap path even if
//! we wanted it to.
//!
//! Compressed inputs always go through the stream path. They lose the
//! mmap-zero-copy property (every record's bytes are owned), but gain
//! SIGBUS safety: the file underneath can't be truncated or replaced
//! and crash us, because we've already read it.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

impl Compression {
    /// Inspect the first bytes of `file` and rewind. Returns
    /// `Compression::None` if the prefix doesn't match a known magic.
    pub fn detect(file: &mut File) -> Result<Compression> {
        let mut head = [0u8; 4];
        let n = file.read(&mut head)?;
        file.seek(SeekFrom::Start(0))?;
        if n >= 2 && head[0] == 0x1f && head[1] == 0x8b {
            return Ok(Compression::Gzip);
        }
        if n >= 4 && head[0] == 0x28 && head[1] == 0xb5 && head[2] == 0x2f && head[3] == 0xfd {
            return Ok(Compression::Zstd);
        }
        Ok(Compression::None)
    }
}

/// Open `path` and return a `BufRead` over its decompressed content
/// (or the raw bytes if not compressed). The returned reader is owned;
/// the caller wraps it in a `StreamProducer`.
pub fn open_decompressed(path: &Path) -> Result<(Compression, Box<dyn BufRead + Send>)> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let compression = Compression::detect(&mut file)?;
    let reader: Box<dyn BufRead + Send> = match compression {
        Compression::None => Box::new(BufReader::with_capacity(64 * 1024, file)),
        Compression::Gzip => Box::new(BufReader::with_capacity(
            64 * 1024,
            flate2::read::GzDecoder::new(file),
        )),
        Compression::Zstd => Box::new(BufReader::with_capacity(
            64 * 1024,
            zstd::stream::read::Decoder::new(file)?,
        )),
    };
    Ok((compression, reader))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mgi-pulse-compressed-{}", name));
        p
    }

    #[test]
    fn detects_plain_when_no_magic() {
        let p = tmp_path("plain.log");
        std::fs::write(&p, b"just text\nmore text\n").unwrap();
        let mut f = File::open(&p).unwrap();
        assert_eq!(Compression::detect(&mut f).unwrap(), Compression::None);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn detects_gzip_by_magic() {
        let p = tmp_path("data.gz");
        let mut buf = Vec::new();
        let mut enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        enc.write_all(b"line one\nline two\n").unwrap();
        drop(enc);
        std::fs::write(&p, &buf).unwrap();
        let mut f = File::open(&p).unwrap();
        assert_eq!(Compression::detect(&mut f).unwrap(), Compression::Gzip);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn detects_zstd_by_magic() {
        let p = tmp_path("data.zst");
        let encoded = zstd::stream::encode_all(&b"alpha\nbeta\n"[..], 3).unwrap();
        std::fs::write(&p, &encoded).unwrap();
        let mut f = File::open(&p).unwrap();
        assert_eq!(Compression::detect(&mut f).unwrap(), Compression::Zstd);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn gzip_round_trip_through_decompressor() {
        let p = tmp_path("round.gz");
        let mut buf = Vec::new();
        let mut enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        enc.write_all(b"alpha\nbeta\ngamma\n").unwrap();
        drop(enc);
        std::fs::write(&p, &buf).unwrap();

        let (compression, mut reader) = open_decompressed(&p).unwrap();
        assert_eq!(compression, Compression::Gzip);
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        assert_eq!(text, "alpha\nbeta\ngamma\n");

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn zstd_round_trip_through_decompressor() {
        let p = tmp_path("round.zst");
        let encoded = zstd::stream::encode_all(&b"x\ny\nz\n"[..], 3).unwrap();
        std::fs::write(&p, &encoded).unwrap();

        let (compression, mut reader) = open_decompressed(&p).unwrap();
        assert_eq!(compression, Compression::Zstd);
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        assert_eq!(text, "x\ny\nz\n");

        let _ = std::fs::remove_file(&p);
    }
}
