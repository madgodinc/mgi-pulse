//! Loading sources into an `Engine`: files (mmap, compressed, JSON
//! arrays), merged files, and byte streams.

use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use mgi_pulse_core::engine::format::LogFormat;
use mgi_pulse_core::engine::parse::FieldNames;
use mgi_pulse_core::engine::{indexer, Engine};
use mgi_pulse_core::io::compressed::{open_decompressed, Compression};
use mgi_pulse_core::io::file::FileProducer;
use mgi_pulse_core::io::merge::MergeProducer;
use mgi_pulse_core::io::multiline::MultiLineProducer;
use mgi_pulse_core::io::stream::StreamProducer;
use mgi_pulse_core::io::RecordProducer;

/// Map a `--format` name to a format.
pub fn parse_format(name: &str) -> Result<LogFormat> {
    Ok(match name {
        "ndjson" | "json" => LogFormat::Ndjson,
        "logfmt" => LogFormat::Logfmt,
        "edn" => LogFormat::Edn,
        "python" => LogFormat::Python,
        "syslog" => LogFormat::Syslog,
        "csv" => LogFormat::Csv,
        "tsv" => LogFormat::Tsv,
        "access" => LogFormat::Access,
        "logback" | "log4j" | "java" => LogFormat::Logback,
        "journalctl" | "journal" | "systemd" => LogFormat::Journalctl,
        "regex" => LogFormat::Regex,
        other => anyhow::bail!(
            "unknown --format value '{other}'; valid: ndjson, logfmt, edn, python, syslog, csv, tsv, access, logback, journalctl, regex"
        ),
    })
}

/// Read a small probe from the first file and let `LogFormat::detect`
/// vote on its shape. Returns `None` when the input is stdin-only, the
/// file is empty, or none of the format signatures match.
///
/// Probe size: up to 16 KiB or 64 newline-terminated lines, whichever
/// ends first. Anything bigger is unnecessary — the detect heuristic
/// hits a stable verdict well before that, and we don't want a huge
/// log to slow down `mgi-pulse` startup just to make a format guess.
pub fn detect_format_from_files(files: &[PathBuf]) -> Option<LogFormat> {
    let path = files.iter().find(|p| p.as_os_str() != "-")?;
    let bytes = read_probe(path, 16 * 1024).ok()?;
    detect_format_from_bytes(&bytes)
}

/// Format vote over a probe of raw bytes. A trailing partial line is
/// still offered to the detector; it only ever adds one vote.
pub fn detect_format_from_bytes(bytes: &[u8]) -> Option<LogFormat> {
    if bytes.is_empty() {
        return None;
    }
    // JSON-array files have one logical record per element but are
    // packed on one or two lines; the standard newline-splitter would
    // give the heuristic a single giant "line" loaded with commas and
    // mis-vote CSV. The ingest path flattens them to real NDJSON
    // before indexing, so report Ndjson here.
    if mgi_pulse_core::io::json_array::looks_like_json_array(bytes) {
        return Some(LogFormat::Ndjson);
    }
    // Take up to 64 newline-terminated lines from the probe. Strip
    // trailing CR for CRLF inputs.
    let lines: Vec<&[u8]> = bytes
        .split(|&b| b == b'\n')
        .take(64)
        .map(|l| match l.last() {
            Some(&b'\r') => &l[..l.len() - 1],
            _ => l,
        })
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(LogFormat::detect(&lines))
}

pub fn read_probe(path: &PathBuf, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max_bytes];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

