//! Optional LLM analysis through any OpenAI-compatible chat endpoint:
//! Ollama, LM Studio, llama.cpp server, vLLM, or a cloud provider.
//!
//! Off unless a model is configured. Before anything leaves the machine
//! the context is cut to a fixed character budget and passed through
//! [`redact`], which masks emails, credentials in `key=value` form,
//! bearer tokens, JWTs, private keys and well-known API key shapes.

use std::io::{BufRead, BufReader};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use mgi_pulse_core::engine::dsl;
use mgi_pulse_core::engine::predicate::{
    OrPredicate, Predicate, RegexBytesPredicate, SeverityInPredicate,
};
use mgi_pulse_core::engine::record::{severity, TS_UNTIMED};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::inspect::{self, Hit, Window};
use crate::sources::{SourceKind, SourceSpec};
use crate::util;

pub const URL_ENV: &str = "MGI_PULSE_LLM_URL";
pub const MODEL_ENV: &str = "MGI_PULSE_LLM_MODEL";
pub const KEY_ENV: &str = "MGI_PULSE_LLM_KEY";

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConf {
    /// Base URL of the OpenAI-compatible API, e.g.
    /// `http://localhost:11434/v1` for Ollama.
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Environment variable holding the API key. Default
    /// `MGI_PULSE_LLM_KEY`. Local servers usually need none.
    pub api_key_env: Option<String>,
    /// Characters of log context per request. Default 12000.
    pub max_context_chars: Option<usize>,
    /// Upper bound on the answer length. Default 800 tokens.
    pub max_answer_tokens: Option<u32>,
    /// Default 120 seconds.
    pub timeout_secs: Option<u64>,
    /// Mask secrets before sending. Default true.
    pub redact: Option<bool>,
    /// Add a short model-written summary to Telegram alert batches of
    /// three or more alerts. Default false.
    pub alert_summary: Option<bool>,
    /// Send an API key over plain http to a non-local host anyway.
    pub allow_insecure: Option<bool>,
}

pub struct Llm {
    agent: ureq::Agent,
    endpoint: String,
    pub model: String,
    key: Option<String>,
    pub max_context: usize,
    max_tokens: u32,
    pub redact: bool,
    pub alert_summary: bool,
}

impl std::fmt::Debug for Llm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Llm")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("key", &self.key.as_ref().map(|_| "***"))
            .finish()
    }
}

