//! Shared helpers of the eviction and admission tests: a store of entries the test builds by
//! hand (with scripted removal failures and races), entries filled with present cells, a pause
//! that moves the injected clock and lets the test act between polls, and a rig that makes an
//! evictor past its grace window.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the helpers"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex};

use tierstore_evict::{CacheDevice, EvictConfig, Evictor, EvictorLock, Pause};
use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::clock::{Clock, ManualClock};
use tierstore_fetch_api::entry::{
    Entry, EntryError, EntryInfo, EntrySpec, EntryStore, HolderCounters,
};
use tierstore_fetch_api::fake::MemEntry;
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId, PackKey, PackRoot};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_journal::BootId;
use tierstore_journal::fake::{FsEntry, MemFs};
use tierstore_metrics::{Cache, Metrics};
use tierstore_testkit::dir::TestDir;

/// One cell.
pub const MIB: u64 = 1 << 20;

/// The boot the tests run in.
pub const BOOT: BootId = BootId([0xa1; 16]);

/// The pack named `n`.
pub fn pack(n: u32) -> PackId {
    PackId {
        key: PackKey(format!("packs/{n:04}.pack")),
        root: PackRoot::Digest([n as u8; 32]),
    }
}

/// The spec of pack `n` of `size` bytes.
pub fn spec(n: u32, size: u64) -> EntrySpec {
    EntrySpec {
        pack: pack(n),
        size,
    }
}

/// The length of `cell` of a pack of `size` bytes.
pub fn cell_len(size: u64, cell: u32) -> usize {
    let start = u64::from(cell) * MIB;
    ((start + MIB).min(size) - start) as usize
}

/// Bytes of `cell`, never all zeros.
pub fn cell_bytes(size: u64, cell: u32) -> Vec<u8> {
    (0..cell_len(size, cell))
        .map(|i| (cell as usize * 31 + i % 251 + 1) as u8)
        .collect()
}

/// Claims and publishes each of `cells`.
pub fn publish(entry: &dyn Entry, cells: impl IntoIterator<Item = u32>) {
    let size = entry.size();
    for c in cells {
        for token in entry.claim(CellRun { first: c, count: 1 }) {
            entry.publish(token, &cell_bytes(size, c)).unwrap();
        }
    }
}

/// An in-memory entry of pack `n` with `cells` whole cells, `present` ones as listed.
pub fn mem(n: u32, cells: u32, present: impl IntoIterator<Item = u32>) -> Arc<MemEntry> {
    let e = Arc::new(MemEntry::new(&spec(n, u64::from(cells) * MIB)).unwrap());
    publish(e.as_ref(), present);
    e
}

/// A journaled entry of pack `n` of `size` bytes on its own file system, with `present` cells
/// published and committed (durable, with their `ADD` records).
pub fn journaled(
    n: u32,
    size: u64,
    present: impl IntoIterator<Item = u32>,
) -> (MemFs, Arc<FsEntry>) {
    let fs = MemFs::new();
    let (e, _) = FsEntry::open(fs.clone(), &spec(n, size), Some(BOOT)).unwrap();
    publish(&e, present);
    e.commit_batch().unwrap();
    (fs, Arc::new(e))
}

/// The state of `cell` of `entry`.
pub fn state(entry: &dyn Entry, cell: u32) -> CellState {
    entry.state().load(cell).and_then(CellByte::state).unwrap()
}

/// The states of `cells` of `entry`.
pub fn states(entry: &dyn Entry, cells: impl IntoIterator<Item = u32>) -> Vec<CellState> {
    cells.into_iter().map(|c| state(entry, c)).collect()
}

/// An entry store the test fills by hand, with scripted removal answers and races.
#[derive(Default)]
pub struct TestStore {
    entries: Mutex<BTreeMap<PackId, Arc<dyn Entry>>>,
    /// Packs whose removal fails with an I/O error.
    pub fail_remove: Mutex<BTreeSet<PackId>>,
    /// Packs whose removal answers "held" although the listing said unheld (a hold taken
    /// between the listing and the removal).
    pub held_at_remove: Mutex<BTreeSet<PackId>>,
    /// Packs `get` no longer finds although `packs` lists them (removed between the two).
    pub gone_at_get: Mutex<BTreeSet<PackId>>,
    /// Every removal asked for, in order.
    pub removals: Mutex<Vec<PackId>>,
}

impl TestStore {
    /// An empty store.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Adds `entry`.
    pub fn insert(&self, entry: Arc<dyn Entry>) {
        self.entries
            .lock()
            .unwrap()
            .insert(entry.pack().clone(), entry);
    }

    /// Whether the store has `pack`.
    pub fn has(&self, n: u32) -> bool {
        self.entries.lock().unwrap().contains_key(&pack(n))
    }
}

impl EntryStore for TestStore {
    fn open(&self, _spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError> {
        unreachable!("the evictor never opens an entry")
    }

    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        if self.gone_at_get.lock().unwrap().contains(pack) {
            return None;
        }
        self.entries.lock().unwrap().get(pack).cloned()
    }

