//! Telegram: watch alerts to chats, and read-only commands from an
//! allowlist, through the user's own bot.
//!
//! Plain HTTPS against the Bot API with long polling; no webhook, no
//! inbound port. The token comes from an environment variable or a
//! file, never from the config itself, and never reaches a log line:
//! request errors are rebuilt from the status code and Telegram's own
//! description, never from the URL that carries the token.
//!
//! Commands are default-deny. A message is answered only when its
//! sender is in `allowed_users` or its chat is in `allowed_chats`;
//! everything else is dropped without a reply. Commands read the
//! configured sources by name and cannot reach other paths or run
//! anything.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use mgi_pulse_core::engine::dsl;
use mgi_pulse_core::engine::predicate::SeverityInPredicate;
use mgi_pulse_core::engine::record::severity;
use serde_json::{json, Value};

use crate::config::TelegramConf;
use crate::inspect::{self, Hit, Window};
use crate::sources::{SourceKind, SourceSpec};
use crate::util;
use crate::watch::{Notice, Notifier, WatchState};

pub const DEFAULT_TOKEN_ENV: &str = "MGI_PULSE_TELEGRAM_TOKEN";
/// Replies longer than this go out as a .txt attachment.
const TEXT_MAX: usize = 3500;
/// Telegram's hard limit for one message.
const MESSAGE_MAX: usize = 4096;
const BATCH_WINDOW: Duration = Duration::from_secs(2);
const COMMANDS_PER_MINUTE: u32 = 20;

/// Bot token. `Debug` is redacted and there is no `Display`, so it
/// cannot end up in a log line by accident.
#[derive(Clone)]
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

impl Token {
    pub fn parse(raw: &str) -> Result<Token> {
        let t = raw.trim();
        let (id, secret) = t
            .split_once(':')
            .ok_or_else(|| anyhow!("telegram token does not look like `<digits>:<secret>`"))?;
        let ok = !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && secret.len() >= 30
            && secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !ok {
            bail!("telegram token does not look like `<digits>:<secret>` from @BotFather");
        }
        Ok(Token(t.to_string()))
    }

    fn redact(&self, s: &str) -> String {
        s.replace(&self.0, "***")
    }
}

pub fn load_token(conf: &TelegramConf) -> Result<Token> {
    if conf.token.is_some() {
        bail!(
            "[telegram] token must not be written into the config; \
             put it in ${DEFAULT_TOKEN_ENV} or in a file named by token_file"
        );
    }
    if let Some(path) = &conf.token_file {
        warn_if_shared(path);
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read telegram token_file {}", path.display()))?;
        return Token::parse(raw.lines().next().unwrap_or(""));
    }
    let var = conf.token_env.as_deref().unwrap_or(DEFAULT_TOKEN_ENV);
    let raw = std::env::var(var).map_err(|_| anyhow!("telegram: ${var} is not set"))?;
    Token::parse(&raw)
}

#[cfg(unix)]
fn warn_if_shared(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = std::fs::metadata(path) {
        if m.permissions().mode() & 0o077 != 0 {
            eprintln!(
                "mgi-pulse: warning: {} is readable by other users; chmod 600 it",
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_shared(_path: &std::path::Path) {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub msg: String,
    pub retry_after: Option<u64>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

pub trait Api: Send + Sync {
    fn call(&self, method: &str, body: &Value) -> Result<Value, ApiError>;
    fn send_document(
        &self,
        chat: i64,
        name: &str,
        body: &[u8],
        caption: &str,
    ) -> Result<(), ApiError>;
}

pub struct HttpApi {
    agent: ureq::Agent,
    base: String,
    token: Token,
}

impl HttpApi {
    pub fn new(base: Option<&str>, token: Token) -> Result<HttpApi> {
        let base = base
            .unwrap_or("https://api.telegram.org")
            .trim_end_matches('/')
            .to_string();
        // The token travels in the URL path: never over plain HTTP to
        // anything but this machine.
        let local = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
            .iter()
            .any(|p| base.starts_with(p));
        if !base.starts_with("https://") && !local {
            bail!("telegram api_base must be https:// (plain http only for localhost)");
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(50))
            .timeout_write(Duration::from_secs(30))
            .build();
        Ok(HttpApi { agent, base, token })
    }

    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.base, self.token.0)
    }

    fn finish(&self, r: Result<ureq::Response, ureq::Error>) -> Result<Value, ApiError> {
        let (status, body) = match r {
            Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
            // Transport errors print the URL; keep only the kind.
            Err(ureq::Error::Transport(t)) => {
                return Err(ApiError {
                    msg: format!("telegram unreachable ({:?})", t.kind()),
                    retry_after: None,
                })
            }
        };
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if v["ok"] == Value::Bool(true) {
            return Ok(v["result"].clone());
        }
        let desc = v["description"].as_str().unwrap_or("no description");
        Err(ApiError {
            msg: self
                .token
                .redact(&format!("telegram HTTP {status}: {desc}")),
            retry_after: v["parameters"]["retry_after"].as_u64(),
        })
    }
}

impl Api for HttpApi {
    fn call(&self, method: &str, body: &Value) -> Result<Value, ApiError> {
        let r = self
            .agent
            .post(&self.url(method))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        self.finish(r)
    }

    fn send_document(
        &self,
        chat: i64,
        name: &str,
        body: &[u8],
        caption: &str,
    ) -> Result<(), ApiError> {
        let boundary = format!("mgi-pulse-{:x}", util::now_micros());
        let data = multipart(&boundary, chat, name, body, caption);
        let r = self
            .agent
            .post(&self.url("sendDocument"))
            .set(
                "Content-Type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .send_bytes(&data);
        self.finish(r).map(|_| ())
    }
}

fn multipart(boundary: &str, chat: i64, name: &str, body: &[u8], caption: &str) -> Vec<u8> {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    let mut d = Vec::with_capacity(body.len() + 512);
    for (field, value) in [
        ("chat_id", chat.to_string()),
        ("caption", caption.to_string()),
    ] {
        d.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    d.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"document\"; filename=\"{safe}\"\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
        )
        .as_bytes(),
    );
    d.extend_from_slice(body);
    d.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    d
}

/// Token bucket: `per_minute` sends, refilled continuously.
pub struct RateLimiter {
    cap: f64,
    tokens: f64,
    per_micro: f64,
    last: i64,
}

impl RateLimiter {
    pub fn new(per_minute: u32, now: i64) -> Self {
        let cap = per_minute.max(1) as f64;
        Self {
            cap,
            tokens: cap,
            per_micro: cap / 60_000_000.0,
            last: now,
        }
    }

    /// Take one token, or say how many micros until one is free.
    pub fn try_take(&mut self, now: i64) -> Result<(), i64> {
        let elapsed = now.saturating_sub(self.last).max(0) as f64;
        self.tokens = (self.tokens + elapsed * self.per_micro).min(self.cap);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - self.tokens) / self.per_micro).ceil() as i64)
        }
    }
}

