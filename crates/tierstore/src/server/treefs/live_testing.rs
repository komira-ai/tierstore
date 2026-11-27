//! A core of the file system over the node's real tree store, for the tests of the placements at
//! fs locations end to end: a cache with the pack `cas/pack/one` in its upstream, the tree store
//! over it (the bench of `local_cache/trees`), and a core that calls both in the process, as the
//! `fs` interface's core does.
//!
//! The core's requests block on the tasks of the cache that run on the test's runtime: run the
//! test on a runtime of more than one thread and call the core through [`Live::on_core`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use crate::server::local_cache::LocalCache;
use crate::server::local_cache::trees::testing::Bench;
use crate::server::local_cache::trees::{Created, Location, TreeStore};
use crate::{Clock, Mono};

use super::core::{Core, Looked, Settings};
use super::in_process::InProcess;
use super::ino;
use super::table::TreeTable;
use super::testing::{OWNER, TREE_IDLE, TREE_LIST_RAM};

/// The mount id of the core of a [`Live`]: the run its holds of placements are made under.
pub(in crate::server) const MOUNT: &str = "mount-1";

/// How long the core of a [`Live`] waits for an answer: longer than any test.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// A core over the real tree store, calling the cache and the tree store in the process.
pub(in crate::server) struct Live {
    /// The cache, its upstream with the pack, and the tree store.
    pub bench: Bench,
    pub core: Arc<Core>,
    /// The clock of the core, which only the test moves (the store's is `bench.rig.clock`).
    pub clock: Arc<Mono>,
    /// The cache the core calls: the bench's, or one of the test's.
    pub cache: LocalCache,
    /// How long the core waits for an answer.
    fetch_timeout: Duration,
    pub runtime: PathBuf,
    pub mountpoint: PathBuf,
    pub dir: TestDir,
}

impl Live {
    /// A bench called `name`, and a core that counts lookups and calls the bench's cache and
    /// tree store.
    pub(in crate::server) async fn new(name: &str) -> Self {
        let bench = Bench::new(name).await;
        let cache = bench.rig.cache.clone();
        Self::made(bench, name, (cache, FETCH_TIMEOUT)).await
    }

    /// A bench called `name`, and a core that calls the tree store of the bench and `cache_of` the
    /// bench (a cache of [`Rig::locked`](crate::server::local_cache::testing::Rig::locked), whose
    /// write role a test takes away), waiting `fetch_timeout` for the write role.
    pub(in crate::server) async fn in_process_with(
        name: &str,
        cache_of: impl FnOnce(&Bench) -> LocalCache,
        fetch_timeout: Duration,
    ) -> Self {
        let bench = Bench::new(name).await;
        let cache = cache_of(&bench);
        Self::made(bench, name, (cache, fetch_timeout)).await
    }

    async fn made(
        bench: Bench,
        name: &str,
        (cache, fetch_timeout): (LocalCache, Duration),
    ) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), &format!("{name}-fs"));
        let runtime = dir.path().join("run");
        let mountpoint = dir.path().join("mnt");
        for path in [&runtime, &mountpoint] {
            std::fs::create_dir_all(path).unwrap();
        }
        let clock = Arc::new(Mono::new());
        let asks = Self::asks_of((&cache, &bench.store), fetch_timeout);
        let core = Arc::new(Core::new(
            Settings {
                owner: OWNER,
                cache_path: bench.rig.config.path.clone(),
                mountpoint: mountpoint.clone(),
                mount_id: MOUNT.to_owned(),
                generation: 1,
                count_lookups: true,
                tree_idle: TREE_IDLE,
                tree_list_ram_bytes: TREE_LIST_RAM,
                clock: Arc::clone(&clock) as Arc<dyn Clock>,
            },
            asks,
            TreeTable::create(&runtime).unwrap(),
            &runtime,
        ));
        Self {
            bench,
            core,
            clock,
            cache,
            fetch_timeout,
            runtime,
            mountpoint,
            dir,
        }
    }

    /// What a core asks: the cache and the tree store in process, each request waiting
    /// `fetch_timeout`, with no pause between the core's tries.
    fn asks_of(
        (cache, trees): (&LocalCache, &TreeStore),
        fetch_timeout: Duration,
    ) -> Arc<InProcess> {
        Arc::new(InProcess::new(
            (cache.clone(), Some(trees.clone())),
            tokio::runtime::Handle::current(),
            fetch_timeout,
            Arc::new(|_| {}),
        ))
    }

    /// Creates the bench's small tree (see `Bench::entries`) at `at`, with no fill.
    pub(in crate::server) fn place(&self, at: Vec<Location>) -> Created {
        self.bench
            .store
            .create(self.bench.request(at))
            .expect("a placement")
    }

    /// Runs `work` on the core, on a thread of its own: the core's requests block on the tasks of
    /// the cache that this runtime runs.
    pub(in crate::server) async fn on_core<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Core) -> T + Send + 'static,
    ) -> T {
        let core = Arc::clone(&self.core);
        tokio::task::spawn_blocking(move || work(&core))
            .await
            .expect("the core's work ends")
    }

    /// The core of the process that takes the mount over from this one: the generation after it,
    /// over the same table of tree numbers, runtime directory and cache, with nothing in memory.
    pub(in crate::server) fn successor(&self) -> Arc<Core> {
        self.successor_over((&self.cache, &self.bench.store))
    }

    /// The same, reaching `cache` and `trees`: the core of a process that takes the mount over
    /// with a cache and a tree store of its own, opened before the hand-over over the same
    /// directory.
    pub(in crate::server) fn successor_over(
        &self,
        (cache, trees): (&LocalCache, &TreeStore),
    ) -> Arc<Core> {
        let mut settings = self.core.settings().clone();
        settings.generation += 1;
        Arc::new(Core::new(
            settings,
            Self::asks_of((cache, trees), self.fetch_timeout),
            TreeTable::open(&self.runtime).unwrap(),
            &self.runtime,
        ))
    }
}

/// The inode at the path `names` from the root of the mount, looked up on `core`.
///
/// # Panics
/// When a name is not there.
pub(in crate::server) fn walk(core: &Core, names: &[&str]) -> u64 {
    let mut at = ino::ROOT;
    for name in names {
        at = match core.lookup(at, name).unwrap() {
            Looked::Found(entry) => entry.attr.ino,
            Looked::Absent(_) => panic!("{name} is not there"),
        };
    }
    at
}
