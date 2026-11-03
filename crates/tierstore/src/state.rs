//! What a [`Reconciler`](crate::Reconciler) keeps in memory: what the hot tier holds, as the
//! journals say, and how far each proxy's journal has been applied.
//!
//! None of it is a record of anything. It is rebuilt from the checkpoints and the journal
//! objects, which are what the reconciler persists, by reading them again.

use std::collections::{BTreeMap, VecDeque};

use crate::journal::{Checkpoint, JournalEntry, JournalOp};

/// Where a journal entry sits in the journal of a proxy: the number of the object, and the
/// index of the entry in the object. Positions are ordered as the entries were appended.
pub(crate) type Position = (u64, u64);

/// An object the journals say the hot tier holds, and the entry that put it there.
struct Resident {
    proxy: String,
    at: Position,
    size: u64,
    /// Whether the cold tier holds it: the journal object of the entry is applied.
    copied: bool,
    /// Whether the entry is a mirror (a copy-back), which owns the hot object only while no put
    /// of another proxy is the word on the key.
    mirror: bool,
}

impl Resident {
    /// Whether the object was put by the entry at `at` in the journal of `proxy`.
    fn is_entry(&self, proxy: &str, at: Position) -> bool {
        (self.proxy.as_str(), self.at) == (proxy, at)
    }
}

/// A put entry whose object the cold tier holds, which the hot tier may drop.
#[derive(Clone)]
pub(crate) struct Candidate {
    pub(crate) at: Position,
    pub(crate) key: String,
    pub(crate) time: u64,
    /// The content tag of the hot tier's object when it was found to be in the cold tier, which
    /// the eviction deletes on; none for an entry applied by a reconciler before this one.
    pub(crate) tag: Option<String>,
}

/// Where the reconciler is in the journal of one proxy.
#[derive(Default)]
pub(crate) struct Stream {
    /// The number of the last journal object that was read.
    pub(crate) read_through: u64,
    /// The journal objects up to this number are applied.
    pub(crate) checkpoint: u64,
    /// The entries before this position are done with. It is where the journal is read from
    /// after a restart.
    pub(crate) cursor: Position,
    /// The puts the cold tier holds, in the order they were appended, that the hot tier has
    /// not dropped.
    pub(crate) candidates: VecDeque<Candidate>,
    /// The journal objects that are read and not applied yet, in order.
    pub(crate) pending: VecDeque<(u64, Vec<JournalEntry>)>,
    /// The journal objects that were read, with the time they were written, in order. They
    /// are deleted once they are applied, left behind by the cursor, and old enough. After a
    /// restart the objects before the cursor are not in it: they are not read again.
    pub(crate) retained: VecDeque<(u64, u64)>,
    /// The checkpoint as it is stored.
    pub(crate) persisted: Checkpoint,
}

impl Stream {
    /// A stream that resumes from a stored checkpoint.
    pub(crate) fn resume(checkpoint: Checkpoint) -> Self {
        Self {
            read_through: checkpoint.evict_seq.saturating_sub(1),
            checkpoint: checkpoint.seq,
            cursor: (checkpoint.evict_seq, checkpoint.evict_idx),
            persisted: checkpoint,
            ..Self::default()
        }
    }

    /// The checkpoint to store now.
    pub(crate) fn snapshot(&self) -> Checkpoint {
        Checkpoint {
            seq: self.checkpoint,
            evict_seq: self.cursor.0,
            evict_idx: self.cursor.1,
        }
    }
}

/// The objects the hot tier holds by the journals, the bytes they take, and the streams.
#[derive(Default)]
pub(crate) struct Books {
    /// The latest put entry of each key that the hot tier holds.
    index: BTreeMap<String, Resident>,
    usage: u64,
    lag: u64,
    pub(crate) streams: BTreeMap<String, Stream>,
    /// The keys whose latest put entry that was read is a reserved put or a mirror, with the time
    /// of that entry: a delete entry of such a key was written by a proxy that removes the cold
    /// tier's object itself, so the reconciler does not apply it to the cold tier. A key whose
    /// latest put entry is a plain put (an entry of a release without reservations) is not here.
    reserved: BTreeMap<String, u64>,
}

