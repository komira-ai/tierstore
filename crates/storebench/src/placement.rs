//! C-7, placement, offline: where the real key set and new stripes land on the storage nodes
//! (design `docs/design/fetch-cache-serve.md`, sections 10.2, 10.4, 10.5, 10.15 and 21).
//!
//! No tier code runs: the rendezvous rank of a key, the node order and the drive choice of a
//! stripe are computed here from the rules the design states, for both deployment profiles
//! ([`Profile::later`], [`Profile::now`], and NOW's first state before erasure coding ships,
//! [`Profile::now_replicas`]: two whole copies of a block, modelled as a `1+1` code), with zero,
//! one and two nodes down.
//!
//! Three questions are answered per profile:
//!
//! 1. **Keys** ([`tally_keys`]): for each choice of nodes down, how many keys and bytes of the
//!    listing each node writes (first live node of the key's rank) and holds a replica of (first
//!    `replicas` live nodes), and for a profile whose replica set is smaller than the cluster,
//!    how the replica sets (unordered) share the keys.
//! 2. **New stripes** ([`place_population`]): a population of stripes placed one after another
//!    (free bytes drop by one block per shard), for each choice of nodes down: how many are placed
//!    and refused, and how evenly the shards fill nodes and drives. Two populations: the stripes
//!    that silo's logical bytes make ([`silo_stripes`]) and the stripes that fill the capacity
//!    guard of design 10.15 ([`guard_stripes`]). Fill is the file system's (design 10.4), so
//!    bytes on a drive that are not the tier's count against `fill_limit`:
//!    [`Profile::shared_bytes_per_drive`] seeds every drive's used bytes and is taken off the
//!    guard. It is 0 by default: nothing else is on the drives of either profile now that silo
//!    has left the NOW drives (design 21.1).
//! 3. **Existing stripes under failure** ([`failure`]): the stripes placed with every node live,
//!    then nodes go down: how many shards each stripe keeps beyond `k` (negative: unreadable)
//!    and what share of the data shards sit on the down nodes (the share of cold reads that are
//!    degraded).
//!
//! # Encodings (this measurement's, not frozen formats)
//!
//! - `H(x)` is the first 8 bytes of `blake3(x)` read as a little-endian `u64`.
//! - A node id is one byte, a drive id is two bytes (node id, drive index), a stripe id is its
//!   `u64` in little-endian; `H` hashes the concatenation `key || node`, `stripe || node` or
//!   `stripe || node || drive`.
//! - Node score of a key: `H(key || node)`; rank: nodes by descending score (ties, which
//!   a 64-bit hash makes unlikely, by ascending id). The writer is the first live node of the
//!   rank; the replica set is the first `replicas` live nodes. When the set is smaller than the
//!   cluster (NOW) the design's writer is a fixed rule of the set that task T-S1 defines (10.2);
//!   the first live node of the rank stands in for it, and the tables say so.
//! - A drive id is the node and the drive's index, where the tier's is a UUID, and a stripe id
//!   is a counter, where the tier's carries the allocating node: the distributions are the
//!   design's, the individual placements are not.
//! - Node order of a stripe: live nodes by descending `H(stripe || node)`; shard `i` goes to
//!   `order[i mod live]`, so the data shards (indexes `0..k`) rotate over the nodes with the
//!   order.
//! - Drive score: `-free_bytes / ln(u)` with `u = (H(stripe || node || drive) + 0.5) / 2^64`, so
//!   `u` is never 0 or 1. A node's drives that are below `fill_limit` take the shards, best
//!   score first; a drive holds at most one shard of a stripe.
//!
//! # Refusals
//!
//! A stripe is refused ([`Refusal`]) when the code cannot survive a node loss in the profile
//! (`m < max_shards_per_domain`), when a live node would hold more than `m` shards of it (the
//! loss of that node would leave fewer than `k`), when no node is live, or when a node lacks
//! drives below `fill_limit` for its shards. The cap with nodes down is `m`, as the design's
//! placement test reads it ("with one node down `4+4` places 4 + 4", section 14), not
//! `max_shards_per_domain`, which the design also states as a write-time invariant (10.4): under
//! that reading every new stripe is refused with a node down on both profiles.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::listing::Object;

