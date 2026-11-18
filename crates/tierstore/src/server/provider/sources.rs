//! The sources this process prepared, and the four calls of the contract over them.
//!
//! A source this process prepared is held in a table, with the stage of its preparation, which a
//! `Mount` waits on. What a release, a sweep or a mount needs beyond the table it finds in the tree
//! store by the source's path (module `names`), so that after a restart of this process, which
//! empties the table, a source the agent prepared before is still mounted and released.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tierstore_trees_api::provider::v1::{MountMode, Source};
use tokio::sync::{Semaphore, watch};

use super::names::{SourceId, check_name, source_of};
use super::prepare::{Job, run};
use super::{MOUNT_WAIT, ProviderError, ProviderFlags};
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::trees::{LifeError, PlacementId, TreeStore};
use crate::{MAX_TREE_LIST_BYTES, TreeDigest, tree_list_digest};

/// Where the preparation of a source is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    /// The tree list is being read, or the tree made.
    Preparing,
    /// The tree is placed: it can be opened, and its files may still be filling.
    Placed,
    /// What the source names cannot be used: why.
    Refused(String),
    /// The preparation failed for a reason that may pass: why. The next `Mount` starts it again.
    Failed(String),
    /// The source was released.
    Released,
}

/// Whose a source is, as a preparation that placed its tree finds it.
pub(super) enum Owner {
    /// Still the preparation's own.
    Ours,
    /// A later preparation's: the source was released and prepared again meanwhile.
    Newer,
    /// Nobody's: the source was released.
    Gone,
}

/// What the provider holds of a source it prepared.
struct Held {
    /// The key of the tree list the source was prepared with, and the digest it names.
    key: String,
    digest: TreeDigest,
    /// The stage, which a `Mount` waits on.
    stage: Arc<watch::Sender<Stage>>,
    /// Set by a release: the preparation stops and gives up what it made.
    released: Arc<AtomicBool>,
}

/// The settings of the provider.
#[derive(Clone, Debug)]
pub(super) struct Settings {
    /// The mount of `tierstore fs`.
    pub mountpoint: PathBuf,
    /// The longest read of a tree list.
    pub list_timeout: Duration,
    /// The longest read of the bytes of one file.
    pub fill_timeout: Duration,
    /// The longest a `Mount` waits for its tree.
    pub mount_wait: Duration,
    /// How long a placement lasts unless it is released.
    pub ttl: Duration,
    /// The largest tree list read.
    pub max_list_bytes: usize,
}

/// What the preparations share.
pub(super) struct Shared {
    /// The node's trees.
    pub store: TreeStore,
    /// The node's cache, which the tree lists are read through.
    pub cache: LocalCache,
    pub settings: Settings,
    /// The reads of files of every source together: `trees.fill_concurrency` at once. Closed when
    /// the process stops, and the reads not begun are then not made.
    pub fills: Semaphore,
    sources: Mutex<BTreeMap<SourceId, Held>>,
}

/// The mount provider of a node. A handle: clones share the table.
#[derive(Clone)]
pub(in crate::server) struct Provider {
    pub(super) shared: Arc<Shared>,
}

impl Shared {
    /// Whose the source `id` is, to the preparation whose release flag is `released`.
    pub(super) fn owner(&self, id: &SourceId, released: &Arc<AtomicBool>) -> Owner {
        let sources = self.sources.lock().unwrap_or_else(PoisonError::into_inner);
        match sources.get(id) {
            Some(held) if Arc::ptr_eq(&held.released, released) => Owner::Ours,
            Some(_) => Owner::Newer,
            None => Owner::Gone,
        }
    }
}

impl Provider {
    /// The provider of the node whose trees are `store`, over `cache`, as the interface's `flags`
    /// say: its placements last `trees.ttl_max`, and it reads at most `trees.fill_concurrency`
    /// files at once.
    pub(in crate::server) fn new(
        store: TreeStore,
        cache: LocalCache,
        flags: &ProviderFlags,
    ) -> Self {
        let settings = Settings {
            mountpoint: flags.mountpoint.clone(),
            list_timeout: flags.fetch_timeout,
            fill_timeout: flags.fetch_timeout,
            mount_wait: MOUNT_WAIT,
            ttl: store.config().ttl_max,
            max_list_bytes: MAX_TREE_LIST_BYTES,
        };
        Self::with(store, cache, settings)
    }