impl Books {
    /// The bytes of the objects the hot tier holds.
    pub(crate) fn usage(&self) -> u64 {
        self.usage
    }

    /// The bytes of the objects the hot tier holds that the cold tier does not.
    pub(crate) fn lag(&self) -> u64 {
        self.lag
    }

    /// Whether the latest put entry read for `key` is a reserved put or a mirror.
    pub(crate) fn is_reserved(&self, key: &str) -> bool {
        self.reserved.contains_key(key)
    }

    /// Forgets the keys whose latest reserved put or mirror was written before `time`: a delete
    /// of such a key that has not been applied by then is applied by the rule for a key of a
    /// release without reservations.
    pub(crate) fn forget_reserved_before(&mut self, time: u64) {
        self.reserved.retain(|_, written| *written >= time);
    }

    pub(crate) fn stream(&mut self, proxy: &str) -> &mut Stream {
        self.streams.entry(proxy.to_owned()).or_default()
    }

    /// Whether the put entry at `at` is the latest word on `key`.
    pub(crate) fn is_current(&self, proxy: &str, at: Position, key: &str) -> bool {
        let found = self.index.get(key);
        found.is_some_and(|resident| resident.is_entry(proxy, at))
    }

    /// Whether what the hot tier holds under `key` is, by the journals, the object of a put (not
    /// a mirror) that a proxy other than `proxy` wrote.
    fn is_put_of_another_proxy(&self, proxy: &str, key: &str) -> bool {
        let found = self.index.get(key);
        found.is_some_and(|resident| !resident.mirror && resident.proxy != proxy)
    }

    /// Takes a key out of what the hot tier holds. A key that is not there is ignored.
    pub(crate) fn forget(&mut self, key: &str) {
        if let Some(old) = self.index.remove(key) {
            self.usage -= old.size;
            if !old.copied {
                self.lag -= old.size;
            }
        }
    }

    /// Records a put entry: the hot tier holds the object, and the cold tier does not yet.
    pub(crate) fn put(&mut self, proxy: &str, at: Position, key: &str, size: u64, mirror: bool) {
        self.forget(key);
        self.usage += size;
        self.lag += size;
        let resident = Resident {
            proxy: proxy.to_owned(),
            at,
            size,
            copied: false,
            mirror,
        };
        self.index.insert(key.to_owned(), resident);
    }

    /// Records that the cold tier holds the object of a put entry, which makes it a
    /// candidate for eviction, with the content tag the hot tier's object had. An entry that a
    /// later one replaced is ignored.
    pub(crate) fn copied(
        &mut self,
        proxy: &str,
        at: Position,
        key: &str,
        time: u64,
        tag: Option<String>,
    ) {
        let found = self.index.get_mut(key);
        let Some(resident) = found.filter(|r| r.is_entry(proxy, at)) else {
            return;
        };
        resident.copied = true;
        self.lag -= resident.size;
        let candidate = Candidate {
            at,
            key: key.to_owned(),
            time,
            tag,
        };
        self.stream(proxy).candidates.push_back(candidate);
    }

    /// Records that the hot tier does not hold the object of a put entry after all (it was
    /// deleted before the copy). An entry that a later one replaced is ignored.
    pub(crate) fn gone(&mut self, proxy: &str, at: Position, key: &str) {
        if self.is_current(proxy, at, key) {
            self.forget(key);
        }
    }

