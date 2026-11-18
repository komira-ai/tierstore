//! [`FjallBackend`]: the index store on `fjall`, an LSM-tree (the design's choice, 10.2).
//!
//! One database holds one keyspace per [`Space`]. A commit is one write batch across keyspaces and
//! is not synced: the database runs with its journal persisted by hand, so a batch reaches
//! the journal's buffer and becomes durable at the next [`Backend::persist`]. `fjall` takes no empty
//! key, so a commit with one fails before it writes anything.

use std::ops::Bound;
use std::path::Path;

use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};

use super::{Backend, BackendError, Op, Pairs, Space};

const ENGINE: &str = "fjall";

/// The error of `op` for an engine failure: for `map_err`.
fn fail<E: std::fmt::Display>(op: &'static str) -> impl FnOnce(E) -> BackendError {
    move |e| BackendError::new(ENGINE, op, e)
}

/// The settings of a [`FjallBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FjallOptions {
    /// The capacity of the block cache, in bytes.
    pub cache_bytes: u64,
}

impl Default for FjallOptions {
    /// A cache of 256 MiB.
    fn default() -> Self {
        Self {
            cache_bytes: 256 * 1024 * 1024,
        }
    }
}

/// The index store on one `fjall` database. See the module documentation.
pub struct FjallBackend {
    db: Database,
    /// The keyspaces, in the order of [`Space::ALL`].
    spaces: Vec<Keyspace>,
}

impl std::fmt::Debug for FjallBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FjallBackend")
            .field("keyspaces", &self.spaces.len())
            .finish_non_exhaustive()
    }
}

impl FjallBackend {
    /// Opens the database in the directory `path`, making it and its keyspaces when absent.
    ///
    /// # Errors
    /// If the database cannot be opened or a keyspace cannot be made.
    pub fn open(path: &Path, opts: &FjallOptions) -> Result<Self, BackendError> {
        let db = Database::builder(path)
            .cache_size(opts.cache_bytes)
            .manual_journal_persist(true)
            .open()
            .map_err(fail("open"))?;
        let spaces = Space::ALL
            .iter()
            .map(|space| db.keyspace(space.name(), KeyspaceCreateOptions::default))
            .collect::<Result<Vec<_>, _>>()
            .map_err(fail("open keyspace"))?;
        Ok(Self { db, spaces })
    }

    fn keyspace(&self, space: Space) -> &Keyspace {
        &self.spaces[space as usize]
    }
}

impl Backend for FjallBackend {
    fn engine(&self) -> &'static str {
        ENGINE
    }

    fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>, BackendError> {
        self.keyspace(space)
            .get(key)
            .map(|value| value.map(|v| v.to_vec()))
            .map_err(fail("get"))
    }

    fn scan(&self, space: Space, from: Bound<&[u8]>, limit: usize) -> Result<Pairs, BackendError> {
        self.keyspace(space)
            .range::<&[u8], _>((from, Bound::Unbounded))
            .take(limit)
            .map(|guard| {
                let (key, value) = guard.into_inner().map_err(fail("scan"))?;
                Ok((key.to_vec(), value.to_vec()))
            })
            .collect()
    }

    fn commit(&self, ops: Vec<Op>) -> Result<(), BackendError> {
        let empty_key = ops.iter().any(|op| match op {
            Op::Put { key, .. } | Op::Delete { key, .. } => key.is_empty(),
        });
        if empty_key {
            return Err(BackendError::new(ENGINE, "commit", "an empty key"));
        }
        let mut batch = self.db.batch();
        for op in ops {
            match op {
                Op::Put { space, key, value } => batch.insert(self.keyspace(space), key, value),
                Op::Delete { space, key } => batch.remove(self.keyspace(space), key),
            }
        }
        batch.commit().map_err(fail("commit"))
    }

    fn persist(&self) -> Result<(), BackendError> {
        self.db
            .persist(PersistMode::SyncAll)
            .map_err(fail("persist"))
    }
}
