//! FileProducer: mmap-backed source.
//!
//! Owns `Arc<Mmap>`. Lines are found via `memchr::memchr_iter(b'\n', ...)`.
//! Emits records that reference the mmap via offset/length, not a borrowed
//! slice with a lifetime. The engine resolves bytes against a per-pass
//! snapshot of `Arc<Mmap>` keyed by `source_id`.
//!
//! M1 task: implement.

// TODO(M1): pub struct FileProducer { ... }
