//! systemd journal as a source, read through `journalctl -o json`.
//!
//! A subprocess instead of linking libsystemd keeps every platform
//! building from one tree; on hosts without `journalctl` the spawn
//! fails with a clear message at run time.

use std::io::BufReader;
use std::process::{Child, ChildStdout, Command, Stdio};

use anyhow::{bail, Context, Result};
use mgi_pulse_core::engine::format::LogFormat;
use mgi_pulse_core::engine::record::RawRecord;
use mgi_pulse_core::engine::Engine;
use mgi_pulse_core::io::stream::StreamProducer;
use mgi_pulse_core::io::RecordProducer;
use mgi_pulse_core::schema::{project_field, unquote_if_string};

use crate::util;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub units: Vec<String>,
    /// Passed to `--since` verbatim (see [`since_arg`]).
    pub since: Option<String>,
    /// `--until`, verbatim.
    pub until: Option<String>,
    /// Limit to the current boot when no `since` is given.
    pub boot: bool,
    /// `-n`: newest N entries of the window.
    pub lines: Option<usize>,
    pub follow: bool,
    pub after_cursor: Option<String>,
}

impl Query {
    pub fn args(&self) -> Vec<String> {
        let mut a = vec!["-o".into(), "json".into(), "--no-pager".into()];
        for u in &self.units {
            a.push("-u".into());
            a.push(u.clone());
        }
        if let Some(c) = &self.after_cursor {
            a.push(format!("--after-cursor={c}"));
        } else if let Some(s) = &self.since {
            a.push(format!("--since={s}"));
        } else if self.boot {
            a.push("-b".into());
        }
        if let Some(u) = &self.until {
            a.push(format!("--until={u}"));
        }
        if let Some(n) = self.lines {
            a.push(format!("--lines={n}"));
        }
        if self.follow {
            a.push("--follow".into());
        }
        a
    }

    pub fn label(&self) -> String {
        if self.units.is_empty() {
            "journal".into()
        } else {
            format!("journal: {}", self.units.join(", "))
        }
    }
}

/// Our own time syntax (`2h`, RFC3339) becomes `@<epoch seconds>`;
/// anything else (`today`, `-1h`, `2026-06-01 12:00`) goes to
/// journalctl untouched, since it parses more forms than we do.
pub fn since_arg(s: &str) -> String {
    match util::parse_time_bound(s, util::now_micros()) {
        Ok(micros) => format!("@{}", micros.div_euclid(1_000_000)),
        Err(_) => s.to_string(),
    }
}

pub struct Spawned {
    pub child: Child,
    pub stdout: ChildStdout,
}

/// Start journalctl. `quiet_stderr` keeps its warnings off a terminal
/// the TUI is drawing on.
pub fn spawn(q: &Query, quiet_stderr: bool) -> Result<Spawned> {
    let mut cmd = Command::new("journalctl");
    cmd.args(q.args())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if quiet_stderr {
            Stdio::null()
        } else {
            Stdio::inherit()
        });
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("journalctl not found: reading the journal needs a Linux host with systemd")
        }
        Err(e) => return Err(e).context("start journalctl"),
    };
    let stdout = child.stdout.take().context("journalctl stdout")?;
    Ok(Spawned { child, stdout })
}

/// Read a finite journal window into `engine`. Returns the cursor of
/// the last entry so a follower can continue without a gap.
pub fn ingest(q: &Query, engine: &mut Engine) -> Result<Option<String>> {
    let mut sp = spawn(&q.clone_without_follow(), false)?;
    let before = engine.indexes.len();
    let reader = BufReader::with_capacity(64 * 1024, &mut sp.stdout);
    crate::ingest::ingest_reader(reader, engine, None, Some(LogFormat::Journalctl))?;
    let status = sp.child.wait().context("wait for journalctl")?;
    let added = engine.indexes.len() - before;
    if !status.success() && added == 0 {
        bail!("journalctl exited with {status}");
    }
    if added == 0 {
        return Ok(None);
    }
    let last = engine.line_bytes(engine.indexes.len() as u64 - 1);
    Ok(cursor_of(last))
}

pub fn cursor_of(line: &[u8]) -> Option<String> {
    project_field(line, "__CURSOR").map(|c| unquote_if_string(c).to_string())
}

impl Query {
    fn clone_without_follow(&self) -> Query {
        Query {
            follow: false,
            ..self.clone()
        }
    }
}

/// Producer over a running journalctl. Owns the child so dropping the
/// producer kills it.
pub struct JournalProducer {
    inner: StreamProducer<BufReader<ChildStdout>>,
    child: Child,
}

