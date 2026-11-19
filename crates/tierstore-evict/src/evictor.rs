//! [`Evictor`]: `evict(target)` in the order of section 7.

use std::collections::BTreeMap;
use std::sync::Arc;

use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::clock::Clock;
use tierstore_fetch_api::entry::{Entry, EntryError, EntryInfo, EntryStore};
use tierstore_fetch_api::ids::PackId;
use tierstore_fetch_api::page::GROUP_CELLS;
use tierstore_metrics::Cache;

use crate::config::{CELL_BYTES, EvictConfig};
use crate::lock::EvictorLock;
use crate::pause::Pause;
use crate::protocol::{self, Batch};

/// Why [`Evictor::evict`] did nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvictError {
    /// The grace window since process start has not passed (section 7): the mounts may not have
    /// rebound their holds yet.
    #[error("eviction waits for the grace window: {now_ms} ms of {until_ms} ms")]
    Grace {
        /// The clock now.
        now_ms: u64,
        /// The clock at which eviction may start.
        until_ms: u64,
    },
}

/// An entry the evictor could not finish.
#[derive(Debug, thiserror::Error)]
pub enum EvictFailure {
    /// Removing a whole unheld entry failed; it stays in the cache.
    #[error("removing {pack:?}: {source}")]
    Remove {
        /// The entry.
        pack: PackId,
        /// The store's error.
        #[source]
        source: EntryError,
    },
    /// The journaled punch of an entry's cells failed. The cells went to `absent` all the same
    /// (a later fetch overwrites whatever bytes are left), so none is served or resurrected.
    #[error("punching cells of {pack:?}: {source}")]
    Punch {
        /// The entry.
        pack: PackId,
        /// The entry's error.
        #[source]
        source: EntryError,
    },
}

/// What one [`Evictor::evict`] did.
#[derive(Debug, Default)]
pub struct EvictReport {
    /// The cache's use when the call began, in bytes (cells x [`CELL_BYTES`]).
    pub used_before: u64,
    /// The use the call left, as the evictor counts it: the use it began with less what it
    /// removed and punched. Cells fetched meanwhile are not in it.
    pub used_after: u64,
    /// Whole unheld entries removed (eviction order 1).
    pub entries_removed: u32,
    /// The use those entries had, in bytes.
    pub removed_bytes: u64,
    /// Cells of held entries taken `present -> evicting -> absent` (eviction order 2).
    pub cells_punched: u64,
    /// Bytes the punches released (a pack's short last cell counts its length).
    pub bytes_punched: u64,
    /// Journal `fsync`s of the punches: at most one per entry punched.
    pub journal_syncs: u32,
    /// Groups whose readers did not leave within `evict_wait`.
    pub groups_timed_out: u32,
    /// Cells put back `present` because their group stayed busy.
    pub cells_restored: u64,
    /// Entries the call could not finish.
    pub failures: Vec<EvictFailure>,
}

/// The node cache's evictor (section 7). There is one per cache directory: making one takes the
/// [`EvictorLock`].
pub struct Evictor {
    store: Arc<dyn EntryStore>,
    clock: Arc<dyn Clock>,
    pause: Arc<dyn Pause>,
    config: EvictConfig,
    metrics: Arc<Cache>,
    grace_until_ms: u64,
    _lock: EvictorLock,
}

impl std::fmt::Debug for Evictor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Evictor")
            .field("config", &self.config)
            .field("grace_until_ms", &self.grace_until_ms)
            .finish_non_exhaustive()
    }
}

/// A 16-cell group of an entry with `present` cells, a candidate of eviction order 2.
struct Candidate {
    epoch: u32,
    pack: PackId,
    entry: Arc<dyn Entry>,
    group: u32,
    cells: Vec<u32>,
}

