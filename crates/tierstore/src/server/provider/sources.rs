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

use tierstore_trees_api::provider::v1::{KeptAttempt, MountMode, Source};
use tokio::sync::{Semaphore, watch};

use super::names::{SourceId, check_name, source_of};
use super::prepare::{Job, run};
use super::{MOUNT_WAIT, ProviderError, ProviderFlags};
use crate::server::local_cache::trees::{LifeError, PlacementId, PreparePrefetch, TreeStore};
use crate::server::local_cache::{LocalCache, QueuedReads, Unavailable};
use crate::{MAX_TREE_LIST_BYTES, TreeDigest, tree_list_digest};

/// Where the preparation of a source is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    /// The tree's sidecar is being read, or the tree made.
    Preparing,
    /// The tree is placed: it can be opened, and its files may still be filling.
    Placed,
    /// What the source names cannot be used: why.
    Refused(String),
    /// The preparation could not read the tree's sidecar or list, for a reason that may pass: why.
    /// The next `Mount` starts it again.
    Failed(String),
    /// The tree store could not make the tree now (too many placements, trees off or read-only, a
    /// disk error): why. The next `Mount` starts it again.
    Unmade(String),
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
    /// The mount of the node's file system.
    pub mountpoint: PathBuf,
    /// Whether this process serves the file system at `mountpoint` (the file has an `fs`
    /// interface); every `Prepare` and `Mount` is [`no_fs`] if not.
    pub served: bool,
    /// The longest read of a tree's sidecar.
    pub list_timeout: Duration,
    /// The longest read of the bytes of one file.
    pub fill_timeout: Duration,
    /// The longest a `Mount` waits for its tree.
    pub mount_wait: Duration,
    /// How long a placement lasts unless it is released.
    pub ttl: Duration,
    /// The largest sidecar read.
    pub max_list_bytes: usize,
    /// The most bytes of heap the reads queued by every preparation together hold
    /// ([`PREFETCH_QUEUE_BYTES`]).
    pub max_queued_bytes: u64,
}

/// The most bytes of heap the reads queued by the preparations of the node hold together: about a
/// hundred bytes for each file, so the reads of two or three million files. A preparation whose
/// reads do not fit queues none, and its files are read when first opened
/// (`local_cache/queued_reads.rs`).
pub(super) const PREFETCH_QUEUE_BYTES: u64 = 256 << 20;

/// The reason of every `Prepare` and `Mount` of a provider whose mount `mountpoint` no process of
/// this release serves: the node's file has no `fs` interface.
pub(super) fn no_fs(mountpoint: &std::path::Path) -> String {
    format!(
        "no file system serves {}: the node's file has no [[interface]] with type = \"fs\", and \
         tierstore fs is no longer a process of its own",
        mountpoint.display()
    )
}

/// What the preparations share.
pub(super) struct Shared {
    /// The node's trees.
    pub store: TreeStore,
    /// The node's cache, which the sidecars are read through.
    pub cache: LocalCache,
    pub settings: Settings,
    /// The reads of files of every source together: `trees.fill_concurrency` at once. Closed when
    /// the process stops, and the reads not begun are then not made.
    pub fills: Semaphore,
    /// The room of the reads queued, the node's (`local_cache/queued_reads.rs`).
    pub queued: Arc<QueuedReads>,
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

/// The line the log has at the start of a provider: the mode of its `Prepare`.
pub(super) fn mode_line(mode: PreparePrefetch) -> String {
    let what = match mode {
        PreparePrefetch::Background => "Prepare reads every file of a tree in the background",
        PreparePrefetch::None => "Prepare reads no file: each is read when first opened",
    };
    format!(
        "tierstore: mount provider: trees.prepare_prefetch = \"{}\": {what}",
        mode.name()
    )
}

impl Provider {
    /// The provider of the node whose trees are `store`, over `cache`, as the interface's `flags`
    /// say: its placements last `trees.ttl_max`, and it reads at most `trees.fill_concurrency`
    /// files at once, or none if `trees.prepare_prefetch` is `none`, which the log says once here.
    pub(in crate::server) fn new(
        store: TreeStore,
        cache: LocalCache,
        flags: &ProviderFlags,
    ) -> Self {
        tracing::info!("{}", mode_line(store.config().prepare_prefetch));
        let settings = Settings {
            mountpoint: flags.mountpoint.clone(),
            served: flags.served,
            list_timeout: flags.fetch_timeout,
            fill_timeout: flags.fetch_timeout,
            mount_wait: MOUNT_WAIT,
            ttl: store.config().ttl_max,
            max_list_bytes: MAX_TREE_LIST_BYTES,
            max_queued_bytes: PREFETCH_QUEUE_BYTES,
        };
        Self::with(store, cache, settings)
    }

