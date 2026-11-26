//! A core of `tierstore fs` over the node's real tree store, for the tests of the placements at fs
//! locations end to end: a cache with the pack `cas/pack/one` in its upstream, the tree store over
//! it (the bench of `local_cache/trees`), and a core that reaches both one of the two ways of
//! [`Fills`] ([`Over`]): over the fill socket, served on a scratch path, or by calls in process.
//! [`over_both!`] runs a test over each.
//!
//! The core's requests block on the socket, which a task of the test's runtime serves, or on the
//! tasks of the cache that run on that runtime: run the test on a runtime of more than one thread
//! and call the core through [`Live::on_core`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::server::fill_socket;
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::testing::Bench;
use crate::server::local_cache::trees::{Created, Location, TreeStore};
use crate::{Clock, Mono};

use super::core::{Core, Looked, Settings};
use super::fill_client::FillClient;
use super::fills::Fills;
use super::in_process::InProcess;
use super::ino;
use super::table::TreeTable;
use super::testing::{OWNER, TREE_IDLE, TREE_LIST_RAM};

/// The mount id of the core of a [`Live`]: the run its holds of placements are made under.
pub(in crate::server) const MOUNT: &str = "mount-1";

/// How long the core of a [`Live`] waits for an answer: longer than any test.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// How the core of a [`Live`] reaches the cache and the tree store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Over {
    /// Over the fill socket, as the process `tierstore fs` does.
    Socket,
    /// By calls in the process that holds them.
    InProcess,
}

/// Two tests of the async function `$test`, which takes an [`Over`]: `$test::over_the_socket` and
/// `$test::in_process`, each on a runtime of more than one thread.
macro_rules! over_both {
    ($test:ident) => {
        mod $test {
            use crate::server::treefs::live_testing::Over;

            #[tokio::test(flavor = "multi_thread")]
            async fn over_the_socket() {
                super::$test(Over::Socket).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn in_process() {
                super::$test(Over::InProcess).await;
            }
        }
    };
}
pub(in crate::server) use over_both;

/// A core over the real tree store, and the fill socket that serves it if it reaches it so.
pub(in crate::server) struct Live {
    /// The cache, its upstream with the pack, and the tree store.
    pub bench: Bench,
    pub core: Arc<Core>,
    /// The clock of the core, which only the test moves (the store's is `bench.rig.clock`).
    pub clock: Arc<Mono>,
    /// How the core reaches the cache and the tree store.
    pub over: Over,
    /// The cache the core calls in process: the bench's, or one of the test's.
    pub cache: LocalCache,
    /// How long the core waits for an answer.
    fetch_timeout: Duration,
    /// The fill socket, served [`Over::Socket`].
    pub socket: PathBuf,
    pub runtime: PathBuf,
    pub mountpoint: PathBuf,
    halt: Arc<Notify>,
    served: Option<JoinHandle<()>>,
    /// Last: the directories go after the socket that is in them is closed.
    pub dir: TestDir,
}

impl Live {
    /// A bench called `name`, its fill socket served, and a core that counts lookups.
    pub(in crate::server) async fn new(name: &str) -> Self {
        Self::over(name, Over::Socket).await
    }

    /// A bench called `name`, and a core that counts lookups and reaches the bench `over`.
    pub(in crate::server) async fn over(name: &str, over: Over) -> Self {
        let name = match over {
            Over::Socket => name.to_owned(),
            Over::InProcess => format!("{name}-in"),
        };
        let bench = Bench::new(&name).await;
        let cache = bench.rig.cache.clone();
        Self::made(bench, (&name, over), (cache, FETCH_TIMEOUT)).await
    }

    /// A bench called `name`, and a core that reaches it in process through `cache_of` the bench
    /// (a cache of [`Rig::locked`](crate::server::local_cache::testing::Rig::locked), whose write
    /// role a test takes away), waiting `fetch_timeout` for the write role.
    pub(in crate::server) async fn in_process_with(
        name: &str,
        cache_of: impl FnOnce(&Bench) -> LocalCache,
        fetch_timeout: Duration,
    ) -> Self {
        let bench = Bench::new(name).await;
        let cache = cache_of(&bench);
        Self::made(bench, (name, Over::InProcess), (cache, fetch_timeout)).await
    }

    async fn made(
        bench: Bench,
        (name, over): (&str, Over),
        (cache, fetch_timeout): (LocalCache, Duration),
    ) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), &format!("{name}-fs"));
        let runtime = dir.path().join("run");
        let mountpoint = dir.path().join("mnt");
        for path in [&runtime, &mountpoint] {
            std::fs::create_dir_all(path).unwrap();
        }
        let socket = dir.path().join("fill.sock");
        let halt = Arc::new(Notify::new());
        let served = (over == Over::Socket).then(|| {
            let stores = (bench.rig.cache.clone(), Some(bench.store.clone()));
            let (path, notify) = (socket.clone(), Arc::clone(&halt));
            tokio::spawn(async move { fill_socket::serve(stores, &path, notify).await })
        });
        if served.is_some() {
            until("the fill socket is bound", || socket.exists()).await;
        }
        let clock = Arc::new(Mono::new());
        let fills = Self::fills_of(over, &socket, (&cache, &bench.store), fetch_timeout);
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
            fills,
            TreeTable::create(&runtime).unwrap(),
            &runtime,
        ));
        Self {
            bench,
            core,
            clock,
            over,
            cache,
            fetch_timeout,
            socket,
            runtime,
            mountpoint,
            halt,
            served,
            dir,
        }
    }

    /// What a core asks `over` the socket at `socket` or the stores: a client of the socket that
    /// does not pause between tries, or the stores in process; each waiting `fetch_timeout`.
    fn fills_of(
        over: Over,
        socket: &std::path::Path,
        (cache, trees): (&LocalCache, &TreeStore),
        fetch_timeout: Duration,
    ) -> Arc<dyn Fills> {
        match over {
            Over::Socket => Arc::new(FillClient::pausing(
                socket.to_owned(),
                3,
                fetch_timeout,
                Arc::new(|_| {}),
            )),
            Over::InProcess => Arc::new(InProcess::new(
                (cache.clone(), Some(trees.clone())),
                tokio::runtime::Handle::current(),
                fetch_timeout,
                Arc::new(|_| {}),
            )),
        }
    }

    /// Creates the bench's small tree (see `Bench::entries`) at `at`, with no fill.
    pub(in crate::server) fn place(&self, at: Vec<Location>) -> Created {
        self.bench
            .store
            .create(self.bench.request(at))
            .expect("a placement")
    }

    /// Runs `work` on the core, on a thread of its own: the core's requests block on the socket
    /// that this runtime serves.
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
    /// over the same table of tree numbers, runtime directory and fill socket (or the same cache
    /// in process), with nothing in memory.
    pub(in crate::server) fn successor(&self) -> Arc<Core> {
        let mut settings = self.core.settings().clone();
        settings.generation += 1;
        Arc::new(Core::new(
            settings,
            Self::fills_of(
                self.over,
                &self.socket,
                (&self.cache, &self.bench.store),
                self.fetch_timeout,
            ),
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

impl Drop for Live {
    fn drop(&mut self) {
        self.halt.notify_one();
        if let Some(served) = &self.served {
            served.abort();
        }
    }
}
