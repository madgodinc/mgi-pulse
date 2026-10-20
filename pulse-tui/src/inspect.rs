//! On-demand reads of named sources for remote callers (bot commands,
//! MCP tools): last lines, filtered search, per-source summaries.
//!
//! Every call re-reads the source, so answers are current and nothing
//! is held in memory between calls. Output sizes are capped by the
//! caller's limits.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Result};
use mgi_pulse_core::engine::format::{FieldCache, LogFormat};
use mgi_pulse_core::engine::predicate::Predicate;
use mgi_pulse_core::engine::record::{severity, RawRecord, RecordBytes, TS_UNTIMED};
use mgi_pulse_core::engine::Engine;
use mgi_pulse_core::io::compressed::Compression;
use serde_json::{json, Value};

use crate::journal;
use crate::sources::{message_of, SourceKind, SourceSpec};
use crate::util;

/// Newest journal entries read per query. Keeps a "search the whole
/// boot" request on a busy host bounded.
pub const JOURNAL_CAP: usize = 200_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub source: String,
    pub ts_micros: i64,
    pub severity: u8,
    pub msg: String,
    pub line: String,
    /// Named fields shown instead of the raw line (journal entries,
    /// whose raw JSON is mostly bookkeeping).
    pub fields: Vec<(&'static str, String)>,
}

/// Journal fields worth showing, under their short alias.
const JOURNAL_FIELDS: [(&str, &str); 4] = [
    ("unit", "_SYSTEMD_UNIT"),
    ("ident", "SYSLOG_IDENTIFIER"),
    ("pid", "_PID"),
    ("host", "_HOSTNAME"),
];

impl Hit {
    pub fn new(source: &str, fmt: LogFormat, ts_micros: i64, severity: u8, line: &[u8]) -> Hit {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let fields = if fmt == LogFormat::Journalctl {
            JOURNAL_FIELDS
                .iter()
                .filter_map(|(alias, key)| {
                    fmt.project_field(line, key)
                        .map(|v| (*alias, v.into_owned()))
                })
                .collect()
        } else {
            Vec::new()
        };
        Hit {
            source: source.to_string(),
            ts_micros,
            severity,
            msg: message_of(fmt, line),
            line: String::from_utf8_lossy(line).into_owned(),
            fields,
        }
    }

    pub fn to_json(&self, max_line: usize) -> Value {
        let (msg, msg_cut) = util::truncate(&self.msg, max_line);
        let mut v = json!({
            "source": self.source,
            "ts": (self.ts_micros != TS_UNTIMED).then(|| util::format_ts(self.ts_micros)),
            "level": severity::name(self.severity),
            "msg": msg,
        });
        let mut cut = msg_cut;
        if self.fields.is_empty() {
            let (line, line_cut) = util::truncate(&self.line, max_line);
            v["line"] = json!(line);
            cut |= line_cut;
        } else {
            for (k, val) in &self.fields {
                v[*k] = json!(val);
            }
        }
        v["truncated"] = json!(cut);
        v
    }

