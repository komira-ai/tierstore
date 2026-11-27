//! The read path a build node builds at its start when its file has `[cache]`
//! (`docs/design/fetch-cache-serve.md`, sections 4, 7, 8, 9, 12 and 13), attached to the S3
//! front's large objects
//! ([`LocalCache::route_large`](crate::server::local_cache::LocalCache::route_large)).
//!
//! What [`start`] builds, over the upstream it is given (the daemon's is the S3 cold store of
//! its file, [`S3Upstream`](super::upstream::S3Upstream)):
//! - the in-process entry store of `tierstore-entry` at `<cache.root>/packs-v2/`, each pack a
//!   sparse `<aa>/<bb>/<hash>.pack` with its journal and snapshot beside it. An entry opened again
//!   after a restart comes back from its journal: the cells committed before are `present` in
//!   the same boot (and `recheck` in another, or with no boot id), so they are served without
//!   being fetched again;
//! - the node's one committer (section 8), whose loop commits every dirty entry once a second, at
//!   once when 32 MiB are pending ([`Committed`]), and once more at the stop, before the store's
//!   journals are folded;
//! - the evictor (section 7) behind the directory lock `packs-v2/.lock`: one process is the
//!   evictor; a process that finds the lock held (the other side of a hand-over) runs without
//!   evicting and tries the lock again at each pass ([`EVICT_EVERY`]). A pass removes whole
//!   unheld entries by age, then punches cells of held ones, down to the budget, after the grace
//!   window of 60 s from the moment the lock was taken. When the lock is taken, the entries left on
//!   disk by an earlier process that no process has open (their data file is not locked) are
//!   counted against the budget, and the oldest of them are removed until the budget holds them;
//! - the budget of section 4.3 over one device (the store,
//!   [`STORE`](super::upstream::STORE)) that takes
//!   [`QUEUE_DEPTH`] parts at once, the hash pool (one thread: an S3 object has no cell
//!   digests), and the fetch engine with the design's constants;
//! - the registry of the section 12 families of the engine, its budget and the node cache, which
//!   `/metricz` shows after the local cache's own counters.
//!
//! **The overlap budget** (section 13). While the layout of the release before (`digest-v1`:
//! files under `obj/` or `extents-v1/` of `local_cache.path`) holds a file, the budget of
//! `packs-v2/` is half of `cache.budget`, so the two layouts fit the file system they share; with
//! neither holding a file at the start, it is the whole of `cache.budget`. It is judged once, at
//! the start: `tierstore gc legacy` removes the old layout while no process serves it.
//!
//! **Stop.** Dropping the [`Stack`] stops both loops, commits what is pending and folds every
//! open entry's journal into its snapshot.

use std::num::NonZeroUsize;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, UNIX_EPOCH};

use tierstore_budget::{BudgetConfig, ClientBudget, Topology};
use tierstore_entry::PACKS_DIR;
use tierstore_entry::inproc::InProcStore;
use tierstore_evict::{EvictConfig, EvictReport, Evictor, EvictorLock, SleepPause};
use tierstore_fetch::timer::TokioTimer;
use tierstore_fetch::{EngineConfig, EngineParts, FetchEngine, GetKind, Stats};
use tierstore_fetch_api::clock::{Clock, MonotonicClock};
use tierstore_fetch_api::entry::EntryStore;
use tierstore_fetch_api::ids::HolderId;
use tierstore_fetch_api::upstream::Upstream;
use tierstore_hashpool::HashPool;
use tierstore_journal::{BootId, CommitPolicy, Committer, FileKind};
use tierstore_metrics::labels::GetKind as GetLabel;
use tierstore_metrics::{Cache, CopyGeneration, Fetch, FetchEngineConfig, Metrics};

use super::committed::{Committed, Wake};
use super::upstream::S3Upstream;
use super::{LargeObjects, LargeParts};
use crate::server::build_info::VERSION;
use crate::server::cache_cli::upstream_flags;
use crate::server::cli::s3_backend;
use crate::server::config_cache::CacheSettings;
use crate::server::local_cache::LocalCache;

