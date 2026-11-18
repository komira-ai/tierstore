//! The locks that keep a pin from following an unpin of the same tree, one for each of 256 sets
//! of trees.
//!
//! An open of a blob pins it for its tree, the first time, while the tree is attached; the end of
//! the last attachment of a tree unpins it. Both are round trips on the fill socket. The pin
//! reads the lock of its tree's set, from its look at whether the tree is attached to its record
//! of the pin, and the unpin writes it, from its look at whether the tree is attached to the
//! end of the round trip. So a pin is never made after the unpin of a tree it saw attached, and a
//! tree that is attached again before its unpin is not unpinned. The pins of opens read the lock
//! together, so their round trips run at once, and a tree's unpin waits only for the pins of the
//! trees of its own set.
//!
//! The set of a tree is the last byte of its digest: uniform for the digest of a tree of
//! manifests, and for the key of a placed tree, whose last eight bytes are the start of the hash of
//! its location, so the trees at different locations of one placement are in different sets
//! (nearly always). Trees of different sets never wait for each other.

use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::tree_manifest::TreeDigest;

/// The locks of the sets of trees.
pub(super) struct PinLocks {
    sets: [RwLock<()>; 256],
    /// The times a lock was asked for an unpin, counted before the wait.
    #[cfg(test)]
    unpins: std::sync::atomic::AtomicUsize,
    /// The times a lock was asked for a pin, counted before the wait.
    #[cfg(test)]
    pins: std::sync::atomic::AtomicUsize,
}

impl PinLocks {
    /// Locks that nobody holds.
    pub(super) fn new() -> Self {
        Self {
            sets: std::array::from_fn(|_| RwLock::new(())),
            #[cfg(test)]
            unpins: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            pins: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The lock of the set that the tree `digest` is in.
    fn of(&self, digest: &TreeDigest) -> &RwLock<()> {
        &self.sets[usize::from(digest.as_bytes()[31])]
    }

    /// Takes the lock of the tree `digest` for a pin, together with the pins of every other open.
    pub(super) fn pin(&self, digest: &TreeDigest) -> RwLockReadGuard<'_, ()> {
        #[cfg(test)]
        let _ = self.pins.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.of(digest)
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the lock of the tree `digest` for an unpin, with no pin of its set in flight.
    pub(super) fn unpin(&self, digest: &TreeDigest) -> RwLockWriteGuard<'_, ()> {
        #[cfg(test)]
        let _ = self
            .unpins
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.of(digest)
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The number of the set that the tree `digest` is in.
    #[cfg(test)]
    pub(super) fn set_of(digest: &TreeDigest) -> usize {
        usize::from(digest.as_bytes()[31])
    }

    /// The times a lock was asked for a pin.
    #[cfg(test)]
    pub(super) fn pins_asked(&self) -> usize {
        self.pins.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The times a lock was asked for an unpin.
    #[cfg(test)]
    pub(super) fn unpins_asked(&self) -> usize {
        self.unpins.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether a pin of the tree `digest` could be made now: no unpin of its set is in flight.
    #[cfg(test)]
    pub(super) fn free(&self, digest: &TreeDigest) -> bool {
        self.of(digest).try_write().is_ok()
    }
}

#[cfg(test)]
#[path = "pins_tests.rs"]
mod tests;
