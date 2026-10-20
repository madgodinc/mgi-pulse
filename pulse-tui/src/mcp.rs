//! MCP server: read-only log tools for AI agents, over stdio or
//! Streamable HTTP.
//!
//! Dual-era per the 2026-07-28 revision: a request that carries
//! `_meta["io.modelcontextprotocol/protocolVersion"]` is served
//! statelessly (modern), and an `initialize` handshake selects the
//! legacy behaviour of 2024-11-05 through 2025-11-25, which most
//! clients still speak.
//!
//! Tools reach only the sources configured at start, by name. There is
//! no path argument and nothing is executed apart from `journalctl` for
//! configured journal sources. Every answer is bounded.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Args;
use mgi_pulse_core::engine::dsl;
use mgi_pulse_core::engine::predicate::{Predicate, SeverityInPredicate};
use mgi_pulse_core::engine::record::severity;
use serde_json::{json, Map, Value};

use crate::inspect::{self, Hit, Window};
use crate::sources::{self, Feed, FeedMsg, FeedOptions, SourceKind, SourceSpec};
use crate::util;
use crate::watch::Rule;

pub const MODERN: &str = "2026-07-28";
/// Newest first. Legacy versions are negotiated through `initialize`.
pub const LEGACY: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const HEADER_MISMATCH: i64 = -32020;
const UNSUPPORTED_VERSION: i64 = -32022;

const MAX_LIMIT: u64 = 200;
const MAX_LINE: usize = 1000;
const MAX_BODY: usize = 1024 * 1024;

#[derive(Debug, Args)]
pub struct McpArgs {
    /// Log files the tools may read. Each is addressed by its file name.
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,

    /// Offer the systemd journal as a source named `journal` (Linux).
    #[arg(long)]
    pub journal: bool,

    /// Journal unit (repeatable). Implies --journal.
    #[arg(short = 'u', long = "unit", value_name = "UNIT")]
    pub units: Vec<String>,

    /// TOML config; its [[source]] entries are offered too.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Serve Streamable HTTP on this address instead of stdio, e.g.
    /// 127.0.0.1:8765. The endpoint is /mcp.
    #[arg(long, value_name = "ADDR")]
    pub http: Option<SocketAddr>,

    /// Environment variable with a bearer token for --http. Required
    /// when binding to anything but loopback.
    #[arg(long, value_name = "VAR", default_value = "MGI_PULSE_MCP_TOKEN")]
    pub token_env: String,

    /// Extra allowed Origin for browser clients of --http (repeatable).
    /// Localhost origins are always allowed.
    #[arg(long = "allow-origin", value_name = "ORIGIN")]
    pub allow_origins: Vec<String>,
}

pub fn run(args: McpArgs) -> Result<()> {
    let (cfg, dir) = crate::watch::load_config(args.config.as_ref())?;
    let mut specs = cfg.source_specs(dir.as_deref())?;
    if args.files.iter().any(|p| p.as_os_str() == "-") {
        bail!("stdin carries the MCP protocol and cannot be a log source");
    }
    specs.extend(sources::specs_from_args(
        &args.files,
        args.journal || !args.units.is_empty(),
        &args.units,
        None,
    ));
    sources::finalize_names(&mut specs)?;
    let server = Arc::new(Server::new(specs));
    match args.http {
        None => serve_stdio(&server, std::io::stdin().lock(), std::io::stdout()),
        Some(addr) => {
            let token = std::env::var(&args.token_env)
                .ok()
                .filter(|t| !t.is_empty());
            if !addr.ip().is_loopback() && token.is_none() {
                bail!(
                    "--http on a non-loopback address needs a bearer token in ${}",
                    args.token_env
                );
            }
            let listener = TcpListener::bind(addr).with_context(|| format!("bind {addr}"))?;
            eprintln!(
                "mgi-pulse: MCP on http://{}/mcp (sources: {}, auth {})",
                listener.local_addr()?,
                server.specs.len(),
                if token.is_some() { "bearer" } else { "none" }
            );
            serve_http(
                listener,
                server,
                Arc::new(HttpPolicy {
                    token,
                    origins: args.allow_origins,
                }),
            )
        }
    }
}

pub struct Server {
    pub specs: Vec<SourceSpec>,
}

/// What a transport should send back.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Notification: nothing to send (HTTP 202).
    None,
    Reply {
        body: Value,
        status: u16,
    },
}

