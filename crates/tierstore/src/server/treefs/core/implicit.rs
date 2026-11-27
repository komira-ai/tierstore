//! Trees that resolve on lookup: a lookup of `trees/<digest>` that no run has attached reads the
//! tree's manifests (the root `cas/l/<digest>` through the cache's fill of a manifest, which
//! checks the bytes against the digest and takes at most 64 MiB, where the cache does not hold
//! them; the new manifests of a new tree by one fill of the tree first, `core/placed.rs`),
//! flattens it, and attaches it with no run: an implicit attachment. A root that is a version 2
//! tree list (its first bytes are `TSTREE\0\0`) is the whole tree, named by the list's own digest:
//! it is read as the list, mapped and walked for its shape, and nothing below it is read
//! (`core/placed.rs`). It is detached when no
//! request has named the tree or one of its inodes for `fs.tree_idle_secs`, by a thread that looks
//! every quarter of that time (at least a second, at most a minute apart).
//!
//! A lookup of a digest whose root manifest the upstream does not have is `ENOENT`; any other
//! tree that cannot be read (a manifest below the root missing or refused, the cache not answering)
//! is `EIO`, said on stderr. The read ends 30 s after it began, all the tree's fills together
//! (`core/placed.rs`, `TREE_READ`), so a lookup is answered while the job agent that made it still
//! waits for it. A digest that cannot be read gets no tree number, so lookups of digests that are
//! not trees do not fill the table of tree numbers.
//!
//! A placement's tree at its fs location is attached the same way (`core/placed.rs`), by its key;
//! its idle end gives back the tree store's hold of the placement instead of unpinning it, and a
//! key is not a name of `trees/`.
//!
//! The implicit attachments are in memory only. While a successor takes over (`Core::hold`) no
//! tree is attached and none detached: a lookup of a tree that has a tree number is answered with
//! no attachment made, and of one that has none is `EIO`, since only the successor may number it.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::server::local_cache::attaches::{Attach, Outcome};
use crate::tree_manifest::TreeDigest;

use super::super::attach::Failure;
use super::super::fills::Report;
use super::fetched::Fetched;
use super::placed::{Unread, placement_of};
use super::said::Failing;
use super::{Core, Errno, locked};

/// The shortest and the longest time between two looks for idle trees.
const LOOK_BOUNDS: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(60));

/// How often to look for trees idle for `idle`: a quarter of it, within [`LOOK_BOUNDS`].
pub(in crate::server::treefs) fn look_every(idle: Duration) -> Duration {
    (idle / 4).clamp(LOOK_BOUNDS.0, LOOK_BOUNDS.1)
}

impl Core {
    /// The number of the tree `digest` names in `trees/`: one a run or a lookup has attached, or
    /// one this lookup reads and attaches.
    ///
    /// # Errors
    /// [`Errno::NoEnt`] for a name that is not a digest or a tree whose root manifest the upstream
    /// does not have, [`Errno::Io`] for any other tree that cannot be read or numbered.
    pub(super) fn resolve(&self, digest: &str) -> Result<u64, Errno> {
        let digest = TreeDigest::from_hex(digest)
            .filter(|digest| placement_of(digest).is_none())
            .ok_or(Errno::NoEnt)?;
        self.resolve_digest(digest)
    }

    /// The number of the tree `digest`, or of the placement whose key it is: attached already,
    /// or read and attached now.
    ///
    /// # Errors
    /// As [`Core::resolve`].
    pub(super) fn resolve_digest(&self, digest: TreeDigest) -> Result<u64, Errno> {
        let attached = self
            .table
            .find(&digest)
            .filter(|number| self.used_if_attached(*number));
        match attached {
            Some(number) => Ok(number),
            None => self.attach_implicitly(digest),
        }
    }

    /// Reads the tree `digest` before the lifecycle lock is taken, so that a slow fill holds up
    /// no attach, no detach and no lookup. A tree that has a number is read once for every
    /// request that asks for it at the same time (`core/memory.rs`), and is in memory when this
    /// returns; one that has none yet is read here and given back, to be kept under its number
    /// (only the request that attaches a digest can name it: the kernel makes one lookup of a
    /// name at a time, and the control socket one attach). The manifests this request fetches
    /// itself are counted in `fetched`.
    pub(super) fn read_ahead(
        &self,
        digest: &TreeDigest,
        fetched: &Fetched,
    ) -> Result<Option<super::Tree>, Unread> {
        match self.table.find(digest) {
            Some(number) => self.load(number, *digest).map(|_| None),
            None => self.read_tree(digest, fetched).map(Some),
        }
    }

