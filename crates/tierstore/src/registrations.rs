//! What the leader has seen of the registrations `_tiering/proxies/<proxy id>`: when each was
//! first seen as it is now, on the leader's own monotonic clock, and the removal of the
//! registrations of processes that left long ago.
//!
//! The leader compares no time that another machine wrote. A registration is "the same" while its
//! size and the time the store gives it are the ones the leader saw; the proxy rewrites it every
//! heartbeat, which moves the store's time, so a registration that has not changed for a while is
//! the one of a process that stopped beating. A new leader starts with nothing seen, so it counts
//! a departure up to one period late and never early.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::deadline::millis;
use crate::journal::{PROXIES_PREFIX, journal_prefix, proxy_key};
use crate::registration::Registration;
use crate::{Backend, Error, Listed};

/// A registration as the leader saw it, and when it first saw it so.
struct Seen {
    /// The size, and the time of the last write that the store gave.
    version: (u64, u64),
    /// The leader's monotonic time at which it first saw that version.
    first: u64,
    /// The time at which it last asked whether this version may be removed, if it did.
    asked: Option<u64>,
}

/// The registrations seen so far.
#[derive(Default)]
pub(crate) struct Registrations {
    seen: BTreeMap<String, Seen>,
}

impl Registrations {
    /// Takes a listing of `_tiering/proxies/` made at `now` (the leader's monotonic clock): a
    /// registration that is new or changed is first seen now, an unchanged one keeps its time,
    /// and one that is no longer listed is forgotten.
    pub(crate) fn observe(&mut self, now: u64, listed: &[Listed]) {
        let mut seen = BTreeMap::new();
        for object in listed {
            let Some(id) = object.key.strip_prefix(PROXIES_PREFIX) else {
                continue;
            };
            let version = (object.size, object.modified);
            let same = self.seen.get(id).filter(|before| before.version == version);
            let first = same.map_or(now, |before| before.first);
            let asked = same.and_then(|before| before.asked);
            seen.insert(
                id.to_owned(),
                Seen {
                    version,
                    first,
                    asked,
                },
            );
        }
        self.seen = seen;
    }

    /// The ids of the registrations that have been unchanged for `wait` at `now` and that were
    /// not asked about in the last `wait`: the ones to ask the store about.
    pub(crate) fn due(&self, now: u64, wait: Duration) -> Vec<String> {
        let wait = millis(wait);
        let since = |at: u64| now.saturating_sub(at) >= wait;
        let due = self
            .seen
            .iter()
            .filter(|(_, seen)| since(seen.first) && seen.asked.is_none_or(|asked| since(asked)));
        due.map(|(id, _)| id.clone()).collect()
    }

    /// Notes that the store was asked about `id` at `now`, so that it is not asked again before
    /// the wait has passed, or the registration has changed.
    pub(crate) fn ask(&mut self, id: &str, now: u64) {
        if let Some(seen) = self.seen.get_mut(id) {
            seen.asked = Some(now);
        }
    }
}

/// Removes the registration of `id` if its owner beat (so it is gone, having not changed for as
/// long as the caller waited) and left no journal object. Gives whether it was removed.
///
/// A registration of a process that never beats (an empty one of an older release, or the body of
/// a proxy that did not start the heartbeat) is never removed: it never changes while its owner
/// runs, and the reconciler would stop finding the journals of a live proxy. A registration
/// removed under a proxy that is alive after all (it beat once more than the leader saw) is
/// written again by that proxy's next beat.
///
/// # Errors
/// What the reads and the delete give, a registration that is gone at the read excepted.
pub(crate) async fn remove_if_departed(hot: &dyn Backend, id: &str) -> Result<bool, Error> {
    let body = match hot.get(&proxy_key(id), None).await {
        Ok(body) => body,
        Err(Error::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    };
    if !Registration::decode(&body).is_some_and(|registration| registration.beats()) {
        return Ok(false);
    }
    if !hot.list(&journal_prefix(id)).await?.is_empty() {
        return Ok(false);
    }
    hot.delete(&proxy_key(id)).await.map(|()| true)
}