fn ok(id: &Value, result: Value) -> Outcome {
    Outcome::Reply {
        body: json!({"jsonrpc": "2.0", "id": id, "result": result}),
        status: 200,
    }
}

fn err(id: &Value, code: i64, message: &str, data: Option<Value>, status: u16) -> Outcome {
    let mut e = json!({"code": code, "message": message});
    if let Some(d) = data {
        e["data"] = d;
    }
    Outcome::Reply {
        body: json!({"jsonrpc": "2.0", "id": id, "error": e}),
        status,
    }
}

fn server_info() -> Value {
    json!({
        "name": "mgi-pulse",
        "title": "mgi-pulse log tools",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn supported_versions() -> Vec<&'static str> {
    std::iter::once(MODERN).chain(LEGACY).collect()
}

const INSTRUCTIONS: &str = "Read-only access to the log sources this server was started with. \
Call list_sources first; every other tool takes a source by name. \
Queries use the mgi-pulse DSL: field=value, field!=value, field~/regex/, \
ts>=2026-06-01T12:00, combined with AND, OR, NOT and parentheses. \
Common fields: level (trace, debug, info, warn, error, fatal), msg, logger, any JSON key; \
journal sources add unit, ident, host and raw fields such as _PID. \
Times are ages like 15m, 2h, 1d or RFC3339 timestamps. Results are capped and say when they were truncated.";

/// Request-level facts from the HTTP layer, used for header checks.
#[derive(Debug, Default)]
pub struct HttpHeaders {
    pub protocol_version: Option<String>,
    pub method: Option<String>,
    pub name: Option<String>,
}

impl Server {
    pub fn new(specs: Vec<SourceSpec>) -> Server {
        Server { specs }
    }

    pub fn handle_text(&self, text: &str, http: Option<&HttpHeaders>) -> Outcome {
        match serde_json::from_str::<Value>(text) {
            Ok(v) => self.handle(&v, http),
            Err(_) => err(&Value::Null, PARSE_ERROR, "Parse error", None, 400),
        }
    }

    pub fn handle(&self, msg: &Value, http: Option<&HttpHeaders>) -> Outcome {
        let Some(obj) = msg.as_object() else {
            return err(
                &Value::Null,
                INVALID_REQUEST,
                "Invalid Request: expected one JSON-RPC object",
                None,
                400,
            );
        };
        let method = obj.get("method").and_then(Value::as_str);
        let id = obj.get("id").cloned();
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_none() {
            // A response or junk from the client: nothing to answer.
            if id.is_none() || method.is_none() && obj.contains_key("result") {
                return Outcome::None;
            }
            return err(
                &id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "Invalid Request",
                None,
                400,
            );
        }
        let method = method.unwrap();
        let Some(id) = id.filter(|i| i.is_string() || i.is_i64() || i.is_u64()) else {
            // Notifications (initialized, cancelled) need no answer.
            return Outcome::None;
        };
        let params = obj.get("params").cloned().unwrap_or(json!({}));
        let meta_version = params["_meta"]["io.modelcontextprotocol/protocolVersion"]
            .as_str()
            .map(String::from);

        if let Some(h) = http {
            if let Some(o) = check_headers(&id, method, &params, meta_version.as_deref(), h) {
                return o;
            }
        }
        let modern = match meta_version.as_deref() {
            Some(v) if v == MODERN => true,
            Some(v) if LEGACY.contains(&v) => false,
            Some(v) => {
                return err(
                    &id,
                    UNSUPPORTED_VERSION,
                    "Unsupported protocol version",
                    Some(json!({"supported": supported_versions(), "requested": v})),
                    400,
                )
            }
            None => false,
        };
        let finish = |mut result: Value| {
            if modern {
                result["resultType"] = json!("complete");
                result["_meta"] = json!({"io.modelcontextprotocol/serverInfo": server_info()});
            }
            ok(&id, result)
        };

        match method {
            "initialize" => {
                let requested = params["protocolVersion"].as_str().unwrap_or("");
                let version = if LEGACY.contains(&requested) {
                    requested
                } else {
                    LEGACY[0]
                };
                ok(
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": server_info(),
                        "instructions": INSTRUCTIONS,
                    }),
                )
            }
            "server/discover" => finish(json!({
                "supportedVersions": supported_versions(),
                "capabilities": {"tools": {"listChanged": false}},
                "instructions": INSTRUCTIONS,
                "ttlMs": 3_600_000,
                "cacheScope": "public",
            })),
            "ping" => finish(json!({})),
            "tools/list" => {
                let mut r = json!({"tools": tool_defs()});
                if modern {
                    r["ttlMs"] = json!(3_600_000);
                    r["cacheScope"] = json!("public");
                }
                finish(r)
            }
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                if !TOOL_NAMES.contains(&name) {
                    return err(
                        &id,
                        INVALID_PARAMS,
                        &format!("Unknown tool: {name}"),
                        None,
                        400,
                    );
                }
                let args = params["arguments"].as_object().cloned().unwrap_or_default();
                finish(match self.call(name, &args) {
                    Ok(v) => json!({
                        "content": [{"type": "text", "text": v.to_string()}],
                        "structuredContent": v,
                        "isError": false,
                    }),
                    // Tool failures go back as results so the model can
                    // read the reason and correct its call.
                    Err(e) => json!({
                        "content": [{"type": "text", "text": format!("{e:#}")}],
                        "isError": true,
                    }),
                })
            }
            _ => err(
                &id,
                METHOD_NOT_FOUND,
                &format!("Method not found: {method}"),
                None,
                404,
            ),
        }
    }

    fn spec(&self, name: &str) -> Result<&SourceSpec> {
        match self.specs.iter().find(|s| s.name == name) {
            Some(s) => Ok(s),
            None => bail!(
                "no source named `{name}`; known sources: {}",
                self.names().join(", ")
            ),
        }
    }

    fn names(&self) -> Vec<&str> {
        self.specs.iter().map(|s| s.name.as_str()).collect()
    }

    /// The named source, or all of them.
    fn pick(&self, name: Option<&str>) -> Result<Vec<&SourceSpec>> {
        match name {
            Some(n) => Ok(vec![self.spec(n)?]),
            None if self.specs.is_empty() => {
                bail!("this server was started without log sources")
            }
            None => Ok(self.specs.iter().collect()),
        }
    }

    pub fn call(&self, tool: &str, a: &Map<String, Value>) -> Result<Value> {
        let now = util::now_micros();
        match tool {
            "list_sources" => Ok(json!({
                "sources": self.specs.iter().map(|s| json!({
                    "name": s.name,
                    "kind": match s.kind {
                        SourceKind::File(_) => "file",
                        SourceKind::Journal(_) => "journal",
                        SourceKind::Stdin => "stdin",
                    },
                    "description": s.describe(),
                })).collect::<Vec<_>>(),
            })),
            "tail" => {
                let spec = self.spec(str_arg(a, "source")?.context("`source` is required")?)?;
                let n = int_arg(a, "lines", 50, 1, MAX_LIMIT)? as usize;
                let hits = inspect::tail(spec, n)?;
                Ok(json!({
                    "source": spec.name,
                    "returned": hits.len(),
                    "hits": hits.iter().map(|h| h.to_json(MAX_LINE)).collect::<Vec<_>>(),
                }))
            }
            "search" => {
                let pred = match str_arg(a, "query")? {
                    Some(q) => Some(dsl::compile(q).map_err(|e| anyhow::anyhow!("query: {e}"))?),
                    None => None,
                };
                let w = window(a, now, None)?;
                let limit = int_arg(a, "limit", 50, 1, MAX_LIMIT)? as usize;
                let specs = self.pick(str_arg(a, "source")?)?;
                search_json(&specs, pred.as_deref(), &w, limit)
            }
            "errors" => {
                let w = window(a, now, Some("1h"))?;
                let limit = int_arg(a, "limit", 50, 1, MAX_LIMIT)? as usize;
                let specs = self.pick(str_arg(a, "source")?)?;
                let pred = SeverityInPredicate::new(&[severity::ERROR, severity::FATAL]);
                search_json(&specs, Some(&pred), &w, limit)
            }
            "stats" => {
                let w = window(a, now, None)?;
                let top = int_arg(a, "top", 5, 1, 20)? as usize;
                let mut out = Vec::new();
                for spec in self.pick(str_arg(a, "source")?)? {
                    out.push(inspect::stats(spec, &w, top)?.to_json());
                }
                Ok(json!({"sources": out}))
            }
            "watch_once" => {
                let rule = match str_arg(a, "rule")? {
                    Some(q) => Rule::compile("rule", q, false)?,
                    None => Rule::default_errors(),
                };
                let timeout = int_arg(a, "timeout_secs", 30, 1, 120)?;
                let limit = int_arg(a, "limit", 20, 1, MAX_LIMIT)? as usize;
                let specs: Vec<SourceSpec> = self
                    .pick(str_arg(a, "source")?)?
                    .into_iter()
                    .filter(|s| s.kind != SourceKind::Stdin)
                    .cloned()
                    .collect();
                watch_once(&specs, &rule, Duration::from_secs(timeout), limit)
            }
            other => bail!("unknown tool {other}"),
        }
    }
}

