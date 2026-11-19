//! The model of C-10: an hourly replay of the tier's sealing, destaging, stripe formation and
//! freeing over a stream of objects that arrive and die (design 10.2, 10.3, 10.5).
//!
//! What it models, in the order an object meets it:
//!
//! - A **small** object (at most `large_min`) is a record appended to the open segment of its
//!   writer and expiry class. A segment seals when the next record would pass `seal_bytes`, or
//!   by age: past `seal_max_age` it seals only if it holds `seal_min_bytes`, else when it is
//!   older than `destage.max_age`.
//! - A **large** object is rounded up to the alignment and goes to the window of its writer and
//!   class. A window of `pack_window` objects (or one whose oldest object is `seal_max_age`
//!   old) is packed first-fit decreasing into pack blocks of `pack_block_bytes`; an object
//!   larger than a pack block is a chain: full pack blocks of its own and the rest in the window.
//! - A sealed block is **hot**. Once `destage.min_age` has passed, the eligible blocks of a
//!   class are sorted by length and each `k` consecutive ones become a stripe (`k` data blocks
//!   and `m` parity shards as long as the longest). A block still waiting `partial_after` after
//!   it sealed makes a partial stripe of the `j < k` eligible blocks that are left. A sealed hot
//!   block whose records are all dead is unlinked at once and never goes cold.
//! - A stripe is freed whole when every record of its data blocks is dead. The raw bytes it
//!   holds are `sum(aligned data lengths) + m * longest aligned length` (zeros are not stored).
//!
//! Time is whole hours since the Unix epoch; an object that dies at hour `d` is dead from the
//! step `d` on. The caller states the writer an object goes to (a hash of its key).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Milliseconds in an hour.
pub const HOUR_MS: u64 = 3_600_000;
/// Milliseconds in a day.
pub const DAY_MS: u64 = 24 * HOUR_MS;

const MIB: u64 = 1 << 20;

/// Every constant of the model. [`Policy::now`] and [`Policy::later`] hold the design's values
/// of the two deployment profiles (design section 21); a test sets its own with tiny blocks.
/// `data_blocks`, `nodes`, `hot_replicas`, `align`, `classes_max` and `pack_block_bytes` are at least 1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    /// The profile's name.
    pub name: String,
    /// Storage nodes (`nodes`).
    pub nodes: u64,
    /// Copies of a block on the hot tier (`hot.replicas`); with `nodes` it fixes the open segments
    /// per class, see [`Policy::open_segments`].
    pub hot_replicas: u64,
    /// Data blocks of a stripe (`k`).
    pub data_blocks: usize,
    /// Parity shards of a stripe (`m`).
    pub parity_blocks: usize,
    /// Size a segment may not pass (`seal_bytes`).
    pub seal_bytes: u64,
    /// Size a segment needs to seal by age past `seal_max_age` (`seal_min_bytes`).
    pub seal_min_bytes: u64,
    /// Hours after which a segment may seal by age (`seal_max_age`).
    pub seal_max_age_h: u64,
    /// Largest small object, in bytes (`large_min`).
    pub large_min: u64,
    /// Size of a pack block (1 GiB less 1 MiB).
    pub pack_block_bytes: u64,
    /// Large objects packed together (the window).
    pub pack_window: usize,
    /// Alignment of a pack item and of a shard (4 KiB).
    pub align: u64,
    /// Expiry classes (`gc.classes_max`); a larger class falls into class 0.
    pub classes_max: u64,
    /// Hours a sealed block waits before it may go cold (`destage.min_age`).
    pub destage_min_age_h: u64,
    /// Hours after which a short segment seals anyway (`destage.max_age`).
    pub destage_max_age_h: u64,
    /// Hours after which a block left over makes a partial stripe (`destage.partial_after`).
    pub partial_after_h: u64,
    /// The `gc.rewrite_below` values the rewrite cost is reported at.
    pub rewrite_below: Vec<f64>,
    /// Days replayed after the last arrival.
    pub tail_days: u64,
}

