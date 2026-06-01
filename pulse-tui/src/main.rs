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
use mgi_pulse_core::engine::{indexer, Engine};
use mgi_pulse_core::io::file::FileProducer;
use mgi_pulse_core::io::stream::StreamProducer;

/// mgi-pulse — not browse logs, navigate them.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// NDJSON file to open. Use `-` for stdin.
    /// If neither is given and stdin is a TTY, usage is printed and the
    /// process exits — never block silently waiting on a terminal.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Enable mouse capture (opt-in, breaks terminal text selection).
    #[arg(long, default_value_t = false)]
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

    let mut engine = Engine::new();
    let source_label: String;

    if cli.files.is_empty() {
        source_label = "<stdin>".to_string();
        ingest_stdin(&mut engine)?;
    } else {
        // M1 is single-source. k-way merge of multiple files is M1.5.
        if cli.files.len() > 1 {
            anyhow::bail!(
                "multi-source merge is not in v0.1 M1; pass one file or pipe stdin"
            );
        }
        let path = &cli.files[0];
        if path.as_os_str() == "-" {
            source_label = "<stdin>".to_string();
            ingest_stdin(&mut engine)?;
        } else {
            source_label = path.display().to_string();
            ingest_file(path, &mut engine)?;
        }
    }

    if cli.dry_run {
        let idx = &engine.indexes;
        println!(
            "indexed {} records — untimed: {} (ts missing/bad: {}/{}), json errors: {}",
            idx.len(),
            idx.untimed_on_file,
            idx.untimed_on_file - idx.ts_parse_errors,
            idx.ts_parse_errors,
            idx.json_parse_errors
        );
        return Ok(());
    }

    let app = app::App::new(engine, source_label);
    app::run(app)
}

fn ingest_file(path: &PathBuf, engine: &mut Engine) -> Result<()> {
    let t0 = Instant::now();
    let mut producer = FileProducer::open(path, 0)
        .with_context(|| format!("open {}", path.display()))?;
    engine.mmaps.push(producer.mmap());
    let total_bytes = producer.total_bytes();
    indexer::drain(&mut producer, engine);
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

fn ingest_stdin(engine: &mut Engine) -> Result<()> {
    let t0 = Instant::now();
    let stdin = io::stdin();
    let reader = BufReader::new(stdin.lock());
    let mut producer = StreamProducer::new(reader, 0);
    // The stream path doesn't use mmaps; engine.mmaps stays empty. The
    // renderer routes stream rows through `owned_lines` and never touches
    // mmaps[source_id], so a missing entry is fine here.
    indexer::drain(&mut producer, engine);
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
    let filter = EnvFilter::try_from_env("MGI_PULSE_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}
