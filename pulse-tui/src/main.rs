//! mgi-pulse — TUI log navigator.
//!
//! v0.1 M1 — paritet with Toolong on a single source:
//! - open one NDJSON file (or stdin) by mmap (or BufRead);
//! - index ts + level once;
//! - render a table sorted by line_id;
//! - `/regex` filter, `Esc` clears, arrow / page / g / G navigation.

mod app;
mod panes;

use std::io::{self, BufReader, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use mgi_pulse_core::engine::parse::FieldNames;
use mgi_pulse_core::engine::{indexer, Engine};
use mgi_pulse_core::io::file::FileProducer;
use mgi_pulse_core::io::merge::MergeProducer;
use mgi_pulse_core::io::stream::StreamProducer;
use mgi_pulse_core::io::RecordProducer;

/// mgi-pulse — not browse logs, navigate them.
///
/// Opens NDJSON files via mmap for speed. Safe for static log snapshots.
/// For active logs that another process may rotate or truncate, pipe
/// instead: `tail -F file | mgi-pulse -`. Reading an mmap'd file that
/// gets truncated under us delivers SIGBUS (kills the process), which
/// the stream path avoids by using owned buffers.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// NDJSON file to open. Use `-` for stdin.
    /// If neither is given and stdin is a TTY, usage is printed and the
    /// process exits — never block silently waiting on a terminal.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Disable mouse capture. By default mouse capture is on so wheel
    /// scrolls the table and clicks switch tabs; use Shift+drag for
    /// terminal selection while capture is active. Pass `--no-mouse` if
    /// you need the unmodified terminal selection back (useful over SSH
    /// or when copying via mouse without modifier).
    #[arg(long, default_value_t = false)]
    no_mouse: bool,

    /// JSON field name to read as the record timestamp. Default `ts`. Use
    /// `--time-field=@timestamp` for ECS-shaped logs, `--time-field=@t` for
    /// Serilog, `--time-field=eventTime` for k8s audit, etc.
    #[arg(long, value_name = "FIELD")]
    time_field: Option<String>,

    /// JSON field name to read as the severity level. Default `level`. Use
    /// `--level-field=severity_text` for OTel, `--level-field=severity` for
    /// GCP, etc.
    #[arg(long, value_name = "FIELD")]
    level_field: Option<String>,

    /// Maximum number of auto-derived columns to show. Default unbounded
    /// (capped by terminal width). Useful when you have a wide schema and
    /// want to focus on the most-present fields.
    #[arg(long, value_name = "N")]
    columns: Option<usize>,

    /// Deprecated legacy flag. Mouse is on by default now; pass
    /// `--no-mouse` to disable.
    #[arg(long, default_value_t = false, hide = true)]
    mouse: bool,

    /// Index the input, print a summary, and exit without entering the TUI.
    /// Smoke-test escape hatch; intentionally undocumented in the public help.
    #[arg(long, hide = true, default_value_t = false)]
    dry_run: bool,
}

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("mgi-pulse: {e:#}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    if cli.files.is_empty() && std::io::stdin().is_terminal() {
        eprintln!(
            "mgi-pulse: no input.\n\
             usage:\n  \
               mgi-pulse <file.ndjson>\n  \
               tail -F live.log | mgi-pulse -\n\
             see --help for more."
        );
        return Ok(());
    }

    // Build the override field-names from CLI flags. Defaults to None so
    // the fast hardcoded path is used unless a flag was passed.
    let fields = match (cli.time_field.clone(), cli.level_field.clone()) {
        (None, None) => None,
        _ => {
            let mut f = FieldNames::default();
            if let Some(t) = cli.time_field.clone() {
                f.ts = t;
            }
            if let Some(l) = cli.level_field.clone() {
                f.level = l;
            }
            Some(f)
        }
    };

    let mut engine = Engine::new();
    let source_label: String;

    if cli.files.is_empty() {
        source_label = "<stdin>".to_string();
        ingest_stdin(&mut engine, fields.clone())?;
    } else {
        // Single file: fast path that keeps line_id == arrival order.
        // Multiple files: k-way merge by ts_micros — line_id becomes
        // time-sorted (see engine::record bifurcation note).
        let stdin_count = cli.files.iter().filter(|p| p.as_os_str() == "-").count();
        if stdin_count > 0 && cli.files.len() > 1 {
            anyhow::bail!(
                "mixing stdin and files in one run is not supported in v0.1; \
                 pick either files OR stdin"
            );
        }
        if cli.files.len() == 1 {
            let path = &cli.files[0];
            if path.as_os_str() == "-" {
                source_label = "<stdin>".to_string();
                ingest_stdin(&mut engine, fields.clone())?;
            } else {
                source_label = path.display().to_string();
                ingest_file(path, &mut engine, fields.clone())?;
            }
        } else {
            // Multi-file merge.
            source_label = cli
                .files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            ingest_merged(&cli.files, &mut engine, fields.clone())?;
        }
    }

    // Schema warmup: scan the first 10k records to derive auto-columns. This
    // is opportunistic — non-JSON or schema-poor inputs simply produce fewer
    // columns and we fall back to the raw payload.
    engine.scan_schema();

    if cli.dry_run {
        let idx = &engine.indexes;
        let ps = idx.parse_stats;
        println!(
            "indexed {} records — untimed: {} (ts missing/bad: {}/{}), json errors: {}",
            idx.len(),
            ps.untimed,
            ps.untimed - ps.ts_parse_errors,
            ps.ts_parse_errors,
            ps.json_parse_errors
        );
        let cols: Vec<String> = engine
            .schema
            .as_ref()
            .map(|s| s.auto_columns(8).iter().map(|c| c.to_string()).collect())
            .unwrap_or_default();
        println!(
            "schema: {} fields scanned ({} records in warmup), auto-columns: {:?}",
            engine
                .schema
                .as_ref()
                .map(|s| s.ordered_fields.len())
                .unwrap_or(0),
            engine
                .schema
                .as_ref()
                .map(|s| s.records_scanned)
                .unwrap_or(0),
            cols
        );
        return Ok(());
    }

    let app = app::App::new(engine, source_label, cli.columns);
    app::run(app, !cli.no_mouse)
}

