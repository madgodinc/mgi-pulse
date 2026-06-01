//! mgi-pulse — TUI log navigator.
//!
//! v0.1 skeleton. Layers:
//! - `mgi_pulse_core` — IO, engine, schema.
//! - this crate — CLI parsing, viewmodel, ratatui rendering.
//!
//! The core has no UI dependencies and could not pull ratatui even if
//! someone tried — the workspace split enforces it at the compiler level.

mod app;
mod panes;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

/// mgi-pulse — not browse logs, navigate them.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// NDJSON files to open. Use `-` for stdin.
    /// If neither is given and stdin is a TTY, usage is printed and the
    /// process exits — never block silently waiting on a terminal.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Enable mouse capture (opt-in, breaks terminal text selection).
    #[arg(long, default_value_t = false)]
    mouse: bool,
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

    // IsTerminal guard: never block on stdin when it's a TTY and no files
    // were provided. Classic first-run footgun otherwise.
    if cli.files.is_empty() && std::io::stdin().is_terminal() {
        eprintln!(
            "mgi-pulse: no input.\n\
             usage:\n  \
               mgi-pulse <file.ndjson>...\n  \
               tail -F live.log | mgi-pulse -\n\
             see --help for more."
        );
        return Ok(());
    }

    // M1: open sources, spawn indexer, hand to App::run.
    // Skeleton just reports what would happen.
    eprintln!(
        "mgi-pulse 0.1.0-dev: M0 skeleton. \
         Files: {:?}, stdin: {}, mouse: {}.",
        cli.files,
        !std::io::stdin().is_terminal(),
        cli.mouse,
    );
    eprintln!("M1 not yet implemented. See project plan.");

    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("MGI_PULSE_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}
