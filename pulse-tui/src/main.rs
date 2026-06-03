//! mgi-pulse — TUI log navigator.
//!
//! v0.1 M1 — paritet with Toolong on a single source:
//! - open one NDJSON file (or stdin) by mmap (or BufRead);
//! - index ts + level once;
//! - render a table sorted by line_id;
//! - `/regex` filter, `Esc` clears, arrow / page / g / G navigation.

mod app;
mod bookmarks_store;
mod panes;
mod theme;

use std::io::{self, BufReader, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use mgi_pulse_core::engine::format::LogFormat;
use mgi_pulse_core::engine::parse::FieldNames;
use mgi_pulse_core::engine::{indexer, Engine};
use mgi_pulse_core::io::compressed::{open_decompressed, Compression};
use mgi_pulse_core::io::file::FileProducer;
use mgi_pulse_core::io::merge::MergeProducer;
use mgi_pulse_core::io::multiline::MultiLineProducer;
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

    /// Force a specific log format instead of auto-detect. Valid values:
    /// `ndjson`, `logfmt`, `edn`. When omitted, the first ~200 lines of
    /// the input are sampled to guess.
    #[arg(long, value_name = "FORMAT")]
    format: Option<String>,

    /// Colour theme: `dark` (default), `light`, or `nocolor`. The
    /// MGI_PULSE_THEME env var sets the same thing without a flag.
    #[arg(long, value_name = "THEME")]
    theme: Option<String>,

    /// Deprecated legacy flag. Mouse is on by default now; pass
    /// `--no-mouse` to disable.
    #[arg(long, default_value_t = false, hide = true)]
    mouse: bool,

    /// Index the input, print a summary, and exit without entering the TUI.
    /// Smoke-test escape hatch; intentionally undocumented in the public help.
    #[arg(long, hide = true, default_value_t = false)]
    dry_run: bool,

    /// Follow the file in real time, like `tail -F`. Reads existing
    /// content first, then blocks for new writes and survives rotation
    /// (inode-based detection, 500ms polling). Only meaningful for a
    /// single file argument; ignored when combined with multiple files
    /// or stdin.
    #[arg(short = 'f', long, default_value_t = false)]
    follow: bool,
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

    // Validate --theme up front so a typo fails before we do any work.
    let theme = match cli.theme.as_deref() {
        Some(s) => match theme::Theme::parse(s) {
            Some(t) => t,
            None => anyhow::bail!("unknown --theme value '{}'; valid: dark, light, nocolor", s),
        },
        None => theme::Theme::from_env_or_default(),
    };
    // NO_COLOR / TERM=dumb / non-tty stdout override even an explicit
    // --theme=dark. Sampled before we touch the terminal because once
    // we're in alt-screen the tty check is meaningless.
    let theme = theme::Theme::env_override(std::io::stdout().is_terminal()).unwrap_or(theme);

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

    // Resolve --format flag → LogFormat. Auto-detect (None) is the v0.2
    // story; for v0.1.x the flag is the only way to pick logfmt.
    let forced_format = match cli.format.as_deref() {
        None => None,
        Some("ndjson") => Some(LogFormat::Ndjson),
        Some("logfmt") => Some(LogFormat::Logfmt),
        Some("edn") => Some(LogFormat::Edn),
        Some("python") => Some(LogFormat::Python),
        Some("syslog") => Some(LogFormat::Syslog),
        Some("csv") => Some(LogFormat::Csv),
        Some("tsv") => Some(LogFormat::Tsv),
        Some("access") => Some(LogFormat::Access),
        Some(other) => anyhow::bail!(
            "unknown --format value '{}'; valid: ndjson, logfmt, edn, python, syslog, csv, tsv, access",
            other
        ),
    };

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
    // Path to the single underlying file, when there is one. Used only
    // for bookmark persistence; stdin / merged sources stay `None`.
    let mut single_source_path: Option<PathBuf> = None;
    // When `--follow` is in play, hold onto the parameters so the
    // worker thread can be spun up after the synchronous backfill but
    // before the UI loop starts. `None` for static runs.
    let mut pending_follow: Option<FollowPlan> = None;

    // Pick the source format. Explicit --format wins; otherwise sniff
    // a small probe from the first file (no probe for stdin — we'd
    // have to buffer and replay it). Probe size is bounded so a huge
    // file doesn't load megabytes just to vote on its shape.
    let fmt = match forced_format {
        Some(f) => f,
        None => detect_format_from_files(&cli.files).unwrap_or(LogFormat::Ndjson),
    };

    if cli.files.is_empty() {
        source_label = "<stdin>".to_string();
        ingest_stdin(&mut engine, fields.clone(), fmt)?;
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
                ingest_stdin(&mut engine, fields.clone(), fmt)?;
            } else if cli.follow {
                // Two-phase follow:
                //   1. Synchronous backfill from disk so the UI opens
                //      with every record that was already in the file.
                //   2. A worker thread takes over with a `TailReader`
                //      seeked to EOF and streams new lines through a
                //      crossbeam channel into the engine on every UI
                //      tick. The two phases are guaranteed not to
                //      overlap because the worker only starts after
                //      the sync drain finishes.
                source_label = format!("{} (follow)", path.display());
                single_source_path = Some(path.clone());
                ingest_file(path, &mut engine, fields.clone(), fmt)?;
                pending_follow = Some(FollowPlan {
                    path: path.clone(),
                    fields: fields.clone(),
                    fmt,
                });
            } else {
                source_label = path.display().to_string();
                single_source_path = Some(path.clone());
                ingest_file(path, &mut engine, fields.clone(), fmt)?;
            }
        } else {
            // Multi-file merge.
            source_label = cli
                .files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            ingest_merged(&cli.files, &mut engine, fields.clone(), fmt)?;
        }
    }

    // CSV/TSV: capture per-source headers and re-derive ts/level for
    // every record. The indexer ran before the header was known, so
    // this is the moment to fix that up. Stateless formats no-op.
    engine.capture_csv_headers();

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

    let app = app::App::new_with_source(
        engine,
        source_label,
        cli.columns,
        theme,
        single_source_path,
    );
    // Wire the follow worker if requested. The channel buffer (8192)
    // absorbs short read bursts so the worker doesn't block on a slow
    // UI tick. Worker drops out on producer EOF (e.g. file truncated
    // and rotation gave us an empty new file) — UI keeps showing the
    // index built so far.
    let app = if let Some(plan) = pending_follow {
        let (tx, rx) = crossbeam_channel::bounded(8192);
        spawn_follow_worker(plan, tx);
        app.with_live(rx)
    } else {
        app
    };
    app::run(app, !cli.no_mouse)
}