/// The holder whose hold keeps an entry while the S3 front reads it.
pub(crate) const FRONT: HolderId = HolderId(1);

/// How often the evictor's loop runs a pass.
pub(crate) const EVICT_EVERY: Duration = Duration::from_secs(1);

/// The parts the store is sent at once (the queue depth of its one device, 4.3).
pub(crate) const QUEUE_DEPTH: u32 = 8;

/// The directories of the layout of the release before under `local_cache.path`.
pub(crate) const LEGACY_DIRS: [&str; 2] = ["obj", "extents-v1"];

/// What [`start`] builds the read path of.
pub(crate) struct StackParts {
    /// The directory `packs-v2/` is made in (`cache.root`).
    pub root: PathBuf,
    /// `cache.budget`, before the overlap halves it.
    pub budget: u64,
    /// The directory of the layout of the release before (`local_cache.path`).
    pub legacy: PathBuf,
    /// Where the engine's parts come from.
    pub upstream: Arc<dyn Upstream>,
    /// The boot this process runs in: `None` makes every restored cell `recheck`.
    pub boot: Option<BootId>,
    /// The clock of the committer and of the evictor.
    pub clock: Arc<dyn Clock>,
    /// The time between two passes of the evictor's loop ([`EVICT_EVERY`] in the daemon).
    pub evict_every: Duration,
}

/// The read path, running (see the module).
pub(crate) struct Stack {
    large: Arc<LargeObjects>,
    store: Arc<InProcStore>,
    #[cfg(test)]
    evicting: Arc<Evicting>,
    /// The committer's loop: a round due, or the stop.
    commits: Arc<Wake>,
    /// The evictor's loop: the stop.
    passes: Arc<Wake>,
    loops: Vec<JoinHandle<()>>,
}

/// The lock of a mutex whose data stays usable after a panic of its holder.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The read path of the `[cache]` table of the build node of `settings`, over its S3 cold store,
/// attached to `cache`; none for a file without the table. The boot is read from the file the
/// settings name (`/proc/sys/kernel/random/boot_id` on Linux; a system without it restores every
/// cell as `recheck`).
///
/// # Errors
/// As [`start`], and a cold store whose client cannot be made.
pub(in crate::server) fn for_build_node(
    settings: &CacheSettings,
    cache: &LocalCache,
) -> Result<Option<Stack>, String> {
    let Some(read) = &settings.read_path else {
        return Ok(None);
    };
    let tier = &settings.upstream;
    let bucket = tier.spec.strip_prefix("s3:").unwrap_or_default();
    let limits = (settings.s3_request_timeout, &settings.s3_client);
    let backend = s3_backend("--cold", &tier.spec, bucket, &upstream_flags(tier), limits)?;
    let stack = start(StackParts {
        root: read.root.clone(),
        budget: read.budget,
        legacy: settings.cache.path.clone(),
        upstream: Arc::new(S3Upstream::new(Arc::new(backend))),
        boot: BootId::read(&read.boot_id_file).ok(),
        clock: Arc::new(MonotonicClock::new()),
        evict_every: EVICT_EVERY,
    })?;
    cache.route_large(stack.large());
    Ok(Some(stack))
}