    /// Reads the tree `digest`, unless it is in memory, and attaches it with no run; its number.
    /// The time from the start of the lookup to its answer, on the core's clock, is reported to the
    /// node with the outcome (`attached`, for `tierstore_fs_tree_attach_seconds`) and said on
    /// stderr with the manifests the read fetched, by level, and its `fill-tree`
    /// (`Fetched::by_level`).
    fn attach_implicitly(&self, digest: TreeDigest) -> Result<u64, Errno> {
        let start = self.now();
        let fetched = Fetched::default();
        let attached = self.attach_read(digest, &fetched);
        let ms = self.now().saturating_sub(start);
        let outcome = match attached {
            Ok(_) => Outcome::Ok,
            Err(Errno::NoEnt) => Outcome::Absent,
            Err(_) => Outcome::Failed,
        };
        self.fills.report(Report::Attached(Attach { outcome, ms }));
        let counts = fetched.counts();
        tracing::info!(
            "tierstore fs: lookup of {digest}: {} in {ms} ms; {} manifests: {} from the cache, \
             {} filled, {} not had{}{}",
            outcome.word(),
            counts.manifests(),
            counts.cache,
            counts.filled,
            counts.failed,
            fetched.by_level(),
            self.list_said(&attached),
        );
        attached
    }

    /// [`Core::attach_implicitly`] but for its report: reads the tree, counting the manifests in
    /// `fetched`, and attaches it.
    fn attach_read(&self, digest: TreeDigest, fetched: &Fetched) -> Result<u64, Errno> {
        let tree = self
            .read_ahead(&digest, fetched)
            .map_err(|why| self.refusal(&digest, why))?;
        let _lifecycle = locked(&self.lifecycle);
        let held = self.is_held();
        let number = if held {
            self.table.find(&digest).ok_or_else(|| {
                self.say_failed((0, Failing::Other), None, &|| {
                    format!(
                        "tierstore fs: cannot attach {digest} on a lookup: a successor is taking \
                         over, and only it may number a tree"
                    )
                });
                Errno::Io
            })?
        } else {
            self.table.number_of(&digest).map_err(|why| {
                self.say_failed((0, Failing::Other), None, &|| {
                    format!("tierstore fs: cannot number {digest} on a lookup: {why}")
                });
                Errno::Io
            })?
        };
        if let Some(tree) = tree {
            let _ = self.keep(number, digest, tree);
        }
        let now = self.now();
        let mut state = self.state();
        // A tree that stayed in memory is used now too, before the lifecycle lock lets a sweep see it.
        let loaded = self.trees.get(number);
        loaded
            .iter()
            .for_each(|loaded| loaded.used.store(now, Ordering::Release));
        if !held && !state.attachments.is_attached(number) {
            state.attachments.add_implicit(number, digest);
        }
        Ok(number)
    }

    /// Whether the tree `number` is attached, by a run or a lookup; if it is, it is used now. One
    /// look under the state lock, which a sweep decides under too, so that a tree found attached
    /// here is not found idle by a sweep that follows.
    fn used_if_attached(&self, number: u64) -> bool {
        let now = self.now();
        let state = self.state();
        let attached = state.attachments.is_attached(number);
        let loaded = self.trees.get(number).filter(|_| attached);
        loaded
            .iter()
            .for_each(|loaded| loaded.used.store(now, Ordering::Release));
        attached
    }