impl Policy {
    /// The values the two profiles share.
    fn common(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            nodes: 6,
            hot_replicas: 2,
            data_blocks: 4,
            parity_blocks: 2,
            seal_bytes: 1023 * MIB,
            seal_min_bytes: 256 * MIB,
            seal_max_age_h: 24,
            large_min: 4 * MIB,
            pack_block_bytes: 1024 * MIB - MIB,
            pack_window: 64,
            align: 4096,
            classes_max: 8,
            destage_min_age_h: 1,
            destage_max_age_h: 24,
            partial_after_h: 72,
            rewrite_below: vec![0.2, 0.3, 0.5],
            tail_days: 60,
        }
    }

    /// NOW (design 21): 6 nodes, 2 hot replicas, 4+2, `max_age` 1 d, `partial_after` 3 d.
    #[must_use]
    pub fn now() -> Self {
        Self::common("now")
    }

    /// LATER (design 21): 3 nodes, 3 hot replicas, 4+4, `max_age` 3 d, `partial_after` 7 d.
    #[must_use]
    pub fn later() -> Self {
        Self {
            nodes: 3,
            hot_replicas: 3,
            parity_blocks: 4,
            destage_max_age_h: 72,
            partial_after_h: 168,
            ..Self::common("later")
        }
    }

    /// Open segments per expiry class (design 10.2, 10.3): one per writer of each replica set.
    /// When every node holds a copy there is one replica set and `nodes` writers; otherwise a
    /// replica set is an unordered choice of `hot_replicas` of the `nodes`, each with one writer,
    /// so C(`nodes`, `hot_replicas`) (NOW: 15, LATER: 3).
    #[must_use]
    pub fn open_segments(&self) -> u64 {
        if self.hot_replicas >= self.nodes {
            return self.nodes;
        }
        let k = self.hot_replicas.min(self.nodes - self.hot_replicas);
        (1..=k).fold(1, |c, i| c * (self.nodes - k + i) / i)
    }

    /// `n` rounded up to the alignment.
    #[must_use]
    pub fn aligned(&self, n: u64) -> u64 {
        n.div_ceil(self.align) * self.align
    }
}

/// The class of an object that is expected to die at `death_ms`: the week it dies in (whole days
/// since the epoch divided by 7), wrapped into `classes_max` classes.
#[must_use]
pub fn expiry_class(death_ms: u64, classes_max: u64) -> u64 {
    (death_ms / DAY_MS / 7) % classes_max
}

/// One object of the replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// When it arrived, in milliseconds since the epoch.
    pub arrival_ms: u64,
    /// Its size in bytes (not zero).
    pub size: u64,
    /// When it dies, in milliseconds since the epoch (not before its arrival).
    pub death_ms: u64,
    /// The hash that picks its open segment: the segment is this modulo `open_segments`.
    pub writer_hash: u64,
    /// Its expiry class; one not below `classes_max` is class 0.
    pub class: u64,
}

/// A stripe as the rewrite cost sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripeView {
    /// The bytes of its data blocks.
    pub data: u64,
    /// The bytes of those that are live.
    pub live: u64,
    /// The raw bytes it holds (data shards and parity).
    pub raw: u64,
}

/// A day's reading of the cold tier.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Days since the first arrival.
    pub day: u64,
    /// Raw bytes held by the stripes that are not freed.
    pub cold_raw_bytes: u64,
    /// Live bytes in those stripes' data blocks.
    pub live_bytes: u64,
    /// `live_bytes / cold_raw_bytes` (0 when nothing is cold).
    pub cold_fill_efficiency: f64,
    /// Stripes not yet freed.
    pub stripes_live: u64,
    /// Stripes freed whole so far.
    pub stripes_freed: u64,
    /// Partial stripes formed so far.
    pub partial_stripes: u64,
    /// Sealed blocks unlinked hot because they were wholly dead before destage, so far.
    pub blocks_dropped_hot: u64,
}