/// Builds the read path of `parts` and starts its loops, on the runtime of the caller.
///
/// # Errors
/// What the start could not do, named: making `packs-v2/`, starting the hash pool, a loop's
/// thread.
pub(crate) fn start(parts: StackParts) -> Result<Stack, String> {
    let budget = overlap_budget(parts.budget, &parts.legacy);
    let store = InProcStore::new(&parts.root, parts.boot);
    let store = Arc::new(store.map_err(|error| format!("the new cache: {error}"))?);
    let committer = Arc::new(Committer::new(CommitPolicy::default()));
    let commits = Arc::new(Wake::default());
    let entries: Arc<dyn EntryStore> = Arc::new(Committed::new(
        store.clone() as Arc<dyn EntryStore>,
        committer.clone(),
        commits.clone(),
    ));
    let timer = Arc::new(TokioTimer::new());
    let topology = Topology {
        cold_drives: 0,
        hot_devices: 1,
        clients: 1,
        drive_streams: 1,
        hot_queue_depth: QUEUE_DEPTH,
    };
    let client = ClientBudget::new(BudgetConfig::new(topology), timer.clone());
    let client = client.map_err(|error| format!("the budget of the new cache: {error}"))?;
    let pool = HashPool::start(NonZeroUsize::MIN, timer.clone());
    let pool = pool.map_err(|error| format!("the hash pool of the new cache: {error}"))?;
    let engine = Arc::new(FetchEngine::new(
        EngineParts {
            store: entries.clone(),
            upstream: parts.upstream.clone(),
            budget: Arc::new(client.clone()),
            verifier: Arc::new(pool),
            timer,
        },
        EngineConfig::default(),
        &tokio::runtime::Handle::current(),
    ));
    let registry = Metrics::new(VERSION);
    let cache = registry.node_cache().cache;
    let fetch = registry.fetch_engine(&FetchEngineConfig { drives: Vec::new() });
    let exported = Exported {
        registry,
        fetch: fetch.fetch,
        copy: fetch.copy_generation,
        engine: engine.clone(),
        budget: client,
        seen: Mutex::new(Stats::default()),
    };
    let large = Arc::new(LargeObjects::new(LargeParts {
        engine,
        upstream: parts.upstream,
        store: entries.clone(),
        holder: FRONT,
    }));
    large.export(Arc::new(exported));
    let evicting = Arc::new(Evicting {
        root: parts.root,
        store: entries,
        clock: parts.clock.clone(),
        budget,
        cache,
        evictor: Mutex::new(None),
    });
    let is_evictor = evicting.take_lock();
    tracing::info!(
        "tierstore: new cache at {}: budget {budget} bytes, evictor: {is_evictor}",
        store.dir().display()
    );
    let passes = Arc::new(Wake::default());
    let committing = {
        let (committer, wake, clock) = (committer.clone(), commits.clone(), parts.clock);
        move || commit_loop(&committer, &wake, &*clock)
    };
    let passing = {
        let (evicting, wake, every) = (evicting.clone(), passes.clone(), parts.evict_every);
        move || evict_loop(&evicting, &wake, every)
    };
    let mut stack = Stack {
        large,
        store,
        #[cfg(test)]
        evicting,
        commits,
        passes,
        loops: Vec::new(),
    };
    for (name, body) in [
        (
            "ts-commit",
            Box::new(committing) as Box<dyn FnOnce() + Send>,
        ),
        ("ts-evict", Box::new(passing)),
    ] {
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(body);
        stack
            .loops
            .push(spawned.map_err(|error| format!("the {name} thread: {error}"))?);
    }
    Ok(stack)
}

/// The committer's loop: a round when one is due, until the stop; then the last round.
fn commit_loop(committer: &Committer, wake: &Wake, clock: &dyn Clock) {
    let mut stop = false;
    while !stop {
        stop = wake
            .wait(Duration::from_millis(CommitPolicy::default().interval_ms))
            .stop;
        say_round(committer.commit_due(clock.now_ms()));
    }
    say_round(committer.commit_due(u64::MAX));
}

/// Says on stderr which entries a round could not commit (the next round tries them again).
fn say_round(round: Option<tierstore_journal::CommitRound>) {
    for (pack, error) in round.map(|round| round.failed).unwrap_or_default() {
        tracing::warn!("tierstore: new cache: committing {pack:?}: {error}");
    }
}

/// The evictor's loop: a pass every `every`, until the stop.
fn evict_loop(evicting: &Evicting, wake: &Wake, every: Duration) {
    let mut stop = false;
    while !stop {
        stop = wake.wait(every).stop;
        let _ = evicting.pass();
    }
}

