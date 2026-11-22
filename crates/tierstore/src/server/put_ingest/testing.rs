//! The bench of the put ingest's tests: a cache that takes writes over a tamper upstream, a put
//! root in its scratch directory, the ingest serving them on a task of its own with a clock the
//! test sets, and the reports it makes; and the commit of an object as kbf-task makes it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use super::{PutIngest, Report};
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::testing::Rig;
use crate::{Backend as _, TestClock, TreeDigest};

/// A cache, a put root and the ingest of the root into the cache.
pub(super) struct Bench {
    pub(super) rig: Rig,
    pub(super) cache: LocalCache,
    pub(super) root: PathBuf,
    /// The clock of the ingest: the time of the grace and of abandoned directories.
    pub(super) clock: Arc<TestClock>,
    reports: Option<mpsc::UnboundedReceiver<Report>>,
    halt: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl Bench {
    /// A bench in a scratch directory called `name`, with the root made and nothing started.
    pub(super) fn new(name: &str) -> Self {
        let rig = Rig::writing(name);
        let cache = rig.locked();
        let root = rig.dir.path().join("put");
        std::fs::create_dir_all(&root).unwrap();
        Self {
            rig,
            cache,
            root,
            clock: Arc::new(TestClock::new()),
            reports: None,
            halt: Arc::new(Notify::new()),
            task: None,
        }
    }

    /// The job's directory `cas/b/<build>/<nonce>`, made with its parents.
    pub(super) fn job(&self, build: &str, nonce: &str) -> PathBuf {
        let dir = self.root.join("cas/b").join(build).join(nonce);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Starts the ingest of the root into `cache`, and does not wait.
    pub(super) fn spawn_on(&mut self, cache: LocalCache) {
        let (tell, reports) = mpsc::unbounded_channel();
        let clock: Arc<dyn crate::Clock> = self.clock.clone();
        let ingest = PutIngest::new(cache, self.root.clone()).telling(clock, tell);
        let halt = Arc::clone(&self.halt);
        self.task = Some(tokio::spawn(ingest.serve(halt)));
        self.reports = Some(reports);
    }

    /// Starts the ingest of the root into `cache`, and waits until it has taken the root.
    pub(super) async fn start_on(&mut self, cache: LocalCache) {
        self.spawn_on(cache);
        self.wait(|report| *report == Report::Watching).await;
    }

    /// Whether the ingest has reported nothing that was not taken yet.
    pub(super) fn quiet(&mut self) -> bool {
        let reports = self.reports.as_mut().expect("a started ingest");
        reports.try_recv().is_err()
    }

    /// Starts the ingest of the root into the bench's cache.
    pub(super) async fn start(&mut self) {
        let cache = self.cache.clone();
        self.start_on(cache).await;
    }

    /// The next report.
    pub(super) async fn next(&mut self) -> Report {
        let reports = self.reports.as_mut().expect("a started ingest");
        reports.recv().await.expect("the ingest to report")
    }

    /// Waits for the first report that `want` takes, and gives it.
    pub(super) async fn wait(&mut self, want: impl Fn(&Report) -> bool) -> Report {
        loop {
            let report = self.next().await;
            if want(&report) {
                return report;
            }
        }
    }

    /// Waits for the next report that is not of a scan or of the start: what was done to a file.
    pub(super) async fn outcome(&mut self) -> Report {
        self.wait(|report| !matches!(report, Report::Scanned | Report::Watching))
            .await
    }

    /// Makes a whole scan run, and waits for its end: a directory appears under `cas/b`, which the
    /// watcher says.
    pub(super) async fn scan(&mut self, marker: &str) {
        std::fs::create_dir_all(self.root.join("cas/b").join(marker)).unwrap();
        self.wait(|report| *report == Report::Scanned).await;
    }

    /// The bytes of the object `key` in the upstream, or none.
    pub(super) async fn object(&self, key: &str) -> Option<Vec<u8>> {
        let got = self.rig.tamper.get(key, None).await.ok()?;
        Some(got.to_vec())
    }

    /// Halts the ingest and waits for it to end.
    pub(super) async fn stop(&mut self) {
        self.halt.notify_one();
        if let Some(task) = self.task.take() {
            task.await.unwrap();
        }
    }
}

/// The digest of `bytes`.
pub(super) fn sha(bytes: &[u8]) -> TreeDigest {
    TreeDigest::of(bytes)
}

/// Writes `bytes` into `dir` and commits it as `name`, as kbf-task commits its object: created as
/// `<name>.part`, renamed to `<name>.sha256-<hex of declared>.part`, and that renamed to `name`.
pub(super) fn commit(dir: &Path, name: &str, bytes: &[u8], declared: TreeDigest) {
    let part = dir.join(format!("{name}.part"));
    std::fs::write(&part, bytes).unwrap();
    let staged = dir.join(format!("{name}.sha256-{declared}.part"));
    std::fs::rename(&part, &staged).unwrap();
    std::fs::rename(&staged, dir.join(name)).unwrap();
}
