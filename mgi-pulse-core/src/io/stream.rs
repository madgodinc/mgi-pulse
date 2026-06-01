//! StreamProducer: BufRead-backed source for stdin and (in v0.2) growing tails.
//!
//! Emits records with `RecordBytes::Owned(Box<[u8]>)`. No mmap, no lifetimes
//! crossing thread boundaries.
//!
//! M1 task: implement for stdin only.

// TODO(M1): pub struct StreamProducer { ... }
