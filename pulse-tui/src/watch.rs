//! Headless watch mode: stream sources, keep the records that match a
//! rule, collapse repeats, print one line (or one JSON object) each.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use clap::Args;
use mgi_pulse_core::engine::dsl;
use mgi_pulse_core::engine::format::FieldCache;
use mgi_pulse_core::engine::predicate::{Predicate, SeverityInPredicate};
use mgi_pulse_core::engine::record::{severity, RawRecord, TS_UNTIMED};
use mgi_pulse_core::RecordBytes;
use serde_json::json;

use crate::collapse::{Collapser, Verdict};
use crate::config::Config;
use crate::sources::{self, Event, Feed, FeedMsg, FeedOptions, SourceSpec};
use crate::util;

#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Files to watch; `-` reads stdin.
    #[arg(value_name = "SOURCE")]
    pub sources: Vec<PathBuf>,

    /// Also watch the systemd journal (Linux).
    #[arg(long)]
    pub journal: bool,

    /// Journal unit (repeatable). Implies --journal.
    #[arg(short = 'u', long = "unit", value_name = "UNIT")]
    pub units: Vec<String>,

    /// Query DSL expression (repeatable). The first rule that matches
    /// a record wins. Without any rule, error and fatal records match.
    #[arg(long = "rule", value_name = "EXPR")]
    pub rules: Vec<String>,

    /// Hold back repeats of the same message for this long and report
    /// them as one count. `0` turns collapsing off.
    #[arg(long, value_name = "DURATION")]
    pub collapse: Option<String>,

    /// Output: `text` (default) or `json` (one object per line).
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,

    /// Force the log format of file and stdin sources.
    #[arg(long, value_name = "FORMAT")]
    pub log_format: Option<String>,

    /// TOML config with sources, rules and alert settings.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Read existing file content first instead of only new lines.
    #[arg(long)]
    pub from_start: bool,

    /// Read what is there now and exit instead of following.
    #[arg(long)]
    pub once: bool,

    /// Skip records older than this: an age (`2h`) or RFC3339 time.
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// Ignore the [telegram] section of the config.
    #[arg(long)]
    pub no_telegram: bool,
}

impl WatchState {
    pub fn new(rules: &[Rule]) -> Self {
        WatchState {
            rules: rules
                .iter()
                .map(|r| (r.name.clone(), r.query.clone()))
                .collect(),
            matches: Mutex::default(),
        }
    }
}

pub struct Rule {
    pub name: String,
    pub query: String,
    pub alert: bool,
    pred: Box<dyn Predicate>,
}

impl Rule {
    pub fn compile(name: &str, query: &str, alert: bool) -> Result<Rule> {
        let pred = dsl::compile(query).map_err(|e| anyhow::anyhow!("rule `{name}`: {e}"))?;
        Ok(Rule {
            name: name.into(),
            query: query.into(),
            alert,
            pred,
        })
    }

    /// The rule used when none is configured: error and fatal records.
    pub fn default_errors() -> Rule {
        Rule {
            name: "errors".into(),
            query: "severity >= error".into(),
            alert: true,
            pred: Box::new(SeverityInPredicate::new(&[
                severity::ERROR,
                severity::FATAL,
            ])),
        }
    }

