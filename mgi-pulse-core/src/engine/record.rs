//! The record contract between IO and engine.
//!
//! Critical: bytes never cross thread boundaries as a borrowed slice with
//! a synthetic `'static` lifetime. They are either:
//!
//! - `Owned(Box<[u8]>)` — stream-side (stdin), no mmap, no surprises;
//! - `FileRef { source_id, offset, len }` — file-side, resolved against the
//!   reader's per-pass snapshot of `Arc<Mmap>` keyed by `source_id`.
//!
//! This is the fix for the dangling-slice hazard the first design walked into
//! via `Cow<'static, [u8]>`. Do not "improve" this back to a borrow.

/// Sentinel for records that have no parseable timestamp on a static file.
/// On streams, arrival time is used instead; on files, such records go into
/// the untimed bucket and are excluded from the time axis.
pub const TS_UNTIMED: i64 = i64::MIN;

/// Severity enum, byte-sized for the parallel `SeverityIndex`.
///
/// Values are stable across releases — they hit disk in v0.2's on-disk index
/// format. Treat them as a wire format, not an internal enum to reshuffle.
pub mod severity {
    pub const UNKNOWN: u8 = 0;
    pub const TRACE: u8 = 1;
    pub const DEBUG: u8 = 2;
    pub const INFO: u8 = 3;
    pub const WARN: u8 = 4;
    pub const ERROR: u8 = 5;
    pub const FATAL: u8 = 6;

    /// Match the byte form of `level` against the enum. Case-insensitive on
    /// ASCII. Returns `UNKNOWN` for unrecognized values — the hot path never
    /// allocates.
    pub fn from_bytes(b: &[u8]) -> u8 {
        // Equality on lowercased ASCII without allocating: compare against
        // both lower and upper forms in fixed tables. The set is small enough
        // that an open-coded match wins over a HashMap.
        match b.len() {
            4 => match b {
                b"INFO" | b"Info" | b"info" => INFO,
                b"WARN" | b"Warn" | b"warn" => WARN,
                _ => UNKNOWN,
            },
            5 => match b {
                b"TRACE" | b"Trace" | b"trace" => TRACE,
                b"DEBUG" | b"Debug" | b"debug" => DEBUG,
                b"ERROR" | b"Error" | b"error" => ERROR,
                b"FATAL" | b"Fatal" | b"fatal" => FATAL,
                _ => UNKNOWN,
            },
            7 => match b {
                b"WARNING" | b"Warning" | b"warning" => WARN,
                _ => UNKNOWN,
            },
            _ => UNKNOWN,
        }
    }

    pub fn name(s: u8) -> &'static str {
        match s {
            TRACE => "trace",
            DEBUG => "debug",
            INFO => "info",
            WARN => "warn",
            ERROR => "error",
            FATAL => "fatal",
            _ => "?",
        }
    }
}

#[derive(Debug, Clone)]
pub enum RecordBytes {
    /// Owned bytes — stream-side (stdin), or a multi-line record assembled
    /// from non-contiguous spans (rare; e.g. interleaved stack traces).
    Owned(Box<[u8]>),
    /// Single contiguous span inside the source's mmap. Multi-line records
    /// whose continuation lines are physically adjacent in the file (the
    /// common case for stack traces emitted by one thread) get a `len`
    /// extended to cover the whole block — still zero-copy.
    FileRef {
        source_id: u32,
        offset: u64,
        len: u32,
    },
    /// Multiple non-contiguous spans inside the same source's mmap. This is
    /// the rare case: a multi-line record whose continuation lines are
    /// interleaved with other records (e.g. two threads writing stack
    /// traces in parallel). Resolving requires concatenation, so
    /// `Engine::line_bytes` returns owned bytes; the variant exists in v0.1
    /// to lock down the on-disk format ahead of the format dispatch work,
    /// not to be emitted yet.
    #[allow(dead_code)]
    FileRefMulti {
        source_id: u32,
        spans: Vec<(u64, u32)>,
    },
}

#[derive(Debug, Clone)]
pub struct RawRecord {
    pub source_id: u32,

    /// Monotonic per-engine identifier. Single-source: arrival order.
    /// Merged (k-way by ts): order in the merged stream. The bifurcation
    /// is documented in the project memory — TablePane always orders by
    /// `line_id`, never directly by `ts_micros`.
    pub line_id: u64,

    /// Microseconds since the Unix epoch, or `TS_UNTIMED` if no timestamp
    /// could be extracted and no honest fallback applies.
    pub ts_micros: i64,

    /// Severity as a small enum byte. See `severity::*` constants.
    pub severity: u8,

    pub bytes: RecordBytes,
}

#[cfg(test)]
mod tests {
    use super::severity::*;

    #[test]
    fn level_byte_matching() {
        assert_eq!(from_bytes(b"info"), INFO);
        assert_eq!(from_bytes(b"INFO"), INFO);
        assert_eq!(from_bytes(b"Info"), INFO);
        assert_eq!(from_bytes(b"warn"), WARN);
        assert_eq!(from_bytes(b"warning"), WARN);
        assert_eq!(from_bytes(b"error"), ERROR);
        assert_eq!(from_bytes(b"fatal"), FATAL);
        assert_eq!(from_bytes(b"trace"), TRACE);
        assert_eq!(from_bytes(b"debug"), DEBUG);
        assert_eq!(from_bytes(b"weird"), UNKNOWN);
        assert_eq!(from_bytes(b""), UNKNOWN);
        assert_eq!(from_bytes(b"DEBUGX"), UNKNOWN);
    }
}