fn is_local(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', ':']).next().unwrap_or("");
    let host = if rest.starts_with('[') {
        rest.split(']').next().unwrap_or("").trim_start_matches('[')
    } else {
        host
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// `http://host:11434` gains `/v1`; anything with a path is kept.
fn endpoint_of(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let after_scheme = base.split_once("://").map_or(base, |(_, r)| r);
    if after_scheme.contains('/') {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

impl Llm {
    /// `Ok(None)` when no model is configured: the features stay off.
    /// Environment variables override the config file.
    pub fn from_config(conf: Option<&LlmConf>) -> Result<Option<Llm>> {
        let conf = conf.cloned().unwrap_or_default();
        let base = std::env::var(URL_ENV).ok().or(conf.base_url.clone());
        let model = std::env::var(MODEL_ENV).ok().or(conf.model.clone());
        let (base, model) = match (base, model) {
            (None, None) => return Ok(None),
            (Some(b), Some(m)) if !b.is_empty() && !m.is_empty() => (b, m),
            _ => bail!("LLM needs both a base URL and a model (${URL_ENV} and ${MODEL_ENV}, or [llm] base_url and model)"),
        };
        if !base.starts_with("http://") && !base.starts_with("https://") {
            bail!("LLM base URL must start with http:// or https://");
        }
        let key_var = conf.api_key_env.as_deref().unwrap_or(KEY_ENV);
        let key = std::env::var(key_var).ok().filter(|k| !k.is_empty());
        if key.is_some()
            && base.starts_with("http://")
            && !is_local(&base)
            && !conf.allow_insecure.unwrap_or(false)
        {
            bail!(
                "refusing to send the API key over plain http to {base}; use https, or set allow_insecure = true in [llm]"
            );
        }
        let timeout = Duration::from_secs(conf.timeout_secs.unwrap_or(120));
        Ok(Some(Llm {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(timeout)
                .build(),
            endpoint: endpoint_of(&base),
            model,
            key,
            max_context: conf
                .max_context_chars
                .unwrap_or(12_000)
                .clamp(1_000, 200_000),
            max_tokens: conf.max_answer_tokens.unwrap_or(800),
            redact: conf.redact.unwrap_or(true),
            alert_summary: conf.alert_summary.unwrap_or(false),
        }))
    }

    pub fn host(&self) -> &str {
        let rest = self
            .endpoint
            .split_once("://")
            .map_or(self.endpoint.as_str(), |(_, r)| r);
        rest.split('/').next().unwrap_or(rest)
    }

    /// One chat turn. Text is streamed to `on_token` as it arrives
    /// (reasoning inside `<think>` is dropped) and returned whole.
    pub fn chat(&self, system: &str, user: &str, on_token: &mut dyn FnMut(&str)) -> Result<String> {
        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "stream": true,
            "temperature": 0.2,
            "max_tokens": self.max_tokens,
        });
        let mut req = self
            .agent
            .post(&self.endpoint)
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream, application/json");
        if let Some(k) = &self.key {
            req = req.set("Authorization", &format!("Bearer {k}"));
        }
        let resp = match req.send_string(&body.to_string()) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                let msg = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| {
                        v["error"]["message"]
                            .as_str()
                            .or(v["error"].as_str())
                            .map(String::from)
                    })
                    .unwrap_or_else(|| util::truncate(&text, 200).0.to_string());
                bail!("model server {} answered HTTP {code}: {msg}", self.host());
            }
            Err(ureq::Error::Transport(t)) => {
                bail!(
                    "cannot reach the model server at {} ({:?})",
                    self.host(),
                    t.kind()
                )
            }
        };
        let mut out = String::new();
        let mut filter = ThinkFilter::default();
        let mut emit = |s: &str, out: &mut String| {
            if !s.is_empty() {
                on_token(s);
                out.push_str(s);
            }
        };
        let mut reader = BufReader::new(resp.into_reader());
        let mut first = String::new();
        reader.read_line(&mut first)?;
        if !first.trim_start().starts_with("data:") && !first.trim().is_empty() {
            // The server ignored `stream` and sent one JSON body.
            let mut rest = String::new();
            std::io::Read::read_to_string(&mut reader, &mut rest)?;
            let v: Value = serde_json::from_str(&(first + &rest))
                .map_err(|_| anyhow!("model server sent something that is not JSON"))?;
            let text = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
            let visible = filter.push(text) + &filter.finish();
            emit(&visible, &mut out);
            return Ok(out);
        }
        let mut line = first;
        loop {
            let l = line.trim();
            if let Some(data) = l.strip_prefix("data:") {
                let data = data.trim();
                if data == "[DONE]" {
                    break;
                }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if let Some(e) = v.get("error") {
                        bail!(
                            "model error: {}",
                            e["message"].as_str().unwrap_or("unknown")
                        );
                    }
                    if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                        let visible = filter.push(t);
                        emit(&visible, &mut out);
                    }
                }
            }
            line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
        }
        let tail = filter.finish();
        emit(&tail, &mut out);
        Ok(out)
    }
}

/// Drops `<think>...</think>` spans that reasoning models put before
/// the answer, also when the tags arrive split across stream chunks.
#[derive(Default)]
pub struct ThinkFilter {
    inside: bool,
    buf: String,
}

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

