//! A core of `tierstore fs` over the node's real tree store, for the tests of the placements at fs
//! locations end to end: a cache with the pack `cas/pack/one` in its upstream, the tree store over
//! it (the bench of `local_cache/trees`), the fill socket serving both on a scratch path, and a
//! core whose fill client asks that socket.
//!
//! The core's requests block on the socket, which a task of the test's runtime serves: run the
//! test on a runtime of more than one thread and call the core through [`Live::on_core`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::server::fill_socket;
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::testing::Bench;
use crate::server::local_cache::trees::{Created, Location};
use crate::{Clock, Mono};

use super::core::{Core, Settings};
use super::fill_client::FillClient;
use super::table::TreeTable;
use super::testing::{OWNER, TREE_IDLE};

/// The mount id of the core of a [`Live`]: the run its holds of placements are made under.
pub(in crate::server) const MOUNT: &str = "mount-1";

/// A core over the real tree store and fill socket.
pub(in crate::server) struct Live {
    /// The cache, its upstream with the pack, and the tree store.
    pub bench: Bench,
    pub core: Arc<Core>,
    /// The clock of the core, which only the test moves (the store's is `bench.rig.clock`).
    pub clock: Arc<Mono>,
    /// The fill socket.
    pub socket: PathBuf,
    pub runtime: PathBuf,
    pub mountpoint: PathBuf,
    halt: Arc<Notify>,
    served: JoinHandle<()>,
    /// Last: the directories go after the socket that is in them is closed.
    pub dir: TestDir,
}

impl Live {
    /// A bench called `name`, its fill socket served, and a core that counts lookups.
    pub(in crate::server) async fn new(name: &str) -> Self {
        let bench = Bench::new(name).await;
        let dir = TestDir::under(std::env::temp_dir(), &format!("{name}-fs"));
        let runtime = dir.path().join("run");
        let mountpoint = dir.path().join("mnt");
        for path in [&runtime, &mountpoint] {
            std::fs::create_dir_all(path).unwrap();
        }
        let socket = dir.path().join("fill.sock");
        let halt = Arc::new(Notify::new());
        let stores = (bench.rig.cache.clone(), Some(bench.store.clone()));
        let (path, notify) = (socket.clone(), Arc::clone(&halt));
        let served = tokio::spawn(async move { fill_socket::serve(stores, &path, notify).await });
        until("the fill socket is bound", || socket.exists()).await;
        let clock = Arc::new(Mono::new());
        let core = Arc::new(Core::new(
            Settings {
                owner: OWNER,
                cache_path: bench.rig.config.path.clone(),
                mountpoint: mountpoint.clone(),
                mount_id: MOUNT.to_owned(),
                max_backing_ids: 100,
                generation: 1,
                count_lookups: true,
                tree_idle: TREE_IDLE,
                clock: Arc::clone(&clock) as Arc<dyn Clock>,
            },
            Self::client(&socket),
            TreeTable::create(&runtime).unwrap(),
            &runtime,
        ));
        Self {
            bench,
            core,
            clock,
            socket,
            runtime,
            mountpoint,
            halt,
            served,
            dir,
        }
    }

    /// A fill client of `socket` that does not pause between tries.
    fn client(socket: &std::path::Path) -> FillClient {
        FillClient::pausing(
            socket.to_owned(),
            3,
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
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
    /// over the same table of tree numbers, runtime directory and fill socket, with nothing in
    /// memory.
    pub(in crate::server) fn successor(&self) -> Arc<Core> {
        let mut settings = self.core.settings().clone();
        settings.generation += 1;
        Arc::new(Core::new(
            settings,
            Self::client(&self.socket),
            TreeTable::open(&self.runtime).unwrap(),
            &self.runtime,
        ))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.halt.notify_one();
        self.served.abort();
    }
}