    /// The same, with `settings`.
    pub(super) fn with(store: TreeStore, cache: LocalCache, settings: Settings) -> Self {
        let fills = Semaphore::new(store.config().fill_concurrency);
        let queued = cache.queued_reads();
        Self {
            shared: Arc::new(Shared {
                store,
                cache,
                settings,
                fills,
                queued,
                sources: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// `UNAVAILABLE` for `reason`, which says `why`, counted once.
    fn unavailable(&self, reason: Unavailable, why: String) -> ProviderError {
        self.shared.cache.provider_unavailable(reason);
        ProviderError::Unavailable(why)
    }

    /// `UNAVAILABLE` with [`no_fs`] if no process serves the mount.
    fn served(&self) -> Result<(), ProviderError> {
        if self.shared.settings.served {
            return Ok(());
        }
        let why = no_fs(&self.shared.settings.mountpoint);
        Err(self.unavailable(Unavailable::NoFs, why))
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
    /// another ref; [`ProviderError::Unavailable`] ([`no_fs`]) if no process serves the mount.
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
        self.served()?;
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
        if matches!(*held.stage.borrow(), Stage::Failed(_) | Stage::Unmade(_)) {
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
    /// [`ProviderError::Unavailable`] if no process serves the mount ([`no_fs`]), the tree is not
    /// placed within the wait, or its preparation failed (and is begun again), or the source was
    /// never prepared here, each counted under its reason;
    /// [`ProviderError::Refused`] if the ref cannot be used, or the source was released.
    pub(super) async fn mount(&self, source: Option<Source>) -> Result<PathBuf, ProviderError> {
        let id = SourceId::of(source)?;
        self.served()?;
        let path = self.shared.settings.mountpoint.join(id.path());
        let mut stage = {
            let sources = self.lock();
            let Some(held) = sources.get(&id) else {
                return self.placed_path(&id).ok_or_else(|| {
                    self.unavailable(
                        Unavailable::NotPreparedHere,
                        "the source is not prepared here".to_owned(),
                    )
                });
            };
            self.again_if_failed(&id, held);
            held.stage.subscribe()
        };
        let waited = tokio::time::timeout(
            self.shared.settings.mount_wait,
            stage.wait_for(|stage| *stage != Stage::Preparing),
        )
        .await
        .map_err(|_| self.unavailable(Unavailable::NotReady, "not ready".to_owned()))?;
        // The sender is dropped only after the source's stage was set to released.
        let now = waited.map_or(Stage::Released, |now| (*now).clone());
        match now {
            // A placement whose lease ended without a release (a run longer than `trees.ttl_max`,
            // or a clock that moved) is made again.
            Stage::Preparing | Stage::Placed if self.placement_at(&id).is_none() => {
                if let Some(held) = self.lock().get(&id) {
                    self.start(&id, held);
                }
                Err(self.unavailable(
                    Unavailable::TreeGone,
                    "the tree is gone: it is being prepared again".to_owned(),
                ))
            }
            Stage::Preparing | Stage::Placed => Ok(path),
            Stage::Refused(why) => Err(ProviderError::Refused(why)),
            Stage::Failed(why) => Err(self.unavailable(Unavailable::ReadFailed, why)),
            Stage::Unmade(why) => Err(self.unavailable(Unavailable::StoreFailed, why)),
            Stage::Released => Err(ProviderError::Refused("the source was released".to_owned())),
        }
    }

    /// The directory of the placement of `id` that this process did not prepare: the placement at
    /// the path of `id`, or, for a source that names its attempt, the one at the path of the same
    /// source without an attempt, which a provider before attempts placed for it.
    fn placed_path(&self, id: &SourceId) -> Option<PathBuf> {
        let unnumbered = id.unnumbered();
        std::iter::once(id)
            .chain((id.attempt != 0).then_some(&unnumbered))
            .find(|id| self.placement_at(id).is_some())
            .map(|id| self.shared.settings.mountpoint.join(id.path()))
    }

    /// `Release`: stops the source's preparation and removes its placement at once, ending the
    /// hold the file system's mount took at a lookup of the source's path
    /// (`TreeStore::release_now`), and forgets the request of its run. A tree in the mount's
    /// memory keeps serving; one it has to read again is gone. The tree goes with the placement
    /// unless another source's placement has the tree. A source that is not known is a success.
    /// A source that names its attempt is that attempt's alone: a later attempt of the run is
    /// another source, which this leaves as it is. Its release also releases the same source
    /// without an attempt, which only a provider before attempts, or the agent before it counted
    /// them, made: an attempt that is over.
    ///
    /// # Errors
    /// [`ProviderError::Invalid`] for a source that is not well formed;
    /// [`ProviderError::Unavailable`] if the store cannot change its directory now.
    pub(super) fn release(&self, source: Option<Source>) -> Result<(), ProviderError> {
        let id = SourceId::of(source)?;
        self.release_id(&id)?;
        if id.attempt != 0 {
            self.release_id(&id.unnumbered())?;
        }
        Ok(())
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
                .release_now(&placement)
                .err()
                .filter(|why| *why != LifeError::UnknownPlacement);
            if let Some(why) = refused {
                let why = format!("the placement was not released: {why}");
                return Err(self.unavailable(Unavailable::NotReleased, why));
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

    /// `Sweep`: releases every source of `agent` that `keep` and `from` do not keep, those this
    /// process prepared and the placements under the agent's path; how many. A source is kept if
    /// its run is in `keep` or in `from`, unless it names its attempt and that is earlier than
    /// every attempt `from` names for its run (an attempt of 0 there keeps every attempt): a later
    /// attempt than the agent named is kept, and so is a source that names no attempt.
    ///
    /// # Errors
    /// [`ProviderError::Invalid`] for an agent that is not well formed; what a release gives.
    pub(super) fn sweep(
        &self,
        agent: &str,
        keep: &[String],
        from: &[KeptAttempt],
    ) -> Result<u32, ProviderError> {
        check_name("agent", agent)?;
        // The earliest attempt kept of each run kept: the least `from` names for it, else 0.
        let mut kept: BTreeMap<&str, u64> = BTreeMap::new();
        for entry in from {
            let floor = kept.entry(entry.run.as_str()).or_insert(entry.attempt);
            *floor = (*floor).min(entry.attempt);
        }
        for run in keep {
            kept.entry(run.as_str()).or_insert(0);
        }
        let swept = |id: &SourceId| {
            id.agent == agent
                && kept
                    .get(id.run.as_str())
                    .is_none_or(|&floor| id.attempt != 0 && id.attempt < floor)
        };
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