/// Sealed blocks so far by size.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockSizes {
    /// Blocks smaller than `seal_min_bytes`.
    pub under_seal_min: u64,
    /// Blocks from `seal_min_bytes` up that could still take a record of `large_min`.
    pub partly_full: u64,
    /// Blocks within `large_min` of `seal_bytes`, or over it.
    pub full: u64,
}

/// What a rewrite of the stripes below one `rewrite_below` costs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rewrite {
    /// The `rewrite_below` value.
    pub below: f64,
    /// Stripes with a live fraction below it.
    pub stripes: u64,
    /// Their live bytes: what rewriting them writes.
    pub rewrite_bytes: u64,
    /// The raw bytes freed when they are rewritten.
    pub raw_freed: u64,
    /// The data bytes freed (the dead bytes of those stripes).
    pub logical_freed: u64,
    /// `rewrite_bytes / raw_freed`.
    pub per_raw_byte_freed: f64,
    /// `rewrite_bytes / logical_freed`: 0.43 for stripes 30% live.
    pub per_logical_byte_freed: f64,
}

/// The state of the cold tier at one moment, with what the daily samples do not carry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The cold tier's reading.
    pub sample: Sample,
    /// Blocks sealed so far by size.
    pub sealed_blocks: BlockSizes,
    /// Stripes not freed by live fraction: ten bins, `[0, 0.1)` to `[0.9, 1]`.
    pub live_fraction_deciles: Vec<u64>,
    /// The cost of a rewrite at each `rewrite_below`.
    pub rewrite: Vec<Rewrite>,
}

/// What a replay gives.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Replay {
    /// One reading per day from the first arrival.
    pub samples: Vec<Sample>,
    /// The state when the last object had arrived.
    pub end_of_data: Snapshot,
    /// The state when the replay ended.
    pub end_of_replay: Snapshot,
}

/// `num / den` (0 when `den` is 0).
#[must_use]
pub fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

/// The stripes by live fraction in ten bins; a fully live stripe is in the last.
#[must_use]
pub fn deciles(stripes: &[StripeView]) -> Vec<u64> {
    let mut bins = vec![0u64; 10];
    for s in stripes {
        let bin = (s.live * 10 / s.data.max(1)).min(9);
        bins[bin as usize] += 1;
    }
    bins
}

/// The cost of rewriting the stripes whose live fraction is below each of `below`.
#[must_use]
pub fn rewrite_costs(stripes: &[StripeView], below: &[f64]) -> Vec<Rewrite> {
    below
        .iter()
        .map(|&t| {
            let mut r = Rewrite {
                below: t,
                stripes: 0,
                rewrite_bytes: 0,
                raw_freed: 0,
                logical_freed: 0,
                per_raw_byte_freed: 0.0,
                per_logical_byte_freed: 0.0,
            };
            for s in stripes.iter().filter(|s| ratio(s.live, s.data) < t) {
                r.stripes += 1;
                r.rewrite_bytes += s.live;
                r.raw_freed += s.raw;
                r.logical_freed += s.data - s.live;
            }
            r.per_raw_byte_freed = ratio(r.rewrite_bytes, r.raw_freed);
            r.per_logical_byte_freed = ratio(r.rewrite_bytes, r.logical_freed);
            r
        })
        .collect()
}

/// A block of the hot tier, then a data block of a stripe.
struct Block {
    class: u64,
    bytes: u64,
    live: u64,
    sealed_h: u64,
    stripe: Option<usize>,
}

/// A stripe of the cold tier.
struct Stripe {
    view: StripeView,
    freed: bool,
}

/// A segment being written.
struct Segment {
    opened_h: u64,
    block: usize,
}

/// A window of large objects waiting to be packed: `(bytes, death hour)` each.
#[derive(Default)]
struct Window {
    oldest_h: u64,
    items: Vec<(u64, u64)>,
}