impl ThinkFilter {
    pub fn push(&mut self, s: &str) -> String {
        self.buf.push_str(s);
        let mut out = String::new();
        loop {
            if self.inside {
                match self.buf.find(CLOSE) {
                    Some(i) => {
                        self.buf.drain(..i + CLOSE.len());
                        self.inside = false;
                    }
                    None => {
                        let keep = partial_suffix(&self.buf, CLOSE);
                        self.buf.drain(..self.buf.len() - keep);
                        return out;
                    }
                }
            } else {
                match self.buf.find(OPEN) {
                    Some(i) => {
                        out.push_str(&self.buf[..i]);
                        self.buf.drain(..i + OPEN.len());
                        self.inside = true;
                    }
                    None => {
                        let keep = partial_suffix(&self.buf, OPEN);
                        let cut = self.buf.len() - keep;
                        out.push_str(&self.buf[..cut]);
                        self.buf.drain(..cut);
                        return out;
                    }
                }
            }
        }
    }

    pub fn finish(&mut self) -> String {
        if self.inside {
            self.buf.clear();
            return String::new();
        }
        std::mem::take(&mut self.buf)
    }
}

/// Length of the longest suffix of `s` that is a proper prefix of `tag`.
fn partial_suffix(s: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&n| {
            s.len() >= n && s.is_char_boundary(s.len() - n) && tag.starts_with(&s[s.len() - n..])
        })
        .unwrap_or(0)
}

/// Mask values that should not leave the machine.
pub fn redact(s: &str) -> String {
    static RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        [
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(-----END [A-Z ]*PRIVATE KEY-----|$)", "<private key>"),
            (r"\b[A-Za-z][A-Za-z0-9+.-]*://[^/\s:@]+:[^/\s@]+@", "<url with credentials>@"),
            (r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}", "Bearer <redacted>"),
            (r"(?i)\bbasic\s+[A-Za-z0-9+/=]{8,}", "Basic <redacted>"),
            (r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", "<jwt>"),
            (
                r"\b(sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}|glpat-[A-Za-z0-9_-]{20,}|\d{6,12}:[A-Za-z0-9_-]{30,})",
                "<secret>",
            ),
            (
                r#"(?i)\b((?:[a-z0-9]+[_-])?(?:password|passwd|pwd|pass|secret|token|api[_-]?key|apikey|access[_-]?key|private[_-]?key|client[_-]?secret|auth|authorization|cookie|session[_-]?id|credentials?))("?\s*[:=]\s*"?)([^\s",;&}]+)"#,
                "${1}${2}<redacted>",
            ),
            (r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "<email>"),
        ]
        .into_iter()
        .map(|(p, r)| (Regex::new(p).expect("redaction pattern"), r))
        .collect()
    });
    let mut out = s.to_string();
    for (re, rep) in rules {
        if !re.is_match(&out) {
            continue;
        }
        out = if rep.starts_with("${1}") {
            // key=value: leave values an earlier rule already masked,
            // and the scheme word of `Authorization: Bearer <redacted>`.
            re.replace_all(&out, |c: &regex::Captures| {
                let v = &c[3];
                if v.starts_with('<')
                    || v.eq_ignore_ascii_case("bearer")
                    || v.eq_ignore_ascii_case("basic")
                {
                    c[0].to_string()
                } else {
                    format!("{}{}<redacted>", &c[1], &c[2])
                }
            })
            .into_owned()
        } else {
            re.replace_all(&out, *rep).into_owned()
        };
    }
    out
}

pub const SYSTEM_ASK: &str = "You analyse application and system logs for an engineer. \
Use only the excerpts and counts given; they are a sample, not the whole log. \
Back each claim with the timestamp and source of the lines that support it. \
If the excerpts cannot answer the question, say what is missing and which query would find it. \
Values shown as <redacted>, <secret>, <email> or <jwt> were removed before sending. \
Answer in the language of the question, briefly.";

pub const SYSTEM_EXPLAIN: &str = "You explain a single log record to an engineer who is \
looking at it in a log viewer. Say what it means, the likely cause, and what to check next. \
Use the surrounding lines and the repeat count given. Values shown as <redacted>, <secret>, \
<email> or <jwt> were removed. Be brief: a few sentences or a short list.";

pub const SYSTEM_SUMMARY: &str = "Summarise a batch of log alerts in at most two short \
sentences for an on-call engineer: what is failing and whether it looks like one incident. \
No preamble.";