    fn remove(&self, pack: &PackId) -> Result<bool, EntryError> {
        self.removals.lock().unwrap().push(pack.clone());
        if self.fail_remove.lock().unwrap().contains(pack) {
            return Err(EntryError::Io {
                pack: pack.clone(),
                op: "unlink",
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            });
        }
        if self.held_at_remove.lock().unwrap().contains(pack) {
            return Ok(false);
        }
        let mut entries = self.entries.lock().unwrap();
        match entries.get(pack) {
            Some(e) if e.holders() == 0 => {
                entries.remove(pack);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn packs(&self) -> Vec<PackId> {
        self.entries.lock().unwrap().keys().cloned().collect()
    }

    fn entries(&self) -> Vec<EntryInfo> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .map(|e| EntryInfo {
                pack: e.pack().clone(),
                size: e.size(),
                last_use: e.last_use(),
                holders: e.holders(),
                counts: e.counts(),
            })
            .collect()
    }

    fn register_holder(
        &self,
        _holder: HolderId,
        _counters: Arc<dyn HolderCounters>,
    ) -> Result<(), EntryError> {
        unreachable!("the evictor registers no holder")
    }

    fn attach(
        &self,
        _pack: &PackId,
        _holder: HolderId,
        _region: CounterRegion,
    ) -> Result<(), EntryError> {
        unreachable!("the evictor attaches no holder")
    }

    fn drop_holder(&self, _holder: HolderId) -> u32 {
        unreachable!("the evictor drops no holder")
    }
}

/// A pause that moves the clock by `step_ms` and then runs the test's hook with the attempt
/// number, counting the pauses.
pub struct ClockPause {
    clock: ManualClock,
    step_ms: u64,
    hook: Mutex<Box<dyn FnMut(u32) + Send>>,
    /// The attempts paused for, in order.
    pub attempts: Mutex<Vec<u32>>,
}

impl ClockPause {
    /// A pause of `step_ms` on `clock` that does nothing else.
    pub fn new(clock: &ManualClock, step_ms: u64) -> Arc<Self> {
        Self::with(clock, step_ms, |_| {})
    }

    /// A pause of `step_ms` on `clock` that runs `hook` after moving it.
    pub fn with(
        clock: &ManualClock,
        step_ms: u64,
        hook: impl FnMut(u32) + Send + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            clock: clock.clone(),
            step_ms,
            hook: Mutex::new(Box::new(hook)),
            attempts: Mutex::new(Vec::new()),
        })
    }

    /// The number of pauses.
    pub fn count(&self) -> usize {
        self.attempts.lock().unwrap().len()
    }
}

impl Pause for ClockPause {
    fn pause(&self, attempt: u32) {
        self.attempts.lock().unwrap().push(attempt);
        self.clock.advance(self.step_ms);
        (self.hook.lock().unwrap())(attempt);
    }
}

/// A cache device with a set free space, or one whose `statvfs` fails.
pub struct FakeDevice {
    /// The free bytes, or `None` for a failing device.
    pub free: Mutex<Option<u64>>,
}

impl FakeDevice {
    /// A device with `free` bytes free.
    pub fn new(free: u64) -> Arc<Self> {
        Arc::new(Self {
            free: Mutex::new(Some(free)),
        })
    }

    /// A device whose free space cannot be read.
    pub fn failing() -> Arc<Self> {
        Arc::new(Self {
            free: Mutex::new(None),
        })
    }
}

impl CacheDevice for FakeDevice {
    fn free_bytes(&self) -> io::Result<u64> {
        self.free
            .lock()
            .unwrap()
            .ok_or_else(|| io::Error::other("statvfs failed"))
    }
}

/// A scratch directory for one test.
pub fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// The node cache's metrics of a fresh registry.
pub fn metrics() -> Arc<Cache> {
    Metrics::new("test").node_cache().cache
}

/// An evictor of `store` with `budget`, its clock, its metrics and its scratch directory (which
/// holds the evictor lock), with the clock already past the grace window.
pub struct Rig {
    pub evictor: Evictor,
    pub clock: ManualClock,
    pub metrics: Arc<Cache>,
    pub dir: TestDir,
}

impl Rig {
    /// The evictor of `store`, made at clock 0 with the default constants, the clock moved to
    /// the end of the grace window, polling with `pause`.
    pub fn new(store: Arc<dyn EntryStore>, clock: ManualClock, pause: Arc<dyn Pause>) -> Self {
        let dir = scratch("evict");
        let lock = EvictorLock::acquire(dir.path()).unwrap();
        let metrics = metrics();
        let evictor = Evictor::new(
            store,
            lock,
            Arc::new(clock.clone()),
            pause,
            EvictConfig::new(64 * MIB),
            metrics.clone(),
        );
        clock.advance(EvictConfig::GRACE_MS - clock.now_ms());
        Self {
            evictor,
            clock,
            metrics,
            dir,
        }
    }

    /// A rig whose pause only moves the clock by 500 ms.
    pub fn plain(store: Arc<dyn EntryStore>) -> Self {
        let clock = ManualClock::new();
        let pause = ClockPause::new(&clock, 500);
        Self::new(store, clock, pause)
    }
}