/// Where a shard is: its node and the drive's index within the node.
pub type Slot = (u8, u8);

/// 2^64 as a float.
const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;

/// One gibibyte, the size of a shard.
pub const GIB: u64 = 1 << 30;

/// The flags of `storebench c7`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The listing of silo's objects (written by `list`).
    #[arg(long)]
    pub listing: PathBuf,
    /// Where the summary JSON is written.
    #[arg(long)]
    pub out: PathBuf,
    /// Bytes on each drive that are not the tier's (none by default: nothing else is on the
    /// drives of either profile).
    #[arg(long, default_value_t = 0)]
    pub shared_bytes_per_drive: u64,
}

/// A deployment profile of design section 21: the hardware and the redundancy placed on it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// The profile's name (`LATER`, `NOW`).
    pub name: String,
    /// Storage nodes.
    pub nodes: u8,
    /// Cold drives per node.
    pub drives_per_node: u8,
    /// Bytes of one drive.
    pub drive_bytes: u64,
    /// Copies of a key in the hot tier.
    pub replicas: usize,
    /// Data shards of a stripe.
    pub k: usize,
    /// Parity shards of a stripe.
    pub m: usize,
    /// The most shards of a stripe one node may hold.
    pub max_shards_per_domain: usize,
    /// A drive above this fraction full takes no new shard.
    pub fill_limit: f64,
    /// Drives of free space held back for a rebuild (design 10.15).
    pub reserve_drives: u64,
    /// Bytes on each drive that are not the tier's (0 by default).
    #[serde(default)]
    pub shared_bytes_per_drive: u64,
    /// Bytes of one shard.
    pub block_bytes: u64,
}

impl Profile {
    /// LATER: three storage nodes of 15 drives of 6 TB, three hot replicas, 4+4.
    #[must_use]
    pub fn later() -> Self {
        Self {
            name: "LATER".to_owned(),
            nodes: 3,
            drives_per_node: 15,
            drive_bytes: 6_000_000_000_000,
            replicas: 3,
            k: 4,
            m: 4,
            max_shards_per_domain: 3,
            fill_limit: 0.85,
            reserve_drives: 2,
            shared_bytes_per_drive: 0,
            block_bytes: GIB,
        }
    }

    /// NOW: six combined nodes of 2 drives of 8 TB, two hot replicas, 4+2.
    #[must_use]
    pub fn now() -> Self {
        Self {
            name: "NOW".to_owned(),
            nodes: 6,
            drives_per_node: 2,
            drive_bytes: 8_000_000_000_000,
            replicas: 2,
            k: 4,
            m: 2,
            max_shards_per_domain: 2,
            fill_limit: 0.85,
            reserve_drives: 1,
            shared_bytes_per_drive: 0,
            block_bytes: GIB,
        }
    }

    /// NOW before erasure coding ships (milestone S2, design 21.1): `cold.replicas = 2`, two
    /// whole copies of a block on two nodes, which is a `1+1` code with one shard per node.
    #[must_use]
    pub fn now_replicas() -> Self {
        Self {
            name: "NOW, cold.replicas = 2".to_owned(),
            k: 1,
            m: 1,
            max_shards_per_domain: 1,
            ..Self::now()
        }
    }

    /// The built-in profiles: LATER, NOW, and NOW before erasure coding.
    #[must_use]
    pub fn all() -> Vec<Self> {
        vec![Self::later(), Self::now(), Self::now_replicas()]
    }

    /// The cold layout as a table cell: `k+m`, or `n copies` for a `1+m` code.
    #[must_use]
    pub fn layout(&self) -> String {
        if self.k == 1 {
            format!("{} copies", self.m + 1)
        } else {
            format!("{}+{}", self.k, self.m)
        }
    }

    /// Shards of a stripe, `k + m`.
    #[must_use]
    pub fn shards(&self) -> usize {
        self.k + self.m
    }

    /// Bytes of a stripe on the drives, `(k + m)` blocks.
    #[must_use]
    pub fn stripe_bytes(&self) -> u64 {
        self.shards() as u64 * self.block_bytes
    }