/// Parameters captured at parse time so a worker thread can re-open
/// the source independently of the synchronous backfill.
struct FollowPlan {
    path: PathBuf,
    fields: Option<FieldNames>,
    fmt: LogFormat,
}

/// Spawn the worker thread that owns the `TailReader` and streams
/// records through the channel. We give it a name so panics show up
/// clearly in tracing; we don't join — the channel disconnect is the
/// only shutdown signal the UI needs.
fn spawn_follow_worker(
    plan: FollowPlan,
    tx: crossbeam_channel::Sender<mgi_pulse_core::engine::record::RawRecord>,
) {
    use mgi_pulse_core::io::tail::TailReader;
    use mgi_pulse_core::io::RecordProducer;
    use mgi_pulse_core::io::stream::StreamProducer;

    std::thread::Builder::new()
        .name("mgi-pulse-follow".into())
        .spawn(move || {
            let tail = match TailReader::open(&plan.path) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(error = %e, path = %plan.path.display(),
                        "follow worker: open failed");
                    return;
                }
            };
            // Source-id 0 matches the synchronous backfill — files
            // index as source_id=0 in single-source runs.
            let mut producer = StreamProducer::new(tail, 0);
            if let Some(f) = plan.fields {
                producer = producer.with_fields(f);
            }
            producer = producer.with_format(plan.fmt);
            // Drain until the channel disconnects (UI quit) or the
            // producer dies. TailReader::read blocks on EOF, so each
            // iteration is "wait for the next line, ship it".
            loop {
                match producer.next() {
                    Some(rec) => {
                        if tx.send(rec).is_err() {
                            // UI dropped the receiver — clean exit.
                            return;
                        }
                    }
                    None => {
                        // Producer closed (e.g. file got removed and
                        // not recreated). Nothing more we can do.
                        return;
                    }
                }
            }
        })
        .ok();
}

/// Read a small probe from the first file and let `LogFormat::detect`
/// vote on its shape. Returns `None` when the input is stdin-only, the
/// file is empty, or none of the format signatures match.
///
/// Probe size: up to 16 KiB or 64 newline-terminated lines, whichever
/// ends first. Anything bigger is unnecessary — the detect heuristic
/// hits a stable verdict well before that, and we don't want a huge
/// log to slow down `mgi-pulse` startup just to make a format guess.
fn detect_format_from_files(files: &[PathBuf]) -> Option<LogFormat> {
    let path = files.iter().find(|p| p.as_os_str() != "-")?;
    let bytes = read_probe(path, 16 * 1024).ok()?;
    if bytes.is_empty() {
        return None;
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

fn read_probe(path: &PathBuf, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max_bytes];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

fn ingest_file(
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

fn ingest_merged(
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

fn ingest_stdin(engine: &mut Engine, fields: Option<FieldNames>, fmt: LogFormat) -> Result<()> {
    let t0 = Instant::now();
    let stdin = io::stdin();
    let reader = BufReader::new(stdin.lock());
    let mut producer = StreamProducer::new(reader, 0);
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