/// The writer and class of a segment or window.
type Slot = (u64, u64);

/// The state of a replay.
struct Sim<'a> {
    policy: &'a Policy,
    blocks: Vec<Block>,
    stripes: Vec<Stripe>,
    open: BTreeMap<Slot, Segment>,
    windows: BTreeMap<Slot, Window>,
    pool: BTreeMap<u64, Vec<usize>>,
    deaths: BTreeMap<u64, Vec<(usize, u64)>>,
    cold_raw: u64,
    cold_live: u64,
    stripes_live: u64,
    stripes_freed: u64,
    partial_stripes: u64,
    dropped_hot: u64,
    sealed: BlockSizes,
}

impl<'a> Sim<'a> {
    fn new(policy: &'a Policy) -> Self {
        Self {
            policy,
            blocks: Vec::new(),
            stripes: Vec::new(),
            open: BTreeMap::new(),
            windows: BTreeMap::new(),
            pool: BTreeMap::new(),
            deaths: BTreeMap::new(),
            cold_raw: 0,
            cold_live: 0,
            stripes_live: 0,
            stripes_freed: 0,
            partial_stripes: 0,
            dropped_hot: 0,
            sealed: BlockSizes::default(),
        }
    }

    /// A new empty block of `class`.
    fn new_block(&mut self, class: u64) -> usize {
        self.blocks.push(Block {
            class,
            bytes: 0,
            live: 0,
            sealed_h: 0,
            stripe: None,
        });
        self.blocks.len() - 1
    }

    /// Puts `bytes` that die at hour `death_h` into `block` at hour `now`.
    fn put(&mut self, block: usize, bytes: u64, death_h: u64, now: u64) {
        let b = &mut self.blocks[block];
        b.bytes += bytes;
        if death_h > now {
            b.live += bytes;
            self.deaths.entry(death_h).or_default().push((block, bytes));
        }
    }

    /// Seals `block` at hour `h`: counts its size and makes it wait for destage.
    fn seal(&mut self, block: usize, h: u64) {
        let p = self.policy;
        let b = &mut self.blocks[block];
        b.sealed_h = h;
        let sizes = &mut self.sealed;
        if b.bytes < p.seal_min_bytes {
            sizes.under_seal_min += 1;
        } else if b.bytes + p.large_min > p.seal_bytes {
            sizes.full += 1;
        } else {
            sizes.partly_full += 1;
        }
        self.pool.entry(b.class).or_default().push(block);
    }

    /// The object `item` arrives at hour `h`.
    fn arrive(&mut self, item: &Item, h: u64) {
        let p = self.policy;
        let class = if item.class < p.classes_max {
            item.class
        } else {
            0
        };
        let slot = (item.writer_hash % p.open_segments(), class);
        let death_h = item.death_ms / HOUR_MS;
        if item.size <= p.large_min {
            self.append_record(slot, item.size, death_h, h);
        } else {
            self.add_large(slot, item.size, death_h, h);
        }
    }

    /// A small record of `size` bytes goes to the open segment of `slot`; a segment it would
    /// not fit in seals first.
    fn append_record(&mut self, slot: Slot, size: u64, death_h: u64, h: u64) {
        let mut segment = self.open.remove(&slot);
        if let Some(s) = &segment
            && self.blocks[s.block].bytes + size > self.policy.seal_bytes
        {
            self.seal(s.block, h);
            segment = None;
        }
        let segment = segment.unwrap_or_else(|| Segment {
            opened_h: h,
            block: self.new_block(slot.1),
        });
        self.put(segment.block, size, death_h, h);
        self.open.insert(slot, segment);
    }