fn check_headers(
    id: &Value,
    method: &str,
    params: &Value,
    meta_version: Option<&str>,
    h: &HttpHeaders,
) -> Option<Outcome> {
    let mismatch = |m: String| Some(err(id, HEADER_MISMATCH, &m, None, 400));
    // Header checks belong to the modern revision; legacy clients send
    // no Mcp-Method.
    let v = meta_version?;
    match h.protocol_version.as_deref() {
        None => return mismatch("Header mismatch: MCP-Protocol-Version header is missing".into()),
        Some(hv) if hv != v => {
            return mismatch(format!(
            "Header mismatch: MCP-Protocol-Version header '{hv}' does not match body value '{v}'"
        ))
        }
        _ => {}
    }
    match h.method.as_deref() {
        None => return mismatch("Header mismatch: Mcp-Method header is missing".into()),
        Some(m) if m != method => {
            return mismatch(format!(
                "Header mismatch: Mcp-Method header '{m}' does not match body value '{method}'"
            ))
        }
        _ => {}
    }
    if method == "tools/call" {
        let body = params["name"].as_str().unwrap_or("");
        match h.name.as_deref().map(decode_header_value) {
            None => return mismatch("Header mismatch: Mcp-Name header is missing".into()),
            Some(None) => return mismatch("Header mismatch: Mcp-Name header is malformed".into()),
            Some(Some(n)) if n != body => {
                return mismatch(format!(
                "Header mismatch: Mcp-Name header value '{n}' does not match body value '{body}'"
            ))
            }
            _ => {}
        }
    }
    None
}

