//! Named log sources for the headless modes (watch, Telegram, MCP) and a
//! live feed that streams their records to one channel.
//!
//! Sources are fixed at start-up from the command line or the config
//! file. Remote callers (bot commands, MCP tools) pick a source by name
//! and never pass a path, so they can only read what the operator
//! configured.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use mgi_pulse_core::engine::format::LogFormat;
use mgi_pulse_core::engine::record::TS_UNTIMED;
use mgi_pulse_core::io::compressed::{open_decompressed, Compression};
use mgi_pulse_core::io::stream::StreamProducer;
use mgi_pulse_core::io::tail::TailReader;
use mgi_pulse_core::io::RecordProducer;

use crate::journal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    File(PathBuf),
    /// Journal units; empty means the whole journal.
    Journal(Vec<String>),
    Stdin,
}

#[derive(Debug, Clone)]
pub struct SourceSpec {
    pub name: String,
    pub kind: SourceKind,
    /// Forced format; `None` sniffs the input.
    pub format: Option<LogFormat>,
}

impl SourceSpec {
    pub fn describe(&self) -> String {
        match &self.kind {
            SourceKind::File(p) => format!("file {}", p.display()),
            SourceKind::Journal(u) if u.is_empty() => "journal, all units".into(),
            SourceKind::Journal(u) => format!("journal {}", u.join(", ")),
            SourceKind::Stdin => "stdin".into(),
        }
    }

    /// Format to parse a file with: forced, else sniffed, else NDJSON.
    pub fn file_format(&self, path: &PathBuf) -> LogFormat {
        self.format.unwrap_or_else(|| {
            crate::ingest::detect_format_from_files(std::slice::from_ref(path))
                .unwrap_or(LogFormat::Ndjson)
        })
    }
}

/// Turn positional arguments into specs. Names default to the file
/// name and are made unique with a numeric suffix.
pub fn specs_from_args(
    files: &[PathBuf],
    journal: bool,
    units: &[String],
    format: Option<LogFormat>,
) -> Vec<SourceSpec> {
    let mut out = Vec::new();
    for p in files {
        if p.as_os_str() == "-" {
            out.push(SourceSpec {
                name: "stdin".into(),
                kind: SourceKind::Stdin,
                format,
            });
        } else {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string());
            out.push(SourceSpec {
                name,
                kind: SourceKind::File(p.clone()),
                format,
            });
        }
    }
    if journal {
        out.push(SourceSpec {
            name: "journal".into(),
            kind: SourceKind::Journal(units.to_vec()),
            format: Some(LogFormat::Journalctl),
        });
    }
    out
}

/// Enforce unique names (remote callers address sources by name) and
/// at most one stdin.
pub fn finalize_names(specs: &mut [SourceSpec]) -> Result<()> {
    if specs.iter().filter(|s| s.kind == SourceKind::Stdin).count() > 1 {
        bail!("stdin can be used only once");
    }
    let mut seen = std::collections::HashSet::new();
    for s in specs.iter_mut() {
        if s.name.is_empty() || s.name.chars().any(|c| c.is_whitespace()) {
            bail!(
                "source name `{}` must be non-empty and have no spaces",
                s.name
            );
        }
        let base = s.name.clone();
        let mut n = 2;
        while !seen.insert(s.name.clone()) {
            s.name = format!("{base}-{n}");
            n += 1;
        }
    }
    Ok(())
}

/// The human-readable message of a record: `msg`, `message` or the
/// journal's `MESSAGE`, else the whole line.
pub fn message_of(fmt: LogFormat, line: &[u8]) -> String {
    for key in ["msg", "message", "MESSAGE", "log"] {
        if let Some(v) = fmt.project_field(line, key) {
            if v.is_empty() {
                continue;
            }
            // JSON projections hand back the string body still escaped.
            if v.contains('\\') && matches!(fmt, LogFormat::Ndjson | LogFormat::Journalctl) {
                if let Ok(s) = serde_json::from_str::<String>(&format!("\"{v}\"")) {
                    return s;
                }
            }
            return v.into_owned();
        }
    }
    String::from_utf8_lossy(line).into_owned()
}

