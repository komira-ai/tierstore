//! What the tests of the staged-file store share: a scratch directory with a stage and a cache in
//! it, a clock and a disk the test sets, and short ways to stage and commit a file.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tierstore_testkit::dir::TestDir;
use tracing::Level;
use tracing::subscriber::{DefaultGuard, set_default};

use super::commit::PutOutcome;
use super::error::PutError;
use super::names::{PutDir, Sha256Digest};
use super::replay::Replayed;
use super::store::{Handle, PutSettings, Rename, StagedStore};
use crate::TestClock;
use crate::server::build_info::stderr_log::LineLog;
use crate::server::local_cache::testing::FakeDisk;

/// The time the clock of a rig starts at.
pub(super) const START_MS: u64 = 1_800_000_000_000;

/// The prefix the settings of a rig allow.
pub(super) const PREFIX: &str = "cas/b/";

/// The settings of the design's defaults, under [`PREFIX`], with no limit.
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
pub(super) struct Rig {
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
