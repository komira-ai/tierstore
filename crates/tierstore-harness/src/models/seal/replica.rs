//! A replica's check of a replication batch (10.3 step 2) and the sealer's Choose (10.3 step 4),
//! as pure functions the model calls and the tests call directly.

use std::collections::BTreeMap;

use tierstore_format::ids::NodeId;
use tierstore_format::seal::Ballot;
use tierstore_store_api::hot::ReplicaReject;

use super::Variant;
use super::types::{Answer, Prop};

/// What a replica knows when a batch arrives.
#[derive(Debug, Clone, Copy)]
pub struct ReplicaView {
    /// Its committed length.
    pub committed: u64,
    /// The crc32c of its bytes `[0, committed)` as it computes them.
    pub copy_crc: u32,
    /// The highest ballot it has promised.
    pub promised: Ballot,
    /// Whether its copy is sealed.
    pub sealed: bool,
}

/// A replication batch.
#[derive(Debug, Clone, Copy)]
pub struct Batch<'a> {
    /// The writer's ballot.
    pub ballot: Ballot,
    /// The offset the bytes start at.
    pub start: u64,
    /// The writer's crc32c of the segment through the batch.
    pub running_crc: u32,
    /// The bytes.
    pub bytes: &'a [u8],
}

/// The replica's decision on a batch: the new committed length, or why it refuses.
///
/// A sealed copy refuses first, then a ballot below the promise (when `fence` is set; the
/// model's variant without it skips the check), then a start offset other than the committed
/// length, then a running crc that does not match the copy's bytes plus the batch.
pub fn check_batch(
    view: &ReplicaView,
    fence: bool,
    batch: &Batch<'_>,
) -> Result<u64, ReplicaReject> {
    if view.sealed {
        return Err(ReplicaReject::Sealed);
    }
    if fence && batch.ballot < view.promised {
        return Err(ReplicaReject::Ballot {
            ballot: batch.ballot,
            promised: view.promised,
        });
    }
    if batch.start != view.committed {
        return Err(ReplicaReject::Offset {
            start: batch.start,
            committed: view.committed,
        });
    }
    let computed = crc32c::crc32c_append(view.copy_crc, batch.bytes);
    if computed != batch.running_crc {
        return Err(ReplicaReject::Crc {
            sent: batch.running_crc,
            computed,
        });
    }
    Ok(view.committed + u64::try_from(batch.bytes.len()).unwrap_or(u64::MAX))
}

/// The proposal a sealer makes from the promises of a majority (10.3 step 4, Choose), or `None`
/// when it makes none.
///
/// If any answer carries an accepted proposal, the one with the highest ballot is re-proposed.
/// Otherwise, among the `q` copies that answered with `verified == committed`, the proposal is
/// the `r`-th longest, `r = max(1, q - replicas + min_acks)`; it is never below `floor` (the
/// sealer's own last acknowledged offset), and `None` when no copy answered or the rule would
/// put it below the floor. The variants change this: `IgnoreAccepted` skips the first step;
/// `Longest` counts copies with damage too and takes the longest, with the crc the copy
/// reports.
#[must_use]
pub fn choose(
    promises: &BTreeMap<NodeId, Answer>,
    replicas: u8,
    min_acks: u8,
    floor: u64,
    variant: Variant,
) -> Option<Prop> {
    if variant != Variant::IgnoreAccepted {
        let top = promises
            .values()
            .filter_map(|a| a.accepted)
            .max_by_key(|(ballot, _)| *ballot);
        if let Some((_, prop)) = top {
            return Some(prop);
        }
    }
    let longest = variant == Variant::Longest;
    let mut lens: Vec<(u64, u32)> = promises
        .values()
        .filter_map(|a| a.copy)
        .filter(|c| longest || c.verified == c.committed)
        .map(|c| (c.committed, c.crc))
        .collect();
    lens.sort_unstable_by(|a, b| b.cmp(a));
    let q = lens.len();
    let r = if longest {
        1
    } else {
        (q + usize::from(min_acks))
            .saturating_sub(usize::from(replicas))
            .max(1)
    };
    let (length, crc) = *lens.get(r - 1)?;
    (length >= floor).then_some(Prop { length, crc })
}