    /// Whether the code survives the loss of a node: `m >= max_shards_per_domain`.
    ///
    /// # Errors
    /// [`Refusal::DomainRule`] when it does not (a 10+4 code on 3 nodes).
    pub fn check(&self) -> Result<(), Refusal> {
        if self.max_shards_per_domain > self.m {
            return Err(Refusal::DomainRule {
                max_shards_per_domain: self.max_shards_per_domain,
                m: self.m,
            });
        }
        Ok(())
    }

    /// The drives of the cluster, `nodes x drives_per_node`.
    #[must_use]
    pub fn drives(&self) -> usize {
        usize::from(self.nodes) * usize::from(self.drives_per_node)
    }

    /// The index of `slot` in a per-drive table.
    #[must_use]
    pub fn index(&self, slot: Slot) -> usize {
        usize::from(slot.0) * usize::from(self.drives_per_node) + usize::from(slot.1)
    }
}

/// Why a stripe was not placed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum Refusal {
    /// The profile's `max_shards_per_domain` exceeds `m`: losing a node could leave fewer than
    /// `k` shards.
    #[error("{max_shards_per_domain} shards per node exceeds the {m} the code tolerates losing")]
    DomainRule {
        /// Shards one node may hold.
        max_shards_per_domain: usize,
        /// Parity shards.
        m: usize,
    },
    /// No node is live.
    #[error("no node is live")]
    NoLiveNodes,
    /// A live node would hold more than `m` shards.
    #[error("a node would hold {per_node} shards, more than the {m} the code tolerates losing")]
    NodeLoss {
        /// The most shards a live node would hold.
        per_node: usize,
        /// Parity shards.
        m: usize,
    },
    /// A node has fewer drives below `fill_limit` than shards to place on it.
    #[error("node {node} needs {needed} drives below the fill limit and has {eligible}")]
    DriveShortage {
        /// The node.
        node: u8,
        /// Shards to place on it.
        needed: usize,
        /// Its drives below the fill limit.
        eligible: usize,
    },
}

// ---------------------------------------------------------------------------------------------
// Hashing, rank and writer
// ---------------------------------------------------------------------------------------------

/// `H` of the concatenation of `parts`: the first 8 bytes of its blake3, little-endian.
#[must_use]
pub fn hash(parts: &[&[u8]]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    let [a, b, c, d, e, f, g, h, ..] = *hasher.finalize().as_bytes();
    u64::from_le_bytes([a, b, c, d, e, f, g, h])
}

/// The score of `node` for `key`: `H(key || node)`.
#[must_use]
pub fn score(key: &[u8], node: u8) -> u64 {
    hash(&[key, &[node]])
}

/// The nodes `0..nodes` by descending score for `key`.
#[must_use]
pub fn rank(key: &[u8], nodes: u8) -> Vec<u8> {
    let mut scored: Vec<(u64, u8)> = (0..nodes).map(|n| (score(key, n), n)).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, n)| n).collect()
}

/// The replica set of a key whose rank is `rank` with the nodes `down`: its first `replicas`
/// live nodes, in rank order.
#[must_use]
pub fn replica_set(rank: &[u8], down: &[u8], replicas: usize) -> Vec<u8> {
    rank.iter()
        .copied()
        .filter(|n| !down.contains(n))
        .take(replicas)
        .collect()
}

/// A number as a float (counts and byte totals here are far below 2^53).
fn float(x: u64) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "counts and sums below 2^53")]
    let f = x as f64;
    f
}

/// `part / whole`, 0 when `whole` is 0.
fn share(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        float(part) / float(whole)
    }
}

// ---------------------------------------------------------------------------------------------
// Keys under rank
// ---------------------------------------------------------------------------------------------