/// Alert silences by rule name; `all` silences every rule.
#[derive(Default)]
pub struct Mutes(Mutex<HashMap<String, i64>>);

impl Mutes {
    pub fn mute(&self, rule: &str, until: i64) {
        self.0.lock().unwrap().insert(rule.to_string(), until);
    }

    /// Returns false when nothing was muted under that name.
    pub fn unmute(&self, rule: Option<&str>) -> bool {
        let mut m = self.0.lock().unwrap();
        match rule {
            None | Some("all") => {
                let had = !m.is_empty();
                m.clear();
                had
            }
            Some(r) => m.remove(r).is_some(),
        }
    }

    pub fn is_muted(&self, rule: &str, now: i64) -> bool {
        let m = self.0.lock().unwrap();
        [rule, "all"]
            .iter()
            .any(|k| m.get(*k).is_some_and(|&until| now < until))
    }

    pub fn until(&self, rule: &str, now: i64) -> Option<i64> {
        self.0
            .lock()
            .unwrap()
            .get(rule)
            .copied()
            .filter(|&u| now < u)
    }
}

/// Feeds watch notices to the alert thread without ever blocking the
/// watch loop: when the queue is full, the notice is counted as lost
/// and the count goes out with the next message.
pub struct AlertNotifier {
    tx: Option<Sender<Notice>>,
    lost: Arc<AtomicU64>,
}

