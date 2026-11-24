//! The cache's side of the hand-over of its lock to a successor (`docs/design/one-service.md`
//! section 3.2; the conversation is `server/handoff/`).
//!
//! The lock is an `flock` on `<cache>/.lock`, which belongs to the open file description, so a
//! descriptor of it sent to the successor holds the same lock. Each step here is one of the
//! conversation's:
//!
//! - **The offer** sends a copy of the descriptor ([`LocalCache::lock_to_hand`]), only from a cache
//!   that holds the lock and whose own predecessor holds no descriptor of it any more: a lock
//!   handed on while the process before still holds it could be unlocked by that process when its
//!   successor exits, under the one it was handed on to.
//! - **At `ready`** the cache starts no write any more ([`LocalCache::hand_writes`]): it is read-only
//!   as on a lost lock (the generation moves on, so a write under way stops at its next look), but
//!   keeps the descriptor, disarmed, so that nothing of this process unlocks it. Then
//!   [`LocalCache::writes_ended`] returns once no write it started is running, and the
//!   conversation says `written`.
//! - **A successor that dies after `ready`** leaves this process the lock's only holder; it
//!   unlocks ([`LocalCache::unlock_handed`]) so that the successor started again takes the lock at
//!   its next tick.
//! - **The successor** opened its cache read-only (another process holds the lock) and keeps the
//!   received descriptor as a [`Received`], which a drop only closes. On `written`, or when the
//!   connection ends, it takes the write role with it ([`LocalCache::take_handed`]): what a take of
//!   the lock does, at that moment instead of at the old process's exit.

use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::server::lock::Received;

use super::super::{Inner, LocalCache, locked};
use super::{Mode, Standing, Tick};

/// How often a cache that handed its lock over looks whether its writes in flight have ended.
const WRITES_LOOK: Duration = Duration::from_millis(20);

impl Inner {
    /// Whether a write this cache started is running: a fill of an object or of a range, a put,
    /// a keep, or a pass of the eviction.
    fn writing(&self) -> bool {
        let keeps = self.config.max_concurrent_keeps.max(1);
        !locked(&self.flights).is_empty()
            || self.extents.filling()
            || !locked(&self.writers).is_empty()
            || self.keeps.available_permits() < keeps
            || self.evicting.load(Ordering::SeqCst)
    }
}

impl LocalCache {
    /// The path of the lock and a copy of its descriptor, for an offer of the lock: `None` when the
    /// cache holds no lock (it waits for one, takes none, or handed it over), or when the process
    /// it was handed over from still holds a descriptor of it.
    ///
    /// # Errors
    /// What the system said about the copy.
    pub(in crate::server) fn lock_to_hand(&self) -> io::Result<Option<(PathBuf, OwnedFd)>> {
        let hold = &self.inner.hold;
        let standing = locked(&hold.standing);
        match &*standing {
            Standing::Holding(file) if !hold.inherited.load(Ordering::SeqCst) => {
                Ok(Some((file.path().to_owned(), file.share()?)))
            }
            _ => Ok(None),
        }
    }

    /// The step of `ready`: the pins file is written, and from here the cache starts no write. It
    /// is read-only for good and never tries the lock again, whatever it stood (a cache that lost
    /// the lock since the offer does not take it back); a lock it held is kept, disarmed, so that
    /// the end of this process closes it and does not unlock it. A write already under way stops
    /// at its next look, as on a lost lock.
    pub(in crate::server) fn hand_writes(&self) {
        let inner = &self.inner;
        inner.save_pins(true);
        let hold = &inner.hold;
        let mut standing = locked(&hold.standing);
        let before = std::mem::replace(&mut *standing, Standing::Waiting);
        *standing = Standing::Handed(match before {
            Standing::Holding(mut file) => {
                file.disarm();
                Some(file)
            }
            // A lock lost between the offer and `ready` (its file replaced), or none: nothing to
            // keep, and the cache does not take the lock again either.
            Standing::Handed(file) => file,
            Standing::Waiting | Standing::Lockless => None,
        });
        // The generation first: a fill that sees the cache read-only has the old number.
        let _ = hold.generation.fetch_add(1, Ordering::SeqCst);
        hold.read_write.store(false, Ordering::SeqCst);
        let _ = hold.mode.send_replace(Mode::ReadOnly);
    }