/// How many keys and bytes each node writes and replicates for one choice of nodes down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tally {
    /// The nodes down.
    pub down: Vec<u8>,
    /// Keys counted.
    pub keys: u64,
    /// Bytes of the keys counted.
    pub bytes: u64,
    /// Per node: keys it writes.
    pub writer_keys: Vec<u64>,
    /// Per node: bytes of the keys it writes.
    pub writer_bytes: Vec<u64>,
    /// Per node: keys it holds a replica of.
    pub replica_keys: Vec<u64>,
    /// Per node: bytes of the keys it holds a replica of.
    pub replica_bytes: Vec<u64>,
    /// Per replica set (a bit mask of its nodes): keys and bytes.
    pub sets: BTreeMap<u32, (u64, u64)>,
}

impl Tally {
    /// An empty tally for `nodes` nodes with `down` down.
    #[must_use]
    pub fn new(nodes: u8, down: &[u8]) -> Self {
        let zeros = vec![0u64; usize::from(nodes)];
        Self {
            down: down.to_vec(),
            keys: 0,
            bytes: 0,
            writer_keys: zeros.clone(),
            writer_bytes: zeros.clone(),
            replica_keys: zeros.clone(),
            replica_bytes: zeros,
            sets: BTreeMap::new(),
        }
    }

    /// Counts one key of `size` bytes whose rank is `rank`, with `replicas` copies.
    pub fn add(&mut self, rank: &[u8], size: u64, replicas: usize) {
        self.keys += 1;
        self.bytes += size;
        let set = replica_set(rank, &self.down, replicas);
        let mut mask = 0u32;
        for &node in &set {
            let i = usize::from(node);
            self.replica_keys[i] += 1;
            self.replica_bytes[i] += size;
            mask |= 1 << node;
        }
        if let Some(&writer) = set.first() {
            self.writer_keys[usize::from(writer)] += 1;
            self.writer_bytes[usize::from(writer)] += size;
            let entry = self.sets.entry(mask).or_default();
            entry.0 += 1;
            entry.1 += size;
        }
    }
}

/// Counts `keys` (key, size) for every choice of nodes down in `downs`, ranking each key once.
#[must_use]
pub fn tally_keys<'a>(
    keys: impl IntoIterator<Item = (&'a str, u64)>,
    nodes: u8,
    replicas: usize,
    downs: &[Vec<u8>],
) -> Vec<Tally> {
    let mut tallies: Vec<Tally> = downs.iter().map(|d| Tally::new(nodes, d)).collect();
    for (key, size) in keys {
        let order = rank(key.as_bytes(), nodes);
        for tally in &mut tallies {
            tally.add(&order, size, replicas);
        }
    }
    tallies
}

/// Every choice of at most `max_down` (0, 1 or 2) of `nodes` nodes down, fewest first, each in
/// ascending node order.
#[must_use]
pub fn down_sets(nodes: u8, max_down: usize) -> Vec<Vec<u8>> {
    let mut sets = vec![Vec::new()];
    if max_down >= 1 {
        sets.extend((0..nodes).map(|a| vec![a]));
    }
    if max_down >= 2 {
        for a in 0..nodes {
            sets.extend((a + 1..nodes).map(|b| vec![a, b]));
        }
    }
    sets
}

/// The smallest, largest and mean of a set of shares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Spread {
    /// The smallest.
    pub min: f64,
    /// The largest.
    pub max: f64,
    /// The mean.
    pub mean: f64,
}

/// The [`Spread`] of `values` (all zero for no values).
#[must_use]
pub fn spread(values: &[f64]) -> Spread {
    if values.is_empty() {
        return Spread::default();
    }
    Spread {
        min: values.iter().copied().fold(f64::INFINITY, f64::min),
        max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        mean: values.iter().sum::<f64>() / float(values.len() as u64),
    }
}

/// The coefficient of variation (standard deviation over mean, of the population) of `values`;
/// 0 for no values or a mean of 0.
#[must_use]
pub fn cv(values: &[f64]) -> f64 {
    let mean = spread(values).mean;
    if mean == 0.0 {
        return 0.0;
    }
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / float(values.len() as u64);
    var.sqrt() / mean
}