/// Header values may use the `=?base64?...?=` sentinel for non-ASCII.
fn decode_header_value(v: &str) -> Option<String> {
    match v
        .strip_prefix("=?base64?")
        .and_then(|r| r.strip_suffix("?="))
    {
        Some(b64) => String::from_utf8(base64_decode(b64)?).ok(),
        None => Some(v.to_string()),
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn str_arg<'a>(a: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => bail!("`{key}` must be a string"),
    }
}

fn int_arg(a: &Map<String, Value>, key: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => match v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)) {
            Some(n) => Ok(n.clamp(min, max)),
            None => bail!("`{key}` must be a number between {min} and {max}"),
        },
    }
}

fn window(a: &Map<String, Value>, now: i64, default_since: Option<&str>) -> Result<Window> {
    let parse = |k: &str, d: Option<&str>| -> Result<Option<i64>> {
        match str_arg(a, k)?.or(d) {
            Some(s) => Ok(Some(
                util::parse_time_bound(s, now).map_err(|e| anyhow::anyhow!("`{k}`: {e}"))?,
            )),
            None => Ok(None),
        }
    };
    Ok(Window {
        since: parse("since", default_since)?,
        until: parse("until", None)?,
    })
}

fn search_json(
    specs: &[&SourceSpec],
    pred: Option<&dyn Predicate>,
    w: &Window,
    limit: usize,
) -> Result<Value> {
    let mut hits: Vec<Hit> = Vec::new();
    let mut total = 0usize;
    let mut skipped = Vec::new();
    for spec in specs {
        if spec.kind == SourceKind::Stdin {
            skipped.push(spec.name.clone());
            continue;
        }
        let f = inspect::search(spec, pred, w, limit)?;
        total += f.total;
        hits.extend(f.hits);
    }
    hits.sort_by_key(|h| h.ts_micros);
    let skip = hits.len().saturating_sub(limit);
    let hits = hits.split_off(skip);
    let mut out = json!({
        "total_matches": total,
        "returned": hits.len(),
        "truncated": total > hits.len(),
        "hits": hits.iter().map(|h| h.to_json(MAX_LINE)).collect::<Vec<_>>(),
    });
    if total > hits.len() {
        out["note"] =
            json!("only the newest matches are returned; narrow the query or the time window");
    }
    if !skipped.is_empty() {
        out["skipped"] = json!(skipped);
    }
    Ok(out)
}

