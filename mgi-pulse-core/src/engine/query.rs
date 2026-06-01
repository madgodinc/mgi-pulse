//! Query thread: backfills new predicates against the indexed prefix.
//!
//! Protocol (handoff between query thread and indexer):
//! 1. When a new predicate F is registered, take the K-snapshot atomically:
//!    K = current `index.len()`. Hand `(F, [0, K))` to the query thread.
//! 2. Query thread scans `[0, K)`, populates the bitset for F. Honors a
//!    `cancelled` flag set by UI when the user retracts F.
//! 3. The indexer is the *only* writer of bitset bits for `[K, ..)`. It
//!    evaluates every active predicate as it appends new records.
//! 4. UI debounces filter changes (~100ms) so `f, f, Esc, f` collapses to one
//!    request, not four scans.
//!
//! New field-filters are not instant: a re-parse pass over `[0, K)` is
//! seconds on multi-GB inputs. A `FieldCache` is built per-row once, indexed
//! by the union of fields referenced by all currently-active predicates, so
//! adding the third filter does not triple the parse cost.
//!
//! M1 task: implement minimal driver for `RegexBytesPredicate`.