/// What one choice of nodes down does to the key set: shares of the keys and bytes, over the
/// live nodes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyScenario {
    /// The nodes down.
    pub down: Vec<u8>,
    /// Keys counted.
    pub keys: u64,
    /// Bytes counted.
    pub bytes: u64,
    /// Share of the keys each live node writes.
    pub writer_keys: Spread,
    /// Share of the bytes each live node writes.
    pub writer_bytes: Spread,
    /// Share of the keys each live node holds a replica of.
    pub replica_keys: Spread,
    /// Share of the bytes each live node holds a replica of.
    pub replica_bytes: Spread,
    /// Share of the keys of each possible replica set (only when the set is smaller than the
    /// cluster).
    pub set_keys: Option<Spread>,
    /// Share of the bytes of each possible replica set.
    pub set_bytes: Option<Spread>,
}

/// The shares of one [`Tally`] under `profile`.
#[must_use]
pub fn key_scenario(profile: &Profile, tally: &Tally) -> KeyScenario {
    let live: Vec<usize> = (0..profile.nodes)
        .filter(|n| !tally.down.contains(n))
        .map(usize::from)
        .collect();
    let of = |counts: &[u64], whole: u64| -> Spread {
        let shares: Vec<f64> = live.iter().map(|&i| share(counts[i], whole)).collect();
        spread(&shares)
    };
    let sets = (profile.replicas < usize::from(profile.nodes)).then(|| {
        let size = profile.replicas.min(live.len());
        let down_mask: u32 = tally.down.iter().fold(0, |m, &n| m | (1 << n));
        let possible: Vec<u32> = (0..1u32 << profile.nodes)
            .filter(|mask| mask.count_ones() as usize == size && mask & down_mask == 0)
            .collect();
        let of_set = |pick: fn(&(u64, u64)) -> u64, whole: u64| {
            let shares: Vec<f64> = possible
                .iter()
                .map(|mask| share(tally.sets.get(mask).map_or(0, pick), whole))
                .collect();
            spread(&shares)
        };
        (of_set(|s| s.0, tally.keys), of_set(|s| s.1, tally.bytes))
    });
    KeyScenario {
        down: tally.down.clone(),
        keys: tally.keys,
        bytes: tally.bytes,
        writer_keys: of(&tally.writer_keys, tally.keys),
        writer_bytes: of(&tally.writer_bytes, tally.bytes),
        replica_keys: of(&tally.replica_keys, tally.keys),
        replica_bytes: of(&tally.replica_bytes, tally.bytes),
        set_keys: sets.map(|s| s.0),
        set_bytes: sets.map(|s| s.1),
    }
}

// ---------------------------------------------------------------------------------------------
// Stripes
// ---------------------------------------------------------------------------------------------

/// The score of a drive for a stripe: `-free_bytes / ln(u)`, `u` in the open interval (0, 1).
#[must_use]
pub fn drive_score(stripe: u64, node: u8, drive: u8, free_bytes: u64) -> f64 {
    let h = hash(&[&stripe.to_le_bytes(), &[node, drive]]);
    #[expect(
        clippy::cast_precision_loss,
        reason = "a 64-bit hash mapped to the open unit interval"
    )]
    let u = (h as f64 + 0.5) / TWO_POW_64;
    -float(free_bytes) / u.ln()
}

/// The live nodes by descending `H(stripe || node)`: shard `i` of the stripe goes to
/// `order[i mod order.len()]`.
#[must_use]
pub fn node_order(stripe: u64, live: &[u8]) -> Vec<u8> {
    let mut order = live.to_vec();
    order.sort_by_key(|&n| (Reverse(hash(&[&stripe.to_le_bytes(), &[n]])), n));
    order
}

/// The drives of `node` below the fill limit, best score for `stripe` first, given `used` bytes
/// per drive of the cluster.
fn eligible_drives(profile: &Profile, used: &[u64], stripe: u64, node: u8) -> Vec<u8> {
    let mut scored: Vec<(f64, u8)> = (0..profile.drives_per_node)
        .filter_map(|drive| {
            let u = used[profile.index((node, drive))];
            let fill = float(u) / float(profile.drive_bytes);
            (fill < profile.fill_limit).then(|| {
                let free = profile.drive_bytes.saturating_sub(u);
                (drive_score(stripe, node, drive, free), drive)
            })
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, drive)| drive).collect()
}

