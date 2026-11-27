//! What the tests of the staged-file store share: a scratch directory with a stage and a cache in
//! it, a clock and a disk the test sets, and short ways to stage and commit a file.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tierstore_testkit::dir::TestDir;
use tokio::runtime::{Handle as RuntimeHandle, Runtime};
use tokio::sync::Semaphore;
use tracing::Level;
use tracing::subscriber::{DefaultGuard, set_default};

use super::commit::PutOutcome;
use super::error::PutError;
use super::names::{PutDir, Sha256Digest};
use super::replay::Replayed;
use super::space::PutSpace;
use super::store::{Handle, PutSettings, Rename, StagedStore};
use super::upload::Uploader;
use crate::server::build_info::stderr_log::LineLog;
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::testing::{FakeDisk, Rig as NodeRig};
use crate::server::treefs::in_process::InProcess;
use crate::server::treefs::testing::until;
use crate::{Backend as _, Error, Op, TestClock};

/// The time the clock of a rig starts at.
pub(in crate::server::treefs) const START_MS: u64 = 1_800_000_000_000;

/// The prefix the settings of a rig allow.
pub(super) const PREFIX: &str = "cas/b/";

/// The default TTLs (12 hours staged, 10 minutes committed), under [`PREFIX`], with no limit.
pub(super) fn settings() -> PutSettings {
    PutSettings {
        prefixes: vec![PREFIX.to_owned()],
        max_object_bytes: None,
        max_dir_bytes: None,
        max_dir_files: None,
        min_free_bytes: None,
        stage_ttl: Duration::from_secs(43_200),
        committed_ttl: Duration::from_secs(600),
    }
}

/// The directory `path`, under [`PREFIX`].
pub(super) fn dir(path: &str) -> PutDir {
    PutDir::new(path, &[PREFIX.to_owned()]).unwrap()
}

/// The SHA-256 of `bytes`.
pub(super) fn sha(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(Sha256::digest(bytes).into())
}

/// A scratch directory with `stage/` and `cache/`, a clock at [`START_MS`] and a disk whose free
/// space a test sets.
pub(in crate::server::treefs) struct Rig {
    /// The scratch directory.
    pub root: TestDir,
    /// The clock of every store the rig opens.
    pub clock: Arc<TestClock>,
    /// The disk of every store the rig opens.
    pub disk: Arc<FakeDisk>,
    /// The settings of the next store the rig opens.
    pub settings: PutSettings,
}

impl Rig {
    /// A rig in a new scratch directory named after `name`, with [`settings`].
    pub(super) fn new(name: &str) -> Self {
        let root = TestDir::under(std::env::temp_dir(), name);
        std::fs::create_dir(root.path().join("cache")).unwrap();
        let clock = Arc::new(TestClock::new());
        clock.set(START_MS);
        Self {
            root,
            clock,
            disk: Arc::new(FakeDisk::new()),
            settings: settings(),
        }
    }

    /// The stage directory.
    pub(super) fn stage(&self) -> PathBuf {
        self.root.path().join("stage")
    }

    /// The cache directory.
    pub(super) fn cache(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    /// Opens a store over the stage, and says what it replayed.
    pub(super) fn open_replayed(&self) -> (StagedStore, Replayed) {
        StagedStore::open(
            self.settings.clone(),
            &self.stage(),
            &self.cache(),
            self.clock.clone(),
            self.disk.clone(),
        )
        .unwrap()
    }

    /// Opens a store over the stage.
    pub(super) fn open(&self) -> StagedStore {
        self.open_replayed().0
    }

    /// The names in the stage directory, in order.
    pub(super) fn stage_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.stage())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The journal's text.
    pub(super) fn journal(&self) -> String {
        std::fs::read_to_string(self.stage().join("journal")).unwrap()
    }

    /// The staged file (`<number>.stage`) that holds exactly `bytes`.
    fn stage_file_holding(&self, bytes: &[u8]) -> PathBuf {
        std::fs::read_dir(self.stage())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension().is_some_and(|ext| ext == "stage")
                    && std::fs::read(path).unwrap() == bytes
            })
            .expect("a staged file that holds those bytes")
    }

    /// Writes other bytes of the same length over the staged file that holds `bytes`, in place:
    /// a reader that has the file open reads what is now there, which is no longer what hashed
    /// to the digest of the commit.
    pub(super) fn corrupt_staged(&self, bytes: &[u8]) {
        let path = self.stage_file_holding(bytes);
        std::fs::write(path, vec![0xff; bytes.len()]).unwrap();
    }

    /// Moves the staged file that holds `bytes` aside and leaves a symbolic link to it in its
    /// place, which the cache's put does not follow: the file cannot be opened for the put.
    pub(super) fn link_staged(&self, bytes: &[u8]) -> Linked {
        let path = self.stage_file_holding(bytes);
        let moved = path.with_extension("moved");
        std::fs::rename(&path, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &path).unwrap();
        Linked { path, moved }
    }
}

