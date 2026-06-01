//! mgi-pulse-core: engine, IO, indexes, schema.
//!
//! Layering (top depends on bottom, never the other way):
//!
//! ```text
//! ViewModel  (in pulse-tui)
//!     |
//!  engine    --- indexer, query, predicates, histogram
//!     |
//!     io     --- RecordProducer impls (file, stream)
//! ```
//!
//! Hard rules baked into the architecture:
//! - Engine owns all data. Renderer only reads.
//! - Bytes are either Owned or FileRef{source_id, offset, len}.
//!   Never a borrowed slice with a lifetime crossing thread boundaries.
//! - mmap is an optimization inside FileProducer, never the fundament.
//!   Live streams (stdin, growing tails) go through the stream path.
//! - No async runtime. std::thread + crossbeam-channel.

pub mod engine;
pub mod io;
pub mod schema;

pub use engine::record::{RawRecord, RecordBytes};
