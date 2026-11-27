//! A read a preparation queued ([`Group`]), holding its room in the node's bound of queued reads
//! (`local_cache/queued_reads.rs`) until it leaves the queue: read, refused, or dropped with the
//! queue of a source that was released.

use std::mem::size_of;
use std::sync::Arc;

use super::groups::Group;
use crate::TreeDigest;
use crate::server::local_cache::QueuedReads;

/// A queued read and its room.
pub(super) struct Queued {
    group: Group,
    /// The bytes of heap the read holds, taken from `reads` when it was queued.
    room: u64,
    reads: Arc<QueuedReads>,
}

impl Queued {
    /// The read.
    pub(super) fn group(&self) -> &Group {
        &self.group
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.reads.give(self.room);
    }
}

/// The bytes of heap a queued read of `group` holds: the queued read itself and its spans. The
/// name of the pack is shared by every read of the list, and is not counted.
pub(super) fn held(group: &Group) -> u64 {
    let part = size_of::<(u64, u64, TreeDigest)>();
    (size_of::<Queued>() + group.parts.capacity() * part) as u64
}

/// `groups` as queued reads, if room for all of them fits in `reads` under `most` bytes; none if
/// it does not, and the refusal is counted. All or none: a source whose reads are not queued has
/// its files read when first opened.
pub(super) fn admit(
    reads: &Arc<QueuedReads>,
    groups: Vec<Group>,
    most: u64,
) -> Option<Vec<Queued>> {
    let rooms: Vec<u64> = groups.iter().map(held).collect();
    let total = rooms
        .iter()
        .fold(0_u64, |sum, room| sum.saturating_add(*room));
    if !reads.take(total, most) {
        return None;
    }
    let queued = groups.into_iter().zip(rooms).map(|(group, room)| Queued {
        group,
        room,
        reads: Arc::clone(reads),
    });
    Some(queued.collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(parts: usize) -> Group {
        Group {
            pack: Arc::from("cas/pack/one"),
            parts: (0..parts as u64)
                .map(|at| (at * 10, 10, TreeDigest::of(&at.to_le_bytes())))
                .collect(),
        }
    }

    /// Asserts that the reads admitted take the room of each, that every read gives its room
    /// back when it is dropped, one by one, and that reads that do not fit take nothing.
    #[test]
    fn admitted_reads_hold_their_room_until_each_is_dropped() {
        let reads = Arc::new(QueuedReads::default());
        let (one, three) = (group(1), group(3));
        let (small, large) = (held(&one), held(&three));
        assert!(large > small);
        let mut queued = admit(&reads, vec![one, three], small + large).expect("room");
        assert_eq!(reads.bytes(), small + large);
        assert_eq!(queued[1].group().parts.len(), 3);
        assert!(admit(&reads, vec![group(1)], small + large).is_none());
        assert_eq!((reads.bytes(), reads.skipped()), (small + large, 1));
        drop(queued.pop());
        assert_eq!(reads.bytes(), small);
        drop(queued);
        assert_eq!(reads.bytes(), 0);
    }

    /// Asserts that a list with no read to queue is admitted with no room taken, even under a
    /// bound of 0, and that a read is refused under a bound below its own size.
    #[test]
    fn no_reads_take_no_room_and_a_read_over_the_bound_is_refused() {
        let reads = Arc::new(QueuedReads::default());
        let none = admit(&reads, Vec::new(), 0).expect("nothing to hold");
        assert!(none.is_empty());
        let one = group(1);
        let most = held(&one) - 1;
        assert!(admit(&reads, vec![one], most).is_none());
        assert_eq!((reads.bytes(), reads.skipped()), (0, 1));
    }
}
