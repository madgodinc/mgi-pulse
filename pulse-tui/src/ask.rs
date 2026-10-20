//! `mgi-pulse ask`: a question about logs, answered by the configured
//! model from a bounded, redacted sample of the relevant records.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::config::Config;
use crate::inspect::Window;
use crate::llm::{self, Llm};
use crate::sources::{self, SourceKind, SourceSpec};
use crate::util;

#[derive(Debug, Args)]
pub struct AskArgs {
    /// The question, in any language.
    pub question: String,

    /// Log files to look at; `-` reads stdin.
    #[arg(value_name = "SOURCE")]
    pub sources: Vec<PathBuf>,

    /// Include the systemd journal (Linux).
    #[arg(long)]
    pub journal: bool,

    /// Journal unit (repeatable). Implies --journal.
    #[arg(short = 'u', long = "unit", value_name = "UNIT")]
    pub units: Vec<String>,

    /// Only records from this point on: an age (`2h`) or RFC3339.
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// Only records before this point.
    #[arg(long, value_name = "WHEN")]
    pub until: Option<String>,

    /// Pick the records with this DSL query instead of the default
    /// (warnings, errors, and lines that mention words of the question).
    #[arg(long, value_name = "QUERY")]
    pub query: Option<String>,

    /// TOML config with [llm] settings and sources. Without it the user
    /// config (~/.config/mgi-pulse/pulse.toml) is read when present.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Print the exact context that would be sent and exit. No model
    /// is contacted.
    #[arg(long)]
    pub print_context: bool,
}

pub fn run(args: AskArgs) -> Result<()> {
    let (cfg, dir) = match &args.config {
        Some(p) => (Config::load(p)?, p.parent().map(|d| d.to_path_buf())),
        None => (crate::config::load_user_config()?.unwrap_or_default(), None),
    };
    let mut specs = if args.sources.is_empty() && !args.journal && args.units.is_empty() {
        cfg.source_specs(dir.as_deref())?
    } else {
        Vec::new()
    };
    specs.extend(sources::specs_from_args(
        &args.sources,
        args.journal || !args.units.is_empty(),
        &args.units,
        None,
    ));
    sources::finalize_names(&mut specs)?;
    if specs.is_empty() {
        bail!("nothing to ask about: pass files, `-`, --journal, or a config with [[source]]");
    }
    // Stdin can't be re-read by the inspect layer: spool it to a file.
    let _spool = spool_stdin(&mut specs)?;

    let llm = if args.print_context {
        None
    } else {
        match Llm::from_config(cfg.llm.as_ref())? {
            Some(l) => Some(l),
            None => bail!(
                "no model configured: set {} and {} (e.g. http://localhost:11434/v1 and qwen3:8b for Ollama) or add [llm] to the config",
                llm::URL_ENV,
                llm::MODEL_ENV
            ),
        }
    };
    let budget = llm
        .as_ref()
        .map(|l| l.max_context)
        .or(cfg.llm.as_ref().and_then(|c| c.max_context_chars))
        .unwrap_or(12_000);
    let redact_on = llm
        .as_ref()
        .map(|l| l.redact)
        .unwrap_or_else(|| cfg.llm.as_ref().and_then(|c| c.redact).unwrap_or(true));

    let now = util::now_micros();
    let bound = |s: &Option<String>| -> Result<Option<i64>> {
        s.as_deref()
            .map(|v| util::parse_time_bound(v, now))
            .transpose()
            .map_err(anyhow::Error::msg)
    };
    let w = Window {
        since: bound(&args.since)?,
        until: bound(&args.until)?,
    };
    let refs: Vec<&SourceSpec> = specs.iter().collect();
    let g = llm::gather(
        &refs,
        &args.question,
        args.query.as_deref(),
        &w,
        budget,
        redact_on,
    )?;
    if args.print_context {
        println!("{}", g.context);
        return Ok(());
    }
    let llm = llm.unwrap();
    eprintln!(
        "mgi-pulse: asking {} at {} ({} records matched, {} characters of context)",
        llm.model,
        llm.host(),
        g.records,
        g.context.len()
    );
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    llm.chat(
        llm::SYSTEM_ASK,
        &llm::question_prompt(&args.question, &g.context),
        &mut |t| {
            let _ = out.write_all(t.as_bytes());
            let _ = out.flush();
        },
    )?;
    let _ = writeln!(out);
    Ok(())
}

/// Removes the spooled stdin copy when dropped.
pub struct Spool(Option<PathBuf>);

impl Drop for Spool {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn spool_stdin(specs: &mut [SourceSpec]) -> Result<Spool> {
    let Some(spec) = specs.iter_mut().find(|s| s.kind == SourceKind::Stdin) else {
        return Ok(Spool(None));
    };
    let path = std::env::temp_dir().join(format!("mgi-pulse-ask-{}.log", std::process::id()));
    let mut f = std::fs::File::create(&path).context("spool stdin")?;
    std::io::copy(&mut std::io::stdin().lock(), &mut f).context("spool stdin")?;
    spec.kind = SourceKind::File(path.clone());
    Ok(Spool(Some(path)))
}