impl Evictor {
    /// The evictor of `store`, made at process start: the grace window runs from the clock's
    /// reading now. `lock` is the cache directory's evictor lock.
    pub fn new(
        store: Arc<dyn EntryStore>,
        lock: EvictorLock,
        clock: Arc<dyn Clock>,
        pause: Arc<dyn Pause>,
        config: EvictConfig,
        metrics: Arc<Cache>,
    ) -> Self {
        let grace_until_ms = clock.now_ms().saturating_add(config.grace_ms);
        Self {
            store,
            clock,
            pause,
            config,
            metrics,
            grace_until_ms,
            _lock: lock,
        }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &EvictConfig {
        &self.config
    }

    /// Brings the cache's use down to `target` bytes, or as far as it can: whole unheld entries
    /// by LRU first, then the coldest groups of held entries by the protocol of 5.1. Use is
    /// `present + claimed + recheck` cells x [`CELL_BYTES`]; only `present` cells are evicted
    /// (a `claimed` cell is a fetch in flight, a `recheck` cell waits for its hash).
    ///
    /// Before the grace window has passed it does nothing and returns [`EvictError::Grace`].
    pub fn evict(&self, target: u64) -> Result<EvictReport, EvictError> {
        let now_ms = self.clock.now_ms();
        if now_ms < self.grace_until_ms {
            return Err(EvictError::Grace {
                now_ms,
                until_ms: self.grace_until_ms,
            });
        }
        let infos = self.store.entries();
        let holds: u64 = infos.iter().map(|i| u64::from(i.holders)).sum();
        self.metrics.holds.set((), holds as f64);

        let mut report = EvictReport {
            used_before: infos.iter().map(used_bytes).sum(),
            ..EvictReport::default()
        };
        let mut used = report.used_before;
        if used > target {
            used = self.remove_unheld(infos, used, target, &mut report);
        }
        if used > target {
            used = self.punch_groups(used - target, used, &mut report);
        }
        report.used_after = used;
        Ok(report)
    }

    /// Eviction order 1: whole unheld entries, least recently used first (ties in pack order),
    /// until `used` is at most `target`. The use left.
    fn remove_unheld(
        &self,
        mut infos: Vec<EntryInfo>,
        mut used: u64,
        target: u64,
        report: &mut EvictReport,
    ) -> u64 {
        infos.retain(|i| i.holders == 0);
        infos.sort_by(|a, b| (a.last_use, &a.pack).cmp(&(b.last_use, &b.pack)));
        for info in infos {
            if used <= target {
                break;
            }
            match self.store.remove(&info.pack) {
                // A hold taken since the scan keeps the entry.
                Ok(false) => {}
                Ok(true) => {
                    let bytes = used_bytes(&info);
                    used = used.saturating_sub(bytes);
                    report.entries_removed += 1;
                    report.removed_bytes += bytes;
                    self.metrics.evictions_total.add((), info.counts.used());
                }
                Err(source) => report.failures.push(EvictFailure::Remove {
                    pack: info.pack,
                    source,
                }),
            }
        }
        used
    }

    /// Eviction order 2: the `present` cells of the groups untouched longest (by touch epoch;
    /// ties by pack and group), until `need` bytes are chosen, evicted by the protocol of 5.1.
    /// The use left.
    fn punch_groups(&self, need: u64, used: u64, report: &mut EvictReport) -> u64 {
        let mut candidates = Vec::new();
        for pack in self.store.packs() {
            // An entry removed since the listing has nothing left to evict.
            let Some(entry) = self.store.get(&pack) else {
                continue;
            };
            candidates.extend(groups_with_present(&pack, &entry));
        }
        candidates.sort_by(|a, b| (a.epoch, &a.pack, a.group).cmp(&(b.epoch, &b.pack, b.group)));

        let mut chosen: BTreeMap<PackId, Batch> = BTreeMap::new();
        let mut bytes = 0u64;
        for c in candidates {
            if bytes >= need {
                break;
            }
            bytes += c.cells.len() as u64 * CELL_BYTES;
            chosen
                .entry(c.pack)
                .or_insert_with(|| Batch {
                    entry: c.entry,
                    cells: Vec::new(),
                })
                .cells
                .extend(c.cells);
        }
        let batches: Vec<Batch> = chosen
            .into_values()
            .map(|mut b| {
                b.cells.sort_unstable();
                b
            })
            .collect();

        let out = protocol::run(
            &batches,
            self.clock.as_ref(),
            self.pause.as_ref(),
            self.config.evict_wait_ms,
        );
        report.cells_punched += out.evicted;
        report.bytes_punched += out.bytes_punched;
        report.journal_syncs += out.journal_syncs;
        report.groups_timed_out += out.groups_timed_out;
        report.cells_restored += out.restored;
        report.failures.extend(
            out.punch_failures
                .into_iter()
                .map(|(pack, source)| EvictFailure::Punch { pack, source }),
        );
        self.metrics.evictions_total.add((), out.evicted);
        self.metrics.punched_bytes_total.add((), out.bytes_punched);
        self.metrics
            .evict_timeouts_total
            .add((), u64::from(out.groups_timed_out));
        used.saturating_sub(out.evicted * CELL_BYTES)
    }
}

/// An entry's use in bytes.
fn used_bytes(info: &EntryInfo) -> u64 {
    info.counts.used() * CELL_BYTES
}

/// The groups of `entry` with at least one `present` cell, with those cells.
fn groups_with_present(pack: &PackId, entry: &Arc<dyn Entry>) -> Vec<Candidate> {
    let state = entry.state();
    let cells = state.cell_count();
    let mut out: Vec<Candidate> = Vec::new();
    for cell in 0..cells {
        let present = state.load(cell).and_then(|b| b.state()) == Some(CellState::Present);
        if !present {
            continue;
        }
        let group = cell / GROUP_CELLS;
        match out.last_mut() {
            Some(c) if c.group == group => c.cells.push(cell),
            _ => out.push(Candidate {
                epoch: entry.touch_epoch(group).unwrap_or(0),
                pack: pack.clone(),
                entry: entry.clone(),
                group,
                cells: vec![cell],
            }),
        }
    }
    out
}
