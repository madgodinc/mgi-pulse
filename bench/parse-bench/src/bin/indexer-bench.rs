//! End-to-end indexer benchmark.
//!
//! Drives the real `mgi_pulse_core::engine::indexer` path against an
//! NDJSON file of arbitrary size and reports wall-clock time, total
//! records indexed, and throughput in MB/s and records/s.
//!
//! Run with:
//!
//! ```
//! cargo run --release -p parse-bench --bin indexer-bench -- <file.ndjson>
//! ```
//!
//! Typical use: the 2 GB / 11 M-record synthetic from
//! `bench/gen-ndjson.sh` is the regression target. As of v0.3,
//! end-to-end index lands at ~2.8-2.9 s on an i5-12400F.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use mgi_pulse_core::engine::format::LogFormat;
use mgi_pulse_core::engine::{indexer, Engine};
use mgi_pulse_core::io::file::FileProducer;

fn main() -> Result<()> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!(
            "usage: indexer-bench <file.ndjson>"
        ))?
        .into();

    let file_size = std::fs::metadata(&path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    println!(
        "input: {} ({:.2} MB)",
        path.display(),
        file_size as f64 / 1_048_576.0
    );

    let mut engine = Engine::new();
    let mut producer = FileProducer::open(&path, 0)
        .with_context(|| format!("open {}", path.display()))?;
    producer = producer.with_format(LogFormat::Ndjson);
    engine.mmaps.push(producer.mmap());
    engine.source_formats.push(LogFormat::Ndjson);

    let t0 = Instant::now();
    indexer::drain(&mut producer, &mut engine);
    let dt = t0.elapsed();

    engine.scan_schema();
    let dt_total = t0.elapsed();

    let records = engine.indexes.len();
    let mb = file_size as f64 / 1_048_576.0;
    let secs = dt.as_secs_f64();
    let secs_total = dt_total.as_secs_f64();

    println!(
        "indexed: {} records in {:.3} s",
        records, secs
    );
    println!(
        "throughput: {:.1} MB/s, {:.1} k records/s",
        mb / secs,
        records as f64 / secs / 1000.0
    );
    println!(
        "with schema scan: {:.3} s total",
        secs_total
    );

    Ok(())
}
