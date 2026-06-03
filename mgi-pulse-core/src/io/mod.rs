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

pub mod compressed;
pub mod file;
pub mod json_array;
pub mod merge;
pub mod multiline;
pub mod stream;
pub mod tail;

use crate::engine::record::RawRecord;

pub trait RecordProducer {
    /// Blocking pull of the next record. None = EOF (static) or stream closed.
    fn next(&mut self) -> Option<RawRecord>;

    /// True if records may continue to arrive after a transient None.
    /// File: false. Stdin/growing-tail: true.
    fn is_live(&self) -> bool;
}

/// Background-friendly producer. Implementors promise to be `Send` so
/// they can be moved into a worker thread that ingests a live source
/// (e.g. native `--follow` with a `TailReader`). Most existing
/// producers already satisfy this — stdin's `StdinLock` is the
/// notable exception and stays on the main thread.
pub trait SendableProducer: RecordProducer + Send {}
impl<T: RecordProducer + Send> SendableProducer for T {}