fn ingest_file(path: &PathBuf, engine: &mut Engine, fields: Option<FieldNames>) -> Result<()> {
    let t0 = Instant::now();
    let mut producer =
        FileProducer::open(path, 0).with_context(|| format!("open {}", path.display()))?;
    if let Some(f) = fields {
        producer = producer.with_fields(f);
    }
    engine.mmaps.push(producer.mmap());
    engine
        .source_formats
        .push(mgi_pulse_core::engine::format::LogFormat::Ndjson);
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

fn ingest_merged(paths: &[PathBuf], engine: &mut Engine, fields: Option<FieldNames>) -> Result<()> {
    let t0 = Instant::now();
    let mut producers: Vec<Box<dyn RecordProducer>> = Vec::with_capacity(paths.len());
    let mut total_bytes: u64 = 0;
    for (i, path) in paths.iter().enumerate() {
        let mut producer = FileProducer::open(path, i as u32)
            .with_context(|| format!("open {}", path.display()))?;
        if let Some(f) = fields.clone() {
            producer = producer.with_fields(f);
        }
        engine.mmaps.push(producer.mmap());
        engine
            .source_formats
            .push(mgi_pulse_core::engine::format::LogFormat::Ndjson);
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

fn ingest_stdin(engine: &mut Engine, fields: Option<FieldNames>) -> Result<()> {
    let t0 = Instant::now();
    let stdin = io::stdin();
    let reader = BufReader::new(stdin.lock());
    let mut producer = StreamProducer::new(reader, 0);
    if let Some(f) = fields {
        producer = producer.with_fields(f);
    }
    // Stream source slot. Format is hardcoded NDJSON in v0.1; the engine
    // looks this up to drive the per-source FieldCache.
    engine
        .source_formats
        .push(mgi_pulse_core::engine::format::LogFormat::Ndjson);
    // The stream path doesn't use mmaps; engine.mmaps stays empty. The
    // renderer routes stream rows through `owned_lines` and never touches
    // mmaps[source_id], so a missing entry is fine here.
    indexer::drain(&mut producer, engine);
    engine.indexes.parse_stats.fold(producer.stats());
    let dt = t0.elapsed();
    tracing::info!(
        records = engine.indexes.len(),
        elapsed_ms = dt.as_millis() as u64,
        "indexed stream"
    );
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter =
        EnvFilter::try_from_env("MGI_PULSE_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
