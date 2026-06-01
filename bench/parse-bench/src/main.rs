//! parse-bench — measure the cost of extracting `ts` and `level` from a 2 GB
//! NDJSON file using different JSON-parsing strategies.
//!
//! This is the M1 measurement step from the mgi-pulse plan: serde borrow vs
//! simd-json are the two candidates for the indexer hot path. The bench is
//! deliberately narrow — only `ts` and `level` are extracted, matching what
//! the real indexer needs.
//!
//! Run with:
//!
//! ```
//! cargo run --release -p parse-bench -- <file.ndjson>
//! ```
//!
//! Reports lines/sec, MB/s, and the value distribution as a sanity check that
//! every parser saw the same data.

use std::fs::File;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use memmap2::Mmap;
use serde::Deserialize;

#[derive(Deserialize)]
struct TsLevelBorrow<'a> {
    #[serde(borrow)]
    ts: &'a str,
    #[serde(borrow)]
    level: &'a str,
}

#[derive(Default, Debug)]
struct Stats {
    lines: u64,
    bytes: u64,
    // Length-summed checksums of ts and level — equal across parsers when they
    // see identical fields. Cheap, anti-optimization-elimination, no allocs.
    ts_chk: u64,
    lv_chk: u64,
    // Severity tally — lets us eyeball that the distribution matches.
    trace: u64,
    debug: u64,
    info: u64,
    warn: u64,
    error: u64,
    other: u64,
    parse_errors: u64,
}

impl Stats {
    fn tally(&mut self, ts: &[u8], level: &[u8]) {
        // Avoid str checks on the hot path; the generator writes ASCII.
        self.ts_chk = self.ts_chk.wrapping_add(ts.len() as u64);
        self.lv_chk = self.lv_chk.wrapping_add(level.len() as u64);
        match level {
            b"trace" => self.trace += 1,
            b"debug" => self.debug += 1,
            b"info" => self.info += 1,
            b"warn" => self.warn += 1,
            b"error" => self.error += 1,
            _ => self.other += 1,
        }
    }
}

fn iter_lines(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    // memchr SIMD newline split. ~ГБ/с on this hardware.
    let mut start = 0usize;
    memchr::memchr_iter(b'\n', buf).map(move |nl| {
        let line = &buf[start..nl];
        start = nl + 1;
        line
    })
}

fn run_serde_borrow(buf: &[u8]) -> Stats {
    let mut s = Stats::default();
    for line in iter_lines(buf) {
        if line.is_empty() {
            continue;
        }
        s.lines += 1;
        s.bytes += line.len() as u64 + 1; // +1 for the newline
        match serde_json::from_slice::<TsLevelBorrow>(line) {
            Ok(rec) => s.tally(rec.ts.as_bytes(), rec.level.as_bytes()),
            Err(_) => s.parse_errors += 1,
        }
    }
    s
}

fn run_simd_json(buf: &[u8]) -> Stats {
    // simd-json mutates the input, so we copy each line into a scratch buffer.
    // This is a fair cost — the indexer would have to do the same if it chose
    // simd-json, since the underlying mmap is read-only.
    let mut s = Stats::default();
    let mut scratch: Vec<u8> = Vec::with_capacity(4096);

    for line in iter_lines(buf) {
        if line.is_empty() {
            continue;
        }
        s.lines += 1;
        s.bytes += line.len() as u64 + 1;
        scratch.clear();
        scratch.extend_from_slice(line);
        match simd_json::to_borrowed_value(&mut scratch) {
            Ok(simd_json::BorrowedValue::Object(obj)) => {
                let ts = obj
                    .get("ts")
                    .and_then(|v| match v {
                        simd_json::BorrowedValue::String(s) => Some(s.as_bytes()),
                        _ => None,
                    })
                    .unwrap_or(b"");
                let level = obj
                    .get("level")
                    .and_then(|v| match v {
                        simd_json::BorrowedValue::String(s) => Some(s.as_bytes()),
                        _ => None,
                    })
                    .unwrap_or(b"");
                s.tally(ts, level);
            }
            Ok(_) => s.parse_errors += 1,
            Err(_) => s.parse_errors += 1,
        }
    }
    s
}

/// Pure raw-line scan — no JSON parsing at all. Establishes the floor: how
/// long does it take to just walk the file line-by-line? Any real parser
/// strategy must be measured against this.
fn run_raw_scan(buf: &[u8]) -> Stats {
    let mut s = Stats::default();
    for line in iter_lines(buf) {
        if line.is_empty() {
            continue;
        }
        s.lines += 1;
        s.bytes += line.len() as u64 + 1;
        // Touch the bytes so the compiler does not optimize the loop away.
        s.ts_chk = s.ts_chk.wrapping_add(line.len() as u64);
    }
    s
}

fn report(name: &str, s: &Stats, elapsed_s: f64) {
    let mbps = (s.bytes as f64) / 1_048_576.0 / elapsed_s;
    let lps = (s.lines as f64) / elapsed_s;
    println!(
        "{name:>18}: {lines:>10} lines  {bytes:>6} MB  in {elapsed:>6.2}s  \
         => {mbps:>7.1} MB/s  {lps:>10.0} lines/s  errors={errs}  \
         chk(ts/lv)={ts}/{lv}  tally trace={tr} debug={db} info={inf} warn={wn} error={er} other={ot}",
        name = name,
        lines = s.lines,
        bytes = s.bytes / 1_048_576,
        elapsed = elapsed_s,
        mbps = mbps,
        lps = lps,
        errs = s.parse_errors,
        ts = s.ts_chk,
        lv = s.lv_chk,
        tr = s.trace,
        db = s.debug,
        inf = s.info,
        wn = s.warn,
        er = s.error,
        ot = s.other,
    );
}

fn main() -> Result<()> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .context("usage: parse-bench <file.ndjson>")?
        .into();

    let file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let mmap = unsafe { Mmap::map(&file)? };
    let buf: &[u8] = &mmap[..];
    println!("file = {} ({} MB)", path.display(), buf.len() / 1_048_576);

    // Run each strategy twice. The first run warms the page cache (the kernel
    // pulls the file into RAM); the second is what we report. With 2 GB and
    // 48 GB RAM the file fits comfortably.
    let strategies: &[(&str, fn(&[u8]) -> Stats)] = &[
        ("raw-scan", run_raw_scan),
        ("serde-borrow", run_serde_borrow),
        ("simd-json", run_simd_json),
    ];

    for (name, f) in strategies {
        let _warm = f(buf);
        let t0 = Instant::now();
        let s = f(buf);
        let dt = t0.elapsed().as_secs_f64();
        report(name, &s, dt);
    }

    Ok(())
}