#[derive(Debug, Clone)]
pub struct Event {
    pub source: usize,
    pub fmt: LogFormat,
    pub ts_micros: i64,
    pub severity: u8,
    pub line: Box<[u8]>,
}

#[derive(Debug)]
pub enum FeedMsg {
    Event(Event),
    Ended { source: usize },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FeedOptions {
    /// Read files from the beginning instead of only new lines.
    pub from_start: bool,
    /// Keep waiting for new lines (files, journal).
    pub follow: bool,
    /// Drop timed records older than this (micros).
    pub since: Option<i64>,
}

/// Running readers, one thread per source. Dropping the feed stops file
/// followers and kills journalctl children.
pub struct Feed {
    pub rx: Receiver<FeedMsg>,
    stop: Arc<AtomicBool>,
    children: Vec<Child>,
}

impl Feed {
    pub fn start(specs: &[SourceSpec], opts: FeedOptions) -> Result<Feed> {
        let (tx, rx) = crossbeam_channel::bounded(8192);
        let stop = Arc::new(AtomicBool::new(false));
        let mut children = Vec::new();
        // Open everything first so a missing file fails the start.
        let mut readers: Vec<(usize, Box<dyn BufRead + Send>, Option<LogFormat>)> = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            match &spec.kind {
                SourceKind::File(path) => {
                    let fmt = spec.file_format(path);
                    let mut probe = std::fs::File::open(path)
                        .with_context(|| format!("open {}", path.display()))?;
                    let compressed = Compression::detect(&mut probe)? != Compression::None;
                    drop(probe);
                    let reader: Box<dyn BufRead + Send> = if compressed {
                        // Compressed archives are read once, never followed.
                        let (_, r) = open_decompressed(path)?;
                        Box::new(r)
                    } else if opts.follow {
                        let t = if opts.from_start {
                            TailReader::open_from_start(path)
                        } else {
                            TailReader::open(path)
                        }
                        .with_context(|| format!("open {}", path.display()))?;
                        Box::new(t.with_stop(stop.clone()))
                    } else {
                        let f = std::fs::File::open(path)
                            .with_context(|| format!("open {}", path.display()))?;
                        Box::new(BufReader::with_capacity(64 * 1024, f))
                    };
                    readers.push((i, reader, Some(fmt)));
                }
                SourceKind::Journal(units) => {
                    let q = feed_journal_query(units, opts);
                    let sp = journal::spawn(&q, false)?;
                    children.push(sp.child);
                    readers.push((
                        i,
                        Box::new(BufReader::with_capacity(64 * 1024, sp.stdout)),
                        Some(LogFormat::Journalctl),
                    ));
                }
                SourceKind::Stdin => {
                    let r = BufReader::with_capacity(64 * 1024, std::io::stdin());
                    readers.push((i, Box::new(r), spec.format));
                }
            }
        }
        for (i, reader, fmt) in readers {
            let tx = tx.clone();
            let since = if matches!(specs[i].kind, SourceKind::Journal(_)) {
                None
            } else {
                opts.since
            };
            std::thread::Builder::new()
                .name(format!("mgi-pulse-src-{i}"))
                .spawn(move || pump(i, reader, fmt, since, tx))
                .context("spawn reader thread")?;
        }
        Ok(Feed { rx, stop, children })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.children.clear();
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop();
    }
}

fn feed_journal_query(units: &[String], opts: FeedOptions) -> journal::Query {
    let mut q = journal::Query {
        units: units.to_vec(),
        follow: opts.follow,
        ..Default::default()
    };
    match opts.since {
        Some(s) => q.since = Some(format!("@{}", s.div_euclid(1_000_000))),
        // Following without a window means "from now on".
        None if opts.follow => q.lines = Some(0),
        None => q.boot = true,
    }
    q
}