/// Places the shards of stripe `stripe` on the live nodes (every node but `down`), given the
/// bytes `used` on each drive (indexed by [`Profile::index`]). Returns the slot of shard 0, 1,
/// ... in order.
///
/// # Errors
/// [`Refusal`] when the profile cannot survive a node loss, no node is live, a live node would
/// hold more than `m` shards, or a node lacks drives below the fill limit.
pub fn place_stripe(
    profile: &Profile,
    used: &[u64],
    down: &[u8],
    stripe: u64,
) -> Result<Vec<Slot>, Refusal> {
    profile.check()?;
    let live: Vec<u8> = (0..profile.nodes).filter(|n| !down.contains(n)).collect();
    if live.is_empty() {
        return Err(Refusal::NoLiveNodes);
    }
    let order = node_order(stripe, &live);
    let n = profile.shards();
    let per_node = n.div_ceil(order.len());
    if per_node > profile.m {
        return Err(Refusal::NodeLoss {
            per_node,
            m: profile.m,
        });
    }
    let mut chosen: Vec<Vec<u8>> = Vec::with_capacity(order.len());
    for (pos, &node) in order.iter().enumerate() {
        let needed = (n + order.len() - 1 - pos) / order.len();
        let drives = eligible_drives(profile, used, stripe, node);
        if drives.len() < needed {
            return Err(Refusal::DriveShortage {
                node,
                needed,
                eligible: drives.len(),
            });
        }
        chosen.push(drives);
    }
    Ok((0..n)
        .map(|i| {
            (
                order[i % order.len()],
                chosen[i % order.len()][i / order.len()],
            )
        })
        .collect())
}

/// The stripes that `logical_bytes` of silo make: `ceil(L / (k x block))`.
#[must_use]
pub fn silo_stripes(profile: &Profile, logical_bytes: u64) -> u64 {
    let data = profile.k as u64 * profile.block_bytes;
    logical_bytes.div_ceil(data)
}

/// The stripes that fill the capacity guard of design 10.15: `fill_limit` of the raw bytes of
/// every node but one, less the rebuild reserve and less the bytes on those drives that are not the tier's
/// ([`Profile::shared_bytes_per_drive`]), over the bytes of a stripe, rounded down.
#[must_use]
pub fn guard_stripes(profile: &Profile) -> u64 {
    let drives = u64::from(profile.nodes.saturating_sub(1)) * u64::from(profile.drives_per_node);
    let raw = drives.saturating_sub(profile.reserve_drives) * profile.drive_bytes;
    let guard = float(raw) * profile.fill_limit;
    let shared = float(drives.saturating_mul(profile.shared_bytes_per_drive));
    let guard = (guard - shared).max(0.0);
    let stripes = (guard / float(profile.stripe_bytes())).floor();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative count of stripes far below 2^64"
    )]
    let n = stripes as u64;
    n
}

/// What placing a population of stripes did, for one choice of nodes down.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacementStats {
    /// The nodes down.
    pub down: Vec<u8>,
    /// Stripes offered.
    pub stripes: u64,
    /// Stripes placed.
    pub placed: u64,
    /// Stripes refused.
    pub refused: u64,
    /// Why the first refused stripe was refused.
    pub first_refusal: Option<Refusal>,
    /// Fewest shards on a live node.
    pub node_shards_min: u64,
    /// Most shards on a live node.
    pub node_shards_max: u64,
    /// Fewest shards on a drive of a live node.
    pub drive_shards_min: u64,
    /// Most shards on a drive of a live node.
    pub drive_shards_max: u64,
    /// Mean shards per drive of a live node.
    pub drive_shards_mean: f64,
    /// Coefficient of variation of shards per drive of a live node.
    pub drive_shards_cv: f64,
    /// Fill fraction of the emptiest live drive.
    pub fill_min: f64,
    /// Fill fraction of the fullest live drive.
    pub fill_max: f64,
}

