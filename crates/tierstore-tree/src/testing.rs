//! A rig for the tests of this crate and of the fronts on it (feature `testing`): one tree's
//! packs served by the real mount server of T5 over the real entry store of T1, with the fake
//! engine of `tierstore-fetch-api` for fills, to a mount client of holder 1.
//!
//! Every wait on a pack parks in the fake engine until the test answers it ([`Rig::answer`]);
//! cells become `present` only when the test publishes them ([`Rig::publish`]), with the pack's
//! own bytes. Nothing here reads a clock or sleeps: the client's timer is a [`ManualClock`], and a
//! test waits for a count (the engine's parked waits, the client's counts).
//!
//! Its helpers panic on a failure: a test cannot go on without them.

use std::fs::File;
use std::os::unix::net::UnixStream as StdStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tierstore_entry::{FileEntry, FileEntryStore, MemfdCounters};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::entry::{ClaimedCell, Entry as _, EntryError, EvictStep, HolderCounters};
use tierstore_fetch_api::fake::FakeEngine;
use tierstore_fetch_api::ids::{CellRun, HolderId, JobId, PackId, TreeRef};
use tierstore_fetch_api::list::ListL2;
use tierstore_list::TreeListL2;
use tierstore_mount_client::{Config, Connector, PackClient};
use tierstore_mount_server::{
    Config as ServerConfig, CounterMapper, MountServer, Parts, TreeError as ServerTreeError, Trees,
};
use tierstore_testkit::dir::TestDir;
use tokio::net::UnixStream;
use tokio::runtime::Runtime;

use crate::{Image, ImageOptions, Tree, TreeConfig};

/// The job of every tree a rig opens.
pub const JOB: JobId = JobId(7);

/// The reference of the rig's tree.
#[must_use]
pub fn tree_ref() -> TreeRef {
    TreeRef("tree-under-test".into())
}

/// The value of a step of the rig that cannot fail for a good test: its failure is the test's.
fn must<T, E: std::fmt::Display>(what: &str, result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{what}: {error}"),
    }
}

/// As [`must`], for a step that finds nothing.
fn must_some<T>(what: &str, option: Option<T>) -> T {
    match option {
        Some(value) => value,
        None => panic!("{what}"),
    }
}

struct OneTree {
    list: Arc<TreeListL2<Vec<u8>>>,
}

#[async_trait::async_trait]
impl Trees for OneTree {
    async fn list(&self, tree: &TreeRef) -> Result<Arc<dyn ListL2>, ServerTreeError> {
        if *tree == tree_ref() {
            Ok(self.list.clone())
        } else {
            Err(ServerTreeError::Unknown(tree.clone()))
        }
    }
}

struct Mapper;

impl CounterMapper for Mapper {
    fn map(
        &self,
        _holder: HolderId,
        memfd: std::os::fd::OwnedFd,
    ) -> Result<Arc<dyn HolderCounters>, EntryError> {
        Ok(Arc::new(must("rig counters", MemfdCounters::new(memfd))))
    }
}

struct Connect {
    runtime: tokio::runtime::Handle,
    server: MountServer,
}

#[async_trait::async_trait]
impl Connector for Connect {
    async fn connect(&self) -> std::io::Result<UnixStream> {
        let (mine, theirs) = StdStream::pair()?;
        mine.set_nonblocking(true)?;
        theirs.set_nonblocking(true)?;
        let server = self.server.clone();
        self.runtime.spawn(async move {
            server
                .serve_connection(must("rig stream", UnixStream::from_std(theirs)))
                .await;
        });
        UnixStream::from_std(mine)
    }
}

/// One tree's packs served to a mount client.
pub struct Rig {
    runtime: Runtime,
    dir: TestDir,
    store: Arc<FileEntryStore>,
    engine: Arc<FakeEngine>,
    l2: Vec<u8>,
    tree_id: [u8; 32],
    ids: Vec<PackId>,
    packs: Vec<Vec<u8>>,
    client: PackClient,
    image: Mutex<Option<std::path::PathBuf>>,
}

