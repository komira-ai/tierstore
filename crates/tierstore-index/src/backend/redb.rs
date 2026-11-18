//! [`RedbBackend`]: the index store on `redb`, a copy-on-write B-tree (the measured alternative of
//! 10.2).
//!
//! One table per [`Space`] in the file `<path>/index.redb`. A commit is one write transaction with
//! no durability of its own; [`Backend::persist`] commits an empty transaction with immediate
//! durability, which makes every earlier commit durable.

use std::ops::Bound;
use std::path::Path;

use redb::{Database, Durability, ReadableDatabase, Table, TableDefinition};

use super::{Backend, BackendError, Op, Pairs, Space};

const ENGINE: &str = "redb";

/// The error of `op` for an engine failure: for `map_err`.
fn fail<E: std::fmt::Display>(op: &'static str) -> impl FnOnce(E) -> BackendError {
    move |e| BackendError::new(ENGINE, op, e)
}

/// The name of the database file inside the backend's directory.
const FILE: &str = "index.redb";

type Def = TableDefinition<'static, &'static [u8], &'static [u8]>;

/// The settings of a [`RedbBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedbOptions {
    /// The size of the page cache, in bytes.
    pub cache_bytes: u64,
}

impl Default for RedbOptions {
    /// A cache of 256 MiB.
    fn default() -> Self {
        Self {
            cache_bytes: 256 * 1024 * 1024,
        }
    }
}

/// The index store on one `redb` database. See the module documentation.
#[derive(Debug)]
pub struct RedbBackend {
    db: Database,
}

const fn def(space: Space) -> Def {
    TableDefinition::new(space.name())
}

impl RedbBackend {
    /// Opens the database `<path>/index.redb`, making the directory `path`, the file and every
    /// table when absent.
    ///
    /// # Errors
    /// If the directory cannot be made, the database cannot be opened or a table cannot be made.
    pub fn open(path: &Path, opts: &RedbOptions) -> Result<Self, BackendError> {
        std::fs::create_dir_all(path).map_err(fail("create dir"))?;
        let cache = usize::try_from(opts.cache_bytes).unwrap_or(usize::MAX);
        let db = Database::builder()
            .set_cache_size(cache)
            .create(path.join(FILE))
            .map_err(fail("open"))?;
        let txn = db.begin_write().map_err(fail("open begin"))?;
        for space in Space::ALL {
            txn.open_table(def(space)).map_err(fail("open table"))?;
        }
        txn.commit().map_err(fail("open commit"))?;
        Ok(Self { db })
    }
}

impl Backend for RedbBackend {
    fn engine(&self) -> &'static str {
        ENGINE
    }

    fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>, BackendError> {
        let txn = self.db.begin_read().map_err(fail("get"))?;
        let table = txn.open_table(def(space)).map_err(fail("get"))?;
        let value = table.get(key).map_err(fail("get"))?;
        Ok(value.map(|v| v.value().to_vec()))
    }

    fn scan(&self, space: Space, from: Bound<&[u8]>, limit: usize) -> Result<Pairs, BackendError> {
        let txn = self.db.begin_read().map_err(fail("scan"))?;
        let table = txn.open_table(def(space)).map_err(fail("scan"))?;
        table
            .range::<&[u8]>((from, Bound::Unbounded))
            .map_err(fail("scan"))?
            .take(limit)
            .map(|item| {
                let (key, value) = item.map_err(fail("scan"))?;
                Ok((key.value().to_vec(), value.value().to_vec()))
            })
            .collect()
    }

    fn commit(&self, ops: Vec<Op>) -> Result<(), BackendError> {
        let mut txn = self.db.begin_write().map_err(fail("commit"))?;
        txn.set_durability(Durability::None)
            .map_err(fail("commit"))?;
        // The tables of the spaces the batch touches, opened on first use.
        let mut tables: Vec<Option<Table<&[u8], &[u8]>>> =
            Space::ALL.iter().map(|_| None).collect();
        for op in &ops {
            let space = match op {
                Op::Put { space, .. } | Op::Delete { space, .. } => *space,
            };
            let slot = &mut tables[space as usize];
            let table = match slot {
                Some(table) => table,
                None => slot.insert(txn.open_table(def(space)).map_err(fail("commit"))?),
            };
            match op {
                Op::Put { key, value, .. } => {
                    table
                        .insert(key.as_slice(), value.as_slice())
                        .map_err(fail("commit"))?;
                }
                Op::Delete { key, .. } => {
                    table.remove(key.as_slice()).map_err(fail("commit"))?;
                }
            }
        }
        drop(tables);
        txn.commit().map_err(fail("commit"))
    }

    fn persist(&self) -> Result<(), BackendError> {
        let mut txn = self.db.begin_write().map_err(fail("persist"))?;
        txn.set_durability(Durability::Immediate)
            .map_err(fail("persist"))?;
        txn.commit().map_err(fail("persist"))
    }
}