/// A staged file that [`Rig::link_staged`] replaced by a link.
#[must_use]
pub(super) struct Linked {
    path: PathBuf,
    moved: PathBuf,
}

impl Linked {
    /// Puts the staged file back in its place, over the link.
    pub(super) fn restore(self) {
        std::fs::rename(self.moved, self.path).unwrap();
    }
}

/// Creates `name` in `dir`, writes `bytes` at 0 in one write, and gives the handle, still open.
pub(super) fn written(store: &StagedStore, dir: &PutDir, name: &str, bytes: &[u8]) -> Handle {
    let handle = store.create(dir, name).unwrap();
    assert_eq!(store.write(handle, 0, bytes).unwrap(), bytes.len());
    handle
}

/// Creates `name` in `dir`, writes `bytes`, and releases the handle.
pub(super) fn staged(store: &StagedStore, dir: &PutDir, name: &str, bytes: &[u8]) {
    let handle = written(store, dir, name, bytes);
    store.release(handle);
}

/// Renames `src` to the final name `dst` and finishes the commit with `outcome`.
pub(super) fn commit(
    store: &StagedStore,
    dir: &PutDir,
    src: &str,
    dst: &str,
    outcome: PutOutcome,
) -> Result<(), PutError> {
    match store.rename(dir, src, dst)? {
        Rename::Commit(commit) => commit.finish(outcome),
        Rename::Renamed => panic!("{src} -> {dst} is a commit"),
    }
}

/// The warnings the store logs while a test holds the guard of [`warnings`]: the operator's
/// stderr.
#[derive(Clone, Default)]
pub(super) struct Warnings(Arc<Mutex<Vec<u8>>>);

