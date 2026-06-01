//! IO layer: producers of raw records.
//!
//! A `RecordProducer` is a synchronous, blocking iterator of raw records.
//! Two concrete impls in v0.1:
//!
//! - [`file::FileProducer`] — mmap'd file. Owns `Arc<Mmap>`. Records carry
//!   `RecordBytes::FileRef { source_id, offset, len }`. Resolved against
//!   the per-pass mmap snapshot in the engine.
//! - [`stream::StreamProducer`] — `BufRead` source (stdin, growing-tail).
//!   Records carry `RecordBytes::Owned(Box<[u8]>)`.
//!
//! Native follow / inotify is NOT in v0.1. Live = `tail -F file | pulse -`.
//! See project memory for the rationale.

pub mod file;
pub mod stream;

use crate::engine::record::RawRecord;

pub trait RecordProducer: Send {
    /// Blocking pull of the next record. None = EOF (static) or stream closed.
    fn next(&mut self) -> Option<RawRecord>;

    /// True if records may continue to arrive after a transient None.
    /// File: false. Stdin/growing-tail: true.
    fn is_live(&self) -> bool;
}
