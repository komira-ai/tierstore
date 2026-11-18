//! The queue of one class (4.5): deficit round robin across jobs, quantum one part of the current
//! `P`, oldest job first; within a job by `(rank, offset)`; and the active-pack window (at most
//! `window` packs partly filled per class). Pure: the engine says which fill's items exist and
//! which fills are active.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use tierstore_fetch_api::ids::{JobId, Rank};

/// What a queued item is, as the engine names it: a fill and an item of that fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemRef {
    /// The fill's sequence number (older fills first on a tie).
    pub fill: u64,
    /// The item within the fill.
    pub item: u64,
}

/// One queued item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Queued {
    /// The job whose want ranks the item.
    pub job: JobId,
    /// The first-touch rank of the item's first needed cell.
    pub rank: Rank,
    /// The item's offset in its pack.
    pub offset: u64,
    /// The bytes it would fetch.
    pub size: u64,
    /// The item.
    pub what: ItemRef,
}

type Key = (Rank, u64, ItemRef);

#[derive(Debug, Default)]
struct JobQueue {
    items: BTreeMap<Key, u64>,
    deficit: u64,
}

/// The queue of one class.
#[derive(Debug)]
pub struct ClassQueue {
    jobs: BTreeMap<JobId, JobQueue>,
    /// Jobs with items, in round order (a new job joins at the back).
    ring: VecDeque<JobId>,
    /// Every item of a fill: its job and key.
    by_fill: BTreeMap<u64, Vec<(JobId, Key)>>,
    /// Fills with a part of this class in flight or queued after one was dispatched.
    active: BTreeSet<u64>,
    /// The active-pack window of this class (`usize::MAX`: none).
    window: usize,
}

impl ClassQueue {
    /// An empty queue with an active-pack window of `window` packs.
    #[must_use]
    pub fn new(window: usize) -> Self {
        Self {
            jobs: BTreeMap::new(),
            ring: VecDeque::new(),
            by_fill: BTreeMap::new(),
            active: BTreeSet::new(),
            window,
        }
    }

    /// Replaces every item of `fill` with `items`.
    pub fn set_fill(&mut self, fill: u64, items: &[Queued]) {
        for (job, key) in self.by_fill.remove(&fill).unwrap_or_default() {
            for q in self.jobs.get_mut(&job).into_iter() {
                q.items.remove(&key);
            }
        }
        let mut keys = Vec::with_capacity(items.len());
        for it in items {
            let key = (it.rank, it.offset, it.what);
            let q = self.jobs.entry(it.job).or_insert_with(|| {
                self.ring.push_back(it.job);
                JobQueue::default()
            });
            q.items.insert(key, it.size);
            keys.push((it.job, key));
        }
        if !keys.is_empty() {
            self.by_fill.insert(fill, keys);
        }
        self.drop_empty_jobs();
    }

    fn drop_empty_jobs(&mut self) {
        let jobs = &mut self.jobs;
        self.ring.retain(|j| {
            let keep = jobs.get(j).is_some_and(|q| !q.items.is_empty());
            if !keep {
                jobs.remove(j);
            }
            keep
        });
    }

    /// Marks `fill` as partly filled in this class (a part of it was dispatched).
    pub fn activate(&mut self, fill: u64) {
        self.active.insert(fill);
    }

    /// `fill` has no part of this class queued or in flight any more.
    pub fn deactivate(&mut self, fill: u64) {
        self.active.remove(&fill);
    }

    /// Whether `fill` is in the active window.
    #[must_use]
    pub fn is_active(&self, fill: u64) -> bool {
        self.active.contains(&fill)
    }

    fn eligible(&self, fill: u64) -> bool {
        self.active.len() < self.window || self.active.contains(&fill)
    }

    fn head(&self, job: JobId) -> Option<(Key, u64)> {
        self.jobs
            .get(&job)?
            .items
            .iter()
            .find(|(k, _)| self.eligible(k.2.fill))
            .map(|(k, s)| (*k, *s))
    }

    /// The next item by deficit round robin with `quantum` bytes, removed from the queue, or
    /// `None` when no queued item is in the window.
    pub fn pick(&mut self, quantum: u64) -> Option<Queued> {
        if !self.ring.iter().any(|j| self.head(*j).is_some()) {
            return None;
        }
        let quantum = quantum.max(1);
        loop {
            let job = *self.ring.front()?;
            let head = self.head(job);
            let q = self.jobs.get_mut(&job)?;
            match head {
                Some((key, size)) if size <= q.deficit => {
                    q.deficit -= size;
                    q.items.remove(&key);
                    for keys in self.by_fill.get_mut(&key.2.fill).into_iter() {
                        keys.retain(|(_, k)| *k != key);
                    }
                    if q.items.is_empty() {
                        self.jobs.remove(&job);
                        self.ring.pop_front();
                    }
                    return Some(Queued {
                        job,
                        rank: key.0,
                        offset: key.1,
                        size,
                        what: key.2,
                    });
                }
                Some(_) => {
                    q.deficit = q.deficit.saturating_add(quantum);
                    self.ring.rotate_left(1);
                }
                None => self.ring.rotate_left(1),
            }
        }
    }

    /// The number of queued items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs.values().map(|q| q.items.len()).sum()
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
