//! Repeat collapsing for watch mode.
//!
//! The first occurrence of a key goes out at once and opens a window.
//! Repeats inside the window are only counted; when the window closes,
//! one summary reports how many were held back. The next occurrence
//! after that opens a fresh window.
//!
//! Time is passed in by the caller as microseconds, so replays can run
//! on record timestamps and tests drive it directly.

use std::collections::HashMap;
use std::time::Duration;

/// Hard cap on tracked keys. Past it, new keys are emitted without
/// collapsing rather than growing memory without bound.
const MAX_KEYS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Emit,
    Suppress,
}

/// A closed window that held back `count` repeats. `sample` is whatever
/// the caller attached to the first occurrence.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary<T> {
    pub key: String,
    pub count: u64,
    pub sample: T,
    /// Payload of the most recent suppressed repeat.
    pub last: T,
}

struct Entry<T> {
    opened: i64,
    count: u64,
    sample: T,
    last: Option<T>,
}

pub struct Collapser<T> {
    window: i64,
    entries: HashMap<String, Entry<T>>,
    closed: Vec<Summary<T>>,
}

impl<T: Clone> Collapser<T> {
    pub fn new(window: Duration) -> Self {
        Self {
            window: window.as_micros().min(i64::MAX as u128) as i64,
            entries: HashMap::new(),
            closed: Vec::new(),
        }
    }

    pub fn offer(&mut self, key: &str, payload: T, now: i64) -> Verdict {
        if self.window == 0 {
            return Verdict::Emit;
        }
        if let Some(e) = self.entries.get_mut(key) {
            if now.saturating_sub(e.opened) < self.window {
                e.count += 1;
                e.last = Some(payload);
                return Verdict::Suppress;
            }
            let old = self.entries.remove(key).unwrap();
            self.close(key.to_string(), old);
        }
        if self.entries.len() >= MAX_KEYS {
            self.expire(now);
            if self.entries.len() >= MAX_KEYS {
                return Verdict::Emit;
            }
        }
        self.entries.insert(
            key.to_string(),
            Entry {
                opened: now,
                count: 0,
                sample: payload,
                last: None,
            },
        );
        Verdict::Emit
    }

    /// Summaries for windows that have closed by `now`.
    pub fn flush(&mut self, now: i64) -> Vec<Summary<T>> {
        self.expire(now);
        std::mem::take(&mut self.closed)
    }

    /// Close every window, e.g. at shutdown.
    pub fn finish(&mut self) -> Vec<Summary<T>> {
        let keys: Vec<String> = self.entries.keys().cloned().collect();
        for k in keys {
            let e = self.entries.remove(&k).unwrap();
            self.close(k, e);
        }
        std::mem::take(&mut self.closed)
    }

    fn expire(&mut self, now: i64) {
        let window = self.window;
        let done: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| now.saturating_sub(e.opened) >= window)
            .map(|(k, _)| k.clone())
            .collect();
        for k in done {
            let e = self.entries.remove(&k).unwrap();
            self.close(k, e);
        }
    }

    fn close(&mut self, key: String, e: Entry<T>) {
        if e.count > 0 {
            let last = e.last.unwrap_or_else(|| e.sample.clone());
            self.closed.push(Summary {
                key,
                count: e.count,
                sample: e.sample,
                last,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: i64) -> i64 {
        n * 1_000_000
    }

    #[test]
    fn first_emits_repeats_are_counted_then_summarised() {
        let t0 = 5_000_000i64;
        let mut c = Collapser::new(Duration::from_secs(600));
        assert_eq!(c.offer("k", 1, t0), Verdict::Emit);
        assert_eq!(c.offer("k", 2, t0 + secs(10)), Verdict::Suppress);
        assert_eq!(c.offer("k", 3, t0 + secs(20)), Verdict::Suppress);
        assert!(c.flush(t0 + secs(599)).is_empty());
        let s = c.flush(t0 + secs(600));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].count, 2);
        assert_eq!((s[0].sample, s[0].last), (1, 3));
        assert!(c.flush(t0 + secs(700)).is_empty());
    }

    #[test]
    fn a_lone_event_closes_without_summary() {
        let t0 = 5_000_000i64;
        let mut c = Collapser::new(Duration::from_secs(60));
        c.offer("k", (), t0);
        assert!(c.flush(t0 + secs(61)).is_empty());
    }

    #[test]
    fn late_repeat_opens_a_new_window_and_reports_the_old_one() {
        let t0 = 5_000_000i64;
        let mut c = Collapser::new(Duration::from_secs(60));
        c.offer("k", 1, t0);
        c.offer("k", 2, t0 + secs(5));
        // No flush between: the next offer after the window still has
        // to report the closed one.
        assert_eq!(c.offer("k", 3, t0 + secs(90)), Verdict::Emit);
        let s = c.flush(t0 + secs(91));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].count, 1);
    }

    #[test]
    fn keys_are_independent() {
        let t0 = 5_000_000i64;
        let mut c = Collapser::new(Duration::from_secs(60));
        assert_eq!(c.offer("a", (), t0), Verdict::Emit);
        assert_eq!(c.offer("b", (), t0), Verdict::Emit);
        assert_eq!(c.offer("a", (), t0), Verdict::Suppress);
    }

    #[test]
    fn zero_window_never_collapses() {
        let t0 = 5_000_000i64;
        let mut c = Collapser::new(Duration::ZERO);
        assert_eq!(c.offer("k", (), t0), Verdict::Emit);
        assert_eq!(c.offer("k", (), t0), Verdict::Emit);
    }

    #[test]
    fn finish_reports_open_windows() {
        let t0 = 5_000_000i64;
        let mut c = Collapser::new(Duration::from_secs(600));
        c.offer("k", 1, t0);
        c.offer("k", 2, t0);
        c.offer("j", 1, t0);
        let s = c.finish();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].key, "k");
    }
}