fn watch_once(specs: &[SourceSpec], rule: &Rule, timeout: Duration, limit: usize) -> Result<Value> {
    if specs.is_empty() {
        bail!("no followable sources");
    }
    let feed = Feed::start(
        specs,
        FeedOptions {
            follow: true,
            ..Default::default()
        },
    )?;
    let deadline = Instant::now() + timeout;
    let mut hits = Vec::new();
    let mut seen = 0u64;
    while hits.len() < limit {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match feed.rx.recv_timeout(left) {
            Ok(FeedMsg::Event(ev)) => {
                seen += 1;
                if rule.matches(&ev) {
                    hits.push(Hit::new(
                        &specs[ev.source].name,
                        ev.fmt,
                        ev.ts_micros,
                        ev.severity,
                        &ev.line,
                    ));
                }
            }
            Ok(FeedMsg::Ended { .. }) => {}
            Err(_) => break,
        }
    }
    Ok(json!({
        "rule": rule.query,
        "records_seen": seen,
        "returned": hits.len(),
        "timed_out": hits.len() < limit,
        "hits": hits.iter().map(|h| h.to_json(MAX_LINE)).collect::<Vec<_>>(),
    }))
}

const TOOL_NAMES: [&str; 6] = [
    "list_sources",
    "tail",
    "search",
    "errors",
    "stats",
    "watch_once",
];

fn tool_defs() -> Value {
    let source = |req: bool| {
        json!({"type": "string", "description": if req {
            "Source name from list_sources."
        } else {
            "Source name from list_sources. Omit to use every source."
        }})
    };
    let since = |d: &str| json!({"type": "string", "description": format!("Start of the time window: an age such as 15m, 2h, 1d, or an RFC3339 timestamp. {d}")});
    let until = json!({"type": "string", "description": "End of the time window (exclusive), same forms as since."});
    let limit = |d: u64| json!({"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": d, "description": "Maximum records to return; the newest are kept."});
    let ro = json!({"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false});
    json!([
        {
            "name": "list_sources",
            "title": "List log sources",
            "description": "List the log sources this server can read: files and systemd journal units, with the names the other tools take.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": ro,
        },
        {
            "name": "tail",
            "title": "Last log lines",
            "description": "Return the last N records of one source, oldest first, with timestamp, level and message.",
            "inputSchema": {"type": "object", "properties": {
                "source": source(true),
                "lines": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": 50, "description": "How many records."},
            }, "required": ["source"], "additionalProperties": false},
            "annotations": ro,
        },
        {
            "name": "search",
            "title": "Search logs",
            "description": "Find records matching a query in one or all sources, optionally inside a time window. Returns the newest matches and the total count.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "mgi-pulse query DSL, e.g. level=error AND msg~/timeout/ or unit=nginx.service AND NOT msg~/healthcheck/. Omit to match everything."},
                "source": source(false),
                "since": since("Omit for no lower bound (journal: current boot)."),
                "until": until,
                "limit": limit(50),
            }, "additionalProperties": false},
            "annotations": ro,
        },
        {
            "name": "errors",
            "title": "Recent errors",
            "description": "Return error and fatal records from one or all sources, newest last. Default window is the last hour.",
            "inputSchema": {"type": "object", "properties": {
                "source": source(false),
                "since": since("Default 1h."),
                "until": until,
                "limit": limit(50),
            }, "additionalProperties": false},
            "annotations": ro,
        },
        {
            "name": "stats",
            "title": "Log summary",
            "description": "Summarise sources: record counts per level, first and last timestamp, top loggers and the most frequent messages (numbers folded together).",
            "inputSchema": {"type": "object", "properties": {
                "source": source(false),
                "since": since("Omit for everything (journal: current boot)."),
                "until": until,
                "top": {"type": "integer", "minimum": 1, "maximum": 20, "default": 5, "description": "Entries in each top list."},
            }, "additionalProperties": false},
            "annotations": ro,
        },
        {
            "name": "watch_once",
            "title": "Wait for matching records",
            "description": "Follow sources from now on and return the first records that match a rule, or whatever arrived when the timeout ends. Use it to catch what happens while something is being reproduced.",
            "inputSchema": {"type": "object", "properties": {
                "rule": {"type": "string", "description": "Query DSL rule. Omit to catch error and fatal records."},
                "source": source(false),
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 120, "default": 30, "description": "How long to wait."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": 20, "description": "Stop after this many matches."},
            }, "additionalProperties": false},
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false},
        },
    ])
}