    /// Reads a journal object into the books: registers its entries, and queues the object
    /// to be applied unless the checkpoint says it is applied.
    pub(crate) fn read_object(
        &mut self,
        proxy: &str,
        seq: u64,
        modified: u64,
        entries: Vec<JournalEntry>,
    ) {
        let stream = self.stream(proxy);
        let cursor = stream.cursor;
        let applied = seq <= stream.checkpoint;
        stream.read_through = seq;
        stream.retained.push_back((seq, modified));
        for (index, entry) in entries.iter().enumerate() {
            let at = (seq, index as u64);
            match entry.op {
                JournalOp::PutReserved | JournalOp::PutMirror => {
                    self.reserved.insert(entry.key.clone(), entry.time);
                }
                JournalOp::Put => {
                    self.reserved.remove(&entry.key);
                }
                _ => {}
            }
            if at < cursor {
                continue;
            }
            match entry.op {
                op if op.is_put() => {
                    let mirror = op == JournalOp::PutMirror;
                    if mirror && self.is_put_of_another_proxy(proxy, &entry.key) {
                        // A copy-back, read after a put of another proxy that the hot tier holds
                        // (the proxies' journals are read in no order that is the order of the
                        // writes): the mirror cannot be told to be the later, so it owns nothing
                        // and is never the resident, whichever journal was read first.
                        continue;
                    }
                    self.put(proxy, at, &entry.key, entry.size, mirror);
                    if applied {
                        self.copied(proxy, at, &entry.key, entry.time, None);
                    }
                }
                JournalOp::Delete => self.forget(&entry.key),
                _ => {}
            }
        }
        if !applied {
            self.stream(proxy).pending.push_back((seq, entries));
        }
    }

    /// The first candidate of the stream whose first candidate is the oldest, by its time
    /// and then the name of the proxy, with the name of the proxy.
    pub(crate) fn next_victim(&self) -> Option<(String, Candidate)> {
        let mut oldest: Option<(&String, &Candidate)> = None;
        for (id, stream) in &self.streams {
            let Some(front) = stream.candidates.front() else {
                continue;
            };
            match oldest {
                Some((have_id, have)) if (have.time, have_id) <= (front.time, id) => {}
                _ => oldest = Some((id, front)),
            }
        }
        oldest.map(|(id, front)| (id.clone(), front.clone()))
    }

    /// Takes the first candidate of a stream off, and moves the cursor to the next candidate,
    /// or to the end of what is applied if there is none.
    pub(crate) fn pop(&mut self, proxy: &str) {
        let stream = self.stream(proxy);
        stream.candidates.pop_front();
        let next = stream.candidates.front().map(|front| front.at);
        stream.cursor = next.unwrap_or((stream.checkpoint + 1, 0));
    }

    fn first_candidate(&self, proxy: &str) -> Option<Candidate> {
        self.streams[proxy].candidates.front().cloned()
    }

    /// Takes the candidates that a later entry replaced, or a delete removed, off the front
    /// of each stream, so that the cursor does not stay behind them.
    pub(crate) fn trim_stale(&mut self) {
        let proxies: Vec<String> = self.streams.keys().cloned().collect();
        for proxy in proxies {
            while let Some(first) = self.first_candidate(&proxy) {
                if self.is_current(&proxy, first.at, &first.key) {
                    break;
                }
                self.pop(&proxy);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts which residents count as the put of another proxy: a put of another proxy does, a
    /// put of the same proxy does not, a mirror of either proxy does not, and a key the hot tier
    /// does not hold does not.
    #[test]
    fn only_a_put_of_another_proxy_is_the_put_of_another_proxy() {
        let mut books = Books::default();
        books.put("p1", (1, 0), "put", 1, false);
        books.put("p1", (1, 1), "mirror", 1, true);

        assert!(books.is_put_of_another_proxy("p2", "put"));
        assert!(!books.is_put_of_another_proxy("p1", "put"));
        assert!(!books.is_put_of_another_proxy("p2", "mirror"));
        assert!(!books.is_put_of_another_proxy("p1", "mirror"));
        assert!(!books.is_put_of_another_proxy("p2", "absent"));
    }
}