    /// A large object of `size` bytes goes to the window of `slot`: it takes whole pack blocks
    /// of its own when it is larger than one, and the rest waits in the window, which is packed
    /// when it is full.
    fn add_large(&mut self, slot: Slot, size: u64, death_h: u64, h: u64) {
        let p = self.policy;
        let aligned = p.aligned(size);
        for _ in 0..aligned / p.pack_block_bytes {
            let block = self.new_block(slot.1);
            self.put(block, p.pack_block_bytes, death_h, h);
            self.seal(block, h);
        }
        let rest = aligned % p.pack_block_bytes;
        if rest > 0 {
            let w = self.windows.entry(slot).or_default();
            if w.items.is_empty() {
                w.oldest_h = h;
            }
            w.items.push((rest, death_h));
            if w.items.len() >= p.pack_window {
                let full = std::mem::take(w);
                self.pack(slot.1, full, h);
            }
        }
    }

    /// Packs `window` (of `class`) first-fit decreasing into pack blocks and seals them.
    fn pack(&mut self, class: u64, mut window: Window, h: u64) {
        window
            .items
            .sort_by_key(|&(bytes, _)| std::cmp::Reverse(bytes));
        let capacity = self.policy.pack_block_bytes;
        let mut bins: Vec<(u64, Vec<(u64, u64)>)> = Vec::new();
        for item in window.items {
            match bins.iter_mut().find(|(used, _)| *used + item.0 <= capacity) {
                Some((used, items)) => {
                    *used += item.0;
                    items.push(item);
                }
                None => bins.push((item.0, vec![item])),
            }
        }
        for (_, items) in bins {
            let block = self.new_block(class);
            for (bytes, death_h) in items {
                self.put(block, bytes, death_h, h);
            }
            self.seal(block, h);
        }
    }

    /// Seals the segments and packs the windows that the age rules say at hour `h`.
    fn seal_by_age(&mut self, h: u64) {
        let p = self.policy;
        let (due, keep): (BTreeMap<Slot, Segment>, BTreeMap<Slot, Segment>) =
            std::mem::take(&mut self.open)
                .into_iter()
                .partition(|(_, s)| {
                    let age = h - s.opened_h;
                    age >= p.seal_max_age_h
                        && (self.blocks[s.block].bytes >= p.seal_min_bytes
                            || age >= p.destage_max_age_h)
                });
        self.open = keep;
        for s in due.into_values() {
            self.seal(s.block, h);
        }
        let (old, young): (BTreeMap<Slot, Window>, BTreeMap<Slot, Window>) =
            std::mem::take(&mut self.windows)
                .into_iter()
                .partition(|(_, w)| h - w.oldest_h >= p.seal_max_age_h);
        self.windows = young;
        for ((_, class), w) in old {
            self.pack(class, w, h);
        }
    }

    /// Applies the deaths due at hour `h`; a stripe with nothing live is freed whole.
    fn die(&mut self, h: u64) {
        let later = self.deaths.split_off(&(h + 1));
        let due = std::mem::replace(&mut self.deaths, later);
        for (block, bytes) in due.into_values().flatten() {
            let b = &mut self.blocks[block];
            b.live -= bytes;
            if let Some(i) = b.stripe {
                let s = &mut self.stripes[i];
                s.view.live -= bytes;
                self.cold_live -= bytes;
                if s.view.live == 0 {
                    s.freed = true;
                    self.cold_raw -= s.view.raw;
                    self.stripes_live -= 1;
                    self.stripes_freed += 1;
                }
            }
        }
    }

    /// Makes one stripe of the data blocks `blocks` (`partial` when there are fewer than `k`).
    fn stripe(&mut self, blocks: &[usize], partial: bool) {
        let p = self.policy;
        let lengths: Vec<u64> = blocks
            .iter()
            .map(|&b| p.aligned(self.blocks[b].bytes))
            .collect();
        let longest = lengths.iter().copied().max().unwrap_or(0);
        let raw = lengths.iter().sum::<u64>() + p.parity_blocks as u64 * longest;
        let data = blocks.iter().map(|&b| self.blocks[b].bytes).sum();
        let live = blocks.iter().map(|&b| self.blocks[b].live).sum();
        let index = self.stripes.len();
        for &b in blocks {
            self.blocks[b].stripe = Some(index);
        }
        self.stripes.push(Stripe {
            view: StripeView { data, live, raw },
            freed: false,
        });
        self.cold_raw += raw;
        self.cold_live += live;
        self.stripes_live += 1;
        self.partial_stripes += u64::from(partial);
    }

