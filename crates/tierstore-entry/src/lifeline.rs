//! Lifelines (5.1): how the server learns that a holder's process is gone.
//!
//! At handshake a mount sends one end of a `socketpair` that it holds open for its whole life,
//! apart from its protocol connection (which may reconnect many times). The server keeps that end
//! as a [`Lifeline`]. Only `POLLHUP`, which the kernel reports once the mount's end is closed (the
//! process exited, was killed, or closed it), ends the holder; then the server forgets the holder's
//! counters and regions and drops its holds ([`EntryStore::drop_holder`]). A protocol connection
//! that flaps never touches the lifeline, so the holder keeps its counters, regions and holds. A
//! pidfd is not used: mounts run in other pid namespaces.
//!
//! The poll asks for no event: `POLLHUP` is always reported, while data or an end of stream the
//! mount sends on the lifeline (`POLLIN`) is neither asked for nor read, so it can neither end the
//! holder nor make the poll return.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::{Mutex, PoisonError};

use rustix::event::{PollFd, PollFlags, Timespec};
use tierstore_fetch_api::entry::EntryStore;
use tierstore_fetch_api::ids::HolderId;

/// A poll that returns at once.
const NOW: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

/// The server's end of one holder's lifeline.
#[derive(Debug)]
pub struct Lifeline {
    fd: OwnedFd,
}

impl Lifeline {
    /// The server's end `fd` of a lifeline socket pair, as the handshake passed it.
    #[must_use]
    pub fn new(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Whether the holder's end hung up (`POLLHUP`), without waiting.
    pub fn hung_up(&self) -> io::Result<bool> {
        let mut fds = [PollFd::new(&self.fd, PollFlags::empty())];
        rustix::event::poll(&mut fds, Some(&NOW))?;
        Ok(fds[0].revents().contains(PollFlags::HUP))
    }
}

impl AsFd for Lifeline {
    /// The descriptor, for a caller that waits for its hangup in its own event loop.
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// The lifelines of every live holder.
#[derive(Debug, Default)]
pub struct Lifelines {
    lines: Mutex<BTreeMap<HolderId, Lifeline>>,
}

impl Lifelines {
    /// No lifeline.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Keeps `line` as `holder`'s lifeline; `false`, and `line` is dropped, when the holder
    /// already has one (a re-handshake after a reconnect keeps the first lifeline, as
    /// [`EntryStore::register_holder`] keeps the first counters).
    pub fn add(&self, holder: HolderId, line: Lifeline) -> bool {
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.contains_key(&holder) {
            return false;
        }
        lines.insert(holder, line);
        true
    }

    /// The holders with a lifeline.
    #[must_use]
    pub fn holders(&self) -> Vec<HolderId> {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        lines.keys().copied().collect()
    }

    /// Polls every lifeline once, without waiting; for each that hung up, calls
    /// [`EntryStore::drop_holder`] and forgets the lifeline. The holders that ended, in ascending
    /// order.
    ///
    /// The lifelines stay locked until every ended holder is dropped from the store, so a
    /// re-handshake with the same holder id cannot add a lifeline (and be taken for live) while
    /// the store still has the old holder's counters. The store never calls back into the
    /// lifelines, so the lock order is the lifelines, then the store.
    pub fn reap(&self, store: &dyn EntryStore) -> io::Result<Vec<HolderId>> {
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        let mut fds: Vec<PollFd<'_>> = lines
            .values()
            .map(|l| PollFd::new(&l.fd, PollFlags::empty()))
            .collect();
        rustix::event::poll(&mut fds, Some(&NOW))?;
        let hung: Vec<HolderId> = lines
            .keys()
            .zip(&fds)
            .filter(|(_, fd)| fd.revents().contains(PollFlags::HUP))
            .map(|(h, _)| *h)
            .collect();
        drop(fds);
        for holder in &hung {
            store.drop_holder(*holder);
            // The lifeline closes as it drops.
            lines.remove(holder);
        }
        Ok(hung)
    }
}
