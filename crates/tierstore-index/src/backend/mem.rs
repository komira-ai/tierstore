//! [`MemBackend`]: the store in memory, with crashes and failures on demand.
//!
//! It keeps the durable state (every batch up to the last persist) and the batches committed since,
//! in order. [`MemBackend::crashed`] gives the store a crash would leave: the durable state plus
//! any prefix of the later batches, each whole. [`MemBackend::fail_next`] makes the next calls of
//! one kind fail, so the index's handling of a failing store is tested without a broken disk.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{Backend, BackendError, Op, Space};

type Map = BTreeMap<(Space, Vec<u8>), Vec<u8>>;

/// A kind of call of the store, for failure injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// [`Backend::get`].
    Get,
    /// [`Backend::scan`].
    Scan,
    /// [`Backend::commit`].
    Commit,
    /// [`Backend::persist`].
    Persist,
}

#[derive(Debug, Default, Clone)]
struct State {
    durable: Map,
    current: Map,
    since: Vec<Vec<Op>>,
    /// Per kind of call: calls to let through before failing, and how many to fail then.
    fail: Vec<(Call, usize, usize)>,
}

/// The store in memory. See the module documentation.
#[derive(Debug, Default)]
pub struct MemBackend {
    state: Mutex<State>,
}

fn apply(map: &mut Map, ops: &[Op]) {
    for op in ops {
        match op {
            Op::Put { space, key, value } => {
                map.insert((*space, key.clone()), value.clone());
            }
            Op::Delete { space, key } => {
                map.remove(&(*space, key.clone()));
            }
        }
    }
}

impl MemBackend {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The batches committed since the last persist.
    #[must_use]
    pub fn unpersisted(&self) -> usize {
        self.lock().since.len()
    }

    /// The store a crash leaves: the durable state and the first `keep` batches committed after
    /// the last persist (all of them when `keep` is larger). No failure is scheduled in it.
    #[must_use]
    pub fn crashed(&self, keep: usize) -> Self {
        let st = self.lock();
        let mut map = st.durable.clone();
        for ops in st.since.iter().take(keep) {
            apply(&mut map, ops);
        }
        Self {
            state: Mutex::new(State {
                durable: map.clone(),
                current: map,
                since: Vec::new(),
                fail: Vec::new(),
            }),
        }
    }

    /// Makes `count` calls of kind `call` fail after `after` more calls of that kind succeed.
    pub fn fail_next(&self, call: Call, after: usize, count: usize) {
        self.lock().fail.push((call, after, count));
    }

    /// The pairs of every keyspace, in order: the whole visible state, for comparisons in tests.
    #[must_use]
    pub fn dump(&self) -> Vec<(Space, Vec<u8>, Vec<u8>)> {
        self.lock()
            .current
            .iter()
            .map(|((s, k), v)| (*s, k.clone(), v.clone()))
            .collect()
    }

    fn check(st: &mut State, call: Call, op: &'static str) -> Result<(), BackendError> {
        for slot in &mut st.fail {
            if slot.0 == call && slot.2 > 0 {
                if slot.1 > 0 {
                    slot.1 -= 1;
                    return Ok(());
                }
                slot.2 -= 1;
                return Err(BackendError::new("memory", op, "injected failure"));
            }
        }
        Ok(())
    }
}

impl Backend for MemBackend {
    fn engine(&self) -> &'static str {
        "memory"
    }

    fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>, BackendError> {
        let mut st = self.lock();
        Self::check(&mut st, Call::Get, "get")?;
        Ok(st.current.get(&(space, key.to_vec())).cloned())
    }

    fn scan(
        &self,
        space: Space,
        from: Bound<&[u8]>,
        limit: usize,
    ) -> Result<super::Pairs, BackendError> {
        let mut st = self.lock();
        Self::check(&mut st, Call::Scan, "scan")?;
        let lower = match from {
            Bound::Included(k) => Bound::Included((space, k.to_vec())),
            Bound::Excluded(k) => Bound::Excluded((space, k.to_vec())),
            Bound::Unbounded => Bound::Included((space, Vec::new())),
        };
        Ok(st
            .current
            .range((lower, Bound::Unbounded))
            .take_while(|((s, _), _)| *s == space)
            .take(limit)
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect())
    }

    fn commit(&self, ops: Vec<Op>) -> Result<(), BackendError> {
        let mut st = self.lock();
        Self::check(&mut st, Call::Commit, "commit")?;
        apply(&mut st.current, &ops);
        st.since.push(ops);
        Ok(())
    }

    fn persist(&self) -> Result<(), BackendError> {
        let mut st = self.lock();
        Self::check(&mut st, Call::Persist, "persist")?;
        let since = std::mem::take(&mut st.since);
        for ops in &since {
            apply(&mut st.durable, ops);
        }
        Ok(())
    }
}