fn pump(
    source: usize,
    mut reader: Box<dyn BufRead + Send>,
    fmt: Option<LogFormat>,
    since: Option<i64>,
    tx: Sender<FeedMsg>,
) {
    let fmt = fmt.unwrap_or_else(|| crate::ingest::sniff_reader(&mut reader));
    let mut producer = StreamProducer::new(reader, source as u32).with_format(fmt);
    while let Some(rec) = producer.next() {
        if let Some(s) = since {
            if rec.ts_micros != TS_UNTIMED && rec.ts_micros < s {
                continue;
            }
        }
        let line = match rec.bytes {
            mgi_pulse_core::RecordBytes::Owned(b) => b,
            _ => continue,
        };
        let ev = Event {
            source,
            fmt,
            ts_micros: rec.ts_micros,
            severity: rec.severity,
            line,
        };
        if tx.send(FeedMsg::Event(ev)).is_err() {
            return;
        }
    }
    let _ = tx.send(FeedMsg::Ended { source });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_unique() {
        let mut specs = specs_from_args(
            &[PathBuf::from("a/app.log"), PathBuf::from("b/app.log")],
            true,
            &["nginx".into()],
            None,
        );
        finalize_names(&mut specs).unwrap();
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["app.log", "app.log-2", "journal"]);
        assert_eq!(specs[2].kind, SourceKind::Journal(vec!["nginx".into()]));
    }

    #[test]
    fn two_stdins_are_rejected() {
        let mut specs = specs_from_args(&["-".into(), "-".into()], false, &[], None);
        assert!(finalize_names(&mut specs).is_err());
    }

    #[test]
    fn message_prefers_fields_then_line() {
        assert_eq!(
            message_of(LogFormat::Ndjson, br#"{"msg":"disk full","level":"error"}"#),
            "disk full"
        );
        assert_eq!(
            message_of(
                LogFormat::Journalctl,
                br#"{"MESSAGE":"oom","PRIORITY":"3","__REALTIME_TIMESTAMP":"1"}"#
            ),
            "oom"
        );
        assert_eq!(message_of(LogFormat::Ndjson, b"plain text"), "plain text");
    }

    #[test]
    fn journal_feed_windows() {
        let follow = feed_journal_query(
            &[],
            FeedOptions {
                follow: true,
                ..Default::default()
            },
        );
        assert_eq!(follow.lines, Some(0));
        let once = feed_journal_query(&[], FeedOptions::default());
        assert!(once.boot);
        let since = feed_journal_query(
            &[],
            FeedOptions {
                since: Some(1_700_000_000_000_000),
                ..Default::default()
            },
        );
        assert_eq!(since.since.as_deref(), Some("@1700000000"));
    }

    #[test]
    fn feed_reads_a_file_once_and_ends() {
        let dir = std::env::temp_dir().join(format!("mgi-pulse-feed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("app.ndjson");
        std::fs::write(
            &p,
            "{\"ts\":\"2026-06-01T12:00:00Z\",\"level\":\"info\",\"msg\":\"a\"}\n\
             {\"ts\":\"2026-06-01T12:00:01Z\",\"level\":\"error\",\"msg\":\"b\"}\n",
        )
        .unwrap();
        let specs = specs_from_args(std::slice::from_ref(&p), false, &[], None);
        let feed = Feed::start(
            &specs,
            FeedOptions {
                from_start: true,
                ..Default::default()
            },
        )
        .unwrap();
        let mut events = 0;
        loop {
            match feed.rx.recv().unwrap() {
                FeedMsg::Event(e) => {
                    events += 1;
                    assert_eq!(e.fmt, LogFormat::Ndjson);
                }
                FeedMsg::Ended { source } => {
                    assert_eq!(source, 0);
                    break;
                }
            }
        }
        assert_eq!(events, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