    /// Unlinks the sealed hot blocks that are wholly dead and destages the eligible ones.
    fn destage(&mut self, h: u64) {
        let p = self.policy;
        let classes: Vec<u64> = self.pool.keys().copied().collect();
        for class in classes {
            let waiting = self.pool.remove(&class).unwrap_or_default();
            let (alive, dead): (Vec<usize>, Vec<usize>) =
                waiting.into_iter().partition(|&b| self.blocks[b].live > 0);
            self.dropped_hot += dead.len() as u64;
            let (mut eligible, mut left): (Vec<usize>, Vec<usize>) = alive
                .into_iter()
                .partition(|&b| self.blocks[b].sealed_h + p.destage_min_age_h <= h);
            eligible.sort_by_key(|&b| (self.blocks[b].bytes, b));
            let rest = eligible.split_off(eligible.len() / p.data_blocks * p.data_blocks);
            for group in eligible.chunks(p.data_blocks) {
                self.stripe(group, false);
            }
            let due = rest
                .iter()
                .any(|&b| self.blocks[b].sealed_h + p.partial_after_h <= h);
            if due {
                self.stripe(&rest, true);
            } else {
                left.extend(rest);
            }
            self.pool.insert(class, left);
        }
    }

    /// The reading of the cold tier on day `day`.
    fn sample(&self, day: u64) -> Sample {
        Sample {
            day,
            cold_raw_bytes: self.cold_raw,
            live_bytes: self.cold_live,
            cold_fill_efficiency: ratio(self.cold_live, self.cold_raw),
            stripes_live: self.stripes_live,
            stripes_freed: self.stripes_freed,
            partial_stripes: self.partial_stripes,
            blocks_dropped_hot: self.dropped_hot,
        }
    }

    /// The whole state on day `day`.
    fn snapshot(&self, day: u64) -> Snapshot {
        let views: Vec<StripeView> = self
            .stripes
            .iter()
            .filter(|s| !s.freed)
            .map(|s| s.view)
            .collect();
        Snapshot {
            sample: self.sample(day),
            sealed_blocks: self.sealed.clone(),
            live_fraction_deciles: deciles(&views),
            rewrite: rewrite_costs(&views, &self.policy.rewrite_below),
        }
    }
}

/// Replays `items` (in any order) hour by hour from the first arrival to `tail_days` after the
/// last, and returns a reading per day and the state at the end of the data and of the replay.
/// No items give an empty replay.
#[must_use]
pub fn replay(policy: &Policy, mut items: Vec<Item>) -> Replay {
    items.sort_by_key(|i| i.arrival_ms);
    let (Some(first), Some(last)) = (items.first(), items.last()) else {
        return Replay::default();
    };
    let start = first.arrival_ms / HOUR_MS;
    let data_end = last.arrival_ms / HOUR_MS;
    let end = data_end + policy.tail_days * 24;
    let mut sim = Sim::new(policy);
    let mut out = Replay::default();
    let mut next = 0;
    for h in start..=end {
        while let Some(item) = items.get(next).filter(|i| i.arrival_ms / HOUR_MS <= h) {
            sim.arrive(item, h);
            next += 1;
        }
        sim.seal_by_age(h);
        sim.die(h);
        sim.destage(h);
        let day = (h - start) / 24;
        if (h - start).is_multiple_of(24) {
            out.samples.push(sim.sample(day));
        }
        if h == data_end {
            out.end_of_data = sim.snapshot(day);
        }
    }
    out.end_of_replay = sim.snapshot((end - start) / 24);
    out
}

#[cfg(test)]
#[path = "sim_tests.rs"]
mod tests;
