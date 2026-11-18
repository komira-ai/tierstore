//! `Placement`: where a key's replicas and a stripe's shards go (10.2, 10.4). T-S7 implements
//! it; T-S1 the writer rule of a replica set on top of `rank`.

use tierstore_format::descriptor::{Code, ShardLoc};
use tierstore_format::ids::{NodeId, StripeId};

use crate::view::View;

/// Why a stripe cannot be placed. Destage pauses and the hot tier absorbs (10.5); nothing is
/// placed on an unsafe layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    /// Fewer live domains than the code needs to keep `m >= max_shards_per_domain` (for example
    /// `4+2` with one of three nodes down).
    #[error("stripe {stripe}: {live} live domains cannot hold the code safely")]
    TooFewDomains {
        /// The stripe.
        stripe: StripeId,
        /// The live domains.
        live: usize,
    },
    /// A node has fewer `in` drives below `fill_limit` (and not holding another shard of the
    /// stripe) than the shards it must hold.
    #[error("stripe {stripe}: node {node} has too few drives below the fill limit")]
    TooFewDrives {
        /// The stripe.
        stripe: StripeId,
        /// The node.
        node: NodeId,
    },
}

/// Placement of keys and stripes.
///
/// `rank` orders the storage nodes by rendezvous hashing, `score(n) = H(K || node_id)` (blake3
/// reduced to `u64`, equal weights), highest first. `stripe` places the `k + m` shards (or `N`
/// copies) of a stripe: shard `i` on node `order[i mod domains]` with the nodes ordered by
/// `H(stripe_id || node_id)`, and within a node on the `in` drives below `fill_limit` that hold no
/// other shard of the stripe, by the weighted rendezvous score `-free / ln(H(stripe_id ||
/// drive_id) / 2^64)`. Every placement satisfies the invariants: shards on distinct drives, no
/// node above `max_shards_per_domain`, and, for the node domain, `m >= max_shards_per_domain`.
///
/// Section 14 rows: "Node rank and writer"; "Placement: distribution of shards over 45 drives
/// within tolerance for 1e5 stripes ... with one node down `4+4` places 4 + 4 and `4+2` reports
/// cannot place"; "Deployment profiles: ... `4+2` on six nodes places one shard per node and,
/// after a node is marked `out`, rebuilds with at most two per node".
pub trait Placement: Send + Sync {
    /// Every storage node of the view, highest rendezvous score for `key` first. Deterministic
    /// across processes; a node leaving moves only the keys it ranked among their first.
    fn rank(&self, key: &[u8], view: &View) -> Vec<NodeId>;

    /// The location of each shard of the stripe, shard `i` at index `i` (`k + m` entries, or `N`).
    fn stripe(
        &self,
        stripe: StripeId,
        code: Code,
        view: &View,
    ) -> Result<Vec<ShardLoc>, PlacementError>;
}