/// One line of context: `ts level source [fields] msg (xN)`.
fn hit_line(h: &Hit, count: usize) -> String {
    let ts = if h.ts_micros == TS_UNTIMED {
        "-".to_string()
    } else {
        util::format_ts(h.ts_micros)
    };
    let mut l = format!("{ts} {} {}", severity::name(h.severity), h.source);
    for (k, v) in &h.fields {
        if *k == "unit" || *k == "ident" {
            l.push_str(&format!(" {k}={v}"));
        }
    }
    l.push(' ');
    l.push_str(&crate::watch::one_line(&h.msg, 300));
    if count > 1 {
        l.push_str(&format!(" (x{count})"));
    }
    l
}

/// Records grouped by source and message shape, newest kept; the
/// newest groups that fit in `budget` characters, oldest first.
pub fn render_records(hits: &[Hit], budget: usize) -> (String, usize) {
    let mut order: Vec<(String, usize)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut last: Vec<&Hit> = Vec::new();
    for h in hits {
        let key = format!("{}\u{1f}{}", h.source, util::normalize_message(&h.msg));
        match index.get(&key) {
            Some(&i) => {
                order[i].1 += 1;
                last[i] = h;
            }
            None => {
                index.insert(key.clone(), order.len());
                order.push((key, 1));
                last.push(h);
            }
        }
    }
    let mut groups: Vec<(i64, String)> = order
        .iter()
        .zip(&last)
        .map(|((_, n), h)| (h.ts_micros, hit_line(h, *n)))
        .collect();
    groups.sort_by_key(|g| g.0);
    let mut kept = Vec::new();
    let mut used = 0;
    for (_, l) in groups.iter().rev() {
        if used + l.len() + 1 > budget {
            break;
        }
        used += l.len() + 1;
        kept.push(l.as_str());
    }
    kept.reverse();
    let dropped = groups.len() - kept.len();
    (kept.join("\n"), dropped)
}

const STOPWORDS: [&str; 24] = [
    "what",
    "when",
    "where",
    "which",
    "while",
    "why",
    "there",
    "their",
    "have",
    "does",
    "from",
    "that",
    "this",
    "with",
    "about",
    "into",
    "were",
    "been",
    "your",
    "logs",
    "show",
    "tell",
    "happened",
    "something",
];

