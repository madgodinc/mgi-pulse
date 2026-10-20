//! TUI `e`: ask the configured model to explain the focused record,
//! with its neighbours and how often its message repeats in the view.
//! The request runs on a worker thread; tokens arrive over a channel
//! and the pane fills in as they come.

use std::sync::Arc;

use crossbeam_channel::{Receiver, TryRecvError};
use mgi_pulse_core::engine::record::{severity, TS_UNTIMED};
use mgi_pulse_core::engine::Engine;

use crate::llm::{self, Llm};
use crate::sources::message_of;
use crate::util;

/// Records scanned on each side of the cursor when counting repeats,
/// so a key press stays instant on an 11M-record view.
const CLUSTER_SCAN: usize = 100_000;
const NEIGHBOURS: u64 = 5;

pub enum Msg {
    Token(String),
    Done,
    Failed(String),
}

pub struct Explain {
    pub line_id: u64,
    pub model: String,
    pub text: String,
    pub done: bool,
    rx: Receiver<Msg>,
}

impl Explain {
    pub fn start(engine: &Engine, view: &[u64], cursor: u64, llm: Arc<Llm>) -> Explain {
        let context = build_context(engine, view, cursor, llm.max_context);
        let context = if llm.redact {
            llm::redact(&context)
        } else {
            context
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let model = llm.model.clone();
        std::thread::Builder::new()
            .name("mgi-pulse-explain".into())
            .spawn(move || {
                let res = llm.chat(llm::SYSTEM_EXPLAIN, &context, &mut |t| {
                    let _ = tx.send(Msg::Token(t.to_string()));
                });
                let _ = tx.send(match res {
                    Ok(_) => Msg::Done,
                    Err(e) => Msg::Failed(format!("{e:#}")),
                });
            })
            .ok();
        Explain {
            line_id: cursor,
            model,
            text: String::new(),
            done: false,
            rx,
        }
    }

    /// Pull whatever arrived. True when the pane needs a redraw.
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Token(t)) => {
                    self.text.push_str(&t);
                    changed = true;
                }
                Ok(Msg::Done) => {
                    self.done = true;
                    changed = true;
                }
                Ok(Msg::Failed(e)) => {
                    self.text.push_str(&format!("\n[failed: {e}]"));
                    self.done = true;
                    changed = true;
                }
                Err(TryRecvError::Empty) => return changed,
                Err(TryRecvError::Disconnected) => {
                    self.done = true;
                    return changed;
                }
            }
        }
    }
}

fn describe(engine: &Engine, id: u64) -> String {
    let ts = engine.indexes.time.get(id).unwrap_or(TS_UNTIMED);
    let sev = engine.indexes.severity.get(id).unwrap_or(0);
    let line = String::from_utf8_lossy(engine.line_bytes(id)).into_owned();
    let ts = if ts == TS_UNTIMED {
        String::new()
    } else {
        format!("{} ", util::format_ts(ts))
    };
    let level = if sev == 0 {
        String::new()
    } else {
        format!("{} ", severity::name(sev))
    };
    format!("{ts}{level}{}", crate::watch::one_line(&line, 600))
}

pub fn build_context(engine: &Engine, view: &[u64], cursor: u64, budget: usize) -> String {
    let total = engine.indexes.len() as u64;
    if cursor >= total {
        return String::new();
    }
    let fmt_of = |id: u64| engine.format_of(engine.indexes.line.locs[id as usize].source_id);
    let raw = String::from_utf8_lossy(engine.line_bytes(cursor)).into_owned();
    let key = util::normalize_message(&message_of(fmt_of(cursor), engine.line_bytes(cursor)));

    let pos = view.binary_search(&cursor).unwrap_or_else(|p| p);
    let lo = pos.saturating_sub(CLUSTER_SCAN);
    let hi = (pos + CLUSTER_SCAN).min(view.len());
    let (mut count, mut first, mut last) = (0u64, None::<i64>, None::<i64>);
    for &id in &view[lo..hi] {
        if util::normalize_message(&message_of(fmt_of(id), engine.line_bytes(id))) == key {
            count += 1;
            let ts = engine.indexes.time.get(id).unwrap_or(TS_UNTIMED);
            if ts != TS_UNTIMED {
                first = Some(first.map_or(ts, |f| f.min(ts)));
                last = Some(last.map_or(ts, |l| l.max(ts)));
            }
        }
    }
    let scanned_all = lo == 0 && hi == view.len();

    let mut ctx = format!(
        "Record to explain:\n{}\n\nRaw:\n{}\n\n",
        describe(engine, cursor),
        util::truncate(&raw, 2000).0
    );
    ctx.push_str(&format!(
        "The same message (numbers ignored) appears {}{count} times in the current view",
        if scanned_all { "" } else { "at least " }
    ));
    if let (Some(f), Some(l)) = (first, last) {
        ctx.push_str(&format!(
            ", first at {}, last at {}",
            util::format_ts(f),
            util::format_ts(l)
        ));
    }
    ctx.push_str(".\n\nSurrounding lines in the file:\n");
    let from = cursor.saturating_sub(NEIGHBOURS);
    let to = (cursor + NEIGHBOURS + 1).min(total);
    for id in from..to {
        let mark = if id == cursor { ">> " } else { "   " };
        ctx.push_str(mark);
        ctx.push_str(&describe(engine, id));
        ctx.push('\n');
    }
    util::truncate(&ctx, budget).0.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mgi_pulse_core::io::stream::StreamProducer;

    #[test]
    fn context_has_record_neighbours_and_repeat_count() {
        let mut body = String::new();
        for i in 0..20 {
            let level = if i % 5 == 0 { "error" } else { "info" };
            let msg = if i % 5 == 0 {
                format!("retry {i} failed")
            } else {
                format!("tick {i}")
            };
            body.push_str(&format!(
                "{{\"ts\":\"2026-06-01T12:00:{i:02}Z\",\"level\":\"{level}\",\"msg\":\"{msg}\"}}\n"
            ));
        }
        let mut engine = Engine::new();
        let mut p = StreamProducer::new(std::io::Cursor::new(body.into_bytes()), 0);
        mgi_pulse_core::engine::indexer::drain(&mut p, &mut engine);
        engine
            .source_formats
            .push(mgi_pulse_core::engine::format::LogFormat::Ndjson);
        let view: Vec<u64> = (0..20).collect();
        let ctx = build_context(&engine, &view, 10, 10_000);
        assert!(ctx.contains("retry 10 failed"));
        assert!(ctx.contains("appears 4 times"), "{ctx}");
        assert!(ctx.contains("first at 2026-06-01T12:00:00.000Z, last at 2026-06-01T12:00:15.000Z"));
        assert_eq!(ctx.matches(">> ").count(), 1);
        assert!(ctx.contains("retry 5 failed") && ctx.contains("retry 15 failed"));
        assert!(!ctx.contains("tick 4") && !ctx.contains("tick 16"));
        assert!(build_context(&engine, &view, 10, 50).len() <= 50);
    }
}
