//! mgi-pulse — TUI log navigator.
//!
//! v0.1 M1 — paritet with Toolong on a single source:
//! - open one NDJSON file (or stdin) by mmap (or BufRead);
//! - index ts + level once;
//! - render a table sorted by line_id;
//! - `/regex` filter, `Esc` clears, arrow / page / g / G navigation.

mod app;
mod ask;
mod bookmarks_store;
mod collapse;
mod config;
mod explain;
mod ingest;
mod inspect;
mod journal;
mod llm;
mod mcp;
mod panes;
mod sources;
mod telegram;
mod theme;
mod util;
mod watch;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use ingest::{detect_format_from_files, ingest_file, ingest_merged, ingest_stdin};
use mgi_pulse_core::engine::format::LogFormat;
use mgi_pulse_core::engine::parse::FieldNames;
use mgi_pulse_core::engine::Engine;

/// mgi-pulse — not browse logs, navigate them.
///
/// Opens NDJSON files via mmap for speed. Safe for static log snapshots.
/// For active logs that another process may rotate or truncate, pipe
/// instead: `tail -F file | mgi-pulse -`. Reading an mmap'd file that
/// gets truncated under us delivers SIGBUS (kills the process), which
/// the stream path avoids by using owned buffers.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

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

    /// Regex pattern for the generic regex-extraction format. Named
    /// captures (`(?P<name>...)`) become fields the table and DSL
    /// can read; `ts` is parsed as RFC3339 and `level` as a severity
    /// name. Implies `--format=regex`. Example:
    /// `--pattern='(?P<ts>\d{4}-\d{2}-\d{2}\s[\d:]+)\s+(?P<level>\w+)\s+(?P<msg>.*)'`
    #[arg(long, value_name = "REGEX")]
    pattern: Option<String>,

    /// Read the systemd journal through `journalctl -o json` instead of
    /// a file. Linux only. Combine with `-u` and `--since`; `--follow`
    /// keeps streaming new entries.
    #[arg(long, default_value_t = false)]
    journal: bool,

    /// Journal unit to read (repeatable). Only with `--journal`.
    #[arg(short = 'u', long = "unit", value_name = "UNIT")]
    units: Vec<String>,

    /// Start of the journal window: an age (`2h`, `1d`), an RFC3339
    /// time, or anything `journalctl --since` accepts. Without it the
    /// journal is read from the start of the current boot.
    #[arg(long, value_name = "WHEN")]
    since: Option<String>,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Print only records that match rules, with repeats collapsed.
    /// Runs without a terminal; exits non-zero only on fatal errors.
    Watch(watch::WatchArgs),
    /// Serve read-only log tools to AI agents over MCP (stdio by
    /// default, Streamable HTTP with --http).
    Mcp(mcp::McpArgs),
    /// Ask a configured LLM (Ollama, LM Studio, llama.cpp, vLLM or any
    /// OpenAI-compatible API) a question about logs.
    Ask(ask::AskArgs),
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
    if let Some(cmd) = cli.command {
        return match cmd {
            Command::Watch(args) => watch::run(args),
            Command::Mcp(args) => mcp::run(args),
            Command::Ask(args) => ask::run(args),
        };
    }

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

    if !cli.journal && (!cli.units.is_empty() || cli.since.is_some()) {
        anyhow::bail!("-u/--unit and --since apply to --journal only");
    }
    if cli.journal && !cli.files.is_empty() {
        anyhow::bail!("--journal reads the journal; drop the file arguments");
    }

    if !cli.journal && cli.files.is_empty() && std::io::stdin().is_terminal() {
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
    let forced_format = cli
        .format
        .as_deref()
        .map(ingest::parse_format)
        .transpose()?;

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

    // --pattern implies --format=regex; explicit --format=regex
    // without a pattern is an error caught at compile time below.
    let regex_pattern: Option<std::sync::Arc<regex::bytes::Regex>> = match cli.pattern.as_deref() {
        Some(src) => match mgi_pulse_core::engine::parse_regex::compile_pattern(src) {
            Ok(re) => Some(std::sync::Arc::new(re)),
            Err(e) => anyhow::bail!("{}", e),
        },
        None => None,
    };
    if matches!(forced_format, Some(LogFormat::Regex)) && regex_pattern.is_none() {
        anyhow::bail!("--format=regex requires --pattern='<regex with named captures>'");
    }
    let forced_format = if regex_pattern.is_some() {
        Some(LogFormat::Regex)
    } else {
        forced_format
    };

    // Pick the source format. Explicit --format wins; otherwise sniff
    // a small probe from the first file (no probe for stdin — we'd
    // have to buffer and replay it). Probe size is bounded so a huge
    // file doesn't load megabytes just to vote on its shape.
    let fmt = match forced_format {
        Some(f) => f,
        None => detect_format_from_files(&cli.files).unwrap_or(LogFormat::Ndjson),
    };

    // Live journal entries arrive on this channel when following.
    let mut journal_live = None;

    if cli.journal {
        let query = journal::Query {
            units: cli.units.clone(),
            since: cli.since.as_deref().map(journal::since_arg),
            boot: cli.since.is_none(),
            ..Default::default()
        };
        source_label = query.label();
        let cursor = journal::ingest(&query, &mut engine)?;
        if cli.follow {
            journal_live = Some(journal::spawn_follow(query, cursor));
        }
    } else if cli.files.is_empty() {
        source_label = "<stdin>".to_string();
        ingest_stdin(&mut engine, fields.clone(), forced_format)?;
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
                ingest_stdin(&mut engine, fields.clone(), forced_format)?;
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

    // Regex format: attach the pattern to each source that has format
    // `Regex`, then re-derive ts/level. For single-source pipelines
    // the regex applies to source_id=0; merging across regex sources
    // isn't supported in v0.x and would need a per-CLI-arg pattern.
    if let Some(re) = regex_pattern.clone() {
        let regex_sids: Vec<u32> = engine
            .source_formats
            .iter()
            .enumerate()
            .filter(|(_, &f)| f == LogFormat::Regex)
            .map(|(sid, _)| sid as u32)
            .collect();
        for sid in regex_sids {
            engine.set_source_regex(sid, re.clone());
        }
        engine.recompute_regex_ts_level();
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

    // The explain key reads the model from the environment or the user
    // config; a broken config only disables it.
    let llm = config::load_user_config()
        .and_then(|cfg| llm::Llm::from_config(cfg.as_ref().and_then(|c| c.llm.as_ref())))
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "LLM settings ignored");
            None
        });
    let app =
        app::App::new_with_source(engine, source_label, cli.columns, theme, single_source_path)
            .with_llm(llm);
    // Wire the follow worker if requested. The channel buffer (8192)
    // absorbs short read bursts so the worker doesn't block on a slow
    // UI tick. Worker drops out on producer EOF (e.g. file truncated
    // and rotation gave us an empty new file) — UI keeps showing the
    // index built so far.
    let app = if let Some(plan) = pending_follow {
        let (tx, rx) = crossbeam_channel::bounded(8192);
        spawn_follow_worker(plan, tx);
        app.with_live(rx)
    } else if let Some(rx) = journal_live {
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
    use mgi_pulse_core::io::stream::StreamProducer;
    use mgi_pulse_core::io::tail::TailReader;
    use mgi_pulse_core::io::RecordProducer;

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

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter =
        EnvFilter::try_from_env("MGI_PULSE_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