/// Places stripes `0..stripes` one after another with the nodes `down`; returns the statistics
/// and the slots of every placed stripe (in stripe order).
#[must_use]
pub fn place_population(
    profile: &Profile,
    down: &[u8],
    stripes: u64,
) -> (PlacementStats, Vec<Vec<Slot>>) {
    let mut used = vec![profile.shared_bytes_per_drive; profile.drives()];
    let mut shards = vec![0u64; profile.drives()];
    let mut placements = Vec::new();
    let mut first_refusal = None;
    for stripe in 0..stripes {
        match place_stripe(profile, &used, down, stripe) {
            Ok(slots) => {
                for &slot in &slots {
                    let i = profile.index(slot);
                    used[i] += profile.block_bytes;
                    shards[i] += 1;
                }
                placements.push(slots);
            }
            Err(refusal) => {
                first_refusal.get_or_insert(refusal);
            }
        }
    }
    let placed = placements.len() as u64;
    let live: Vec<u8> = (0..profile.nodes).filter(|n| !down.contains(n)).collect();
    let per_node: Vec<u64> = live
        .iter()
        .map(|&n| {
            (0..profile.drives_per_node)
                .map(|d| shards[profile.index((n, d))])
                .sum()
        })
        .collect();
    let per_drive: Vec<u64> = live
        .iter()
        .flat_map(|&n| (0..profile.drives_per_node).map(move |d| (n, d)))
        .map(|slot| shards[profile.index(slot)])
        .collect();
    let as_floats: Vec<f64> = per_drive.iter().map(|&s| float(s)).collect();
    let fills: Vec<f64> = per_drive
        .iter()
        .map(|&s| {
            float(s * profile.block_bytes + profile.shared_bytes_per_drive)
                / float(profile.drive_bytes)
        })
        .collect();
    let drive = spread(&as_floats);
    let fill = spread(&fills);
    let stats = PlacementStats {
        down: down.to_vec(),
        stripes,
        placed,
        refused: stripes - placed,
        first_refusal,
        node_shards_min: per_node.iter().copied().min().unwrap_or(0),
        node_shards_max: per_node.iter().copied().max().unwrap_or(0),
        drive_shards_min: per_drive.iter().copied().min().unwrap_or(0),
        drive_shards_max: per_drive.iter().copied().max().unwrap_or(0),
        drive_shards_mean: drive.mean,
        drive_shards_cv: cv(&as_floats),
        fill_min: fill.min,
        fill_max: fill.max,
    };
    (stats, placements)
}

/// A population of stripes placed for every choice of at most two nodes down.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Population {
    /// What the population is (`L_silo`, `capacity guard`).
    pub name: String,
    /// Stripes in it.
    pub stripes: u64,
    /// One entry per choice of nodes down, none first.
    pub scenarios: Vec<PlacementStats>,
}

/// What losing nodes does to stripes already placed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    /// The nodes down.
    pub down: Vec<u8>,
    /// Stripes placed.
    pub stripes: u64,
    /// Surviving shards minus `k` (negative: unreadable) and how many stripes have it, ascending.
    pub margins: Vec<(i64, u64)>,
    /// Stripes with fewer than `k` shards left.
    pub unreadable: u64,
    /// The share of data shards (indexes `0..k`) on down nodes: the share of cold reads that are
    /// degraded.
    pub degraded_share: f64,
}

/// What the loss of `down` does to `placements` (stripes placed with every node live).
#[must_use]
pub fn failure(profile: &Profile, placements: &[Vec<Slot>], down: &[u8]) -> Failure {
    let mut margins: BTreeMap<i64, u64> = BTreeMap::new();
    let mut degraded = 0u64;
    for slots in placements {
        let surviving = slots.iter().filter(|s| !down.contains(&s.0)).count();
        *margins
            .entry(surviving as i64 - profile.k as i64)
            .or_default() += 1;
        degraded += slots
            .iter()
            .take(profile.k)
            .filter(|s| down.contains(&s.0))
            .count() as u64;
    }
    let stripes = placements.len() as u64;
    Failure {
        down: down.to_vec(),
        stripes,
        unreadable: margins.range(..0).map(|(_, n)| n).sum(),
        margins: margins.into_iter().collect(),
        degraded_share: share(degraded, stripes * profile.k as u64),
    }
}

// ---------------------------------------------------------------------------------------------
// The summary
// ---------------------------------------------------------------------------------------------

