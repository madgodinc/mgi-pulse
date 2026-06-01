//! Engine layer: ingest, indexes, queries, histogram.
//!
//! Threading:
//! - indexer thread: consumes `RawRecord` from a `RecordProducer`, fills
//!   line/time/severity indexes, updates active predicate bitsets in lockstep.
//! - query thread: backfills new predicates against the indexed prefix `[0, K)`,
//!   honors cancellation.
//! - main thread: UI loop (in pulse-tui), pulls snapshots.
//!
//! Channels: `crossbeam-channel`. No async runtime.

pub mod record;

pub mod indexer;
pub mod indexes;
pub mod predicate;
pub mod query;
pub mod histogram;