pub fn ingest_file(
    path: &PathBuf,
    engine: &mut Engine,
    fields: Option<FieldNames>,
    fmt: LogFormat,
) -> Result<()> {
    let t0 = Instant::now();
    // Magic-byte sniff first: gzip and zstd take the stream path because
    // the decompressor doesn't give us mmap, and we'd rather not buffer
    // 6-8 GB of decompressed NDJSON in RAM up-front.
    let mut probe =
        std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let compression = Compression::detect(&mut probe)?;
    drop(probe);

    if compression != Compression::None {
        let (_, reader) = open_decompressed(path)?;
        let mut producer = StreamProducer::new(reader, 0);
        if let Some(f) = fields {
            producer = producer.with_fields(f);
        }
        producer = producer.with_format(fmt);
        engine.source_formats.push(fmt);
        // Multi-line wrapper folds `^\s+` continuation lines into the
        // preceding record for formats that support it (logfmt, EDN).
        let mut multiline = MultiLineProducer::new(producer, fmt);
        indexer::drain(&mut multiline, engine);
        let dt = t0.elapsed();
        tracing::info!(
            path = %path.display(),
            compression = ?compression,
            records = engine.indexes.len(),
            elapsed_ms = dt.as_millis() as u64,
            "indexed compressed file"
        );
        return Ok(());
    }

    // JSON-array adapter: if the file's first non-whitespace bytes are
    // `[{`, load it into memory, flatten to NDJSON, and stream through
    // the existing StreamProducer path. Caps at ~256 MB to avoid OOM
    // on huge exports — bigger files should pipe through `jq -c '.[]'`.
    {
        let mut head = [0u8; 16];
        let mut f =
            std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        use std::io::Read;
        let n = f.read(&mut head).unwrap_or(0);
        drop(f);
        if mgi_pulse_core::io::json_array::looks_like_json_array(&head[..n]) {
            let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            // 64 MB cap. The naive serde_json::Value flatten path
            // explodes to 3-5× the raw size in resident memory (plus
            // the re-serialised NDJSON buffer); 64 MB raw lands at
            // roughly 250-400 MB RSS which is the most we want to
            // pay for an "I'm opening a JSON dump" convenience.
            // Bigger files: `jq -c '.[]' file.json | mgi-pulse -`.
            if raw.len() > 64 * 1024 * 1024 {
                anyhow::bail!(
                    "JSON-array file is over 64 MB; the in-memory flatten \
                     path would balloon to several GB of RSS. Pipe through \
                     `jq -c '.[]' <file> | mgi-pulse -` instead."
                );
            }
            let ndjson = mgi_pulse_core::io::json_array::flatten_to_ndjson(&raw)
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            let reader = std::io::Cursor::new(ndjson);
            let mut producer = StreamProducer::new(reader, 0);
            if let Some(f) = fields {
                producer = producer.with_fields(f);
            }
            producer = producer.with_format(fmt);
            engine.source_formats.push(fmt);
            let mut multiline = MultiLineProducer::new(producer, fmt);
            indexer::drain(&mut multiline, engine);
            let dt = t0.elapsed();
            tracing::info!(
                path = %path.display(),
                records = engine.indexes.len(),
                elapsed_ms = dt.as_millis() as u64,
                "indexed JSON array"
            );
            return Ok(());
        }
    }

    let mut producer =
        FileProducer::open(path, 0).with_context(|| format!("open {}", path.display()))?;
    if let Some(f) = fields {
        producer = producer.with_fields(f);
    }
    producer = producer.with_format(fmt);
    engine.mmaps.push(producer.mmap());
    engine.source_formats.push(fmt);
    let total_bytes = producer.total_bytes();
    indexer::drain(&mut producer, engine);
    engine.indexes.parse_stats.fold(producer.stats());
    let dt = t0.elapsed();
    tracing::info!(
        path = %path.display(),
        bytes = total_bytes,
        records = engine.indexes.len(),
        elapsed_ms = dt.as_millis() as u64,
        "indexed file"
    );
    Ok(())
}

pub fn ingest_merged(
    paths: &[PathBuf],
    engine: &mut Engine,
    fields: Option<FieldNames>,
    fmt: LogFormat,
) -> Result<()> {
    let t0 = Instant::now();
    let mut producers: Vec<Box<dyn RecordProducer>> = Vec::with_capacity(paths.len());
    let mut total_bytes: u64 = 0;
    for (i, path) in paths.iter().enumerate() {
        let mut producer = FileProducer::open(path, i as u32)
            .with_context(|| format!("open {}", path.display()))?;
        if let Some(f) = fields.clone() {
            producer = producer.with_fields(f);
        }
        producer = producer.with_format(fmt);
        engine.mmaps.push(producer.mmap());
        engine.source_formats.push(fmt);
        total_bytes += producer.total_bytes();
        producers.push(Box::new(producer));
    }
    let mut merge = MergeProducer::new(producers);
    indexer::drain(&mut merge, engine);
    // Per-producer stats are inside the boxed producers and have been
    // consumed by the merge. The merge does not yet expose folded stats —
    // they're not surfaced separately for now. Backlog: fold-on-drop.
    let dt = t0.elapsed();
    tracing::info!(
        sources = paths.len(),
        bytes = total_bytes,
        records = engine.indexes.len(),
        elapsed_ms = dt.as_millis() as u64,
        "indexed merged sources"
    );
    Ok(())
}

/// Index stdin. With no forced format, the first buffered chunk is
/// sniffed without consuming it, so `journalctl -o json | mgi-pulse -`
/// gets the journal parser rather than plain NDJSON.
pub fn ingest_stdin(
    engine: &mut Engine,
    fields: Option<FieldNames>,
    forced: Option<LogFormat>,
) -> Result<LogFormat> {
    let stdin = io::stdin();
    ingest_reader(
        BufReader::with_capacity(64 * 1024, stdin.lock()),
        engine,
        fields,
        forced,
    )
}

/// Peek at the reader's first buffer and vote on a format.
pub fn sniff_reader<R: BufRead>(reader: &mut R) -> LogFormat {
    match reader.fill_buf() {
        Ok(buf) => detect_format_from_bytes(buf).unwrap_or(LogFormat::Ndjson),
        Err(_) => LogFormat::Ndjson,
    }
}

pub fn ingest_reader<R: BufRead>(
    mut reader: R,
    engine: &mut Engine,
    fields: Option<FieldNames>,
    forced: Option<LogFormat>,
) -> Result<LogFormat> {
    let t0 = Instant::now();
    let fmt = match forced {
        Some(f) => f,
        None => sniff_reader(&mut reader),
    };
    let source_id = engine.source_formats.len() as u32;
    let mut producer = StreamProducer::new(reader, source_id);
    if let Some(f) = fields {
        producer = producer.with_fields(f);
    }
    producer = producer.with_format(fmt);
    engine.source_formats.push(fmt);
    // The stream path doesn't use mmaps; engine.mmaps stays empty. The
    // renderer routes stream rows through `owned_lines` and never touches
    // mmaps[source_id], so a missing entry is fine here.
    let mut multiline = MultiLineProducer::new(producer, fmt);
    indexer::drain(&mut multiline, engine);
    let dt = t0.elapsed();
    tracing::info!(
        records = engine.indexes.len(),
        elapsed_ms = dt.as_millis() as u64,
        "indexed stream"
    );
    Ok(fmt)
}