    /// The same, with `settings`.
    pub(super) fn with(store: TreeStore, cache: LocalCache, settings: Settings) -> Self {
        let fills = Semaphore::new(store.config().fill_concurrency);
        Self {
            shared: Arc::new(Shared {
                store,
                cache,
                settings,
                fills,
                sources: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<SourceId, Held>> {
        self.shared
            .sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Stops the reads of files that have not begun: the process is stopping.
    pub(in crate::server) fn halt(&self) {
        self.shared.fills.close();
    }

    /// `Prepare`: checks the source, the ref and the mode, and begins the preparation in the
    /// background. A source prepared before with the same ref is a success, and its preparation is
    /// begun again if it failed. The first `Prepare` of a run is the request of
    /// `tierstore_request_to_start_seconds` (`local_cache/starts.rs`).
    ///
    /// # Errors
    /// [`ProviderError::Invalid`] for a source or a ref that is not well formed;
    /// [`ProviderError::Refused`] for a mode other than read-only, or a source prepared with
    /// another ref.
    pub(super) fn prepare(
        &self,
        source: Option<Source>,
        reference: &str,
        mode: i32,
    ) -> Result<(), ProviderError> {
        let id = SourceId::of(source)?;
        let digest = tree_list_digest(reference).ok_or_else(|| {
            ProviderError::Invalid(
                "the ref is not the key of a tree list: cas/l/ and 64 lowercase hex digits"
                    .to_owned(),
            )
        })?;
        if !matches!(
            MountMode::try_from(mode),
            Ok(MountMode::Unspecified | MountMode::ReadOnly)
        ) {
            let why = "tierstore makes read-only trees: the mode must be READ_ONLY";
            return Err(ProviderError::Refused(why.to_owned()));
        }
        // The first `Prepare` of the run stamps it; a later one keeps the stamp while the cache
        // keeps the run (until a release or a sweep of a source of it, or the cache's bound).
        self.shared.cache.run_requested(&id.run_dir());
        let mut sources = self.lock();
        if let Some(held) = sources.get(&id) {
            if held.key != reference {
                return Err(ProviderError::Refused(format!(
                    "the source was prepared with another ref, {}",
                    held.key
                )));
            }
            self.again_if_failed(&id, held);
            return Ok(());
        }
        let held = Held {
            key: reference.to_owned(),
            digest,
            stage: Arc::new(watch::channel(Stage::Preparing).0),
            released: Arc::new(AtomicBool::new(false)),
        };
        self.start(&id, &held);
        sources.insert(id, held);
        Ok(())
    }

    /// Holds the source `id` as prepared with `key`, and gives the preparation to the test to run.
    #[cfg(test)]
    pub(super) fn hold(&self, id: SourceId, key: &str) -> (Job, watch::Receiver<Stage>) {
        let held = Held {
            key: key.to_owned(),
            digest: tree_list_digest(key).expect("a list key"),
            stage: Arc::new(watch::channel(Stage::Preparing).0),
            released: Arc::new(AtomicBool::new(false)),
        };
        let seen = held.stage.subscribe();
        let job = Job {
            id: id.clone(),
            key: held.key.clone(),
            digest: held.digest,
            stage: Arc::clone(&held.stage),
            released: Arc::clone(&held.released),
        };
        self.lock().insert(id, held);
        (job, seen)
    }

    /// Begins the preparation of `held` again if it failed.
    fn again_if_failed(&self, id: &SourceId, held: &Held) {
        if matches!(*held.stage.borrow(), Stage::Failed(_)) {
            self.start(id, held);
        }
    }

    /// Begins the preparation of the source `id`, held as `held`, on a task of its own.
    fn start(&self, id: &SourceId, held: &Held) {
        held.stage.send_replace(Stage::Preparing);
        let job = Job {
            id: id.clone(),
            key: held.key.clone(),
            digest: held.digest,
            stage: Arc::clone(&held.stage),
            released: Arc::clone(&held.released),
        };
        tokio::spawn(run(Arc::clone(&self.shared), job));
    }

    /// `Mount`: the directory of the source's tree, once it is placed.
    ///
    /// # Errors
    /// [`ProviderError::Unavailable`] if the tree is not placed within the wait, or its
    /// preparation failed (and is begun again), or the source was never prepared here;
    /// [`ProviderError::Refused`] if the ref cannot be used, or the source was released.
    pub(super) async fn mount(&self, source: Option<Source>) -> Result<PathBuf, ProviderError> {
        let id = SourceId::of(source)?;
        let path = self.shared.settings.mountpoint.join(id.path());
        let mut stage = {
            let sources = self.lock();
            let Some(held) = sources.get(&id) else {
                return match self.placement_at(&id) {
                    Some(_) => Ok(path),
                    None => Err(ProviderError::Unavailable(
                        "the source is not prepared here".to_owned(),
                    )),
                };
            };
            self.again_if_failed(&id, held);
            held.stage.subscribe()
        };
        let waited = tokio::time::timeout(
            self.shared.settings.mount_wait,
            stage.wait_for(|stage| *stage != Stage::Preparing),
        )
        .await
        .map_err(|_| ProviderError::Unavailable("not ready".to_owned()))?;
        // The sender is dropped only after the source's stage was set to released.
        let now = waited.map_or(Stage::Released, |now| (*now).clone());
        match now {
            // A placement whose lease ended without a release (a run longer than `trees.ttl_max`,
            // or a clock that moved) is made again.
            Stage::Preparing | Stage::Placed if self.placement_at(&id).is_none() => {
                if let Some(held) = self.lock().get(&id) {
                    self.start(&id, held);
                }
                Err(ProviderError::Unavailable(
                    "the tree is gone: it is being prepared again".to_owned(),
                ))
            }
            Stage::Preparing | Stage::Placed => Ok(path),
            Stage::Refused(why) => Err(ProviderError::Refused(why)),
            Stage::Failed(why) => Err(ProviderError::Unavailable(why)),
            Stage::Released => Err(ProviderError::Refused("the source was released".to_owned())),
        }
    }

    /// `Release`: stops the source's preparation and removes its placement, and forgets the request
    /// of its run. A source that is not known is a success.
    ///
    /// # Errors
    /// [`ProviderError::Invalid`] for a source that is not well formed;
    /// [`ProviderError::Unavailable`] if the store cannot change its directory now.
    pub(super) fn release(&self, source: Option<Source>) -> Result<(), ProviderError> {
        let id = SourceId::of(source)?;
        self.release_id(&id).map(drop)
    }

    /// Releases `id`: whether there was anything to release.
    fn release_id(&self, id: &SourceId) -> Result<bool, ProviderError> {
        self.shared.cache.run_forgotten(&id.run_dir());
        let held = self.lock().remove(id);
        let mut released = false;
        if let Some(held) = held {
            held.released.store(true, Ordering::SeqCst);
            held.stage.send_replace(Stage::Released);
            released = true;
        }
        // After the flag: a preparation that places the tree after this lookup sees the flag and
        // releases the placement itself.
        if let Some(placement) = self.placement_at(id) {
            let refused = self
                .shared
                .store
                .release(&placement)
                .err()
                .filter(|why| *why != LifeError::UnknownPlacement);
            if let Some(why) = refused {
                return Err(ProviderError::Unavailable(format!(
                    "the placement was not released: {why}"
                )));
            }
            released = true;
        }
        Ok(released)
    }

    /// The placement whose location is exactly the path of `id`.
    fn placement_at(&self, id: &SourceId) -> Option<PlacementId> {
        self.shared
            .store
            .locate_fs(&id.path())
            .filter(|located| located.path.is_empty())
            .map(|located| located.placement)
    }

    /// `Sweep`: releases every source of `agent` whose run is not in `keep`, those this process
    /// prepared and the placements under the agent's path; how many.
    ///
    /// # Errors
    /// [`ProviderError::Invalid`] for an agent that is not well formed; what a release gives.
    pub(super) fn sweep(&self, agent: &str, keep: &[String]) -> Result<u32, ProviderError> {
        check_name("agent", agent)?;
        let keep: BTreeSet<&str> = keep.iter().map(String::as_str).collect();
        let swept = |id: &SourceId| id.agent == agent && !keep.contains(id.run.as_str());
        let mut gone: BTreeSet<SourceId> = self
            .lock()
            .keys()
            .filter(|&id| swept(id))
            .cloned()
            .collect();
        let placed = self.shared.store.mount_paths();
        gone.extend(
            placed
                .iter()
                .filter_map(|path| source_of(path, agent))
                .filter(swept),
        );
        let mut released = 0;
        for id in &gone {
            released += u32::from(self.release_id(id)?);
        }
        Ok(released)
    }
}
