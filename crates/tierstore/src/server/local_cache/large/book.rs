//! The S3 front's book of large objects: the generation and size of each object it has met, and
//! the definitive misses it remembers (10.6 step 4). Both are bounded: past the limit, the least
//! recently used is forgotten, and a forgotten object costs one `LOCATE` (or one first part alone)
//! when it is read again.

use std::collections::BTreeMap;

use tierstore_format::ids::Generation;

/// The most objects the book holds, and the most misses, unless the path is made with another
/// limit: 65536 of each, a few MiB.
pub const KNOWN_LIMIT: usize = 65_536;

/// What the front knows of a large object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server::local_cache) struct Known {
    /// The generation: the `ETag`, and the `If-Match` of every part.
    pub generation: Generation,
    /// The size.
    pub size: u64,
    /// When the front learned it (milliseconds of the cache's clock), its `Last-Modified`: no
    /// location says when the object was written.
    pub modified: u64,
}

/// The book.
#[derive(Debug)]
pub(in crate::server::local_cache) struct Book {
    known: Recent<Known>,
    /// Until when each miss is answered without asking.
    misses: Recent<u64>,
    /// How many times anything was forgotten: an answer asked for before a forget is not put in
    /// the book after it.
    epoch: u64,
}

impl Book {
    /// An empty book of at most `limit` objects and as many misses (at least one of each).
    pub fn new(limit: usize) -> Self {
        Self {
            known: Recent::new(limit),
            misses: Recent::new(limit),
            epoch: 0,
        }
    }

    /// What the book knows of `key`, counted as used.
    pub fn known(&mut self, key: &str) -> Option<Known> {
        self.known.get(key).cloned()
    }

    /// The epoch now: it moves at every [`forget`](Self::forget).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Learns `known` for `key` (a miss of it is forgotten), if the epoch is still `epoch`;
    /// whether it did.
    pub fn learn_at(&mut self, key: &str, known: Known, epoch: u64) -> bool {
        if epoch != self.epoch {
            return false;
        }
        self.misses.remove(key);
        self.known.insert(key, known);
        true
    }

    /// Remembers a definitive miss of `key` until `until` (what was known of it is forgotten), if
    /// the epoch is still `epoch`; whether it did.
    pub fn miss_at(&mut self, key: &str, until: u64, epoch: u64) -> bool {
        if epoch != self.epoch {
            return false;
        }
        self.known.remove(key);
        self.misses.insert(key, until);
        true
    }

    /// Whether a definitive miss of `key` is remembered at `now`; one that ran out is forgotten.
    pub fn missed(&mut self, key: &str, now: u64) -> bool {
        match self.misses.get(key).copied() {
            Some(until) if now < until => true,
            Some(_) => {
                self.misses.remove(key);
                false
            }
            None => false,
        }
    }

    /// Forgets `key`: what was known of it, and a miss; the epoch moves.
    pub fn forget(&mut self, key: &str) {
        self.epoch += 1;
        self.known.remove(key);
        self.misses.remove(key);
    }

    /// The number of objects and of misses held.
    #[cfg(test)]
    pub fn len(&self) -> (usize, usize) {
        (self.known.by_key.len(), self.misses.by_key.len())
    }
}

/// A map of at most `limit` keys that forgets the least recently used.
#[derive(Debug)]
struct Recent<V> {
    by_key: BTreeMap<String, (V, u64)>,
    by_use: BTreeMap<u64, String>,
    next: u64,
    limit: usize,
}

impl<V> Recent<V> {
    fn new(limit: usize) -> Self {
        Self {
            by_key: BTreeMap::new(),
            by_use: BTreeMap::new(),
            next: 0,
            limit: limit.max(1),
        }
    }

    /// The value of `key`, now the most recently used.
    fn get(&mut self, key: &str) -> Option<&V> {
        let (_, used) = self.by_key.get_mut(key)?;
        self.by_use.remove(used);
        *used = self.next;
        self.by_use.insert(self.next, key.to_owned());
        self.next += 1;
        self.by_key.get(key).map(|(value, _)| value)
    }

    /// Puts `value` under `key`, the most recently used; the least recently used goes past the
    /// limit.
    fn insert(&mut self, key: &str, value: V) {
        self.remove(key);
        self.by_key.insert(key.to_owned(), (value, self.next));
        self.by_use.insert(self.next, key.to_owned());
        self.next += 1;
        // One insert puts at most one key over the limit.
        let excess = self.by_key.len().saturating_sub(self.limit);
        let oldest: Vec<String> = (0..excess)
            .filter_map(|_| self.by_use.pop_first().map(|(_, key)| key))
            .collect();
        for key in oldest {
            self.by_key.remove(&key);
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some((_, used)) = self.by_key.remove(key) {
            self.by_use.remove(&used);
        }
    }
}