impl JournalProducer {
    pub fn start(q: &Query, quiet_stderr: bool, source_id: u32) -> Result<Self> {
        let sp = spawn(q, quiet_stderr)?;
        let inner = StreamProducer::new(BufReader::with_capacity(64 * 1024, sp.stdout), source_id)
            .with_format(LogFormat::Journalctl);
        Ok(Self {
            inner,
            child: sp.child,
        })
    }
}

impl RecordProducer for JournalProducer {
    fn next(&mut self) -> Option<RawRecord> {
        self.inner.next()
    }

    fn is_live(&self) -> bool {
        self.inner.is_live()
    }
}

impl Drop for JournalProducer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Follow the journal after `cursor` on a worker thread, feeding the
/// TUI's live channel. Without a cursor the follow starts from now.
pub fn spawn_follow(
    mut q: Query,
    cursor: Option<String>,
) -> crossbeam_channel::Receiver<RawRecord> {
    let (tx, rx) = crossbeam_channel::bounded(8192);
    q.follow = true;
    match cursor {
        Some(c) => q.after_cursor = Some(c),
        None => {
            q.since = None;
            q.boot = false;
            q.lines = Some(0);
        }
    }
    std::thread::Builder::new()
        .name("mgi-pulse-journal".into())
        .spawn(move || {
            let mut producer = match JournalProducer::start(&q, true, 0) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "journal follow: start failed");
                    return;
                }
            };
            while let Some(rec) = producer.next() {
                if tx.send(rec).is_err() {
                    return;
                }
            }
        })
        .ok();
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_for_units_since_and_follow() {
        let q = Query {
            units: vec!["nginx.service".into(), "api.service".into()],
            since: Some("@1700000000".into()),
            follow: true,
            ..Default::default()
        };
        assert_eq!(
            q.args(),
            [
                "-o",
                "json",
                "--no-pager",
                "-u",
                "nginx.service",
                "-u",
                "api.service",
                "--since=@1700000000",
                "--follow"
            ]
        );
    }

    #[test]
    fn cursor_wins_over_since_and_boot() {
        let q = Query {
            since: Some("today".into()),
            boot: true,
            after_cursor: Some("s=abc;i=1".into()),
            lines: Some(0),
            ..Default::default()
        };
        let a = q.args();
        assert!(a.contains(&"--after-cursor=s=abc;i=1".to_string()));
        assert!(!a.iter().any(|x| x.starts_with("--since") || x == "-b"));
        assert!(a.contains(&"--lines=0".to_string()));
    }

    #[test]
    fn boot_is_the_default_window() {
        let q = Query {
            boot: true,
            ..Default::default()
        };
        assert!(q.args().contains(&"-b".to_string()));
    }

    #[test]
    fn since_arg_converts_our_syntax_and_passes_the_rest() {
        assert!(since_arg("2h").starts_with('@'));
        assert_eq!(since_arg("2026-06-01T00:00:00Z"), "@1780272000");
        assert_eq!(since_arg("yesterday"), "yesterday");
    }

    #[test]
    fn cursor_is_read_from_a_journal_line() {
        let line = br#"{"__CURSOR":"s=1;i=2","__REALTIME_TIMESTAMP":"1","PRIORITY":"6"}"#;
        assert_eq!(cursor_of(line).as_deref(), Some("s=1;i=2"));
    }

    #[test]
    fn journal_json_on_a_pipe_is_detected() {
        let body = concat!(
            r#"{"__CURSOR":"s=1","__REALTIME_TIMESTAMP":"1717235400123456","PRIORITY":"3","MESSAGE":"boom","_SYSTEMD_UNIT":"api.service","_PID":"42"}"#,
            "\n",
            r#"{"__CURSOR":"s=2","__REALTIME_TIMESTAMP":"1717235400223456","PRIORITY":"6","MESSAGE":"ok","_SYSTEMD_UNIT":"api.service","_PID":"42"}"#,
            "\n"
        );
        let mut engine = Engine::new();
        let fmt = crate::ingest::ingest_reader(
            std::io::Cursor::new(body.as_bytes().to_vec()),
            &mut engine,
            None,
            None,
        )
        .unwrap();
        assert_eq!(fmt, LogFormat::Journalctl);
        assert_eq!(engine.indexes.len(), 2);
        assert_eq!(engine.indexes.severity.get(0), Some(5));
        assert_eq!(
            LogFormat::Journalctl
                .project_field(engine.line_bytes(0), "unit")
                .as_deref(),
            Some("api.service")
        );
    }
}
