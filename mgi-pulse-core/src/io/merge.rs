//! MergeProducer — k-way merge of multiple `RecordProducer`s by `ts_micros`.
//!
//! This is the M1.5 step from the mgi-pulse plan. The trade-off is the
//! bifurcation of `line_id` semantics:
//!
//! - **Single source** (no merge): `line_id` == arrival order == file order.
//! - **Merged**: `line_id` == position in the merged stream == time-sorted.
//!
//! TablePane always renders ordered by `line_id`. That's the rule that keeps
//! the scroll experience predictable; it remains true under merge because
//! the merge step is what assigns `line_id`s.
//!
//! Untimed records (`ts_micros == TS_UNTIMED`) sort to the front (`i64::MIN`)
//! by construction. That's honest: a file with no timestamps interleaves at
//! the start, and the user sees them grouped instead of scattered randomly.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::engine::record::RawRecord;
use crate::io::RecordProducer;

/// One slot in the heap: a buffered record + which producer it came from.
///
/// The heap is keyed by `(ts_micros, producer_index)` so that records with
/// identical timestamps are emitted in deterministic order (lower producer
/// index first). The `Ord` impl is inverted because `BinaryHeap` is a
/// max-heap and we want a min-heap by timestamp.
struct Slot {
    rec: RawRecord,
    producer_idx: usize,
}

impl PartialEq for Slot {
    fn eq(&self, other: &Self) -> bool {
        self.rec.ts_micros == other.rec.ts_micros && self.producer_idx == other.producer_idx
    }
}

impl Eq for Slot {}

impl PartialOrd for Slot {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Slot {
    fn cmp(&self, other: &Self) -> Ordering {
        // Invert: BinaryHeap is max-heap, we want smallest ts on top.
        match other.rec.ts_micros.cmp(&self.rec.ts_micros) {
            Ordering::Equal => other.producer_idx.cmp(&self.producer_idx),
            non_eq => non_eq,
        }
    }
}

pub struct MergeProducer {
    producers: Vec<Box<dyn RecordProducer>>,
    heap: BinaryHeap<Slot>,
    line_id_counter: u64,
    primed: bool,
}

impl MergeProducer {
    pub fn new(producers: Vec<Box<dyn RecordProducer>>) -> Self {
        Self {
            producers,
            heap: BinaryHeap::new(),
            line_id_counter: 0,
            primed: false,
        }
    }

    fn prime(&mut self) {
        for (i, p) in self.producers.iter_mut().enumerate() {
            if let Some(rec) = p.next() {
                self.heap.push(Slot {
                    rec,
                    producer_idx: i,
                });
            }
        }
        self.primed = true;
    }
}

impl RecordProducer for MergeProducer {
    fn next(&mut self) -> Option<RawRecord> {
        if !self.primed {
            self.prime();
        }
        let slot = self.heap.pop()?;
        let Slot {
            mut rec,
            producer_idx,
        } = slot;
        // Replenish that producer's slot.
        if let Some(next_rec) = self.producers[producer_idx].next() {
            self.heap.push(Slot {
                rec: next_rec,
                producer_idx,
            });
        }
        // Reassign line_id to be the position in the merged order. The
        // original line_id from the underlying producer is meaningless after
        // merge — it referred to that producer's arrival order, not the
        // global one.
        rec.line_id = self.line_id_counter;
        self.line_id_counter += 1;
        Some(rec)
    }

    fn is_live(&self) -> bool {
        // Live if any underlying producer is live.
        self.producers.iter().any(|p| p.is_live())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::record::{RecordBytes, TS_UNTIMED};

    struct VecProducer {
        records: std::vec::IntoIter<RawRecord>,
    }

    impl VecProducer {
        fn new(records: Vec<RawRecord>) -> Self {
            Self {
                records: records.into_iter(),
            }
        }
    }

    impl RecordProducer for VecProducer {
        fn next(&mut self) -> Option<RawRecord> {
            self.records.next()
        }
        fn is_live(&self) -> bool {
            false
        }
    }

    fn r(source_id: u32, line_id: u64, ts: i64) -> RawRecord {
        RawRecord {
            source_id,
            line_id,
            ts_micros: ts,
            severity: 0,
            bytes: RecordBytes::Owned(Box::from([])),
        }
    }

    #[test]
    fn merges_two_sources_in_time_order() {
        // Source A: ts 10, 30, 50
        // Source B: ts 20, 40, 60
        let a = VecProducer::new(vec![r(0, 0, 10), r(0, 1, 30), r(0, 2, 50)]);
        let b = VecProducer::new(vec![r(1, 0, 20), r(1, 1, 40), r(1, 2, 60)]);
        let mut merge = MergeProducer::new(vec![Box::new(a), Box::new(b)]);

        let mut emitted = Vec::new();
        while let Some(rec) = merge.next() {
            emitted.push((rec.line_id, rec.source_id, rec.ts_micros));
        }
        assert_eq!(
            emitted,
            vec![
                (0, 0, 10),
                (1, 1, 20),
                (2, 0, 30),
                (3, 1, 40),
                (4, 0, 50),
                (5, 1, 60),
            ]
        );
    }

    #[test]
    fn equal_ts_breaks_tie_by_producer_index() {
        let a = VecProducer::new(vec![r(0, 0, 100)]);
        let b = VecProducer::new(vec![r(1, 0, 100)]);
        let mut merge = MergeProducer::new(vec![Box::new(a), Box::new(b)]);
        let first = merge.next().unwrap();
        let second = merge.next().unwrap();
        assert!(merge.next().is_none());
        // Lower producer_idx (source_id 0) wins ties.
        assert_eq!(first.source_id, 0);
        assert_eq!(second.source_id, 1);
    }

    #[test]
    fn untimed_records_sort_to_front() {
        let a = VecProducer::new(vec![r(0, 0, 500), r(0, 1, 1500)]);
        let b = VecProducer::new(vec![r(1, 0, TS_UNTIMED), r(1, 1, 1000)]);
        let mut merge = MergeProducer::new(vec![Box::new(a), Box::new(b)]);
        let mut emitted = Vec::new();
        while let Some(rec) = merge.next() {
            emitted.push((rec.line_id, rec.source_id, rec.ts_micros));
        }
        assert_eq!(
            emitted,
            vec![(0, 1, TS_UNTIMED), (1, 0, 500), (2, 1, 1000), (3, 0, 1500),]
        );
    }

    #[test]
    fn empty_input_terminates_cleanly() {
        let mut merge = MergeProducer::new(vec![]);
        assert!(merge.next().is_none());
    }

    #[test]
    fn one_source_drained_first_still_merges_other() {
        let a = VecProducer::new(vec![r(0, 0, 10)]);
        let b = VecProducer::new(vec![r(1, 0, 20), r(1, 1, 30), r(1, 2, 40)]);
        let mut merge = MergeProducer::new(vec![Box::new(a), Box::new(b)]);
        let mut emitted = Vec::new();
        while let Some(rec) = merge.next() {
            emitted.push((rec.line_id, rec.source_id, rec.ts_micros));
        }
        assert_eq!(
            emitted,
            vec![(0, 0, 10), (1, 1, 20), (2, 1, 30), (3, 1, 40),]
        );
    }
}
