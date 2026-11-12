//! Trees that resolve on lookup: a lookup of `trees/<digest>` that no run has attached reads the
//! tree's manifests (the root `cas/l/<digest>` through the fill socket's `fill <digest>`, which
//! checks the bytes against the digest and takes at most 64 MiB, where the cache does not hold
//! them), flattens it, and attaches it with no run: an implicit attachment. It is detached when no
//! request has named the tree or one of its inodes for `fs.tree_idle_secs`, by a thread that looks
//! every quarter of that time (at least a second, at most a minute apart).
//!
//! A lookup of a digest whose root manifest the upstream does not have is `ENOENT`; any other
//! tree that cannot be read (a manifest below the root missing or refused, the fill socket away)
//! is `EIO`, said on stderr. A digest that cannot be read gets no tree number, so lookups of
//! digests that are not trees do not fill the table of tree numbers.
//!
//! The implicit attachments are in memory only. While a successor takes over (`Core::hold`) no
//! tree is attached and none detached: a lookup of a tree that has a tree number is answered with
//! no attachment made, and of one that has none is `EIO`, since only the successor may number it.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::tree_manifest::{TreeDigest, TreeError};

use super::{Core, Errno, locked};

/// The answer of the fill socket for an object the upstream does not have.
const ABSENT: &str = "upstream 404";

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
        let digest = TreeDigest::from_hex(digest).ok_or(Errno::NoEnt)?;
        let attached = self
            .table
            .find(&digest)
            .filter(|number| self.used_if_attached(*number));
        match attached {
            Some(number) => Ok(number),
            None => self.attach_implicitly(digest),
        }
    }

    /// Reads the tree `digest`, unless it is in memory, and attaches it with no run; its number.
    fn attach_implicitly(&self, digest: TreeDigest) -> Result<u64, Errno> {
        let in_memory = self
            .table
            .find(&digest)
            .filter(|number| locked(&self.state).loaded.contains_key(number));
        // Read before the lifecycle lock, so that a slow fill holds up no attach and no detach.
        let tree = match in_memory {
            Some(_) => None,
            None => Some(
                self.read_tree(&digest)
                    .map_err(|why| refusal(&digest, why))?,
            ),
        };
        let _lifecycle = locked(&self.lifecycle);
        let held = self.is_held();
        let number = if held {
            self.table.find(&digest).ok_or(Errno::Io)?
        } else {
            self.table.number_of(&digest).map_err(|why| {
                tracing::warn!("tierstore fs: cannot number {digest} on a lookup: {why}");
                Errno::Io
            })?
        };
        if let Some(tree) = tree {
            let _ = self.keep(number, digest, tree);
        }
        let now = self.now();
        let mut state = locked(&self.state);
        // A tree that stayed in memory is used now too, before the lifecycle lock lets a sweep see it.
        let loaded = state.loaded.get(&number);
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
        let state = locked(&self.state);
        let attached = state.attachments.is_attached(number);
        let loaded = state.loaded.get(&number).filter(|_| attached);
        loaded
            .iter()
            .for_each(|loaded| loaded.used.store(now, Ordering::Release));
        attached
    }

    /// Ends the implicit attachment of every tree no request has named for `fs.tree_idle_secs`;
    /// how many ended. A tree no run has is then unpinned, and leaves memory when the kernel holds
    /// none of its inodes, as at the detach of its last run. Nothing ends while a successor takes
    /// over.
    pub(in crate::server::treefs) fn detach_idle(&self) -> usize {
        let _lifecycle = locked(&self.lifecycle);
        let _pinning = locked(&self.pinning);
        if self.is_held() {
            return 0;
        }
        let now = self.now();
        let idle = u64::try_from(self.settings.tree_idle.as_millis()).unwrap_or(u64::MAX);
        let freed: Vec<(u64, TreeDigest)> = {
            let mut state = locked(&self.state);
            let ended: Vec<(u64, TreeDigest)> = state
                .attachments
                .implicit()
                .into_iter()
                .filter(|(number, _)| {
                    state.loaded.get(number).is_none_or(|loaded| {
                        now.saturating_sub(loaded.used.load(Ordering::Acquire)) >= idle
                    })
                })
                .collect();
            for (number, _) in &ended {
                state.attachments.remove_implicit(*number);
            }
            ended
                .into_iter()
                .filter(|(number, _)| !state.attachments.has_run(*number))
                .collect()
        };
        for (number, digest) in &freed {
            let loaded = locked(&self.state).loaded.get(number).cloned();
            loaded
                .iter()
                .for_each(|loaded| locked(&loaded.pinned).clear());
            let _ = self
                .fills
                .unpin(&self.settings.mount_id, &digest.to_string());
            self.drop_if_forgotten(*number);
        }
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

/// What a lookup of the tree `digest` that could not be read is answered: `ENOENT` when the
/// upstream does not have its root manifest, else `EIO`, said on stderr.
fn refusal(digest: &TreeDigest, why: TreeError) -> Errno {
    match why {
        TreeError::Fetch {
            digest: missing,
            reason,
        } if missing == *digest && reason == ABSENT => Errno::NoEnt,
        why => {
            tracing::warn!("tierstore fs: cannot attach {digest} on a lookup: {why}");
            Errno::Io
        }
    }
}

/// Detaches the idle trees of `core` every `every`, until the sender of `wait` is dropped or
/// sends.
fn sweep(wait: &mpsc::Receiver<()>, core: &Weak<Core>, every: Duration) {
    while matches!(wait.recv_timeout(every), Err(RecvTimeoutError::Timeout)) {
        let _ = core.upgrade().map(|core| core.detach_idle());
    }
}

#[cfg(test)]
#[path = "implicit_tests.rs"]
mod tests;