impl std::fmt::Debug for Rig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rig")
            .field("dir", &self.dir.path())
            .finish_non_exhaustive()
    }
}

impl Rig {
    /// A rig under `root` (a test passes `env!("CARGO_TARGET_TMPDIR")`) for the L2 list `l2`,
    /// whose packs' bytes are `packs`, by their number in the list. Every pack's waits park.
    ///
    /// # Panics
    /// If the list does not open or names other packs than `packs`.
    #[must_use]
    pub fn new(root: &Path, name: &str, l2: Vec<u8>, packs: Vec<Vec<u8>>) -> Self {
        let list = must("rig list", TreeListL2::open(l2.clone()));
        assert_eq!(
            list.pack_count() as usize,
            packs.len(),
            "rig: one bytes per pack"
        );
        let ids: Vec<PackId> = (0..list.pack_count())
            .map(|p| PackId {
                key: must_some(
                    &format!("rig: pack {p}"),
                    list.pack_record(p).map(|r| r.key),
                ),
                root: must_some(&format!("rig: root {p}"), list.pack_root(p)),
            })
            .collect();
        let tree_id = *list.tree_id();
        let dir = TestDir::under(root, name);
        let store = Arc::new(must(
            "rig store",
            FileEntryStore::new(&dir.path().join("cache")),
        ));
        let engine = Arc::new(FakeEngine::new());
        for id in &ids {
            engine.park(id.clone());
        }
        let runtime = must(
            "rig runtime",
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build(),
        );
        let server = MountServer::new(
            Parts {
                store: store.clone(),
                engine: engine.clone(),
                trees: Arc::new(OneTree {
                    list: Arc::new(list),
                }),
                counters: Arc::new(Mapper),
            },
            tierstore_fetch_api::wire::Epoch(1),
            ServerConfig::default(),
        );
        let client = must(
            "rig client",
            PackClient::start(
                HolderId(1),
                Arc::new(Connect {
                    runtime: runtime.handle().clone(),
                    server,
                }),
                Arc::new(ManualClock::new()),
                Config {
                    jitter_seed: Some(1),
                    ..Config::default()
                },
            ),
        );
        Self {
            runtime,
            dir,
            store,
            engine,
            l2,
            tree_id,
            ids,
            packs,
            client,
            image: Mutex::new(None),
        }
    }

    /// The tree id the list names.
    #[must_use]
    pub const fn tree_id(&self) -> &[u8; 32] {
        &self.tree_id
    }

    /// The mount client.
    #[must_use]
    pub const fn client(&self) -> &PackClient {
        &self.client
    }

    /// A runtime the test may run tasks on (the server's).
    #[must_use]
    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// The scratch directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// The image file: the list written once to the scratch directory, opened read-only.
    ///
    /// # Panics
    /// If the file cannot be written or opened.
    #[must_use]
    pub fn image_file(&self) -> File {
        let mut path = self
            .image
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = path.get_or_insert_with(|| {
            let path = self.dir.path().join("tree.l2");
            must("rig image", std::fs::write(&path, &self.l2));
            path
        });
        must("rig image", File::open(path))
    }

    /// The image, opened with `options`.
    ///
    /// # Panics
    /// If it does not open.
    #[must_use]
    pub fn image(&self, options: ImageOptions) -> Image {
        must(
            "rig image",
            Image::open(self.image_file(), &self.tree_id, options),
        )
    }

    /// The tree, opened for [`JOB`] with every pack open.
    ///
    /// # Panics
    /// If it does not open.
    #[must_use]
    pub fn tree(&self, options: ImageOptions) -> Tree {
        let image = self.image(options);
        let config = TreeConfig {
            tree: tree_ref(),
            job: JOB,
        };
        must(
            "rig tree",
            self.runtime
                .block_on(Tree::open(image, &self.client, config)),
        )
    }

    /// The id of pack `pack`.
    #[must_use]
    pub fn id(&self, pack: u32) -> &PackId {
        &self.ids[pack as usize]
    }

