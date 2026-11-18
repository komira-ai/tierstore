//! The waits of the commits of the put directory, keyed by what each waits for. A commit that
//! finds its file or its key busy waits for one thing: the release of the file's handle, the end of
//! the commit of the file, its unlink, or the end of the commit of the key. A change tells the
//! waits of that file or key alone, not every wait.
//!
//! The waits sit on a fixed set of [`SHARDS`], each with a lock and a condition variable of its own
//! and a count of the waits on it; a thing maps to the shard its hash falls in, so two things may
//! share one and a wait woken for the other only tries again. A change takes no lock when the shards
//! it tells have no wait.
//!
//! No wake-up is lost: a wait counts itself in its shard, then reads the change counter, then waits
//! (the check and the wait are under the shard's lock); a change bumps the counter, then reads the
//! count of its shard, and tells the shard under that lock when the count is not zero. Every one of
//! those accesses is sequentially consistent, so either the wait sees the bump and does not wait,
//! or the change sees the count and tells the shard, which it can do only once the wait is
//! sleeping, as the wait checks and sleeps with the lock held.

use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::Clock;
use crate::server::treefs::locked;

/// The shards the waits are spread over.
pub(super) const SHARDS: usize = 64;

/// What a wait is for, or what a change is of.
#[derive(Clone, Copy, Debug)]
pub(super) enum Topic<'a> {
    /// The staged file with this number: its release, the end of its commit, its unlink.
    File(u64),
    /// A key: the end of the commit that holds it.
    Key(&'a str),
}

/// The waits that hash to one shard.
#[derive(Debug, Default)]
struct Shard {
    /// The waits counted in, between their count and their end.
    waiters: AtomicUsize,
    /// Held while a wait checks the counter and sleeps, and while a change tells the shard.
    lock: Mutex<()>,
    wake: Condvar,
}

/// The change counter and the shards of waits.
#[derive(Debug)]
pub(super) struct Changes {
    /// The changes made, which a wait reads before it tries and again before it sleeps.
    count: AtomicU64,
    shards: [Shard; SHARDS],
    /// The times a wait went to sleep, for the tests that act once a commit waits.
    #[cfg(test)]
    pub(super) waits: AtomicU64,
    /// The shards a change told (took the lock of and woke), for the tests of who is woken.
    #[cfg(test)]
    pub(super) told: AtomicU64,
}

impl Changes {
    pub(super) fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            shards: std::array::from_fn(|_| Shard::default()),
            #[cfg(test)]
            waits: AtomicU64::new(0),
            #[cfg(test)]
            told: AtomicU64::new(0),
        }
    }

    /// The count of the changes so far: read before the try that a wait may follow.
    pub(super) fn seen(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    /// The position of the shard `topic` falls in.
    pub(super) fn index(topic: Topic<'_>) -> usize {
        let mut hasher = DefaultHasher::new();
        match topic {
            Topic::File(id) => (0_u8, id).hash(&mut hasher),
            Topic::Key(key) => (1_u8, key).hash(&mut hasher),
        }
        usize::try_from(hasher.finish() % SHARDS as u64).unwrap_or(0)
    }

    /// The shard `topic` falls in.
    fn shard(&self, topic: Topic<'_>) -> &Shard {
        &self.shards[Self::index(topic)]
    }

    /// Tells the waits on `topics` that something they wait for changed.
    pub(super) fn notify(&self, topics: &[Topic<'_>]) {
        let _ = self.count.fetch_add(1, Ordering::SeqCst);
        let mut told: Vec<&Shard> = Vec::with_capacity(topics.len());
        for topic in topics {
            let shard = self.shard(*topic);
            if !told.iter().any(|one| std::ptr::eq(*one, shard)) {
                told.push(shard);
            }
        }
        for shard in told {
            if shard.waiters.load(Ordering::SeqCst) > 0 {
                #[cfg(test)]
                let _ = self.told.fetch_add(1, Ordering::AcqRel);
                let _held = locked(&shard.lock);
                shard.wake.notify_all();
            }
        }
    }

    /// Waits until a change is told for `topic` after the count was `seen`, or `deadline` on
    /// `clock`; whether a change came first (a change of another thing that shares the shard
    /// counts: the caller tries again).
    pub(super) fn wait(
        &self,
        topic: Topic<'_>,
        seen: u64,
        deadline: u64,
        clock: &dyn Clock,
    ) -> bool {
        let shard = self.shard(topic);
        let _ = shard.waiters.fetch_add(1, Ordering::SeqCst);
        let mut held = locked(&shard.lock);
        let changed = loop {
            if self.count.load(Ordering::SeqCst) != seen {
                break true;
            }
            let now = clock.now();
            if now >= deadline {
                break false;
            }
            let wait = Duration::from_millis(deadline - now);
            #[cfg(test)]
            let _ = self.waits.fetch_add(1, Ordering::AcqRel);
            held = shard
                .wake
                .wait_timeout(held, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        };
        drop(held);
        let _ = shard.waiters.fetch_sub(1, Ordering::SeqCst);
        changed
    }
}
