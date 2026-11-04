//! What the leader has seen of the registrations `_tiering/proxies/<proxy id>`: when each was
//! first seen as it is now, on the leader's own monotonic clock.
//!
//! The leader compares no time that another machine wrote. A registration is "the same" while its
//! size and the time the store gives it are the ones the leader saw; the proxy rewrites it every
//! heartbeat, which moves the store's time, so a registration that has not changed for a while is
//! the one of a process that stopped beating. A new leader starts with nothing seen, so it counts
//! a departure up to one period late and never early.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::Listed;
use crate::deadline::millis;
use crate::journal::PROXIES_PREFIX;

/// A registration as the leader saw it, and when it first saw it so.
struct Seen {
    /// The size, and the time of the last write that the store gave.
    version: (u64, u64),
    /// The leader's monotonic time at which it first saw that version.
    first: u64,
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
            seen.insert(id.to_owned(), Seen { version, first });
        }
        self.seen = seen;
    }

    /// Whether the owner `id` of an entry has departed at `now` (the leader's monotonic clock): its
    /// registration is not listed, or it has stood unchanged for `wait`. A registration changed
    /// since the leader first saw it starts the count again, and a leader that has seen none
    /// yet counts from its first listing, so a departure is noticed up to `wait` late and never
    /// early.
    pub(crate) fn departed(&self, id: &str, now: u64, wait: Duration) -> bool {
        let wait = millis(wait);
        self.seen
            .get(id)
            .is_none_or(|seen| now.saturating_sub(seen.first) >= wait)
    }
}