    pub fn matches(&self, ev: &Event) -> bool {
        // Predicates read bytes through the cache; the record header
        // only carries ts and severity, so no copy of the line.
        let rec = RawRecord {
            source_id: ev.source as u32,
            line_id: 0,
            ts_micros: ev.ts_micros,
            severity: ev.severity,
            bytes: RecordBytes::FileRef {
                source_id: ev.source as u32,
                offset: 0,
                len: 0,
            },
        };
        let mut cache = FieldCache::new(ev.fmt, &ev.line);
        self.pred.matches(&rec, &mut cache)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Matched {
    pub rule: String,
    pub source: String,
    pub ts_micros: i64,
    pub severity: u8,
    pub msg: String,
    pub line: String,
    pub alert: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    Match(Matched),
    Repeat {
        first: Matched,
        last: Matched,
        count: u64,
        window: Duration,
    },
}

impl Notice {
    pub fn matched(&self) -> &Matched {
        match self {
            Notice::Match(m) => m,
            Notice::Repeat { last, .. } => last,
        }
    }
}

/// Receives every notice watch mode prints (alert channels).
pub trait Notifier: Send {
    fn notify(&self, n: &Notice);
}

/// Live counters that remote commands can read.
#[derive(Default)]
pub struct WatchState {
    /// Rule names and queries, in priority order.
    pub rules: Vec<(String, String)>,
    pub matches: Mutex<HashMap<String, u64>>,
}

pub struct Watcher {
    rules: Vec<Rule>,
    names: Vec<String>,
    collapse: Collapser<Matched>,
    window: Duration,
    /// Replays run the collapse clock on record time, so a burst from
    /// last night collapses the same way it would have live.
    replay: bool,
    pub state: Arc<WatchState>,
}

impl Watcher {
    pub fn new(
        rules: Vec<Rule>,
        names: Vec<String>,
        window: Duration,
        replay: bool,
        state: Arc<WatchState>,
    ) -> Self {
        Self {
            rules,
            names,
            collapse: Collapser::new(window),
            window,
            replay,
            state,
        }
    }

    /// `wall` is the current time in micros; replays use the record's
    /// own timestamp when it has one.
    pub fn on_event(&mut self, ev: &Event, wall: i64) -> Vec<Notice> {
        let now = if self.replay && ev.ts_micros != TS_UNTIMED {
            ev.ts_micros
        } else {
            wall
        };
        let mut out = self.tick(now);
        let Some(rule) = self.rules.iter().find(|r| r.matches(ev)) else {
            return out;
        };
        *self
            .state
            .matches
            .lock()
            .unwrap()
            .entry(rule.name.clone())
            .or_default() += 1;
        let msg = sources::message_of(ev.fmt, &ev.line);
        let source = self.names.get(ev.source).cloned().unwrap_or_default();
        let key = format!(
            "{}\u{1f}{}\u{1f}{}",
            rule.name,
            source,
            util::normalize_message(&msg)
        );
        let m = Matched {
            rule: rule.name.clone(),
            source,
            ts_micros: ev.ts_micros,
            severity: ev.severity,
            msg,
            line: String::from_utf8_lossy(&ev.line).into_owned(),
            alert: rule.alert,
        };
        if self.collapse.offer(&key, m.clone(), now) == Verdict::Emit {
            out.push(Notice::Match(m));
        }
        out
    }

    pub fn tick(&mut self, now: i64) -> Vec<Notice> {
        let summaries = self.collapse.flush(now);
        self.repeats(summaries)
    }

    pub fn finish(&mut self) -> Vec<Notice> {
        let summaries = self.collapse.finish();
        self.repeats(summaries)
    }

    fn repeats(&self, s: Vec<crate::collapse::Summary<Matched>>) -> Vec<Notice> {
        s.into_iter()
            .map(|s| Notice::Repeat {
                first: s.sample,
                last: s.last,
                count: s.count,
                window: self.window,
            })
            .collect()
    }
}

/// One line per notice; newlines inside messages are flattened.
pub fn render_text(n: &Notice) -> String {
    let m = n.matched();
    let ts = util::format_ts(if m.ts_micros == TS_UNTIMED {
        util::now_micros()
    } else {
        m.ts_micros
    });
    let msg = one_line(&m.msg, 2000);
    match n {
        Notice::Match(_) => format!(
            "{ts} {:<5} {} [{}] {msg}",
            severity::name(m.severity),
            m.source,
            m.rule
        ),
        Notice::Repeat { count, window, .. } => format!(
            "{ts} {:<5} {} [{}] +{count} more in {}: {msg}",
            severity::name(m.severity),
            m.source,
            m.rule,
            util::format_duration(*window)
        ),
    }
}

pub fn render_json(n: &Notice) -> String {
    let m = n.matched();
    let ts = (m.ts_micros != TS_UNTIMED).then(|| util::format_ts(m.ts_micros));
    let (line, line_truncated) = util::truncate(&m.line, 4000);
    let v = match n {
        Notice::Match(_) => json!({
            "type": "match",
            "rule": m.rule,
            "source": m.source,
            "ts": ts,
            "level": severity::name(m.severity),
            "msg": m.msg,
            "line": line,
            "line_truncated": line_truncated,
        }),
        Notice::Repeat {
            first,
            count,
            window,
            ..
        } => json!({
            "type": "repeat",
            "rule": m.rule,
            "source": m.source,
            "count": count,
            "window_secs": window.as_secs(),
            "first_ts": (first.ts_micros != TS_UNTIMED).then(|| util::format_ts(first.ts_micros)),
            "last_ts": ts,
            "level": severity::name(m.severity),
            "msg": m.msg,
        }),
    };
    v.to_string()
}

pub fn one_line(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let (cut, more) = util::truncate(&flat, max);
    if more {
        format!("{cut}...")
    } else {
        cut.to_string()
    }
}

/// Everything watch mode needs, resolved from flags plus config.
pub struct Plan {
    pub specs: Vec<SourceSpec>,
    pub rules: Vec<Rule>,
    pub window: Duration,
    pub json: bool,
    pub feed: FeedOptions,
}

pub fn plan(args: &WatchArgs, cfg: &Config, cfg_dir: Option<&std::path::Path>) -> Result<Plan> {
    let log_format = args
        .log_format
        .as_deref()
        .map(crate::ingest::parse_format)
        .transpose()?;
    let mut specs = cfg.source_specs(cfg_dir)?;
    specs.extend(sources::specs_from_args(
        &args.sources,
        args.journal || !args.units.is_empty(),
        &args.units,
        log_format,
    ));
    sources::finalize_names(&mut specs)?;
    if specs.is_empty() {
        bail!("nothing to watch: pass files, `-`, --journal, or a config with [[source]]");
    }

    let mut rules = Vec::new();
    for r in &cfg.rule {
        rules.push(Rule::compile(&r.name, &r.query, r.alert)?);
    }
    for (i, q) in args.rules.iter().enumerate() {
        rules.push(Rule::compile(&format!("rule{}", i + 1), q, true)?);
    }
    if rules.is_empty() {
        rules.push(Rule::default_errors());
    }

    let window = match args.collapse.as_deref().or(cfg.collapse.as_deref()) {
        Some(s) => util::parse_duration(s).map_err(anyhow::Error::msg)?,
        None => Duration::from_secs(600),
    };
    let json = match args.format.as_deref().or(cfg.output.as_deref()) {
        None | Some("text") => false,
        Some("json") => true,
        Some(o) => bail!("unknown output format `{o}`; use text or json"),
    };
    let since = args
        .since
        .as_deref()
        .map(|s| util::parse_time_bound(s, util::now_micros()))
        .transpose()
        .map_err(anyhow::Error::msg)?;
    Ok(Plan {
        specs,
        rules,
        window,
        json,
        feed: FeedOptions {
            from_start: args.from_start || args.once,
            follow: !args.once,
            since,
        },
    })
}

pub fn load_config(path: Option<&PathBuf>) -> Result<(Config, Option<PathBuf>)> {
    match path {
        Some(p) => {
            let cfg = Config::load(p)?;
            let dir = p.parent().map(|d| d.to_path_buf());
            Ok((cfg, dir))
        }
        None => Ok((Config::default(), None)),
    }
}

pub fn run(args: WatchArgs) -> Result<()> {
    let (cfg, dir) = load_config(args.config.as_ref())?;
    let plan = plan(&args, &cfg, dir.as_deref())?;
    let state = Arc::new(WatchState::new(&plan.rules));
    let mut notifiers: Vec<Box<dyn Notifier>> = Vec::new();
    if let Some(tg) = cfg.telegram.as_ref().filter(|_| !args.no_telegram) {
        notifiers.push(crate::telegram::start(
            tg,
            plan.specs.clone(),
            state.clone(),
        )?);
    }
    run_plan(plan, state, notifiers)
}

pub fn run_plan(
    plan: Plan,
    state: Arc<WatchState>,
    notifiers: Vec<Box<dyn Notifier>>,
) -> Result<()> {
    let names: Vec<String> = plan.specs.iter().map(|s| s.name.clone()).collect();
    for r in &plan.rules {
        tracing::info!(rule = %r.name, query = %r.query, "watch rule");
    }
    let mut watcher = Watcher::new(plan.rules, names, plan.window, !plan.feed.follow, state);
    let feed = Feed::start(&plan.specs, plan.feed)?;
    let mut live = plan.specs.len();
    let stdout = std::io::stdout();
    let emit = |notices: Vec<Notice>| -> bool {
        let mut out = stdout.lock();
        for n in &notices {
            let line = if plan.json {
                render_json(n)
            } else {
                render_text(n)
            };
            if writeln!(out, "{line}").is_err() {
                return false;
            }
            if n.matched().alert {
                for nt in &notifiers {
                    nt.notify(n);
                }
            }
        }
        out.flush().is_ok()
    };
    while live > 0 {
        let notices = match feed.rx.recv_timeout(Duration::from_secs(1)) {
            Ok(FeedMsg::Event(ev)) => watcher.on_event(&ev, util::now_micros()),
            Ok(FeedMsg::Ended { source }) => {
                tracing::debug!(source = %plan.specs[source].name, "source ended");
                live -= 1;
                Vec::new()
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) if !watcher.replay => {
                watcher.tick(util::now_micros())
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Vec::new(),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        // A closed stdout (`| head`) ends the watch quietly.
        if !emit(notices) {
            return Ok(());
        }
    }
    emit(watcher.finish());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mgi_pulse_core::engine::format::LogFormat;

    fn ev(source: usize, line: &str) -> Event {
        let mut stats = Default::default();
        let (ts, sev) = LogFormat::Ndjson.parse_ts_level(line.as_bytes(), &mut stats, None);
        Event {
            source,
            fmt: LogFormat::Ndjson,
            ts_micros: ts,
            severity: sev,
            line: line.as_bytes().into(),
        }
    }

    fn watcher(rules: Vec<Rule>) -> Watcher {
        Watcher::new(
            rules,
            vec!["api".into(), "db".into()],
            Duration::from_secs(600),
            false,
            Arc::default(),
        )
    }

    #[test]
    fn default_rule_keeps_errors_only() {
        let mut w = watcher(vec![Rule::default_errors()]);
        let t = 1_000_000i64;
        assert!(w
            .on_event(&ev(0, r#"{"level":"info","msg":"ok"}"#), t)
            .is_empty());
        let n = w.on_event(&ev(0, r#"{"level":"error","msg":"boom"}"#), t);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].matched().msg, "boom");
        assert_eq!(n[0].matched().source, "api");
    }

    #[test]
    fn first_matching_rule_wins() {
        let rules = vec![
            Rule::compile("timeouts", "msg~/timeout/", true).unwrap(),
            Rule::compile("errors", "level=error", false).unwrap(),
        ];
        let mut w = watcher(rules);
        let n = w.on_event(
            &ev(1, r#"{"level":"error","msg":"timeout talking to db"}"#),
            1,
        );
        assert_eq!(n[0].matched().rule, "timeouts");
        assert!(n[0].matched().alert);
        assert_eq!(w.state.matches.lock().unwrap()["timeouts"], 1);
    }

    #[test]
    fn repeats_collapse_by_normalised_message() {
        let mut w = watcher(vec![Rule::default_errors()]);
        let t = 1_000_000i64;
        let a = r#"{"level":"error","msg":"retry 1 failed"}"#;
        let b = r#"{"level":"error","msg":"retry 2 failed"}"#;
        assert_eq!(w.on_event(&ev(0, a), t).len(), 1);
        assert!(w.on_event(&ev(0, b), t).is_empty());
        assert!(w.on_event(&ev(0, b), t).is_empty());
        // Same text on another source is its own stream.
        assert_eq!(w.on_event(&ev(1, a), t).len(), 1);
        let n = w.tick(t + 600_000_000);
        assert_eq!(n.len(), 1);
        match &n[0] {
            Notice::Repeat { count, last, .. } => {
                assert_eq!(*count, 2);
                assert_eq!(last.msg, "retry 2 failed");
            }
            other => panic!("expected repeat, got {other:?}"),
        }
    }

    #[test]
    fn replay_collapses_on_record_time() {
        let mut w = Watcher::new(
            vec![Rule::default_errors()],
            vec!["api".into()],
            Duration::from_secs(600),
            true,
            Arc::default(),
        );
        let a = r#"{"ts":"2026-06-01T12:00:00Z","level":"error","msg":"x"}"#;
        let b = r#"{"ts":"2026-06-01T12:05:00Z","level":"error","msg":"x"}"#;
        let c = r#"{"ts":"2026-06-01T12:30:00Z","level":"error","msg":"x"}"#;
        // Wall clock frozen: only record time can move the window.
        assert_eq!(w.on_event(&ev(0, a), 0).len(), 1);
        assert!(w.on_event(&ev(0, b), 0).is_empty());
        let n = w.on_event(&ev(0, c), 0);
        assert_eq!(n.len(), 2, "summary of the old window, then the new match");
        assert!(matches!(n[0], Notice::Repeat { count: 1, .. }));
        assert!(matches!(n[1], Notice::Match(_)));
    }

    #[test]
    fn bad_rule_is_a_clear_error() {
        let err = Rule::compile("x", "(level=error", true).err().unwrap();
        assert!(err.to_string().contains("rule `x`"));
    }

    #[test]
    fn renders_text_and_json() {
        let mut w = watcher(vec![Rule::default_errors()]);
        let n = w.on_event(
            &ev(
                0,
                r#"{"ts":"2026-06-01T12:00:00Z","level":"error","msg":"two\nlines"}"#,
            ),
            1,
        );
        assert_eq!(
            render_text(&n[0]),
            "2026-06-01T12:00:00.000Z error api [errors] two lines"
        );
        let v: serde_json::Value = serde_json::from_str(&render_json(&n[0])).unwrap();
        assert_eq!(v["type"], "match");
        assert_eq!(v["level"], "error");
        assert_eq!(v["ts"], "2026-06-01T12:00:00.000Z");
    }

    #[test]
    fn plan_merges_config_and_flags() {
        let cfg = Config::parse(
            "[[source]]\nname = \"api\"\npath = \"api.log\"\n[[rule]]\nname = \"e\"\nquery = \"level=error\"\n",
        )
        .unwrap();
        let args = WatchArgs {
            sources: vec!["-".into()],
            journal: false,
            units: vec![],
            rules: vec!["msg~/x/".into()],
            collapse: Some("0".into()),
            format: Some("json".into()),
            log_format: None,
            config: None,
            from_start: false,
            once: true,
            since: None,
            no_telegram: false,
        };
        let p = plan(&args, &cfg, None).unwrap();
        assert_eq!(p.specs.len(), 2);
        assert_eq!(p.rules.len(), 2);
        assert_eq!(p.rules[1].name, "rule1");
        assert!(p.window.is_zero());
        assert!(p.json);
        assert!(!p.feed.follow);
        assert!(p.feed.from_start);
    }
}