impl Stack {
    /// The S3 front's path for large objects over this read path.
    #[must_use]
    pub(crate) fn large(&self) -> Arc<LargeObjects> {
        self.large.clone()
    }

    /// The budget of `packs-v2/`: `cache.budget`, or half of it during the overlap.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn budget(&self) -> u64 {
        self.evicting.budget
    }

    /// Whether this process holds the evictor lock.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_evictor(&self) -> bool {
        locked(&self.evicting.evictor).is_some()
    }

    /// One pass of the evictor now, as its loop runs them: `None` when this process could not
    /// take the lock, or the grace window has not passed.
    #[cfg(test)]
    pub(crate) fn pass(&self) -> Option<EvictReport> {
        self.evicting.pass()
    }

    /// The entry store, unwrapped.
    #[cfg(test)]
    pub(crate) fn store(&self) -> &Arc<InProcStore> {
        &self.store
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.commits.stop();
        self.passes.stop();
        for handle in self.loops.drain(..) {
            let _ = handle.join();
        }
        // A fold that fails leaves the entry's journal as it was, and the next start replays it.
        let _ = self.store.close();
    }
}

/// The evictor, once this process holds the lock.
struct Evicting {
    root: PathBuf,
    store: Arc<dyn EntryStore>,
    clock: Arc<dyn Clock>,
    budget: u64,
    cache: Arc<Cache>,
    evictor: Mutex<Option<Evictor>>,
}

impl Evicting {
    /// Takes the evictor lock if this process does not hold it yet, removing what the entries
    /// left by an earlier process take past the budget when it does: whether it holds it now.
    fn take_lock(&self) -> bool {
        let mut evictor = locked(&self.evictor);
        if evictor.is_some() {
            return true;
        }
        let Ok(lock) = EvictorLock::acquire(&self.root) else {
            return false;
        };
        let used: u64 = self
            .store
            .entries()
            .iter()
            .map(|info| info.counts.used())
            .sum();
        let left = self
            .budget
            .saturating_sub(used * tierstore_evict::CELL_BYTES);
        let removed = sweep(&self.root.join(PACKS_DIR), left);
        tracing::info!(
            "tierstore: new cache: the evictor, after removing {} entries an earlier process left",
            removed
        );
        *evictor = Some(Evictor::new(
            self.store.clone(),
            lock,
            self.clock.clone(),
            Arc::new(SleepPause),
            EvictConfig::new(self.budget),
            self.cache.clone(),
        ));
        true
    }

    /// One pass: the lock if it is not held yet, then the eviction down to the budget.
    fn pass(&self) -> Option<EvictReport> {
        if !self.take_lock() {
            return None;
        }
        let evictor = locked(&self.evictor);
        evictor.as_ref()?.evict(self.budget).ok()
    }
}

/// `budget`, or half of it while the layout of the release before under `legacy` holds a file.
#[must_use]
pub(crate) fn overlap_budget(budget: u64, legacy: &Path) -> u64 {
    let overlap = LEGACY_DIRS
        .iter()
        .any(|dir| holds_a_file(&legacy.join(dir)));
    if overlap { budget / 2 } else { budget }
}

/// Whether `dir`, or a directory under it, holds anything that is not a directory. Links are
/// not followed.
fn holds_a_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            holds_a_file(&entry.path())
        } else {
            true
        }
    })
}

/// An entry on disk that no process had open when the sweep looked: its data file, when it was
/// last written, and the bytes it takes.
struct Orphan {
    data: PathBuf,
    modified: u64,
    bytes: u64,
}