impl Notifier for AlertNotifier {
    fn notify(&self, n: &Notice) {
        if let Some(tx) = &self.tx {
            if tx.try_send(n.clone()).is_err() {
                self.lost.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

pub fn format_alerts(host: &str, batch: &[Notice], lost: u64) -> Vec<String> {
    let mut lines = Vec::with_capacity(batch.len() + 2);
    let head = match batch.len() {
        1 => format!("mgi-pulse on {host}"),
        n => format!("mgi-pulse on {host}: {n} alerts"),
    };
    lines.push(head);
    for n in batch {
        let m = n.matched();
        let msg = crate::watch::one_line(&m.msg, 600);
        lines.push(match n {
            Notice::Match(_) => format!(
                "{} {} [{}] {msg}",
                severity::name(m.severity),
                m.source,
                m.rule
            ),
            Notice::Repeat { count, window, .. } => format!(
                "{} {} [{}] +{count} more in {}: {msg}",
                severity::name(m.severity),
                m.source,
                m.rule,
                util::format_duration(*window)
            ),
        });
    }
    if lost > 0 {
        lines.push(format!("({lost} alerts dropped: queue full)"));
    }
    chunk_lines(&lines, MESSAGE_MAX - 96)
}

/// Pack lines into messages no longer than `max` bytes.
fn chunk_lines(lines: &[String], max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for l in lines {
        let (l, _) = util::truncate(l, max);
        if !cur.is_empty() && cur.len() + 1 + l.len() > max {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(l);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn send_text(api: &dyn Api, chat: i64, text: &str) -> Result<(), ApiError> {
    let body = json!({"chat_id": chat, "text": text, "disable_web_page_preview": true});
    match api.call("sendMessage", &body) {
        Err(ApiError {
            retry_after: Some(s),
            ..
        }) => {
            std::thread::sleep(Duration::from_secs(s.min(60)));
            api.call("sendMessage", &body).map(|_| ())
        }
        r => r.map(|_| ()),
    }
}

struct AlertLoop {
    api: Arc<dyn Api>,
    chats: Vec<i64>,
    mutes: Arc<Mutes>,
    per_minute: u32,
    lost: Arc<AtomicU64>,
    host: String,
}

impl AlertLoop {
    fn run(self, rx: Receiver<Notice>) {
        let mut limiters: HashMap<i64, RateLimiter> = HashMap::new();
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            let deadline = Instant::now() + BATCH_WINDOW;
            while batch.len() < 50 {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(n) => batch.push(n),
                    Err(_) => break,
                }
            }
            let now = util::now_micros();
            batch.retain(|n| !self.mutes.is_muted(&n.matched().rule, now));
            let lost = self.lost.swap(0, Ordering::Relaxed);
            if batch.is_empty() && lost == 0 {
                continue;
            }
            for text in format_alerts(&self.host, &batch, lost) {
                for &chat in &self.chats {
                    let lim = limiters
                        .entry(chat)
                        .or_insert_with(|| RateLimiter::new(self.per_minute, now));
                    // Blocking here is fine: new notices queue (or are
                    // counted as lost) while this thread waits.
                    while let Err(wait) = lim.try_take(util::now_micros()) {
                        std::thread::sleep(Duration::from_micros(wait as u64));
                    }
                    if let Err(e) = send_text(self.api.as_ref(), chat, &text) {
                        tracing::warn!(chat, error = %e, "telegram alert not delivered");
                        eprintln!("mgi-pulse: telegram alert to chat {chat} failed: {e}");
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Help,
    Tail {
        n: usize,
        source: Option<String>,
    },
    Errors {
        since: Duration,
        source: Option<String>,
    },
    Grep {
        query: String,
    },
    Stats {
        since: Duration,
    },
    Sources,
    Rules,
    Mute {
        rule: String,
        dur: Duration,
    },
    Unmute {
        rule: Option<String>,
    },
}

const HELP: &str = "\
mgi-pulse bot. Read-only.
/tail [n] [source] - last lines, n up to 200
/errors [age] [source] - errors and fatals, default age 1h
/grep <query> - query DSL over every source, newest 50
/stats [age] - levels, top loggers and messages, default 1h
/sources - configured sources
/rules - watch rules and mutes
/mute <rule|all> [duration] - silence alerts, default 1h
/unmute [rule|all]";

/// `Ok(None)` for text that is not a command (or is addressed to a
/// different bot in a group).
pub fn parse_command(text: &str, bot: Option<&str>) -> Result<Option<Cmd>, String> {
    let text = text.trim();
    let Some(rest) = text.strip_prefix('/') else {
        return Ok(None);
    };
    let (word, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim();
    let (name, target) = word.split_once('@').unwrap_or((word, ""));
    if !target.is_empty() && bot.is_some_and(|b| !b.eq_ignore_ascii_case(target)) {
        return Ok(None);
    }
    let toks: Vec<&str> = args.split_whitespace().collect();
    let dur = |s: &str| util::parse_duration(s).map_err(|e| e.to_string());
    let cmd = match name.to_ascii_lowercase().as_str() {
        "start" | "help" => Cmd::Help,
        "tail" => {
            let (mut n, mut source) = (20usize, None);
            for t in toks.iter().take(2) {
                match t.parse::<usize>() {
                    Ok(v) => n = v,
                    Err(_) => source = Some(t.to_string()),
                }
            }
            Cmd::Tail {
                n: n.clamp(1, 200),
                source,
            }
        }
        "errors" => {
            let (mut since, mut source) = (Duration::from_secs(3600), None);
            for t in toks.iter().take(2) {
                match util::parse_duration(t) {
                    Ok(d) => since = d,
                    Err(_) => source = Some(t.to_string()),
                }
            }
            Cmd::Errors { since, source }
        }
        "grep" => {
            if args.is_empty() {
                return Err(
                    "usage: /grep <query>, e.g. /grep level=error AND msg~/timeout/".into(),
                );
            }
            Cmd::Grep {
                query: args.to_string(),
            }
        }
        "stats" => Cmd::Stats {
            since: match toks.first() {
                Some(t) => dur(t)?,
                None => Duration::from_secs(3600),
            },
        },
        "sources" => Cmd::Sources,
        "rules" => Cmd::Rules,
        "mute" => {
            let rule = toks
                .first()
                .ok_or("usage: /mute <rule|all> [duration]")?
                .to_string();
            let dur = match toks.get(1) {
                Some(t) => dur(t)?,
                None => Duration::from_secs(3600),
            };
            Cmd::Mute { rule, dur }
        }
        "unmute" => Cmd::Unmute {
            rule: toks.first().map(|s| s.to_string()),
        },
        other => return Err(format!("unknown command /{other}; /help lists them")),
    };
    Ok(Some(cmd))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Text {
        chat: i64,
        text: String,
    },
    File {
        chat: i64,
        name: String,
        body: Vec<u8>,
        caption: String,
    },
}

struct Output {
    text: String,
    /// Full content as an attachment, when the text is only a summary.
    file: Option<String>,
}

impl Output {
    fn text(text: String) -> Self {
        Output { text, file: None }
    }
}

pub struct Bot {
    pub api: Arc<dyn Api>,
    pub username: Option<String>,
    pub allowed_users: Vec<i64>,
    pub allowed_chats: Vec<i64>,
    pub specs: Vec<SourceSpec>,
    pub state: Arc<WatchState>,
    pub mutes: Arc<Mutes>,
    limits: Mutex<HashMap<i64, RateLimiter>>,
}

impl Bot {
    pub fn new(
        api: Arc<dyn Api>,
        conf: &TelegramConf,
        specs: Vec<SourceSpec>,
        state: Arc<WatchState>,
        mutes: Arc<Mutes>,
    ) -> Bot {
        Bot {
            api,
            username: None,
            allowed_users: conf.allowed_users.clone(),
            allowed_chats: conf.allowed_chats.clone(),
            specs,
            state,
            mutes,
            limits: Mutex::default(),
        }
    }

    fn allowed(&self, chat: i64, user: Option<i64>) -> bool {
        user.is_some_and(|u| self.allowed_users.contains(&u)) || self.allowed_chats.contains(&chat)
    }

    pub fn handle_update(&self, u: &Value, now: i64) -> Vec<Reply> {
        let msg = &u["message"];
        let (Some(chat), Some(text)) = (msg["chat"]["id"].as_i64(), msg["text"].as_str()) else {
            return Vec::new();
        };
        let user = msg["from"]["id"].as_i64();
        if !self.allowed(chat, user) {
            tracing::debug!(
                chat,
                user,
                "telegram: message from a sender not on the allowlist"
            );
            return Vec::new();
        }
        let cmd = match parse_command(text, self.username.as_deref()) {
            Ok(Some(c)) => c,
            Ok(None) => return Vec::new(),
            Err(e) => return vec![Reply::Text { chat, text: e }],
        };
        let sender = user.unwrap_or(chat);
        let allowed = self
            .limits
            .lock()
            .unwrap()
            .entry(sender)
            .or_insert_with(|| RateLimiter::new(COMMANDS_PER_MINUTE, now))
            .try_take(now)
            .is_ok();
        if !allowed {
            return Vec::new();
        }
        let out = match self.execute(&cmd, now) {
            Ok(o) => o,
            Err(e) => Output::text(format!("error: {e}")),
        };
        vec![reply_for(chat, out)]
    }

    fn readable(&self, name: Option<&str>) -> Result<Vec<&SourceSpec>> {
        match name {
            Some(n) => match self.specs.iter().find(|s| s.name == n) {
                Some(s) => Ok(vec![s]),
                None => bail!("no source named `{n}`; /sources lists them"),
            },
            None => {
                let v: Vec<_> = self
                    .specs
                    .iter()
                    .filter(|s| s.kind != SourceKind::Stdin)
                    .collect();
                if v.is_empty() {
                    bail!("no re-readable sources (stdin cannot be read again)");
                }
                Ok(v)
            }
        }
    }

    fn execute(&self, cmd: &Cmd, now: i64) -> Result<Output> {
        match cmd {
            Cmd::Help => Ok(Output::text(HELP.into())),
            Cmd::Sources => Ok(Output::text(
                self.specs
                    .iter()
                    .map(|s| format!("{}: {}", s.name, s.describe()))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            Cmd::Rules => {
                let counts = self.state.matches.lock().unwrap().clone();
                let mut lines: Vec<String> = self
                    .state
                    .rules
                    .iter()
                    .map(|(name, query)| {
                        let mut l = format!(
                            "{name}: {query} ({} matches)",
                            counts.get(name).copied().unwrap_or(0)
                        );
                        if let Some(u) = self.mutes.until(name, now) {
                            l.push_str(&format!(", muted until {}", util::format_ts(u)));
                        }
                        l
                    })
                    .collect();
                if let Some(u) = self.mutes.until("all", now) {
                    lines.push(format!("all alerts muted until {}", util::format_ts(u)));
                }
                Ok(Output::text(lines.join("\n")))
            }
            Cmd::Mute { rule, dur } => {
                if rule != "all" && !self.state.rules.iter().any(|(n, _)| n == rule) {
                    bail!("no rule named `{rule}`; /rules lists them");
                }
                let until = now + dur.as_micros() as i64;
                self.mutes.mute(rule, until);
                Ok(Output::text(format!(
                    "{rule} muted for {}",
                    util::format_duration(*dur)
                )))
            }
            Cmd::Unmute { rule } => Ok(Output::text(if self.mutes.unmute(rule.as_deref()) {
                "unmuted".into()
            } else {
                "nothing was muted".into()
            })),
            Cmd::Tail { n, source } => {
                let specs = self.readable(source.as_deref())?;
                let spec = specs[0];
                let hits = inspect::tail(spec, *n)?;
                Ok(hits_output(
                    &format!("last {} of {}", hits.len(), spec.name),
                    &hits,
                    hits.len(),
                ))
            }
            Cmd::Errors { since, source } => {
                let w = Window {
                    since: Some(now - since.as_micros() as i64),
                    until: None,
                };
                let pred = SeverityInPredicate::new(&[severity::ERROR, severity::FATAL]);
                let (hits, total) = self.search_all(source.as_deref(), Some(&pred), &w, 50)?;
                Ok(hits_output(
                    &format!(
                        "{total} {} in the last {}",
                        plural(total, "error"),
                        util::format_duration(*since)
                    ),
                    &hits,
                    total,
                ))
            }
            Cmd::Grep { query } => {
                let pred = dsl::compile(query).map_err(|e| anyhow!("query: {e}"))?;
                let (hits, total) =
                    self.search_all(None, Some(pred.as_ref()), &Window::default(), 50)?;
                Ok(hits_output(
                    &format!("{total} {}", plural(total, "match")),
                    &hits,
                    total,
                ))
            }
            Cmd::Stats { since } => {
                let w = Window {
                    since: Some(now - since.as_micros() as i64),
                    until: None,
                };
                let mut out = vec![format!("last {}", util::format_duration(*since))];
                for spec in self.readable(None)? {
                    let s = inspect::stats(spec, &w, 3)?;
                    let levels: Vec<String> =
                        s.by_level.iter().map(|(l, c)| format!("{l} {c}")).collect();
                    out.push(format!(
                        "{}: {} records, {}",
                        s.source,
                        s.total,
                        levels.join(", ")
                    ));
                    for (m, c) in &s.top_messages {
                        out.push(format!("  {c}x {m}"));
                    }
                }
                let counts = self.state.matches.lock().unwrap();
                for (name, _) in &self.state.rules {
                    out.push(format!(
                        "rule {name}: {} matches since start",
                        counts.get(name).copied().unwrap_or(0)
                    ));
                }
                Ok(Output::text(out.join("\n")))
            }
        }
    }

    fn search_all(
        &self,
        source: Option<&str>,
        pred: Option<&dyn mgi_pulse_core::engine::predicate::Predicate>,
        w: &Window,
        limit: usize,
    ) -> Result<(Vec<Hit>, usize)> {
        let mut hits = Vec::new();
        let mut total = 0;
        for spec in self.readable(source)? {
            let f = inspect::search(spec, pred, w, limit)?;
            total += f.total;
            hits.extend(f.hits);
        }
        hits.sort_by_key(|h| h.ts_micros);
        let skip = hits.len().saturating_sub(limit);
        Ok((hits.split_off(skip), total))
    }
}

fn plural(n: usize, word: &str) -> String {
    match (n, word) {
        (1, w) => w.to_string(),
        (_, w) if w.ends_with("ch") => format!("{w}es"),
        (_, w) => format!("{w}s"),
    }
}

fn hits_output(head: &str, hits: &[Hit], total: usize) -> Output {
    let mut text = head.to_string();
    if total > hits.len() {
        text.push_str(&format!(", newest {} shown", hits.len()));
    }
    let lines: Vec<String> = hits.iter().map(Hit::short).collect();
    let body = lines.join("\n");
    if text.len() + body.len() + 1 > TEXT_MAX {
        let full: Vec<&str> = hits.iter().map(|h| h.line.as_str()).collect();
        return Output {
            text,
            file: Some(full.join("\n") + "\n"),
        };
    }
    if !body.is_empty() {
        text.push('\n');
        text.push_str(&body);
    }
    Output::text(text)
}

fn reply_for(chat: i64, out: Output) -> Reply {
    match out.file {
        Some(body) => Reply::File {
            chat,
            name: "mgi-pulse.txt".into(),
            body: body.into_bytes(),
            caption: util::truncate(&out.text, 1000).0.to_string(),
        },
        None if out.text.len() > TEXT_MAX => Reply::File {
            chat,
            name: "mgi-pulse.txt".into(),
            caption: out
                .text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect(),
            body: out.text.into_bytes(),
        },
        None => Reply::Text {
            chat,
            text: out.text,
        },
    }
}

fn deliver(api: &dyn Api, r: &Reply) {
    let res = match r {
        Reply::Text { chat, text } => send_text(api, *chat, text),
        Reply::File {
            chat,
            name,
            body,
            caption,
        } => api.send_document(*chat, name, body, caption),
    };
    if let Err(e) = res {
        tracing::warn!(error = %e, "telegram reply not delivered");
    }
}

fn poll_loop(bot: Arc<Bot>) {
    let api = bot.api.clone();
    // Commands queued while pulse was down are stale: skip them.
    let mut offset = api
        .call("getUpdates", &json!({"offset": -1, "timeout": 0}))
        .ok()
        .and_then(|v| v.as_array().and_then(|a| a.last()).cloned())
        .and_then(|u| u["update_id"].as_i64())
        .map_or(0, |id| id + 1);
    let mut backoff = 1u64;
    loop {
        let body = json!({"offset": offset, "timeout": 30, "allowed_updates": ["message"]});
        match api.call("getUpdates", &body) {
            Ok(v) => {
                backoff = 1;
                for u in v.as_array().into_iter().flatten() {
                    if let Some(id) = u["update_id"].as_i64() {
                        offset = offset.max(id + 1);
                    }
                    for r in bot.handle_update(u, util::now_micros()) {
                        deliver(api.as_ref(), &r);
                    }
                }
            }
            Err(e) => {
                eprintln!("mgi-pulse: telegram polling: {e}");
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

pub fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this host".into())
}

/// Validate settings, check the token with getMe, start the alert and
/// command threads. The returned notifier goes into the watch loop.
pub fn start(
    conf: &TelegramConf,
    specs: Vec<SourceSpec>,
    state: Arc<WatchState>,
) -> Result<Box<dyn Notifier>> {
    let has_allowlist = !conf.allowed_users.is_empty() || !conf.allowed_chats.is_empty();
    let commands = match conf.commands {
        Some(true) if !has_allowlist => {
            bail!("[telegram] commands = true needs allowed_users or allowed_chats")
        }
        Some(c) => c,
        None => has_allowlist,
    };
    if conf.alert_chats.is_empty() && !commands {
        bail!("[telegram] has nothing to do: set alert_chats, or allowed_users for commands");
    }
    let token = load_token(conf)?;
    let api: Arc<dyn Api> = Arc::new(HttpApi::new(conf.api_base.as_deref(), token)?);
    let me = api
        .call("getMe", &json!({}))
        .map_err(|e| anyhow!("telegram: token check failed: {e}"))?;
    let username = me["username"].as_str().map(String::from);
    let mutes = Arc::new(Mutes::default());

    let lost = Arc::new(AtomicU64::new(0));
    let tx = if conf.alert_chats.is_empty() {
        None
    } else {
        let (tx, rx) = crossbeam_channel::bounded(1000);
        let alert = AlertLoop {
            api: api.clone(),
            chats: conf.alert_chats.clone(),
            mutes: mutes.clone(),
            per_minute: conf.max_per_minute.unwrap_or(20),
            lost: lost.clone(),
            host: host_name(),
        };
        std::thread::Builder::new()
            .name("mgi-pulse-tg-alerts".into())
            .spawn(move || alert.run(rx))?;
        Some(tx)
    };

    if commands {
        let mut bot = Bot::new(api.clone(), conf, specs, state, mutes);
        bot.username = username.clone();
        let bot = Arc::new(bot);
        std::thread::Builder::new()
            .name("mgi-pulse-tg-commands".into())
            .spawn(move || poll_loop(bot))?;
    }
    eprintln!(
        "mgi-pulse: telegram bot @{} ready (alerts to {} {}, commands {})",
        username.as_deref().unwrap_or("?"),
        conf.alert_chats.len(),
        plural(conf.alert_chats.len(), "chat"),
        if commands { "on" } else { "off" }
    );
    Ok(Box::new(AlertNotifier { tx, lost }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::Matched;
    use std::io::{BufRead, BufReader, Read, Write};

    const TOKEN: &str = "123456:AAbbCCddEEffGGhhIIjjKKllMMnnOOppQQ";

    #[derive(Default)]
    struct FakeApi {
        calls: Mutex<Vec<(String, Value)>>,
        docs: Mutex<Vec<(i64, String, Vec<u8>)>>,
    }

    impl Api for FakeApi {
        fn call(&self, method: &str, body: &Value) -> Result<Value, ApiError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), body.clone()));
            Ok(json!(true))
        }
        fn send_document(
            &self,
            chat: i64,
            name: &str,
            body: &[u8],
            _caption: &str,
        ) -> Result<(), ApiError> {
            self.docs
                .lock()
                .unwrap()
                .push((chat, name.to_string(), body.to_vec()));
            Ok(())
        }
    }

    fn bot_with(spec: SourceSpec) -> Bot {
        let conf = TelegramConf {
            allowed_users: vec![42],
            ..Default::default()
        };
        let state = Arc::new(WatchState {
            rules: vec![("errors".into(), "level=error".into())],
            ..Default::default()
        });
        let mut b = Bot::new(
            Arc::new(FakeApi::default()),
            &conf,
            vec![spec],
            state,
            Arc::new(Mutes::default()),
        );
        b.username = Some("pulse_bot".into());
        b
    }

    fn update(user: i64, chat: i64, text: &str) -> Value {
        json!({"update_id": 1, "message": {"chat": {"id": chat}, "from": {"id": user}, "text": text}})
    }

    fn reply_text(r: &[Reply]) -> &str {
        match r {
            [Reply::Text { text, .. }] => text,
            other => panic!("expected one text reply, got {other:?}"),
        }
    }

    #[test]
    fn parses_commands() {
        let p = |t| parse_command(t, Some("pulse_bot"));
        assert_eq!(p("hello").unwrap(), None);
        assert_eq!(p("/help").unwrap(), Some(Cmd::Help));
        assert_eq!(
            p("/tail 50 api").unwrap(),
            Some(Cmd::Tail {
                n: 50,
                source: Some("api".into())
            })
        );
        assert_eq!(
            p("/tail api").unwrap(),
            Some(Cmd::Tail {
                n: 20,
                source: Some("api".into())
            })
        );
        assert_eq!(
            p("/tail 99999").unwrap(),
            Some(Cmd::Tail {
                n: 200,
                source: None
            })
        );
        assert_eq!(
            p("/errors 2h@x").unwrap(),
            Some(Cmd::Errors {
                since: Duration::from_secs(3600),
                source: Some("2h@x".into())
            })
        );
        assert_eq!(
            p("/errors@pulse_bot 15m").unwrap(),
            Some(Cmd::Errors {
                since: Duration::from_secs(900),
                source: None
            })
        );
        assert_eq!(p("/errors@other_bot").unwrap(), None);
        assert_eq!(
            p("/grep level=error AND msg~/a b/").unwrap(),
            Some(Cmd::Grep {
                query: "level=error AND msg~/a b/".into()
            })
        );
        assert!(p("/grep").is_err());
        assert_eq!(
            p("/mute errors 30m").unwrap(),
            Some(Cmd::Mute {
                rule: "errors".into(),
                dur: Duration::from_secs(1800)
            })
        );
        assert!(p("/mute").is_err());
        assert!(p("/mute errors soon").is_err());
        assert!(p("/rm -rf").unwrap_err().contains("unknown command"));
    }

    #[test]
    fn strangers_get_no_reply() {
        let f = inspect::tests::fixture("tg-deny", inspect::tests::LOG);
        let bot = bot_with(f.spec.clone());
        assert!(bot.handle_update(&update(7, 7, "/tail"), 0).is_empty());
        assert!(bot.handle_update(&update(7, 7, "/help"), 0).is_empty());
        assert!(!bot.handle_update(&update(42, 99, "/help"), 0).is_empty());
    }

    #[test]
    fn allowed_chat_admits_its_members() {
        let f = inspect::tests::fixture("tg-chat", inspect::tests::LOG);
        let mut bot = bot_with(f.spec.clone());
        bot.allowed_chats = vec![-100];
        assert!(!bot.handle_update(&update(7, -100, "/help"), 0).is_empty());
        assert!(bot.handle_update(&update(7, -200, "/help"), 0).is_empty());
    }

    #[test]
    fn tail_and_errors_read_the_named_source() {
        let f = inspect::tests::fixture("tg-tail", inspect::tests::LOG);
        let bot = bot_with(f.spec.clone());
        let r = bot.handle_update(&update(42, 42, "/tail 2"), 0);
        let t = reply_text(&r);
        assert!(t.starts_with("last 2 of app"), "{t}");
        assert!(t.contains("slow") && t.contains("done"));

        let r = bot.handle_update(&update(42, 42, "/tail 2 /etc/passwd"), 0);
        assert!(reply_text(&r).contains("no source named"));

        // The fixture is from 2026-06-01; ask from one minute later.
        let now = util::parse_time_bound("2026-06-01T12:01:00Z", 0).unwrap();
        let r = bot.handle_update(&update(42, 42, "/errors 1h"), now);
        let t = reply_text(&r);
        assert!(t.starts_with("2 errors in the last 1h"), "{t}");
    }

    #[test]
    fn long_output_goes_out_as_a_file() {
        let mut body = String::new();
        for i in 0..300 {
            body.push_str(&format!(
                "{{\"ts\":\"2026-06-01T12:00:00Z\",\"level\":\"error\",\"msg\":\"failure number {i} with a long tail of words\"}}\n"
            ));
        }
        let f = inspect::tests::fixture("tg-long", &body);
        let bot = bot_with(f.spec.clone());
        let r = bot.handle_update(&update(42, 42, "/grep level=error"), 0);
        match &r[..] {
            [Reply::File { caption, body, .. }] => {
                assert!(caption.starts_with("300 matches, newest 50 shown"));
                assert_eq!(String::from_utf8_lossy(body).lines().count(), 50);
            }
            other => panic!("expected a file, got {other:?}"),
        }
    }

    #[test]
    fn mute_needs_a_known_rule_and_expires() {
        let f = inspect::tests::fixture("tg-mute", inspect::tests::LOG);
        let bot = bot_with(f.spec.clone());
        let r = bot.handle_update(&update(42, 42, "/mute nope"), 0);
        assert!(reply_text(&r).contains("no rule named"));
        bot.handle_update(&update(42, 42, "/mute errors 10m"), 0);
        assert!(bot.mutes.is_muted("errors", 1));
        assert!(!bot.mutes.is_muted("errors", 600_000_000));
        bot.handle_update(&update(42, 42, "/mute all 1h"), 0);
        assert!(bot.mutes.is_muted("anything", 1));
        bot.handle_update(&update(42, 42, "/unmute"), 0);
        assert!(!bot.mutes.is_muted("errors", 1));
    }

    #[test]
    fn command_flood_is_rate_limited() {
        let f = inspect::tests::fixture("tg-flood", inspect::tests::LOG);
        let bot = bot_with(f.spec.clone());
        let answered = (0..30)
            .filter(|_| !bot.handle_update(&update(42, 42, "/help"), 0).is_empty())
            .count();
        assert_eq!(answered, COMMANDS_PER_MINUTE as usize);
    }

    #[test]
    fn rate_limiter_refills_over_time() {
        let mut l = RateLimiter::new(2, 0);
        assert!(l.try_take(0).is_ok());
        assert!(l.try_take(0).is_ok());
        let wait = l.try_take(0).unwrap_err();
        assert_eq!(wait, 30_000_000);
        assert!(l.try_take(30_000_000).is_ok());
    }

    #[test]
    fn alerts_are_batched_and_chunked() {
        let m = Matched {
            rule: "errors".into(),
            source: "api".into(),
            ts_micros: 0,
            severity: severity::ERROR,
            msg: "x".repeat(500),
            line: String::new(),
            alert: true,
        };
        let batch: Vec<Notice> = (0..20).map(|_| Notice::Match(m.clone())).collect();
        let msgs = format_alerts("box", &batch, 3);
        assert!(msgs.len() > 1);
        assert!(msgs.iter().all(|t| t.len() <= MESSAGE_MAX));
        assert!(msgs[0].starts_with("mgi-pulse on box: 20 alerts"));
        assert!(msgs
            .last()
            .unwrap()
            .ends_with("(3 alerts dropped: queue full)"));
    }

    #[test]
    fn token_is_never_in_config_or_debug() {
        let conf = TelegramConf {
            token: Some(TOKEN.into()),
            ..Default::default()
        };
        let err = load_token(&conf).unwrap_err().to_string();
        assert!(!err.contains(TOKEN));
        let t = Token::parse(TOKEN).unwrap();
        assert_eq!(format!("{t:?}"), "Token(***)");
        assert!(Token::parse("not-a-token").is_err());
        assert!(Token::parse("123:short").is_err());
    }

    #[test]
    fn plain_http_is_refused_for_remote_hosts() {
        let t = Token::parse(TOKEN).unwrap();
        assert!(HttpApi::new(Some("http://example.com"), t.clone()).is_err());
        assert!(HttpApi::new(Some("http://127.0.0.1:8081"), t).is_ok());
    }

    /// Serve canned HTTP responses on a local port; returns the base URL
    /// and a handle yielding the raw requests.
    fn mock_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in responses {
                let (mut s, _) = listener.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut head = String::new();
                let mut len = 0usize;
                loop {
                    let mut l = String::new();
                    r.read_line(&mut l).unwrap();
                    if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    head.push_str(&l);
                    if l == "\r\n" {
                        break;
                    }
                }
                let mut payload = vec![0u8; len];
                r.read_exact(&mut payload).unwrap();
                head.push_str(&String::from_utf8_lossy(&payload));
                seen.push(head);
                write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            seen
        });
        (base, h)
    }

    #[test]
    fn http_api_round_trip_and_redacted_errors() {
        let (base, h) = mock_server(vec![
            (200, r#"{"ok":true,"result":{"username":"pulse_bot"}}"#),
            (
                429,
                r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 7","parameters":{"retry_after":7}}"#,
            ),
            (200, r#"{"ok":true,"result":{}}"#),
        ]);
        let api = HttpApi::new(Some(&base), Token::parse(TOKEN).unwrap()).unwrap();
        let me = api.call("getMe", &json!({})).unwrap();
        assert_eq!(me["username"], "pulse_bot");
        let err = api.call("sendMessage", &json!({"chat_id": 1})).unwrap_err();
        assert_eq!(err.retry_after, Some(7));
        assert!(err.msg.contains("429"));
        assert!(!err.msg.contains(TOKEN));
        api.send_document(5, "../../etc/passwd", b"hello", "cap")
            .unwrap();
        let seen = h.join().unwrap();
        assert!(seen[0].starts_with(&format!("POST /bot{TOKEN}/getMe ")));
        assert!(seen[2].contains("filename=\"....etcpasswd\""));
        assert!(seen[2].contains("name=\"chat_id\"\r\n\r\n5\r\n"));
        assert!(seen[2].contains("hello"));
    }

    #[test]
    fn transport_errors_do_not_leak_the_url() {
        // Nothing listens on port 9 of localhost on CI machines.
        let api = HttpApi::new(Some("http://127.0.0.1:9"), Token::parse(TOKEN).unwrap()).unwrap();
        let err = api.call("getMe", &json!({})).unwrap_err();
        assert!(!err.msg.contains(TOKEN), "{}", err.msg);
        assert!(err.msg.starts_with("telegram unreachable"));
    }
}