pub fn serve_stdio<R: BufRead, W: Write>(server: &Server, input: R, mut out: W) -> Result<()> {
    for line in input.lines() {
        let line = line.context("read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Outcome::Reply { body, .. } = server.handle_text(&line, None) {
            // One message per line; serde_json never emits raw newlines.
            writeln!(out, "{body}")?;
            out.flush()?;
        }
    }
    Ok(())
}

pub struct HttpPolicy {
    pub token: Option<String>,
    pub origins: Vec<String>,
}

impl HttpPolicy {
    fn origin_ok(&self, origin: &str) -> bool {
        let host = origin
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(origin);
        let host = host.split('/').next().unwrap_or("");
        let bare = match host.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or(""),
            None => host.rsplit_once(':').map_or(host, |(h, _)| h),
        };
        matches!(bare, "localhost" | "127.0.0.1" | "::1")
            || self.origins.iter().any(|o| o.eq_ignore_ascii_case(origin))
    }

    fn token_ok(&self, auth: Option<&str>) -> bool {
        let Some(want) = &self.token else {
            return true;
        };
        let got = auth.and_then(|a| a.strip_prefix("Bearer ")).unwrap_or("");
        // Length leaks nothing useful; compare the bytes in fixed time.
        got.len() == want.len()
            && got
                .bytes()
                .zip(want.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }
}

pub fn serve_http(
    listener: TcpListener,
    server: Arc<Server>,
    policy: Arc<HttpPolicy>,
) -> Result<()> {
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let server = server.clone();
        let policy = policy.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            if let Err(e) = handle_conn(stream, &server, &policy) {
                tracing::debug!(error = %e, "mcp http connection");
            }
        });
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn read_request(stream: &TcpStream) -> Result<Option<Request>> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if headers.len() > 100 {
            bail!("too many headers");
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        bail!("body too large");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

fn respond(
    mut s: &TcpStream,
    status: u16,
    extra: &[(&str, &str)],
    body: Option<&Value>,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        payload.len()
    );
    if body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    s.write_all(payload.as_bytes())?;
    s.flush()?;
    Ok(())
}