impl Warnings {
    /// The lines written so far.
    pub(super) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for Warnings {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Logs the warnings of this thread to the [`Warnings`] given back, until the guard drops.
pub(super) fn warnings() -> (DefaultGuard, Warnings) {
    let warnings = Warnings::default();
    let guard = set_default(LineLog::new(warnings.clone(), Level::WARN));
    (guard, warnings)
}

/// The commit deadline of a [`SpaceRig`].
pub(super) const DEADLINE: Duration = Duration::from_secs(30);

/// How long a try of the uploads of a [`SpaceRig::read_only`] waits for the write role of its
/// cache, which never comes: the answer does not depend on it.
const READ_ONLY_WAIT: Duration = Duration::from_millis(10);

/// The owner of the files and of the directories under a prefix of a [`SpaceRig`].
pub(in crate::server::treefs) const OWNER: (u32, u32) = (1234, 5678);

/// A runtime the cache's work runs on, shut down without waiting when dropped.
struct Work(Option<Runtime>);

impl Work {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        Self(Some(runtime))
    }

    fn handle(&self) -> RuntimeHandle {
        self.0.as_ref().unwrap().handle().clone()
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// The node's cache that the puts of a [`SpaceRig`] reach, over an upstream that keeps what is put,
/// records the puts, and fails or holds them where a test says.
pub(in crate::server::treefs) struct Puts {
    /// The cache the puts are made to, opened as a node's is: it writes through for the keys it
    /// keeps.
    cache: LocalCache,
    /// The cache of another process, which holds the lock of the directory of [`Puts::cache`].
    _holder: Option<LocalCache>,
    /// The cache, its upstream and its scratch directory.
    node: NodeRig,
    /// The gate [`Puts::hold`] closed.
    gate: Mutex<Option<Arc<Semaphore>>>,
    /// Last: what the cache's work runs on.
    work: Work,
}

impl Puts {
    /// A cache that has the write role, or one that never has it (`read_only`: another cache
    /// holds the lock of its directory), in a new scratch directory.
    fn new(read_only: bool) -> Self {
        let work = Work::new();
        let handle = work.handle();
        let _entered = handle.enter();
        let node = NodeRig::writing("put-node");
        let (cache, holder) = if read_only {
            // The cache that takes the lock first has the write role; the one after it waits.
            let holder = node.locked();
            (node.locked(), Some(holder))
        } else {
            (node.cache.clone(), None)
        };
        Self {
            cache,
            _holder: holder,
            node,
            gate: Mutex::new(None),
            work,
        }
    }

    /// What the uploads call: the put of [`InProcess`] over the cache.
    pub(super) fn asks(&self) -> Arc<InProcess> {
        Arc::new(InProcess::new(
            (self.cache.clone(), None),
            self.work.handle(),
            DEADLINE,
            Arc::new(|_| {}),
        ))
    }

    /// Makes every put wait, once it reaches the upstream, until [`Puts::let_go`].
    pub(in crate::server::treefs) fn hold(&self) {
        let gate = self.node.tamper.hold_puts();
        *self.gate.lock().unwrap() = Some(gate);
    }

    /// Lets the waiting puts, and the next ones, go on.
    pub(in crate::server::treefs) fn let_go(&self) {
        let gate = self.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.add_permits(1 << 16);
        }
    }

    /// Waits until `count` puts are waiting at the upstream.
    pub(in crate::server::treefs) fn wait_for_held(&self, count: u64) {
        until(|| self.node.tamper.at_puts() >= count);
    }

    /// Fails the next put that reaches the upstream with `error`, after those failed before.
    pub(in crate::server::treefs) fn fail_next(&self, error: Error) {
        self.node.upstream.fail_next_with(Op::Put, error);
    }

    /// Fails the next `count` puts that reach the upstream with the error of an upstream that
    /// does not answer.
    pub(in crate::server::treefs) fn fail_next_times(&self, count: u32) {
        self.node.upstream.fail_next(Op::Put, count);
    }

    /// The bytes of the object `key` in the upstream.
    pub(in crate::server::treefs) fn object(&self, key: &str) -> Option<Vec<u8>> {
        let got = self.work.handle().block_on(self.node.tamper.get(key, None));
        got.ok().map(|bytes| bytes.to_vec())
    }

    /// The puts that reached the upstream.
    pub(in crate::server::treefs) fn count(&self) -> usize {
        usize::try_from(self.node.calls(Op::Put)).unwrap()
    }

    /// The keys of the puts that reached the upstream, in order.
    pub(in crate::server::treefs) fn keys(&self) -> Vec<String> {
        let log = self.node.upstream.call_log();
        log.into_iter()
            .filter_map(|(op, key)| (op == Op::Put).then_some(key))
            .collect()
    }
}

/// A put directory over the store of a [`Rig`], uploading to the cache of a [`Puts`], with the
/// rig's clock, whose pauses between tries move that clock and are recorded.
pub(in crate::server::treefs) struct SpaceRig {
    pub space: PutSpace,
    pub puts: Arc<Puts>,
    /// The pauses the uploads took, in order.
    pub pauses: Arc<Mutex<Vec<Duration>>>,
    /// Last: the scratch directory goes after everything that is in it.
    pub rig: Rig,
}

impl SpaceRig {
    /// A put directory of generation 1 in a new scratch directory named after `name`.
    pub(in crate::server::treefs) fn new(name: &str) -> Self {
        Self::of(Rig::new(name), 1, false)
    }

    /// A put directory whose cache never takes the write role: the uploads wait for it.
    pub(super) fn read_only(name: &str) -> Self {
        Self::of(Rig::new(name), 1, true)
    }

    /// A put directory of `generation` over the store of `rig`, with a cache that has the write
    /// role or one that never has it.
    pub(super) fn of(rig: Rig, generation: u32, read_only: bool) -> Self {
        let puts = Arc::new(Puts::new(read_only));
        let pauses = Arc::new(Mutex::new(Vec::new()));
        let (clock, taken) = (rig.clock.clone(), Arc::clone(&pauses));
        let pause = Arc::new(move |pause: Duration| {
            taken.lock().unwrap().push(pause);
            clock.advance(u64::try_from(pause.as_millis()).unwrap());
        });
        let wait = if read_only { READ_ONLY_WAIT } else { DEADLINE };
        let uploader = Uploader::new(puts.asks(), wait, rig.clock.clone(), pause);
        let space = PutSpace::new(
            rig.open(),
            uploader,
            (OWNER, generation),
            DEADLINE,
            rig.clock.clone(),
        );
        Self {
            space,
            puts,
            pauses,
            rig,
        }
    }

    /// The process after this one: a store opened again over the same stage, and the generation
    /// after, with a cache of its own.
    pub(super) fn successor(self, generation: u32) -> Self {
        let Self {
            space, puts, rig, ..
        } = self;
        drop((space, puts));
        Self::of(rig, generation, false)
    }
}
