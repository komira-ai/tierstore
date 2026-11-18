//! The liveness rule and "the newest fact about a key wins" (10.2), as pure functions of what the
//! index holds for a key and the fact that arrives.
//!
//! - A record `(K, generation, stamp s)` is live iff no tombstone of `K` has a stamp greater than
//!   `s`. So a tombstone at `t` deletes the records below `t`, and a record at or above `t` (a
//!   re-PUT after the delete, even of the same bytes) is live.
//! - Between two records of a key the newer wins: the higher stamp; at equal stamps a relocate
//!   (it moves the record of that stamp, 10.5) wins over a record; then, so that every node that
//!   sees the same facts in any order holds the same entry, the larger generation and then the
//!   larger location. Two different generations at one stamp come only from two writers of one
//!   key under membership views that differ (10.5 step 4), and either answer is a defect the
//!   conflict counter reports.
//! - A relocate applies only while the entry holds the generation and stamp it moves.

use tierstore_format::block::BlockGeometry;
use tierstore_format::ids::BlockId;
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::Named;
use tierstore_store_api::index::{DeleteOutcome, EntryLocation, IndexEntry, LiveCount, PutOutcome};

use crate::codec::{Enc, Held, encode_location};

/// The order of two records of one key: the larger is the newer.
fn rank(e: &IndexEntry) -> (Stamp, bool, [u8; 16], Vec<u8>) {
    let mut loc = Enc::default();
    encode_location(&mut loc, &e.location);
    (
        e.stamp,
        e.flags.contains(RecordFlags::RELOCATE),
        e.generation.0,
        loc.finish(),
    )
}

/// Whether `new` is newer than `cur` (and so replaces it).
#[must_use]
pub(crate) fn supersedes(new: &IndexEntry, cur: &IndexEntry) -> bool {
    rank(new) > rank(cur)
}

/// The blocks an entry's bytes are in, with the live entries and bytes it adds to each: a record's
/// block; each block of a chain with its slice; none for a large file (it is not a block).
#[must_use]
pub(crate) fn contrib(geometry: BlockGeometry, e: &IndexEntry) -> Vec<(BlockId, LiveCount)> {
    match e.location {
        EntryLocation::Block { block, .. } => vec![(
            block,
            LiveCount {
                entries: 1,
                bytes: e.length,
            },
        )],
        EntryLocation::Chain { first } => match geometry.chain(first, e.length) {
            Ok(chain) => (0..chain.blocks())
                .filter_map(|i| chain.slice(i))
                .map(|s| {
                    (
                        s.block,
                        LiveCount {
                            entries: 1,
                            bytes: s.len,
                        },
                    )
                })
                .collect(),
            // An entry the geometry does not see as a chain (a size of at most one block, or ids
            // past the node's counter) counts on its first block alone.
            Err(_) => vec![(
                first,
                LiveCount {
                    entries: 1,
                    bytes: e.length,
                },
            )],
        },
        EntryLocation::Large(_) => Vec::new(),
    }
}

/// The names a tombstone of the key must carry for an entry: its block (each block of a chain) or
/// large file, and every block it was relocated from.
#[must_use]
pub(crate) fn names_of(geometry: BlockGeometry, e: &IndexEntry) -> Vec<Named> {
    let mut out: Vec<Named> = match e.location {
        EntryLocation::Large(f) => vec![Named::LargeFile(f)],
        _ => contrib(geometry, e)
            .into_iter()
            .map(|(b, _)| Named::Block(b))
            .collect(),
    };
    out.extend(e.prior_blocks.iter().map(|b| Named::Block(*b)));
    out.sort();
    out.dedup();
    out
}

/// What `put_if_absent` answers for what the key holds, and whether `new` is published.
#[must_use]
pub(crate) fn put(held: Option<&Held>, new: &IndexEntry) -> PutOutcome {
    match held {
        None => PutOutcome::Created,
        Some(Held::Tomb(t)) if new.stamp < *t => PutOutcome::Stale(*t),
        Some(Held::Tomb(_)) => PutOutcome::Created,
        Some(Held::Live(e)) if e.generation == new.generation => PutOutcome::Identical(e.clone()),
        Some(Held::Live(e)) => PutOutcome::Conflict(e.clone()),
    }
}

/// What a tombstone at `stamp` does to what the key holds: the outcome, and what the key holds
/// after it.
#[must_use]
pub(crate) fn delete(held: Option<&Held>, stamp: Stamp) -> (DeleteOutcome, Held) {
    match held {
        Some(Held::Live(e)) if e.stamp < stamp => {
            (DeleteOutcome::Deleted(e.clone()), Held::Tomb(stamp))
        }
        Some(Held::Live(e)) => (DeleteOutcome::Newer(e.clone()), Held::Live(e.clone())),
        Some(Held::Tomb(t)) => (DeleteOutcome::NoLiveEntry, Held::Tomb((*t).max(stamp))),
        None => (DeleteOutcome::NoLiveEntry, Held::Tomb(stamp)),
    }
}

/// What a record of the key (from replay, replication or a sync) makes the key hold, or `None`
/// when what it holds is as new or newer. A record that replaces one of the same generation and
/// stamp (a relocate found in a trailer) keeps the replaced record's blocks in `prior_blocks`
/// and its promoted copy (the bytes are the same).
#[must_use]
pub(crate) fn record(
    geometry: BlockGeometry,
    held: Option<&Held>,
    new: &IndexEntry,
) -> Option<IndexEntry> {
    match held {
        None => Some(new.clone()),
        Some(Held::Tomb(t)) => (new.stamp >= *t).then(|| new.clone()),
        Some(Held::Live(cur)) => {
            if !supersedes(new, cur) {
                return None;
            }
            if cur.generation == new.generation && cur.stamp == new.stamp {
                Some(moved(
                    geometry,
                    cur,
                    new.location,
                    new.flags,
                    &new.prior_blocks,
                ))
            } else {
                Some(new.clone())
            }
        }
    }
}

/// A relocate of `(generation, stamp)` to `to`: the entry it makes, or `None` when the key does
/// not hold that record (deleted, replaced) or already points at `to`.
#[must_use]
pub(crate) fn relocate(
    geometry: BlockGeometry,
    held: Option<&Held>,
    generation: tierstore_format::ids::Generation,
    stamp: Stamp,
    to: EntryLocation,
) -> Option<IndexEntry> {
    match held {
        Some(Held::Live(cur))
            if cur.generation == generation && cur.stamp == stamp && cur.location != to =>
        {
            Some(moved(
                geometry,
                cur,
                to,
                cur.flags.with(RecordFlags::RELOCATE),
                &[],
            ))
        }
        _ => None,
    }
}

/// `cur` moved to `to`: its old blocks join `prior_blocks`, the new location's blocks leave it.
fn moved(
    geometry: BlockGeometry,
    cur: &IndexEntry,
    to: EntryLocation,
    flags: RecordFlags,
    more_prior: &[BlockId],
) -> IndexEntry {
    let mut next = IndexEntry {
        location: to,
        flags,
        ..cur.clone()
    };
    let now: Vec<BlockId> = contrib(geometry, &next)
        .into_iter()
        .map(|(b, _)| b)
        .collect();
    let mut prior: Vec<BlockId> = cur.prior_blocks.clone();
    prior.extend(contrib(geometry, cur).into_iter().map(|(b, _)| b));
    prior.extend_from_slice(more_prior);
    prior.retain(|b| !now.contains(b));
    prior.sort();
    prior.dedup();
    next.prior_blocks = prior;
    next
}