    /// Ends the implicit attachment of every tree no request has named for `fs.tree_idle_secs`;
    /// how many ended. A tree no run has is then unpinned, with no lock of the core held but that
    /// of its pins (`Core::free_tree`), and leaves memory when the kernel holds none of its
    /// inodes, as at the detach of its last run. Nothing ends while a successor takes over. Then
    /// every tree that nothing has attached and no request has named for that long leaves memory,
    /// whatever lookups of it the kernel holds (`Core::drop_idle`, `core/memory.rs`), once the
    /// unpins are made and a hold no longer waits for them. The opens
    /// that failed under an implicit attachment that ends are said on stderr, one line for the
    /// tree ([`ended_with`]).
    pub(in crate::server::treefs) fn detach_idle(&self) -> usize {
        let now = self.now();
        let idle = u64::try_from(self.settings.tree_idle.as_millis()).unwrap_or(u64::MAX);
        let mut failed = Vec::new();
        let (freed, freeing): (Vec<(u64, TreeDigest)>, _) = {
            let _lifecycle = locked(&self.lifecycle);
            if self.is_held() {
                return 0;
            }
            let freeing = self.freeing();
            let mut state = self.state();
            let ended: Vec<(u64, TreeDigest)> = state
                .attachments
                .implicit()
                .into_iter()
                .filter(|(number, _)| {
                    self.trees.get(*number).is_none_or(|loaded| {
                        now.saturating_sub(loaded.used.load(Ordering::Acquire)) >= idle
                    })
                })
                .collect();
            for (number, digest) in &ended {
                let failures = state.attachments.remove_implicit(*number);
                if !failures.is_empty() {
                    failed.push((*number, *digest, failures));
                }
            }
            let freed = ended
                .into_iter()
                .filter(|(number, _)| !state.attachments.has_run(*number))
                .collect();
            (freed, freeing)
        };
        for (number, digest, failures) in &failed {
            tracing::warn!("{}", ended_with((digest, *number), failures));
        }
        for (number, digest) in &freed {
            self.free_tree(*number, digest);
        }
        // The unpins are made: a hold waits no more. The guard goes before the drop of idle trees
        // takes the lifecycle lock, which a hold keeps while it waits for this guard (`Core::hold`).
        drop(freeing);
        let _ = self.drop_idle(idle);
        freed.len()
    }

    /// Starts the thread that calls [`Core::detach_idle`] every `every`. It holds the core weakly,
    /// and ends when the core is dropped.
    ///
    /// # Errors
    /// What the system said when the thread cannot be started.
    pub(in crate::server::treefs) fn sweep_idle(
        self: &Arc<Self>,
        every: Duration,
    ) -> std::io::Result<()> {
        let (stop, wait) = mpsc::channel();
        let core = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("fs-idle".to_owned())
            .spawn(move || sweep(&wait, &core, every))?;
        *locked(&self.sweeper) = Some(stop);
        Ok(())
    }

    /// Whether the thread that detaches idle trees was started.
    #[cfg(test)]
    pub(in crate::server::treefs) fn sweeping(&self) -> bool {
        locked(&self.sweeper).is_some()
    }

    /// The trees a lookup attached, by number.
    #[cfg(test)]
    pub(in crate::server::treefs) fn implicit(&self) -> Vec<u64> {
        let state = locked(&self.state);
        let implicit = state.attachments.implicit();
        implicit.into_iter().map(|(number, _)| number).collect()
    }
}

impl Core {
    /// What a lookup of the tree `digest` that could not be read is answered: `ENOENT` when there
    /// is no such tree (the upstream does not have its root manifest, or the placement has
    /// ended), else `EIO`, said on stderr (`Core::say_failed`).
    fn refusal(&self, digest: &TreeDigest, why: Unread) -> Errno {
        match why {
            Unread::Gone(_) => Errno::NoEnt,
            Unread::Failed(why) => {
                self.say_failed((0, Failing::Other), Some(digest), &|| {
                    format!("tierstore fs: cannot attach {digest} on a lookup: {why}")
                });
                Errno::Io
            }
        }
    }
}

/// The most failed opens named in the line of an implicit attachment that ended.
const NAMED: usize = 8;

/// The line said when the implicit attachment of the tree `digest`, numbered `number`, ends with
/// `failures`: their count, and the first [`NAMED`] of them by path, each with its reason.
pub(super) fn ended_with((digest, number): (&TreeDigest, u64), failures: &[Failure]) -> String {
    let named: Vec<String> = failures
        .iter()
        .take(NAMED)
        .map(|failure| format!("{} ({}: {})", failure.path, failure.digest, failure.reason))
        .collect();
    let rest = failures.len().saturating_sub(NAMED);
    let more = if rest == 0 {
        String::new()
    } else {
        format!("; and {rest} more")
    };
    let count = match failures.len() {
        1 => "1 failed open".to_owned(),
        n => format!("{n} failed opens"),
    };
    format!(
        "tierstore fs: the attachment of the tree {digest} (number {number}) by a lookup ended \
         with {count}: {}{more}",
        named.join("; "),
    )
}

/// Detaches the idle trees of `core` every `every`, and looks around (`Core::look_around`), until
/// the sender of `wait` is dropped or sends.
fn sweep(wait: &mpsc::Receiver<()>, core: &Weak<Core>, every: Duration) {
    while matches!(wait.recv_timeout(every), Err(RecvTimeoutError::Timeout)) {
        let _ = core.upgrade().map(|core| {
            let _ = core.detach_idle();
            core.look_around();
        });
    }
}

#[cfg(test)]
#[path = "implicit_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "implicit_attach_tests.rs"]
mod attach_tests;