    /// Returns once no write this cache started is running (see [`hand_writes`](Self::hand_writes),
    /// after which none starts): a fill of an object or a range (the fills of the prefetch queue
    /// and of the mounts are fills of the table too), a put, a keep, or an eviction pass. It looks
    /// every 20 ms. The short writes that are not counted (a stamp of file times, the pins file, a
    /// scrub's removal of a damaged object) look at the mode before each file they write, as on a
    /// lost lock, so what is left of them is the step under way.
    pub(in crate::server) async fn writes_ended(&self) {
        while self.inner.writing() {
            tokio::time::sleep(WRITES_LOOK).await;
        }
    }

    /// Unlocks the lock this cache handed over, for every descriptor of it: its successor is gone,
    /// and this process, which writes nothing any more, was its last other holder. A cache that
    /// handed nothing over unlocks nothing.
    ///
    /// # Errors
    /// What the system said.
    pub(in crate::server) fn unlock_handed(&self) -> io::Result<()> {
        match &*locked(&self.inner.hold.standing) {
            Standing::Handed(Some(file)) => file.unlock(),
            _ => Ok(()),
        }
    }

    /// The successor's step of `written` (or of the end of the connection): a cache that waits for
    /// the lock takes the write role with `received`, the descriptor its predecessor handed over.
    /// It does what a take of the lock does ([`tick`](Self::tick)'s [`Tick::Took`]: `.stage/`
    /// emptied, `obj/` scanned again, the index replaced) and is then read-write; until
    /// [`predecessor_gone`](Self::predecessor_gone) it offers the lock to no successor of its own.
    /// It blocks for as long as the scan takes: call it from a thread that may block.
    ///
    /// A cache that does not wait for the lock (it took it at a tick, once its predecessor unlocked,
    /// or takes none) only closes `received`: [`Tick::Held`].
    ///
    /// # Errors
    /// `InvalidData` when the lock file was replaced since the descriptor was checked; what the
    /// system said about the scan. Either way `received` is closed, not unlocked, and the cache
    /// stays read-only: the next tick tries the lock at its path.
    pub(in crate::server) fn take_handed(&self, received: Received) -> io::Result<Tick> {
        let inner = &self.inner;
        let mut standing = locked(&inner.hold.standing);
        if !matches!(*standing, Standing::Waiting) {
            return Ok(Tick::Held);
        }
        if !received.is_current() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the lock file was replaced since it was handed over",
            ));
        }
        inner.promote()?;
        inner.hold.inherited.store(true, Ordering::SeqCst);
        *standing = Standing::Holding(received.into_writer());
        Ok(Tick::Took)
    }

    /// The path of the cache's lock file: where a lock handed to this cache must be.
    pub(in crate::server) fn lock_path(&self) -> PathBuf {
        self.inner.config.path.join(super::LOCK)
    }

    /// The process the lock was handed over from is gone (its end of the connection closed): no
    /// other process holds a descriptor of the lock, and it may be offered again.
    pub(in crate::server) fn predecessor_gone(&self) {
        self.inner.hold.inherited.store(false, Ordering::SeqCst);
    }

    /// Says that a pass of the eviction runs, or no longer does: a write in flight, for a test of
    /// what waits for the writes of the cache.
    #[cfg(test)]
    pub(in crate::server) fn set_evicting(&self, on: bool) {
        self.inner.evicting.store(on, Ordering::SeqCst);
    }
}

#[cfg(test)]
#[path = "handing_tests.rs"]
mod tests;