/// Error-and-warning records plus any record that mentions a word of
/// the question.
pub fn question_predicate(question: &str) -> Result<Box<dyn Predicate>> {
    let words: Vec<String> = question
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
        .filter(|w| w.chars().count() >= 4)
        .map(|w| w.to_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .take(12)
        .collect();
    let mut or = OrPredicate::new();
    or.push(Box::new(SeverityInPredicate::new(&[
        severity::WARN,
        severity::ERROR,
        severity::FATAL,
    ])));
    if !words.is_empty() {
        let alt: Vec<String> = words.iter().map(|w| regex::escape(w)).collect();
        or.push(Box::new(RegexBytesPredicate::new(&format!(
            "(?i){}",
            alt.join("|")
        ))?));
    }
    Ok(Box::new(or))
}

pub struct Gathered {
    pub context: String,
    pub records: usize,
}

/// Build the bounded context for a question over the given sources:
/// a per-source summary, then the relevant records.
pub fn gather(
    specs: &[&SourceSpec],
    question: &str,
    query: Option<&str>,
    w: &Window,
    budget: usize,
    redact_on: bool,
) -> Result<Gathered> {
    let pred: Box<dyn Predicate> = match query {
        Some(q) => dsl::compile(q).map_err(|e| anyhow!("query: {e}"))?,
        None => question_predicate(question)?,
    };
    let mut head = String::new();
    let mut hits = Vec::new();
    let mut total = 0;
    for spec in specs {
        if spec.kind == SourceKind::Stdin {
            continue;
        }
        let s = inspect::stats(spec, w, 5)?;
        let levels: Vec<String> = s.by_level.iter().map(|(l, c)| format!("{l} {c}")).collect();
        head.push_str(&format!(
            "source {} ({}): {} records{}{}; {}\n",
            spec.name,
            // Names and units only: a full path would leak the local
            // directory layout and user name to the model provider.
            match &spec.kind {
                SourceKind::Journal(u) if !u.is_empty() => format!("journal {}", u.join(", ")),
                SourceKind::Journal(_) => "journal".to_string(),
                _ => "file".to_string(),
            },
            s.total,
            s.first_ts
                .map(|t| format!(" from {}", util::format_ts(t)))
                .unwrap_or_default(),
            s.last_ts
                .map(|t| format!(" to {}", util::format_ts(t)))
                .unwrap_or_default(),
            levels.join(", ")
        ));
        for (m, c) in &s.top_messages {
            head.push_str(&format!("  frequent: {c}x {m}\n"));
        }
        let f = inspect::search(spec, Some(pred.as_ref()), w, 2_000)?;
        total += f.total;
        hits.extend(f.hits);
    }
    hits.sort_by_key(|h| h.ts_micros);
    let budget_left = budget.saturating_sub(head.len() + 200);
    let (records, dropped) = render_records(&hits, budget_left);
    let mut ctx = format!("{head}\nRelevant records ({total} matched");
    if dropped > 0 {
        ctx.push_str(&format!(", {dropped} older groups left out"));
    }
    ctx.push_str("; repeats folded as xN):\n");
    ctx.push_str(&records);
    let ctx = if redact_on { redact(&ctx) } else { ctx };
    Ok(Gathered {
        context: util::truncate(&ctx, budget).0.to_string(),
        records: hits.len(),
    })
}

pub fn question_prompt(question: &str, context: &str) -> String {
    format!("Question: {question}\n\nLog context:\n{context}")
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn think_spans_are_dropped_across_chunks() {
        let mut f = ThinkFilter::default();
        let mut out = String::new();
        for chunk in [
            "<thi",
            "nk>plan",
            "ning</th",
            "ink>Answer",
            " is 42",
            "<",
            "b>",
        ] {
            out.push_str(&f.push(chunk));
        }
        out.push_str(&f.finish());
        assert_eq!(out, "Answer is 42<b>");
        let mut f = ThinkFilter::default();
        assert_eq!(f.push("no tags here"), "no tags here");
    }

    #[test]
    fn redaction_masks_common_secrets() {
        let cases = [
            (
                "user=bob password=hunter2 ok",
                "user=bob password=<redacted> ok",
            ),
            (
                r#"{"api_key":"abc123","x":1}"#,
                r#"{"api_key":"<redacted>","x":1}"#,
            ),
            (
                "Authorization: Bearer abcdefghijklmnop",
                "Authorization: Bearer <redacted>",
            ),
            ("mail admin@example.com now", "mail <email> now"),
            (
                "Authorization: Basic dXNlcjpwYXNzd29yZA==",
                "Authorization: Basic <redacted>",
            ),
            (
                "db postgres://app:s3cr3t@db:5432/x",
                "db <url with credentials>@db:5432/x",
            ),
            ("key sk-ABCDEFGHIJKLMNOPQRSTUV", "key <secret>"),
            (
                "tg 123456789:AAbbCCddEEffGGhhIIjjKKllMMnnOOppQQ",
                "tg <secret>",
            ),
            ("db_password: hunter2", "db_password: <redacted>"),
            ("timeout=30s retries=3", "timeout=30s retries=3"),
        ];
        for (input, want) in cases {
            assert_eq!(redact(input), want, "input: {input}");
        }
        let jwt = "t=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop";
        assert_eq!(redact(jwt), "t=<jwt>");
    }

    #[test]
    fn endpoints_and_locality() {
        assert_eq!(
            endpoint_of("http://localhost:11434"),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            endpoint_of("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            endpoint_of("https://api.example.com/openai/v1"),
            "https://api.example.com/openai/v1/chat/completions"
        );
        assert!(is_local("http://127.0.0.1:8080/v1"));
        assert!(is_local("http://[::1]:8080"));
        assert!(!is_local("http://10.0.0.5:8080"));
    }

    fn hit(ts: i64, msg: &str) -> Hit {
        Hit {
            source: "api".into(),
            ts_micros: ts,
            severity: severity::ERROR,
            msg: msg.into(),
            line: msg.into(),
            fields: vec![],
        }
    }

    #[test]
    fn records_fold_repeats_and_respect_the_budget() {
        let hits: Vec<Hit> = (0..50)
            .map(|i| hit(i * 1_000_000, &format!("retry {i} failed")))
            .chain([hit(60_000_000, "disk full")])
            .collect();
        let (text, dropped) = render_records(&hits, 10_000);
        assert_eq!(dropped, 0);
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("retry 49 failed (x50)"));
        assert!(text.ends_with("disk full"));
        let many: Vec<Hit> = (0..500)
            .map(|i| {
                hit(
                    i,
                    &format!("{} message", "word".repeat(1 + (i as usize % 40))),
                )
            })
            .collect();
        let (text, _) = render_records(&many, 300);
        assert!(text.len() <= 300);
    }

    #[test]
    fn question_words_widen_the_search() {
        use mgi_pulse_core::engine::format::{FieldCache, LogFormat};
        use mgi_pulse_core::engine::record::{RawRecord, RecordBytes};
        let p = question_predicate("Why did the payment worker restart?").unwrap();
        let check = |sev: u8, line: &[u8]| {
            let rec = RawRecord {
                source_id: 0,
                line_id: 0,
                ts_micros: 0,
                severity: sev,
                bytes: RecordBytes::Owned(Box::new([])),
            };
            p.matches(&rec, &mut FieldCache::new(LogFormat::Ndjson, line))
        };
        assert!(check(severity::INFO, b"Payment worker started"));
        assert!(check(severity::ERROR, b"anything"));
        assert!(!check(severity::INFO, b"healthcheck ok"));
    }

    /// A one-shot OpenAI-compatible server that streams `chunks`.
    pub fn mock_openai(chunks: Vec<&'static str>) -> (String, std::thread::JoinHandle<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap();
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req);
                if let Some(i) = text.find("\r\n\r\n") {
                    let len: usize = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if req.len() >= i + 4 + len {
                        break;
                    }
                }
            }
            let mut body = String::new();
            for c in chunks {
                body.push_str(&format!(
                    "data: {}\n\n",
                    json!({"choices":[{"delta":{"content":c}}]})
                ));
            }
            body.push_str("data: [DONE]\n\n");
            write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8_lossy(&req).into_owned()
        });
        (base, h)
    }

    #[test]
    fn streams_tokens_from_an_openai_compatible_server() {
        let (base, h) = mock_openai(vec!["<think>hmm</think>", "The disk ", "is full."]);
        let conf = LlmConf {
            base_url: Some(base),
            model: Some("tiny".into()),
            ..Default::default()
        };
        let llm = Llm::from_config(Some(&conf)).unwrap().unwrap();
        let mut streamed = Vec::new();
        let answer = llm
            .chat("sys", "user text", &mut |t| streamed.push(t.to_string()))
            .unwrap();
        assert_eq!(answer, "The disk is full.");
        assert_eq!(streamed.concat(), answer);
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /v1/chat/completions "));
        assert!(req.contains("\"model\":\"tiny\""));
        assert!(req.contains("\"stream\":true"));
        assert!(!req.to_ascii_lowercase().contains("authorization"));
    }

    #[test]
    fn unconfigured_means_off() {
        if std::env::var(URL_ENV).is_err() && std::env::var(MODEL_ENV).is_err() {
            assert!(Llm::from_config(None).unwrap().is_none());
        }
        let half = LlmConf {
            base_url: Some("http://localhost:1".into()),
            ..Default::default()
        };
        if std::env::var(MODEL_ENV).is_err() {
            assert!(Llm::from_config(Some(&half)).is_err());
        }
    }
}
