//! [`FakeFs`]: an in-memory [`Fs`] with a durability model and failure injection.
//!
//! Each file holds its bytes and a durable length; a file made by `append` and never synced has no
//! durable directory entry. [`FakeFs::crash`] returns the file system a power loss leaves.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{Fs, check};

/// An operation of [`Fs`], for failure injection and call counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FsOp {
    /// [`Fs::list`].
    List,
    /// [`Fs::len`].
    Len,
    /// [`Fs::read_at`].
    Read,
    /// [`Fs::append`].
    Append,
    /// [`Fs::truncate`].
    Truncate,
    /// [`Fs::sync`].
    Sync,
    /// [`Fs::remove`].
    Remove,
}

/// What a crash does to the unsynced tail of each file (the bytes after its durable length).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Torn {
    /// The tail is lost.
    Lost,
    /// The tail is kept whole.
    Kept,
    /// The first `n` bytes of the tail are kept.
    Prefix(u64),
}

#[derive(Debug, Clone)]
struct Node {
    bytes: Vec<u8>,
    /// Bytes that survive a crash; never above `bytes.len()`.
    durable: usize,
    /// Whether the directory entry is durable.
    listed: bool,
}

#[derive(Debug)]
struct Failure {
    op: FsOp,
    prefix: String,
    left: usize,
}

#[derive(Debug, Default)]
struct State {
    files: BTreeMap<String, Node>,
    failures: Vec<Failure>,
    calls: BTreeMap<FsOp, u64>,
}

/// An in-memory file system; see the module's documentation.
#[derive(Debug, Default)]
pub struct FakeFs {
    state: Mutex<State>,
}

fn not_found(path: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no file {path}"))
}

impl FakeFs {
    /// An empty file system.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts the call, refuses a bad path and applies a pending injected failure.
    fn begin(&self, op: FsOp, path: &str, dir: bool) -> io::Result<MutexGuard<'_, State>> {
        let mut st = self.state();
        *st.calls.entry(op).or_default() += 1;
        check(path, dir)?;
        if let Some(f) = st
            .failures
            .iter_mut()
            .find(|f| f.op == op && f.left > 0 && path.starts_with(&f.prefix))
        {
            f.left -= 1;
            return Err(io::Error::other(format!(
                "injected {op:?} failure on {path}"
            )));
        }
        Ok(st)
    }

    /// Sets up a durable file with `bytes` (replacing one at `path`), without counting a call.
    pub fn put(&self, path: &str, bytes: &[u8]) {
        self.state().files.insert(
            path.to_owned(),
            Node {
                bytes: bytes.to_vec(),
                durable: bytes.len(),
                listed: true,
            },
        );
    }

    /// The bytes of the file at `path` as they are now, durable or not.
    #[must_use]
    pub fn bytes(&self, path: &str) -> Option<Vec<u8>> {
        self.state().files.get(path).map(|n| n.bytes.clone())
    }

    /// Makes the next `count` calls of `op` on a path that starts with `path_prefix` fail with an
    /// I/O error (the call changes nothing).
    pub fn fail(&self, op: FsOp, path_prefix: &str, count: usize) {
        self.state().failures.push(Failure {
            op,
            prefix: path_prefix.to_owned(),
            left: count,
        });
    }

    /// How many times `op` was called, failed calls and refused paths included.
    #[must_use]
    pub fn calls(&self, op: FsOp) -> u64 {
        self.state().calls.get(&op).copied().unwrap_or(0)
    }

    /// The file system a crash leaves: files whose directory entry was never synced are gone, and
    /// each other file keeps its durable bytes plus what `torn` says of its unsynced tail. Every
    /// surviving byte is durable in the result, which has no failures and no call counts.
    #[must_use]
    pub fn crash(&self, torn: Torn) -> FakeFs {
        let st = self.state();
        let mut files = BTreeMap::new();
        for (path, node) in st.files.iter().filter(|(_, n)| n.listed) {
            let tail = node.bytes.len() - node.durable;
            let kept = match torn {
                Torn::Lost => 0,
                Torn::Kept => tail,
                Torn::Prefix(n) => usize::try_from(n).unwrap_or(usize::MAX).min(tail),
            };
            let bytes = node.bytes[..node.durable + kept].to_vec();
            let durable = bytes.len();
            files.insert(
                path.clone(),
                Node {
                    bytes,
                    durable,
                    listed: true,
                },
            );
        }
        FakeFs {
            state: Mutex::new(State {
                files,
                ..State::default()
            }),
        }
    }
}

impl Fs for FakeFs {
    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        let st = self.begin(FsOp::List, dir, true)?;
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let names: BTreeSet<&str> = st
            .files
            .keys()
            .filter_map(|p| p.strip_prefix(&prefix))
            .map(|rest| rest.split('/').next().unwrap_or(rest))
            .collect();
        Ok(names.into_iter().map(str::to_owned).collect())
    }

    fn len(&self, path: &str) -> io::Result<u64> {
        let st = self.begin(FsOp::Len, path, false)?;
        let node = st.files.get(path).ok_or_else(|| not_found(path))?;
        Ok(node.bytes.len() as u64)
    }

    fn read_at(&self, path: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let st = self.begin(FsOp::Read, path, false)?;
        let node = st.files.get(path).ok_or_else(|| not_found(path))?;
        let size = node.bytes.len();
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(size);
        let end = start
            .saturating_add(usize::try_from(len).unwrap_or(usize::MAX))
            .min(size);
        Ok(node.bytes[start..end].to_vec())
    }

    fn append(&self, path: &str, bytes: &[u8]) -> io::Result<()> {
        let mut st = self.begin(FsOp::Append, path, false)?;
        st.files
            .entry(path.to_owned())
            .or_insert_with(|| Node {
                bytes: Vec::new(),
                durable: 0,
                listed: false,
            })
            .bytes
            .extend_from_slice(bytes);
        Ok(())
    }

    fn truncate(&self, path: &str, len: u64) -> io::Result<()> {
        let mut st = self.begin(FsOp::Truncate, path, false)?;
        let node = st.files.get_mut(path).ok_or_else(|| not_found(path))?;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        node.bytes.resize(len, 0);
        node.durable = node.durable.min(len);
        Ok(())
    }

    fn sync(&self, path: &str) -> io::Result<()> {
        let mut st = self.begin(FsOp::Sync, path, false)?;
        let node = st.files.get_mut(path).ok_or_else(|| not_found(path))?;
        node.durable = node.bytes.len();
        node.listed = true;
        Ok(())
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        let mut st = self.begin(FsOp::Remove, path, false)?;
        st.files
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| not_found(path))
    }
}