/// Removes the oldest of the entries under `packs` that no process has open (see [`Orphan`])
/// until those left take at most `room` bytes of the disk: how many it removed.
fn sweep(packs: &Path, room: u64) -> usize {
    let mut orphans = Vec::new();
    find_orphans(packs, &mut orphans);
    orphans.sort_by_key(|orphan| orphan.modified);
    let mut held: u64 = orphans.iter().map(|orphan| orphan.bytes).sum();
    let mut removed = 0;
    for orphan in orphans {
        if held <= room {
            break;
        }
        // Locked again for its removal: an entry that a process opened since the look is kept.
        let Some(_lock) = unlocked(&orphan.data) else {
            continue;
        };
        held = held.saturating_sub(orphan.bytes);
        let stem = orphan.data.with_extension("");
        for kind in FileKind::ALL {
            let mut name = stem.clone().into_os_string();
            name.push(kind.suffix());
            let _ = std::fs::remove_file(name);
        }
        removed += 1;
    }
    removed
}

/// The data files (`.pack`) under `dir`, to two levels of directories, that no open file holds
/// the lock of.
fn find_orphans(dir: &Path, out: &mut Vec<Orphan>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            find_orphans(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "pack") {
            out.extend(orphan(path));
        }
    }
}

/// The data file `data`, opened and locked, if no other open file holds its lock now.
fn unlocked(data: &Path) -> Option<std::fs::File> {
    let file = std::fs::File::open(data).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// The entry of the data file `data`, if no process has it open now (the lock taken to look is
/// let go at once).
fn orphan(data: PathBuf) -> Option<Orphan> {
    let meta = unlocked(&data)?.metadata().ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(Orphan {
        data,
        modified: u64::try_from(modified.as_nanos()).unwrap_or(u64::MAX),
        bytes: meta.blocks().saturating_mul(512),
    })
}

/// The families of the read path's registry, with the engine's and the budget's counts copied
/// into them each time they are shown.
pub(crate) struct Exported {
    registry: Metrics,
    fetch: Arc<Fetch>,
    copy: Arc<CopyGeneration>,
    engine: Arc<FetchEngine>,
    budget: ClientBudget,
    /// The engine's counts at the last time they were copied.
    seen: Mutex<Stats>,
}

/// The kinds of GET of the engine, with the label of each.
const KINDS: [(GetKind, GetLabel); 4] = [
    (GetKind::GridPart, GetLabel::GridPart),
    (GetKind::DemandSub, GetLabel::DemandSubRange),
    (GetKind::Retry, GetLabel::Retry),
    (GetKind::Hedge, GetLabel::Hedge),
];

impl Exported {
    /// The text of the families, without `tierstore_build_info`, which the page has already.
    pub(crate) fn render(&self) -> String {
        self.copy_counts();
        let text = self.registry.render();
        let mut out = String::with_capacity(text.len());
        let build_info = "tierstore_build_info";
        for line in text.lines() {
            let name = line
                .strip_prefix("# HELP ")
                .or_else(|| line.strip_prefix("# TYPE "))
                .unwrap_or(line);
            let family = name.split(['{', ' ']).next().unwrap_or_default();
            if family != build_info {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// Adds to the counters what the engine counted since the last time, and sets the gauges of
    /// the budget.
    fn copy_counts(&self) {
        let mut seen = locked(&self.seen);
        let now = self.engine.stats();
        for (kind, label) in KINDS {
            let more = now.gets(kind).saturating_sub(seen.gets(kind));
            self.fetch.gets_total.add(label, more);
        }
        let fetched = now.bytes_fetched.saturating_sub(seen.bytes_fetched);
        self.fetch.bytes_fetched_total.add((), fetched);
        let mismatched = now
            .copy_generation_mismatch
            .saturating_sub(seen.copy_generation_mismatch);
        self.copy
            .store_copy_generation_mismatch_total
            .add((), mismatched);
        *seen = now;
        let snapshot = self.budget.snapshot();
        self.fetch
            .budget_ceiling_parts
            .set((), f64::from(snapshot.ceiling));
        self.fetch
            .parts_in_flight
            .set((), f64::from(snapshot.parts_in_flight));
        self.fetch.budget_reason.set((), snapshot.reason);
    }
}
