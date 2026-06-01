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

#[derive(Debug, Clone)]
pub enum RecordBytes {
    Owned(Box<[u8]>),
    FileRef { source_id: u32, offset: u64, len: u32 },
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

    /// Severity as a small enum byte. Parsed by a borrowed serde struct, not
    /// a hand-rolled byte scanner.
    pub severity: u8,

    pub bytes: RecordBytes,
}