/// Everything C-7 found for one profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileSummary {
    /// The profile.
    pub profile: Profile,
    /// The key set under rank, for zero, one and two nodes down.
    pub keys: Vec<KeyScenario>,
    /// The populations of new stripes: `L_silo`, then the capacity guard.
    pub populations: Vec<Population>,
    /// The `L_silo` stripes placed with every node live, then one and two nodes lost.
    pub failures: Vec<Failure>,
}

/// The summary of the measurement: the listing's size and one entry per profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Objects in the listing.
    pub objects: u64,
    /// Their bytes, `L_silo`.
    pub bytes: u64,
    /// One entry per profile.
    pub profiles: Vec<ProfileSummary>,
}

/// The summary of `objects` for `profiles`.
#[must_use]
pub fn summarize(objects: &[Object], profiles: &[Profile]) -> Summary {
    let bytes_total: u64 = objects.iter().map(|o| o.size).sum();
    let profiles = profiles
        .iter()
        .enumerate()
        .map(|(i, profile)| {
            // The keys depend only on the nodes and the replicas: a profile that repeats an
            // earlier one's has no key tally of its own.
            let repeats = profiles[..i]
                .iter()
                .any(|p| (p.nodes, p.replicas) == (profile.nodes, profile.replicas));
            let keys = if repeats {
                Vec::new()
            } else {
                tally_keys(
                    objects.iter().map(|o| (o.key.as_str(), o.size)),
                    profile.nodes,
                    profile.replicas,
                    &down_sets(profile.nodes, 2),
                )
                .iter()
                .map(|t| key_scenario(profile, t))
                .collect()
            };
            let downs = down_sets(profile.nodes, 2);
            let silo = silo_stripes(profile, bytes_total);
            let mut populations = Vec::new();
            let mut failures = Vec::new();
            for (name, stripes) in [("L_silo", silo), ("capacity guard", guard_stripes(profile))] {
                let mut scenarios = Vec::new();
                let mut live_placements = Vec::new();
                for down in &downs {
                    let (stats, placements) = place_population(profile, down, stripes);
                    if down.is_empty() {
                        live_placements = placements;
                    }
                    scenarios.push(stats);
                }
                if name == "L_silo" {
                    failures = downs
                        .iter()
                        .filter(|d| !d.is_empty())
                        .map(|d| failure(profile, &live_placements, d))
                        .collect();
                }
                populations.push(Population {
                    name: name.to_owned(),
                    stripes,
                    scenarios,
                });
            }
            ProfileSummary {
                profile: profile.clone(),
                keys,
                populations,
                failures,
            }
        })
        .collect();
    Summary {
        objects: objects.len() as u64,
        bytes: bytes_total,
        profiles,
    }
}

pub use crate::placement_tables::{down_label, tables};

/// Runs `storebench c7`: reads the listing, writes the summary JSON to `--out` and the tables to
/// `stdout`.
///
/// # Errors
/// [`Error::Io`] when the listing cannot be read, the summary cannot be written or `stdout`
/// fails; [`Error::Parse`] for a bad listing line.
pub fn run(args: &Args, stdout: &mut dyn Write) -> Result<(), Error> {
    let objects = crate::listing::read(&args.listing)?;
    // The bytes that are not the tier's are on NOW's drives, in both of its states.
    let profiles: Vec<Profile> = Profile::all()
        .into_iter()
        .map(|p| {
            if p.nodes == Profile::now().nodes {
                Profile {
                    shared_bytes_per_drive: args.shared_bytes_per_drive,
                    ..p
                }
            } else {
                p
            }
        })
        .collect();
    let summary = summarize(&objects, &profiles);
    let json = serde_json::to_string_pretty(&summary)
        .map_err(|e| Error::io("serialize", &args.out, e.into()))?;
    std::fs::write(&args.out, json).map_err(|e| Error::io("write", &args.out, e))?;
    for table in tables(&summary) {
        writeln!(stdout, "{}", table.markdown())
            .map_err(|e| Error::io("write", "standard output", e))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "placement_tests.rs"]
mod tests;
