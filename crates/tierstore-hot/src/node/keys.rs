//! Per-name async locks: the key lock of 10.3 and 10.5 (a second PUT of a key waits on the
//! first's outcome) and the lock of one segment's seal state. An entry lives while a holder or a
//! waiter has it, so the map holds only names in use.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

type Map = Mutex<BTreeMap<Vec<u8>, Arc<AsyncMutex<()>>>>;

/// Locks by name.
#[derive(Default)]
pub(crate) struct Locks {
    map: Arc<Map>,
}

/// A held lock; dropping it releases the name.
pub(crate) struct Held {
    guard: Option<OwnedMutexGuard<()>>,
    name: Vec<u8>,
    map: Arc<Map>,
}

impl Locks {
    /// Waits for `name` and holds it.
    pub(crate) async fn lock(&self, name: &[u8]) -> Held {
        let entry = Arc::clone(
            self.map
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(name.to_vec())
                .or_default(),
        );
        let guard = entry.lock_owned().await;
        Held {
            guard: Some(guard),
            name: name.to_vec(),
            map: Arc::clone(&self.map),
        }
    }

    /// Names with a holder or a waiter (tests: the map does not grow).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        // The map's own handle is the last one: no holder and no waiter is left.
        if map
            .get(&self.name)
            .is_some_and(|e| Arc::strong_count(e) == 1)
        {
            map.remove(&self.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_released_name_leaves_the_map_and_a_waiter_keeps_it() {
        futures::executor::block_on(async {
            let locks = Locks::default();
            let a = locks.lock(b"k").await;
            assert_eq!(locks.len(), 1);
            let mut second = Box::pin(locks.lock(b"k"));
            assert!(futures::poll!(second.as_mut()).is_pending());
            drop(a);
            assert_eq!(locks.len(), 1, "the waiter still holds the entry");
            let b = second.await;
            drop(b);
            assert_eq!(locks.len(), 0);
        });
    }
}