    /// `12:00:02 error api upstream timeout`, single line.
    pub fn short(&self) -> String {
        let ts = if self.ts_micros == TS_UNTIMED {
            "-".to_string()
        } else {
            util::format_ts(self.ts_micros)
        };
        format!(
            "{ts} {} {} {}",
            severity::name(self.severity),
            self.source,
            crate::watch::one_line(&self.msg, 500)
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Window {
    pub since: Option<i64>,
    pub until: Option<i64>,
}

impl Window {
    fn admits(&self, ts: i64) -> bool {
        if self.since.is_none() && self.until.is_none() {
            return true;
        }
        if ts == TS_UNTIMED {
            return false;
        }
        self.since.is_none_or(|s| ts >= s) && self.until.is_none_or(|u| ts < u)
    }
}

#[derive(Debug, Default)]
pub struct Found {
    pub hits: Vec<Hit>,
    pub total: usize,
}

fn readable(spec: &SourceSpec) -> Result<()> {
    if spec.kind == SourceKind::Stdin {
        bail!("source `{}` is stdin and cannot be re-read", spec.name);
    }
    Ok(())
}

/// Index one source. Journal windows are pushed down to journalctl.
pub fn load(spec: &SourceSpec, w: &Window) -> Result<Engine> {
    readable(spec)?;
    let mut engine = Engine::new();
    match &spec.kind {
        SourceKind::File(path) => {
            let fmt = spec.file_format(path);
            crate::ingest::ingest_file(path, &mut engine, None, fmt)?;
            engine.capture_csv_headers();
        }
        SourceKind::Journal(units) => {
            let at = |m: i64| format!("@{}", m.div_euclid(1_000_000));
            let q = journal::Query {
                units: units.clone(),
                since: w.since.map(at),
                until: w.until.map(at),
                boot: w.since.is_none(),
                lines: Some(JOURNAL_CAP),
                ..Default::default()
            };
            journal::ingest(&q, &mut engine)?;
        }
        SourceKind::Stdin => unreachable!(),
    }
    Ok(engine)
}

fn hit_of(engine: &Engine, spec: &SourceSpec, line_id: u64) -> Hit {
    let loc = &engine.indexes.line.locs[line_id as usize];
    let bytes = engine.line_bytes(line_id);
    let fmt = engine.format_of(loc.source_id);
    Hit::new(
        &spec.name,
        fmt,
        engine.indexes.time.get(line_id).unwrap_or(TS_UNTIMED),
        engine.indexes.severity.get(line_id).unwrap_or(0),
        bytes,
    )
}

/// Matching records in time order; only the newest `limit` are kept,
/// `total` counts all of them.
pub fn search(
    spec: &SourceSpec,
    pred: Option<&dyn Predicate>,
    w: &Window,
    limit: usize,
) -> Result<Found> {
    let engine = load(spec, w)?;
    let n = engine.indexes.len() as u64;
    let mut ids = std::collections::VecDeque::with_capacity(limit.min(1024));
    let mut total = 0usize;
    for line_id in 0..n {
        let ts = engine.indexes.time.get(line_id).unwrap_or(TS_UNTIMED);
        if !w.admits(ts) {
            continue;
        }
        if let Some(p) = pred {
            if !matches(&engine, line_id, p) {
                continue;
            }
        }
        total += 1;
        if limit > 0 {
            if ids.len() == limit {
                ids.pop_front();
            }
            ids.push_back(line_id);
        }
    }
    Ok(Found {
        hits: ids
            .into_iter()
            .map(|id| hit_of(&engine, spec, id))
            .collect(),
        total,
    })
}

fn matches(engine: &Engine, line_id: u64, p: &dyn Predicate) -> bool {
    let loc = &engine.indexes.line.locs[line_id as usize];
    let bytes = engine.line_bytes(line_id);
    let fmt = engine.format_of(loc.source_id);
    let mut cache = FieldCache::new(fmt, bytes);
    if let Some(h) = engine.headers_of(loc.source_id) {
        cache = cache.with_headers(h);
    }
    if let Some(re) = engine.regex_of(loc.source_id) {
        cache = cache.with_regex(re);
    }
    let rec = RawRecord {
        source_id: loc.source_id,
        line_id,
        ts_micros: engine.indexes.time.get(line_id).unwrap_or(TS_UNTIMED),
        severity: engine.indexes.severity.get(line_id).unwrap_or(0),
        bytes: RecordBytes::FileRef {
            source_id: loc.source_id,
            offset: loc.offset,
            len: loc.len,
        },
    };
    p.matches(&rec, &mut cache)
}

/// The last `n` records. Plain files are read backwards from the end,
/// so this stays cheap on multi-gigabyte logs.
pub fn tail(spec: &SourceSpec, n: usize) -> Result<Vec<Hit>> {
    readable(spec)?;
    match &spec.kind {
        SourceKind::File(path) if !is_compressed(path)? => {
            let fmt = spec.file_format(path);
            let lines = tail_lines(path, n)?;
            Ok(lines.iter().map(|l| hit_from_line(spec, fmt, l)).collect())
        }
        SourceKind::Journal(units) => {
            let q = journal::Query {
                units: units.clone(),
                lines: Some(n),
                ..Default::default()
            };
            let mut engine = Engine::new();
            journal::ingest(&q, &mut engine)?;
            let len = engine.indexes.len() as u64;
            Ok((len.saturating_sub(n as u64)..len)
                .map(|id| hit_of(&engine, spec, id))
                .collect())
        }
        _ => Ok(search(spec, None, &Window::default(), n)?.hits),
    }
}

fn is_compressed(path: &Path) -> Result<bool> {
    let mut f = std::fs::File::open(path)?;
    Ok(Compression::detect(&mut f)? != Compression::None)
}

fn hit_from_line(spec: &SourceSpec, fmt: LogFormat, line: &[u8]) -> Hit {
    let mut stats = Default::default();
    let (ts, sev) = fmt.parse_ts_level(line, &mut stats, None);
    Hit::new(&spec.name, fmt, ts, sev, line)
}

/// Read backwards in 64 KiB steps until `n` complete lines are in hand.
/// Stops at 16 MiB so a file of one giant line cannot eat memory.
pub fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<Vec<u8>>> {
    const STEP: u64 = 64 * 1024;
    const MAX: usize = 16 * 1024 * 1024;
    let mut f = std::fs::File::open(path)?;
    let mut pos = f.metadata()?.len();
    let mut buf: Vec<u8> = Vec::new();
    while pos > 0 && buf.len() < MAX {
        let step = STEP.min(pos);
        pos -= step;
        f.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; step as usize];
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        // n + 1 newlines guarantee the first kept line is complete.
        if buf.iter().filter(|&&b| b == b'\n').count() > n {
            break;
        }
    }
    let mut lines: Vec<Vec<u8>> = buf
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l).to_vec())
        .filter(|l| !l.is_empty())
        .collect();
    if pos > 0 && !lines.is_empty() {
        // The first piece may start mid-line.
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(n);
    Ok(lines.split_off(skip))
}

#[derive(Debug, Default)]
pub struct SourceStats {
    pub source: String,
    pub total: u64,
    pub by_level: Vec<(&'static str, u64)>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    /// Field used for the logger breakdown, if any record had one.
    pub logger_field: Option<&'static str>,
    pub top_loggers: Vec<(String, u64)>,
    /// Normalised message, a sample of it, count.
    pub top_messages: Vec<(String, u64)>,
}

impl SourceStats {
    pub fn to_json(&self) -> Value {
        json!({
            "source": self.source,
            "total": self.total,
            "by_level": self.by_level.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
            "first_ts": self.first_ts.map(util::format_ts),
            "last_ts": self.last_ts.map(util::format_ts),
            "logger_field": self.logger_field,
            "top_loggers": self.top_loggers.iter().map(|(k, v)| json!({"name": k, "count": v})).collect::<Vec<_>>(),
            "top_messages": self.top_messages.iter().map(|(k, v)| json!({"msg": k, "count": v})).collect::<Vec<_>>(),
        })
    }
}

const LOGGER_FIELDS: [&str; 5] = ["logger", "ident", "unit", "module", "component"];
const MAX_BUCKETS: usize = 5000;

pub fn stats(spec: &SourceSpec, w: &Window, top: usize) -> Result<SourceStats> {
    let engine = load(spec, w)?;
    let mut out = SourceStats {
        source: spec.name.clone(),
        ..Default::default()
    };
    let mut levels = [0u64; 8];
    let mut loggers: HashMap<String, u64> = HashMap::new();
    let mut msgs: HashMap<String, (String, u64)> = HashMap::new();
    for line_id in 0..engine.indexes.len() as u64 {
        let ts = engine.indexes.time.get(line_id).unwrap_or(TS_UNTIMED);
        if !w.admits(ts) {
            continue;
        }
        out.total += 1;
        let sev = engine.indexes.severity.get(line_id).unwrap_or(0) as usize;
        levels[sev.min(7)] += 1;
        if ts != TS_UNTIMED {
            out.first_ts = Some(out.first_ts.map_or(ts, |t| t.min(ts)));
            out.last_ts = Some(out.last_ts.map_or(ts, |t| t.max(ts)));
        }
        let loc = &engine.indexes.line.locs[line_id as usize];
        let bytes = engine.line_bytes(line_id);
        let fmt = engine.format_of(loc.source_id);
        let fields = match out.logger_field {
            Some(f) => std::slice::from_ref(LOGGER_FIELDS.iter().find(|x| **x == f).unwrap()),
            None => &LOGGER_FIELDS[..],
        };
        for f in fields {
            if let Some(v) = fmt.project_field(bytes, f) {
                out.logger_field = Some(f);
                bump(&mut loggers, v.into_owned());
                break;
            }
        }
        let msg = message_of(fmt, bytes);
        let key = util::normalize_message(&msg);
        if let Some(e) = msgs.get_mut(&key) {
            e.1 += 1;
        } else if msgs.len() < MAX_BUCKETS {
            msgs.insert(key, (crate::watch::one_line(&msg, 300), 1));
        }
    }
    out.by_level = (0..8u8)
        .filter(|&s| levels[s as usize] > 0)
        .map(|s| {
            (
                if s == 0 { "unknown" } else { severity::name(s) },
                levels[s as usize],
            )
        })
        .collect();
    out.top_loggers = top_n(loggers.into_iter().collect(), top);
    out.top_messages = top_n(msgs.into_values().collect(), top);
    Ok(out)
}

fn bump(map: &mut HashMap<String, u64>, key: String) {
    if let Some(c) = map.get_mut(&key) {
        *c += 1;
    } else if map.len() < MAX_BUCKETS {
        map.insert(key, 1);
    }
}

fn top_n(mut v: Vec<(String, u64)>, n: usize) -> Vec<(String, u64)> {
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A throwaway directory with an NDJSON log. Cleaned on drop.
    pub struct Fixture {
        pub dir: PathBuf,
        pub spec: SourceSpec,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub fn fixture(tag: &str, body: &str) -> Fixture {
        let dir =
            std::env::temp_dir().join(format!("mgi-pulse-inspect-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.ndjson");
        std::fs::write(&path, body).unwrap();
        Fixture {
            dir,
            spec: SourceSpec {
                name: "app".into(),
                kind: SourceKind::File(path),
                format: None,
            },
        }
    }

    pub const LOG: &str = "\
{\"ts\":\"2026-06-01T12:00:00Z\",\"level\":\"info\",\"logger\":\"web\",\"msg\":\"start\"}
{\"ts\":\"2026-06-01T12:00:01Z\",\"level\":\"error\",\"logger\":\"db\",\"msg\":\"retry 1 failed\"}
{\"ts\":\"2026-06-01T12:00:02Z\",\"level\":\"error\",\"logger\":\"db\",\"msg\":\"retry 2 failed\"}
{\"ts\":\"2026-06-01T12:00:03Z\",\"level\":\"warn\",\"logger\":\"web\",\"msg\":\"slow\"}
{\"ts\":\"2026-06-01T12:00:04Z\",\"level\":\"info\",\"logger\":\"web\",\"msg\":\"done\"}
";

    #[test]
    fn tail_returns_last_lines_in_order() {
        let f = fixture("tail", LOG);
        let hits = tail(&f.spec, 2).unwrap();
        let msgs: Vec<_> = hits.iter().map(|h| h.msg.as_str()).collect();
        assert_eq!(msgs, ["slow", "done"]);
        assert_eq!(hits[1].severity, severity::INFO);
    }

    #[test]
    fn tail_lines_crosses_chunk_boundaries() {
        let mut body = String::new();
        for i in 0..20_000 {
            body.push_str(&format!("line {i}\n"));
        }
        let f = fixture("tail-big", &body);
        let SourceKind::File(p) = &f.spec.kind else {
            unreachable!()
        };
        let lines = tail_lines(p, 3).unwrap();
        assert_eq!(
            lines,
            [
                b"line 19997".to_vec(),
                b"line 19998".to_vec(),
                b"line 19999".to_vec()
            ]
        );
        assert_eq!(tail_lines(p, 50_000).unwrap().len(), 20_000);
    }

    #[test]
    fn search_keeps_newest_and_counts_all() {
        let f = fixture("search", LOG);
        let p = mgi_pulse_core::engine::dsl::compile("level=error").unwrap();
        let found = search(&f.spec, Some(p.as_ref()), &Window::default(), 1).unwrap();
        assert_eq!(found.total, 2);
        assert!(found.total > found.hits.len());
        assert_eq!(found.hits[0].msg, "retry 2 failed");
    }

    #[test]
    fn window_filters_on_time() {
        let f = fixture("window", LOG);
        let since = util::parse_time_bound("2026-06-01T12:00:03Z", 0).unwrap();
        let w = Window {
            since: Some(since),
            until: None,
        };
        let found = search(&f.spec, None, &w, 10).unwrap();
        assert_eq!(found.total, 2);
    }

    #[test]
    fn stats_count_levels_loggers_and_messages() {
        let f = fixture("stats", LOG);
        let s = stats(&f.spec, &Window::default(), 3).unwrap();
        assert_eq!(s.total, 5);
        assert!(s.by_level.contains(&("error", 2)));
        assert_eq!(s.logger_field, Some("logger"));
        assert_eq!(s.top_loggers[0], ("web".to_string(), 3));
        assert_eq!(s.top_messages[0], ("retry 1 failed".to_string(), 2));
    }

    #[test]
    fn journal_hits_show_named_fields_not_raw_json() {
        let line = br#"{"__CURSOR":"s=1","__REALTIME_TIMESTAMP":"1","PRIORITY":"3","MESSAGE":"oom","_SYSTEMD_UNIT":"api.service","_PID":"42","SYSLOG_IDENTIFIER":"api"}"#;
        let h = Hit::new("journal", LogFormat::Journalctl, 1, severity::ERROR, line);
        let v = h.to_json(1000);
        assert_eq!(v["msg"], "oom");
        assert_eq!(v["unit"], "api.service");
        assert_eq!(v["pid"], "42");
        assert!(v.get("line").is_none());
        let plain = Hit::new(
            "app",
            LogFormat::Ndjson,
            1,
            severity::INFO,
            b"{\"msg\":\"x\"}\r",
        );
        assert_eq!(plain.to_json(1000)["line"], "{\"msg\":\"x\"}");
    }

    #[test]
    fn stdin_is_refused() {
        let spec = SourceSpec {
            name: "in".into(),
            kind: SourceKind::Stdin,
            format: None,
        };
        assert!(tail(&spec, 5).is_err());
    }
}