fn handle_conn(stream: TcpStream, server: &Server, policy: &HttpPolicy) -> Result<()> {
    let req = match read_request(&stream) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(_) => return respond(&stream, 413, &[], None),
    };
    let rpc_error =
        |code: i64, msg: &str| json!({"jsonrpc": "2.0", "error": {"code": code, "message": msg}});
    if let Some(o) = req.header("Origin") {
        if !policy.origin_ok(o) {
            return respond(
                &stream,
                403,
                &[],
                Some(&rpc_error(INVALID_REQUEST, "Origin not allowed")),
            );
        }
    }
    if !policy.token_ok(req.header("Authorization")) {
        return respond(&stream, 401, &[("WWW-Authenticate", "Bearer")], None);
    }
    let path = req.path.split('?').next().unwrap_or("");
    if path != "/mcp" && path != "/" {
        return respond(&stream, 404, &[], None);
    }
    if req.method != "POST" {
        return respond(&stream, 405, &[("Allow", "POST")], None);
    }
    let text = String::from_utf8_lossy(&req.body);
    let headers = HttpHeaders {
        protocol_version: req.header("MCP-Protocol-Version").map(String::from),
        method: req.header("Mcp-Method").map(String::from),
        name: req.header("Mcp-Name").map(String::from),
    };
    match server.handle_text(&text, Some(&headers)) {
        Outcome::None => respond(&stream, 202, &[], None),
        Outcome::Reply { body, status } => {
            // Legacy clients expect JSON-RPC errors on a 200 response;
            // the modern revision maps them to HTTP status codes.
            let status = if headers.protocol_version.as_deref() == Some(MODERN)
                || body.get("error").map(|e| e["code"] == PARSE_ERROR) == Some(true)
            {
                status
            } else {
                200
            };
            respond(&stream, status, &[], Some(&body))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::tests::{fixture, LOG};

    fn modern_meta() -> Value {
        json!({
            "io.modelcontextprotocol/protocolVersion": MODERN,
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
        })
    }

    fn reply(o: Outcome) -> (Value, u16) {
        match o {
            Outcome::Reply { body, status } => (body, status),
            Outcome::None => panic!("expected a reply"),
        }
    }

    fn server_with_fixture(tag: &str) -> (Server, crate::inspect::tests::Fixture) {
        let f = fixture(tag, LOG);
        (Server::new(vec![f.spec.clone()]), f)
    }

    #[test]
    fn legacy_handshake_negotiates_version() {
        let s = Server::new(vec![]);
        let (r, _) = reply(s.handle(
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"x","version":"1"}}}),
            None,
        ));
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["serverInfo"]["name"], "mgi-pulse");
        assert!(r["result"].get("resultType").is_none());
        let (r, _) = reply(s.handle(
            &json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}),
            None,
        ));
        assert_eq!(r["result"]["protocolVersion"], LEGACY[0]);
        assert_eq!(
            s.handle(
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                None
            ),
            Outcome::None
        );
    }

    #[test]
    fn modern_discover_and_list_carry_required_fields() {
        let s = Server::new(vec![]);
        let (r, _) = reply(s.handle(
            &json!({"jsonrpc":"2.0","id":"a","method":"server/discover","params":{"_meta": modern_meta()}}),
            None,
        ));
        let res = &r["result"];
        assert_eq!(res["resultType"], "complete");
        assert_eq!(res["supportedVersions"][0], MODERN);
        assert_eq!(res["cacheScope"], "public");
        assert_eq!(
            res["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "mgi-pulse"
        );
        let (r, _) = reply(s.handle(
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta": modern_meta()}}),
            None,
        ));
        let names: Vec<_> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, TOOL_NAMES);
        assert!(r["result"]["ttlMs"].is_number());
    }

    #[test]
    fn unsupported_modern_version_is_rejected_with_the_list() {
        let s = Server::new(vec![]);
        let (r, status) = reply(s.handle(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2099-01-01"}}}),
            None,
        ));
        assert_eq!(status, 400);
        assert_eq!(r["error"]["code"], UNSUPPORTED_VERSION);
        assert_eq!(r["error"]["data"]["requested"], "2099-01-01");
    }

    #[test]
    fn protocol_errors() {
        let s = Server::new(vec![]);
        let (r, _) = reply(s.handle_text("{not json", None));
        assert_eq!(r["error"]["code"], PARSE_ERROR);
        assert!(r["id"].is_null());
        let (r, _) = reply(s.handle(&json!([1, 2]), None));
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let (r, _) = reply(s.handle(
            &json!({"jsonrpc":"2.0","id":3,"method":"resources/list"}),
            None,
        ));
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        let (r, _) = reply(s.handle(
            &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"rm"}}),
            None,
        ));
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    fn call(s: &Server, name: &str, args: Value) -> Value {
        let (r, _) = reply(s.handle(
            &json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":name,"arguments":args}}),
            None,
        ));
        r["result"].clone()
    }

    #[test]
    fn tools_answer_with_bounded_structured_content() {
        let (s, _f) = server_with_fixture("mcp-tools");
        let r = call(&s, "tail", json!({"source":"app","lines":2}));
        assert_eq!(r["isError"], false);
        assert_eq!(r["structuredContent"]["returned"], 2);
        assert_eq!(r["structuredContent"]["hits"][1]["msg"], "done");

        let r = call(&s, "search", json!({"query":"level=error","limit":1}));
        let sc = &r["structuredContent"];
        assert_eq!(sc["total_matches"], 2);
        assert_eq!(sc["truncated"], true);
        assert_eq!(sc["hits"][0]["msg"], "retry 2 failed");

        let r = call(&s, "errors", json!({"since":"2026-06-01T00:00:00Z"}));
        assert_eq!(r["structuredContent"]["total_matches"], 2);

        let r = call(&s, "stats", json!({}));
        assert_eq!(r["structuredContent"]["sources"][0]["total"], 5);

        let text = r["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap(),
            r["structuredContent"]
        );
    }

    #[test]
    fn tool_errors_are_results_the_model_can_read() {
        let (s, _f) = server_with_fixture("mcp-errs");
        let r = call(&s, "tail", json!({"source":"/etc/passwd"}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no source named"));
        let r = call(&s, "search", json!({"query":"(level=error"}));
        assert_eq!(r["isError"], true);
        let r = call(&s, "tail", json!({}));
        assert!(r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("required"));
        let r = call(&s, "search", json!({"since":"soonish"}));
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn watch_once_catches_new_lines() {
        let (s, f) = server_with_fixture("mcp-watch");
        let SourceKind::File(path) = f.spec.kind.clone() else {
            unreachable!()
        };
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            let mut fh = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            writeln!(
                fh,
                "{{\"ts\":\"2026-06-01T13:00:00Z\",\"level\":\"error\",\"msg\":\"late boom\"}}"
            )
            .unwrap();
        });
        let r = call(&s, "watch_once", json!({"timeout_secs": 5, "limit": 1}));
        writer.join().unwrap();
        let sc = &r["structuredContent"];
        assert_eq!(sc["returned"], 1, "{r}");
        assert_eq!(sc["hits"][0]["msg"], "late boom");
        assert_eq!(sc["timed_out"], false);
    }

    #[test]
    fn http_header_rules_for_modern_requests() {
        let s = Server::new(vec![]);
        let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_sources","arguments":{},"_meta": modern_meta()}});
        let good = HttpHeaders {
            protocol_version: Some(MODERN.into()),
            method: Some("tools/call".into()),
            name: Some("list_sources".into()),
        };
        let (r, st) = reply(s.handle(&body, Some(&good)));
        assert_eq!(st, 200);
        assert_eq!(r["result"]["resultType"], "complete");

        let encoded = HttpHeaders {
            name: Some("=?base64?bGlzdF9zb3VyY2Vz?=".into()),
            ..good
        };
        assert_eq!(reply(s.handle(&body, Some(&encoded))).1, 200);

        let wrong = HttpHeaders {
            protocol_version: Some(MODERN.into()),
            method: Some("tools/list".into()),
            name: Some("list_sources".into()),
        };
        let (r, st) = reply(s.handle(&body, Some(&wrong)));
        assert_eq!(
            (st, r["error"]["code"].as_i64()),
            (400, Some(HEADER_MISMATCH))
        );

        let missing = HttpHeaders::default();
        assert_eq!(
            reply(s.handle(&body, Some(&missing))).0["error"]["code"],
            HEADER_MISMATCH
        );
    }

    #[test]
    fn http_policy_checks_origin_and_token() {
        let p = HttpPolicy {
            token: Some("s3cret".into()),
            origins: vec!["https://chat.example.org".into()],
        };
        assert!(p.origin_ok("http://localhost:3000"));
        assert!(p.origin_ok("http://127.0.0.1"));
        assert!(p.origin_ok("http://[::1]:8080"));
        assert!(p.origin_ok("https://chat.example.org"));
        assert!(!p.origin_ok("https://evil.example"));
        assert!(!p.origin_ok("http://localhost.evil.example"));
        assert!(p.token_ok(Some("Bearer s3cret")));
        assert!(!p.token_ok(Some("Bearer s3creT")));
        assert!(!p.token_ok(None));
    }

    #[test]
    fn http_round_trip() {
        let (s, _f) = server_with_fixture("mcp-http");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/mcp", listener.local_addr().unwrap());
        let policy = Arc::new(HttpPolicy {
            token: Some("tok".into()),
            origins: vec![],
        });
        let server = Arc::new(s);
        std::thread::spawn(move || serve_http(listener, server, policy));

        let agent = ureq::agent();
        let unauth = agent.post(&base).send_string("{}");
        assert!(matches!(unauth, Err(ureq::Error::Status(401, _))));

        let evil = agent
            .post(&base)
            .set("Authorization", "Bearer tok")
            .set("Origin", "https://evil.example")
            .send_string("{}");
        assert!(matches!(evil, Err(ureq::Error::Status(403, _))));

        let get = agent.get(&base).set("Authorization", "Bearer tok").call();
        assert!(matches!(get, Err(ureq::Error::Status(405, _))));

        let r = agent
            .post(&base)
            .set("Authorization", "Bearer tok")
            .set("Content-Type", "application/json")
            .send_string(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#)
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");

        let note = agent
            .post(&base)
            .set("Authorization", "Bearer tok")
            .send_string(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .unwrap();
        assert_eq!(note.status(), 202);

        let body = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tail","arguments":{"source":"app","lines":1},"_meta": modern_meta()}});
        let r = agent
            .post(&base)
            .set("Authorization", "Bearer tok")
            .set("MCP-Protocol-Version", MODERN)
            .set("Mcp-Method", "tools/call")
            .set("Mcp-Name", "tail")
            .send_string(&body.to_string())
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        assert_eq!(v["result"]["structuredContent"]["hits"][0]["msg"], "done");

        let missing = agent
            .post(&base)
            .set("Authorization", "Bearer tok")
            .set("MCP-Protocol-Version", MODERN)
            .send_string(&body.to_string());
        assert!(matches!(missing, Err(ureq::Error::Status(400, _))));
    }
}