    /// The bytes of pack `pack`.
    #[must_use]
    pub fn bytes(&self, pack: u32) -> &[u8] {
        &self.packs[pack as usize]
    }

    /// The server's entry of pack `pack` (made by the first `open-pack`).
    ///
    /// # Panics
    /// If the pack was not opened.
    #[must_use]
    pub fn entry(&self, pack: u32) -> Arc<FileEntry> {
        must_some(
            &format!("rig: pack {pack} not open"),
            self.store.file_entry(self.id(pack)),
        )
    }

    /// Publishes `cells` of pack `pack` with the pack's bytes.
    ///
    /// # Panics
    /// If a cell is not claimable or the publish fails.
    pub fn publish(&self, pack: u32, cells: CellRun) {
        let entry = self.entry(pack);
        let bytes = self.bytes(pack);
        for cell in cells.cells() {
            let start = (u64::from(cell) << 20) as usize;
            let end = (start + (1 << 20)).min(bytes.len());
            let token = self.claim(&entry, pack, cell);
            must("rig publish", entry.publish(token, &bytes[start..end]));
        }
    }

    /// Claims cell `cell` of pack `pack`.
    fn claim(&self, entry: &FileEntry, pack: u32, cell: u32) -> ClaimedCell {
        let claimed = entry
            .claim(CellRun {
                first: cell,
                count: 1,
            })
            .pop();
        must_some(
            &format!("rig: cell {cell} of pack {pack} not claimable"),
            claimed,
        )
    }

    /// Publishes every cell of every pack.
    pub fn publish_all(&self) {
        for pack in 0..self.ids.len() as u32 {
            let count = (self.bytes(pack).len() as u64).div_ceil(1 << 20) as u32;
            self.publish(pack, CellRun { first: 0, count });
        }
    }

    /// One step of the evictor on cell `cell` of pack `pack`; whether the CAS won.
    #[must_use]
    pub fn evict_step(&self, pack: u32, cell: u32, step: EvictStep) -> bool {
        self.entry(pack).evict_cell_cas(cell, step)
    }

    /// Evicts cell `cell` of pack `pack` whole (begin, punch, finish), with no wait for readers:
    /// the test has none in flight.
    ///
    /// # Panics
    /// If the cell is not `present` or the punch fails.
    pub fn evict(&self, pack: u32, cell: u32) {
        let entry = self.entry(pack);
        assert!(
            entry.evict_cell_cas(cell, EvictStep::Begin),
            "rig: cell {cell} not present"
        );
        must(
            "rig punch",
            entry.punch(&[CellRun {
                first: cell,
                count: 1,
            }]),
        );
        assert!(entry.evict_cell_cas(cell, EvictStep::Finish));
    }

    /// Marks cell `cell` of pack `pack` `bad` (claimed, then failed twice).
    ///
    /// # Panics
    /// If the cell is not claimable.
    pub fn mark_bad(&self, pack: u32, cell: u32) {
        let entry = self.entry(pack);
        let token = self.claim(&entry, pack, cell);
        entry.mark_bad(token);
    }

    /// The sum of the reader counters of 16-cell group `group` of pack `pack`.
    #[must_use]
    pub fn readers(&self, pack: u32, group: u32) -> u64 {
        self.entry(pack).group_readers(group)
    }

    /// The waits parked in the engine on pack `pack` now.
    #[must_use]
    pub fn parked(&self, pack: u32) -> usize {
        self.engine.parked(self.id(pack))
    }

    /// Blocks until `n` waits are parked in the engine on pack `pack`.
    pub fn until_parked(&self, pack: u32, n: usize) {
        while self.parked(pack) != n {
            std::thread::yield_now();
        }
    }

    /// Answers every parked wait on pack `pack` with `outcome`; later waits park again, except
    /// after `Lost`, which answers every later wait too. The number answered.
    pub fn answer(&self, pack: u32, outcome: WaitOutcome) -> usize {
        let id = self.id(pack).clone();
        let answered = self.engine.complete(&id, outcome);
        if outcome != WaitOutcome::Lost {
            self.engine.park(id);
        }
        answered
    }
}
