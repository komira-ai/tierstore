//! The core of `tierstore fs`: the operations of a filesystem on inode numbers, answered from the
//! attached trees, with no protocol in it. The FUSE frontend (`fuse.rs`) decodes the kernel's
//! requests into these calls; a test makes them directly.
//!
//! A frontend uses the core in one of two ways. With opens: [`Core::open`] gives a handle that
//! [`Core::read`] reads and [`Core::release`] closes, and every lookup the kernel makes is counted
//! until it is forgotten ([`Core::forget`]), so that a detached tree stays in memory while the
//! kernel holds one of its inodes. By inode alone ([`Core::read_ino`], in `core/read.rs`): no
//! handle, no open and no count, because the protocol has no open, release or forget; a tree
//! leaves memory when it is detached. `Settings::count_lookups` chooses.
//!
//! The namespace is the mount's root (`ino::ROOT`) and `trees/` (`ino::TREES`); below `trees/`
//! are the roots of the trees that some run has attached. Every inode number of a tree is
//! derivable (`ino.rs`), so a tree that is not in memory is read again, from the table of tree
//! numbers and the manifests, whenever a request names one of its inodes.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::server::local_cache::{blob_path, manifest_path};
use crate::tree_manifest::{
    BlobSource, EntryKind, MAX_MANIFEST_BYTES, TreeDigest, TreeEntry, flatten,
};

use super::attach::{Attachments, Failure, is_run};
use super::backing::{self, Backing, BackingTable, Registrar};
use super::fill_client::FillClient;
use super::ino;
use super::locked;
use super::table::{TableError, TreeTable};
use super::tree::Tree;

/// How long the kernel may keep what it was told below a tree's root: the longest it keeps, about
/// a year. Everything there is immutable.
pub(super) const TREE_TTL: Duration = Duration::from_secs(32_000_000);

/// The modification time of every entry of a tree, in seconds since the epoch: the value the
/// executor's materialisation uses (`FIXED_MTIME`, D10).
pub(super) const FIXED_MTIME: u64 = 1_790_000_000;

/// The mode of a directory of the namespace: anyone may pass through, nobody lists.
const NAMESPACE_MODE: u16 = 0o711;

/// Why a request was not done, in the words of the errno the kernel is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Errno {
    /// `ENOENT`.
    NoEnt,
    /// `EACCES`: a listing of the namespace.
    Acces,
    /// `ENOTDIR`.
    NotDir,
    /// `EISDIR`.
    IsDir,
    /// `EINVAL`.
    Inval,
    /// `EIO`: a fill failed, or a tree could not be read.
    Io,
    /// `EROFS`: a request that would change a tree.
    RoFs,
}

/// What kind of entry an inode is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Dir,
    File,
    Symlink,
}

/// The attributes of an inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Attr {
    pub ino: u64,
    pub kind: Kind,
    pub size: u64,
    /// The permission bits.
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
}

/// An inode, and how long the kernel may keep the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub attr: Attr,
    pub ttl: Duration,
}

/// The answer to a lookup.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Looked {
    /// The name leads to this inode.
    Found(Entry),
    /// The name is not there below a tree's root, and will not be for as long as the answer says.
    Absent(Duration),
}

/// An entry of a directory.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct DirItem {
    pub name: String,
    pub entry: Entry,
}

/// A tree that is in memory.
#[derive(Debug)]
struct Loaded {
    digest: TreeDigest,
    tree: Tree,
    /// The lookups of its inodes the kernel has not forgotten yet.
    lookups: AtomicU64,
    /// The blobs this tree has asked the cache to pin.
    pinned: Mutex<HashSet<TreeDigest>>,
}

/// An open file.
#[derive(Debug)]
enum Handle {
    /// A file of no bytes: nothing to read.
    Empty,
    /// The cache file of a blob.
    Cached(File),
    /// The cache file of a blob that got no backing id because `fs.max_backing_ids` were live; the
    /// release is counted on the blob in the backing table.
    Spilled(File, TreeDigest),
    /// A blob served by kernel passthrough: reads never reach this process, and the release is
    /// counted on the blob's backing id.
    Backed(TreeDigest),
}

/// What an open is answered with.
#[derive(Debug)]
pub(super) enum Opened {
    /// A cached open: the page cache is kept, and every read it misses reaches [`Core::read`].
    Cached(u64),
    /// An open served by passthrough from the blob's backing id.
    Backed {
        /// The handle of the open.
        fh: u64,
        /// The id to answer with.
        backing: Backing,
    },
}

/// The trees in memory and the attachments.
#[derive(Debug)]
struct State {
    loaded: HashMap<u64, Arc<Loaded>>,
    attachments: Attachments,
}

/// Why an attach was refused.
#[derive(Debug, thiserror::Error)]
pub(super) enum AttachError {
    /// The digest is not sixty-four lower-case hex digits.
    #[error("not a sha256 digest")]
    Digest,
    /// The run is empty, longer than a name may be, or not visible ASCII.
    #[error("not a usable run name")]
    Run,
    /// The tree's manifests cannot be read, or are refused by name.
    #[error("{0}")]
    Tree(String),
    /// The table of tree numbers failed.
    #[error("{0}")]
    Table(#[from] TableError),
    /// The attachments file could not be written.
    #[error("the attachments file: {0}")]
    File(#[from] io::Error),
}

/// The settings of a core that are not the other parts it is made of.
#[derive(Clone, Debug)]
pub(super) struct Settings {
    pub owner: (u32, u32),
    pub cache_path: PathBuf,
    pub mountpoint: PathBuf,
    /// The id of this mount, which the pins are made under.
    pub mount_id: String,
    /// The most backing ids the kernel is given at once (`fs.max_backing_ids`).
    pub max_backing_ids: usize,
    /// Whether the lookups of a tree's inodes are counted and a detached tree stays in memory
    /// until they are forgotten (a frontend that is told of forgets). If not, a tree is dropped
    /// when it is detached.
    pub count_lookups: bool,
}

/// The core.
pub(in crate::server) struct Core {
    settings: Settings,
    fills: FillClient,
    table: TreeTable,
    state: Mutex<State>,
    /// Held by an attach, a restore and a detach for all of their work, so that a detach cannot
    /// unpin a tree that an attach has just taken.
    lifecycle: Mutex<()>,
    /// The trees a thread is filling now for an attach that asked for `prefetch = "all"`, each
    /// with the token of its thread: a detach removes the entry, and a thread whose token is no
    /// longer the tree's has been replaced and stops.
    prefetching: Mutex<HashMap<u64, Arc<()>>>,
    /// The backing table, once the kernel offered passthrough and it was chosen.
    backing: OnceLock<Arc<BackingTable>>,
    /// Where the backing table is kept.
    backing_file: PathBuf,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    next_handle: AtomicU64,
    /// The requests served, by name, for the tests that count what reaches the core.
    #[cfg(test)]
    pub(super) served: Mutex<Vec<&'static str>>,
}

impl Core {
    /// A core with no tree attached, whose attachments are kept in `runtime_dir`.
    pub(super) fn new(
        settings: Settings,
        fills: FillClient,
        table: TreeTable,
        runtime_dir: &Path,
    ) -> Self {
        Self {
            settings,
            fills,
            table,
            state: Mutex::new(State {
                loaded: HashMap::new(),
                attachments: Attachments::new(runtime_dir),
            }),
            lifecycle: Mutex::new(()),
            prefetching: Mutex::new(HashMap::new()),
            backing: OnceLock::new(),
            backing_file: runtime_dir.join(backing::FILE),
            handles: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
            #[cfg(test)]
            served: Mutex::new(Vec::new()),
        }
    }

    /// Notes that a request of the kind `what` was served.
    #[cfg(test)]
    fn note(&self, what: &'static str) {
        locked(&self.served).push(what);
    }

    /// The attributes of `ino`, a directory of the namespace.
    fn namespace(&self, ino: u64) -> Entry {
        Entry {
            attr: Attr {
                ino,
                kind: Kind::Dir,
                size: 0,
                perm: NAMESPACE_MODE,
                nlink: 2 + u32::from(ino == ino::ROOT),
                uid: 0,
                gid: 0,
            },
            ttl: Duration::ZERO,
        }
    }

    /// The entry `entry` of the tree `number`, as the kernel is told it.
    fn attr_of(&self, number: u64, tree: &Tree, entry: &TreeEntry) -> Attr {
        let (kind, size, perm, nlink) = match &entry.kind {
            EntryKind::Dir { .. } => (Kind::Dir, 0, 0o755, tree.links(entry.index)),
            EntryKind::File { size, exec, .. } => {
                (Kind::File, *size, if *exec { 0o755 } else { 0o644 }, 1)
            }
            EntryKind::Symlink { target } => (Kind::Symlink, target.len() as u64, 0o777, 1),
        };
        Attr {
            ino: ino::of(number, entry.index),
            kind,
            size,
            perm,
            nlink,
            uid: self.settings.owner.0,
            gid: self.settings.owner.1,
        }
    }

    /// Reads the tree `digest`: its manifests, fetched through the fill socket where the cache
    /// does not hold them, and flattened.
    fn read_tree(&self, digest: &TreeDigest) -> Result<Tree, String> {
        let fetch = |wanted: &TreeDigest| -> Result<Vec<u8>, String> {
            let hex = wanted.to_string();
            let path = manifest_path(&self.settings.cache_path, &hex);
            if let Ok(bytes) = read_manifest(&path) {
                return Ok(bytes);
            }
            self.fills.fill_manifest(&hex)?;
            read_manifest(&path).map_err(|error| error.to_string())
        };
        flatten(digest, fetch)
            .map(Tree::new)
            .map_err(|error| error.to_string())
    }

    /// The tree numbered `number`, read if it is not in memory.
    fn tree(&self, number: u64) -> Result<Arc<Loaded>, Errno> {
        if let Some(loaded) = locked(&self.state).loaded.get(&number) {
            return Ok(Arc::clone(loaded));
        }
        let digest = self
            .table
            .digest_of(number)
            .map_err(|_| Errno::Io)?
            .ok_or(Errno::NoEnt)?;
        self.load(number, digest).map_err(|_| Errno::Io)
    }

    /// Reads the tree `digest`, which is numbered `number`, and keeps it in memory.
    fn load(&self, number: u64, digest: TreeDigest) -> Result<Arc<Loaded>, String> {
        let tree = self.read_tree(&digest)?;
        let loaded = Arc::new(Loaded {
            digest,
            tree,
            lookups: AtomicU64::new(0),
            pinned: Mutex::new(HashSet::new()),
        });
        let mut state = locked(&self.state);
        Ok(Arc::clone(state.loaded.entry(number).or_insert(loaded)))
    }

    /// Makes `run` use the tree `digest` (sixty-four hex digits): reads and flattens it, records
    /// the attachment, and gives the path the job is to be given.
    ///
    /// # Errors
    /// [`AttachError`]: why nothing was attached.
    pub(super) fn attach(
        self: &Arc<Self>,
        digest: &str,
        run: &str,
    ) -> Result<PathBuf, AttachError> {
        self.attach_asking(digest, run, false)
    }

    /// [`Core::attach`], and, with `prefetch` (`prefetch = "all"`), every blob of the tree is
    /// filled in the background, in manifest order, without delaying the answer.
    pub(super) fn attach_asking(
        self: &Arc<Self>,
        digest: &str,
        run: &str,
        prefetch: bool,
    ) -> Result<PathBuf, AttachError> {
        let digest = TreeDigest::from_hex(digest).ok_or(AttachError::Digest)?;
        if !is_run(run) {
            return Err(AttachError::Run);
        }
        let _lifecycle = locked(&self.lifecycle);
        let number = self.table.number_of(&digest)?;
        let _ = self.load_unless_loaded(number, digest)?;
        {
            let mut state = locked(&self.state);
            let added = state.attachments.add(run, number, digest);
            if let Err(error) = state.attachments.save() {
                if added {
                    state.attachments.remove_one(run, number);
                }
                return Err(error.into());
            }
        }
        if prefetch {
            self.start_prefetch(number);
        }
        Ok(self
            .settings
            .mountpoint
            .join("trees")
            .join(digest.to_string()))
    }

    /// Attaches again what the attachments file held before a restart: each pair of tree digest
    /// and run whose tree can be read is attached in memory, and the file is written once at the
    /// end. A pair that cannot be attached now is kept in the file, not served, until its run
    /// attaches again or is detached.
    pub(super) fn restore(&self, saved: Vec<(TreeDigest, String)>) {
        let _lifecycle = locked(&self.lifecycle);
        for (digest, run) in saved {
            let loaded = self
                .table
                .number_of(&digest)
                .map_err(|error| error.to_string())
                .and_then(|number| {
                    self.load_unless_loaded(number, digest)
                        .map(|_| number)
                        .map_err(|error| error.to_string())
                });
            let mut state = locked(&self.state);
            match loaded {
                Ok(number) => {
                    let _ = state.attachments.add(&run, number, digest);
                }
                Err(why) => {
                    tracing::warn!("tierstore fs: not attaching {digest} for {run} again: {why}");
                    state.attachments.keep(&run, digest);
                }
            }
        }
        if let Err(why) = locked(&self.state).attachments.save() {
            tracing::warn!("tierstore fs: cannot write the attachments file: {why}");
        }
    }

    /// The tree `number` in memory, read if it is not.
    fn load_unless_loaded(
        &self,
        number: u64,
        digest: TreeDigest,
    ) -> Result<Arc<Loaded>, AttachError> {
        if let Some(loaded) = locked(&self.state).loaded.get(&number) {
            return Ok(Arc::clone(loaded));
        }
        self.load(number, digest).map_err(AttachError::Tree)
    }

    /// Ends every attachment of `run`: the failures its attachments recorded, or none if it
    /// attached nothing. A tree no run has now is unpinned, and left memory when the kernel
    /// forgets its inodes.
    pub(super) fn detach(&self, run: &str) -> Option<Vec<Failure>> {
        let _lifecycle = locked(&self.lifecycle);
        let (freed, failures) = {
            let mut state = locked(&self.state);
            let trees = state.attachments.remove(run)?;
            let _ = state.attachments.save();
            let mut failures = Vec::new();
            let mut freed = Vec::new();
            for (number, digest, mut failed) in trees {
                failures.append(&mut failed);
                if !state.attachments.is_attached(number) {
                    freed.push((number, digest));
                }
            }
            (freed, failures)
        };
        for (number, digest) in freed {
            let _ = locked(&self.prefetching).remove(&number);
            let loaded = locked(&self.state).loaded.get(&number).cloned();
            loaded
                .iter()
                .for_each(|loaded| locked(&loaded.pinned).clear());
            let _ = self
                .fills
                .unpin(&self.settings.mount_id, &digest.to_string());
            self.drop_if_forgotten(number);
        }
        Some(failures)
    }

    /// Ends the attachments of every run that is not in `keep`; the number of runs.
    pub(super) fn detach_all_except(&self, keep: &[String]) -> usize {
        let runs = locked(&self.state).attachments.runs();
        runs.iter()
            .filter(|run| !keep.contains(run))
            .filter(|run| self.detach(run).is_some())
            .count()
    }

    /// Each attachment, as a tree digest and a run.
    pub(super) fn attachments(&self) -> Vec<(TreeDigest, String)> {
        locked(&self.state).attachments.listing()
    }

    /// Tells the cache which trees are attached now, so that it drops every other pin of this
    /// mount.
    pub(super) fn pins_live(&self) {
        let trees: Vec<String> = locked(&self.state)
            .attachments
            .trees()
            .iter()
            .map(ToString::to_string)
            .collect();
        let _ = self.fills.pins_live(&self.settings.mount_id, &trees);
    }

    /// Takes the tree `number` out of memory if no run has it and the kernel holds no inode of it.
    fn drop_if_forgotten(&self, number: u64) {
        let mut state = locked(&self.state);
        let forgotten = state
            .loaded
            .get(&number)
            .is_some_and(|loaded| loaded.lookups.load(Ordering::Acquire) == 0);
        if forgotten && !state.attachments.is_attached(number) {
            let _ = state.loaded.remove(&number);
        }
    }

    /// The tree in memory, if it is, counting one more lookup of it.
    fn counted(&self, number: u64) -> Result<Arc<Loaded>, Errno> {
        let loaded = self.tree(number)?;
        self.count_lookup(&loaded);
        Ok(loaded)
    }

    /// Counts one more lookup of the tree `loaded`, if this core counts them.
    fn count_lookup(&self, loaded: &Loaded) {
        if self.settings.count_lookups {
            let _ = loaded.lookups.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Counts a lookup of `ino`, which a listing with entries (`READDIRPLUS`) made.
    pub(super) fn count(&self, ino: u64) {
        #[cfg(test)]
        self.note("count");
        let _ = self.counted(ino::split(ino).0);
    }

    /// The kernel forgets `nlookup` lookups of `ino`.
    pub(super) fn forget(&self, ino: u64, nlookup: u64) {
        #[cfg(test)]
        self.note("forget");
        let (number, _) = ino::split(ino);
        let loaded = locked(&self.state).loaded.get(&number).cloned();
        if let Some(loaded) = loaded {
            let left = loaded
                .lookups
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |lookups| {
                    Some(lookups.saturating_sub(nlookup))
                })
                .map_or(0, |before| before.saturating_sub(nlookup));
            if left == 0 {
                self.drop_if_forgotten(number);
            }
        }
    }

    /// The tree with digest `digest`, if some run has attached it: its number.
    fn attached_number(&self, digest: &str) -> Option<u64> {
        let number = self.table.find(&TreeDigest::from_hex(digest)?)?;
        locked(&self.state)
            .attachments
            .is_attached(number)
            .then_some(number)
    }

    /// Looks up `name` in the directory `parent`.
    ///
    /// # Errors
    /// [`Errno::NoEnt`] for a name the namespace does not have, [`Errno::NotDir`] for a parent
    /// that is not a directory.
    pub(super) fn lookup(&self, parent: u64, name: &str) -> Result<Looked, Errno> {
        let (number, index) = ino::split(parent);
        #[cfg(test)]
        self.note(if number == 0 { "namespace" } else { "lookup" });
        if number == 0 {
            return match (parent, name) {
                (ino::ROOT, "trees") => Ok(Looked::Found(self.namespace(ino::TREES))),
                (ino::TREES, digest) => {
                    let number = self.attached_number(digest).ok_or(Errno::NoEnt)?;
                    let loaded = self.counted(number)?;
                    let root = self.root_entry(number, &loaded, Duration::ZERO)?;
                    Ok(Looked::Found(root))
                }
                _ => Err(Errno::NoEnt),
            };
        }
        let loaded = self.tree(number)?;
        let dir = loaded.tree.entry(index).ok_or(Errno::NoEnt)?;
        if !matches!(dir.kind, EntryKind::Dir { .. }) {
            return Err(Errno::NotDir);
        }
        let found = loaded.tree.child(index, name);
        let Some(child) = found.and_then(|child| loaded.tree.entry(child)) else {
            return Ok(Looked::Absent(TREE_TTL));
        };
        self.count_lookup(&loaded);
        let attr = self.attr_of(number, &loaded.tree, child);
        Ok(Looked::Found(Entry {
            attr,
            ttl: TREE_TTL,
        }))
    }

    /// The root directory of the tree `number`, with the timeout `ttl`.
    fn root_entry(&self, number: u64, loaded: &Loaded, ttl: Duration) -> Result<Entry, Errno> {
        let root = loaded.tree.entry(0).ok_or(Errno::NoEnt)?;
        Ok(Entry {
            attr: self.attr_of(number, &loaded.tree, root),
            ttl,
        })
    }

    /// The attributes of `ino`.
    ///
    /// # Errors
    /// [`Errno::NoEnt`] for an inode no tree and no namespace has.
    pub(super) fn getattr(&self, ino: u64) -> Result<Entry, Errno> {
        #[cfg(test)]
        self.note("getattr");
        let (number, index) = ino::split(ino);
        if number == 0 {
            return match ino {
                ino::ROOT | ino::TREES => Ok(self.namespace(ino)),
                _ => Err(Errno::NoEnt),
            };
        }
        let loaded = self.tree(number)?;
        let entry = loaded.tree.entry(index).ok_or(Errno::NoEnt)?;
        Ok(Entry {
            attr: self.attr_of(number, &loaded.tree, entry),
            ttl: TREE_TTL,
        })
    }

    /// What the symbolic link `ino` says.
    ///
    /// # Errors
    /// [`Errno::Inval`] for an inode that is not a link.
    pub(super) fn readlink(&self, ino: u64) -> Result<Arc<str>, Errno> {
        #[cfg(test)]
        self.note("readlink");
        let (number, index) = ino::split(ino);
        if number == 0 {
            return Err(Errno::Inval);
        }
        let loaded = self.tree(number)?;
        match loaded.tree.entry(index).map(|entry| &entry.kind) {
            Some(EntryKind::Symlink { target }) => Ok(Arc::clone(target)),
            Some(_) => Err(Errno::Inval),
            None => Err(Errno::NoEnt),
        }
    }

    /// Opens the directory `ino` for listing.
    ///
    /// # Errors
    /// [`Errno::Acces`] for the mount's root and `trees/`, which nobody may list.
    pub(super) fn opendir(&self, ino: u64) -> Result<(), Errno> {
        let (number, index) = ino::split(ino);
        if number == 0 {
            return Err(Errno::Acces);
        }
        let loaded = self.tree(number)?;
        match loaded.tree.entry(index).map(|entry| &entry.kind) {
            Some(EntryKind::Dir { .. }) => Ok(()),
            Some(_) => Err(Errno::NotDir),
            None => Err(Errno::NoEnt),
        }
    }

    /// Gives the entries of the directory `ino` from the position `offset` (0 is the start) to
    /// `sink`, with the position that follows each, until it answers false.
    ///
    /// # Errors
    /// As [`Core::opendir`].
    pub(super) fn readdir(
        &self,
        ino: u64,
        offset: u64,
        sink: &mut dyn FnMut(u64, DirItem) -> bool,
    ) -> Result<(), Errno> {
        #[cfg(test)]
        self.note("readdir");
        self.opendir(ino)?;
        let (number, index) = ino::split(ino);
        let loaded = self.tree(number)?;
        let parent = loaded.tree.entry(index).and_then(|entry| entry.parent);
        let parent_ino = parent.map_or(ino::TREES, |parent| ino::of(number, parent));
        let kids = loaded.tree.children(index);
        let first = usize::try_from(offset).unwrap_or(usize::MAX);
        for position in first..kids.len().saturating_add(2) {
            let item = match position {
                0 => DirItem {
                    name: ".".to_owned(),
                    entry: self.getattr(ino)?,
                },
                1 => DirItem {
                    name: "..".to_owned(),
                    entry: self.getattr(parent_ino)?,
                },
                _ => {
                    let entry = loaded.tree.at(kids[position - 2]);
                    DirItem {
                        name: entry.name.to_string(),
                        entry: Entry {
                            attr: self.attr_of(number, &loaded.tree, entry),
                            ttl: TREE_TTL,
                        },
                    }
                }
            };
            if !sink(position as u64 + 1, item) {
                break;
            }
        }
        Ok(())
    }

    /// Opens the file `ino` for reading: the cache file of its blob, filled first if the cache
    /// does not hold it. A fill that fails fails the open (`Errno::Io`) and is recorded on the
    /// attachments of the tree.
    ///
    /// Where passthrough was chosen (`Core::enable_passthrough`), a non-empty file is answered
    /// with its blob's backing id: the live one, or one that `register` makes from the cache
    /// file. Otherwise, for an empty file, and for a blob that has no id when `fs.max_backing_ids`
    /// are live, the open is a cached one.
    ///
    /// # Errors
    /// [`Errno::RoFs`] for an open for writing, [`Errno::IsDir`] for a directory, [`Errno::Inval`]
    /// for a link, [`Errno::Io`] for a blob that cannot be had or an id that cannot be registered.
    pub(super) fn open(
        &self,
        ino: u64,
        writing: bool,
        register: &mut Registrar<'_>,
    ) -> Result<Opened, Errno> {
        #[cfg(test)]
        self.note("open");
        if writing {
            return Err(Errno::RoFs);
        }
        let (number, index) = ino::split(ino);
        if number == 0 {
            return Err(Errno::IsDir);
        }
        let loaded = self.tree(number)?;
        let entry = loaded.tree.entry(index).ok_or(Errno::NoEnt)?;
        let handle = match &entry.kind {
            EntryKind::Dir { .. } => return Err(Errno::IsDir),
            EntryKind::Symlink { .. } => return Err(Errno::Inval),
            EntryKind::File { size: 0, .. } => Handle::Empty,
            EntryKind::File {
                digest,
                size,
                source,
                ..
            } => {
                let blob = (digest, *size, source.as_ref());
                if let Some(table) = self.backing.get() {
                    return self.open_backed(table, (&loaded, number, entry), blob, register);
                }
                Handle::Cached(self.blob(&loaded, number, entry, blob)?)
            }
        };
        Ok(Opened::Cached(self.new_handle(handle)))
    }

    /// A handle for an open, which stays until [`Core::release`].
    fn new_handle(&self, handle: Handle) -> u64 {
        let fh = self.next_handle.fetch_add(1, Ordering::AcqRel);
        let _ = locked(&self.handles).insert(fh, Arc::new(handle));
        fh
    }

    /// The cache file of a blob, filled if the cache does not hold it; why not, in the words of
    /// the fill socket's answer.
    fn cache_file(
        &self,
        (digest, size, source): (&TreeDigest, u64, Option<&BlobSource>),
    ) -> Result<File, String> {
        let sha = digest.to_string();
        let path = blob_path(&self.settings.cache_path, &sha);
        if let Ok(file) = File::open(&path) {
            return Ok(file);
        }
        let filled = match source {
            Some(source) => self.fills.fill_blob(&sha, size, &source.key, source.offset),
            None => self.fills.fill_blob(&sha, size, &format!("cas/l/{sha}"), 0),
        };
        filled.and_then(|()| File::open(&path).map_err(|_| "io".to_owned()))
    }

    /// Records that the blob of the entry `index` of the tree `number` could not be had.
    fn failed(
        &self,
        (loaded, number): (&Loaded, u64),
        index: u32,
        digest: &TreeDigest,
        reason: String,
    ) {
        let failure = Failure {
            path: path_of(&loaded.tree, index),
            digest: digest.to_string(),
            reason,
        };
        locked(&self.state).attachments.fail(number, &failure);
    }

    /// Asks the cache to keep the blob for `loaded`, the first time it is opened under it.
    fn pin_once(&self, loaded: &Loaded, digest: &TreeDigest) {
        if locked(&loaded.pinned).contains(digest) {
            return;
        }
        let sha = digest.to_string();
        self.fills.touch(&[&sha]);
        let pinned = self
            .fills
            .pin(&self.settings.mount_id, &loaded.digest.to_string(), &[&sha]);
        if pinned.is_ok_and(|answer| answer.starts_with("ok")) {
            let _ = locked(&loaded.pinned).insert(*digest);
        }
    }

    /// The cache file of a blob, filled if it must be; the blob is pinned for the tree the first
    /// time it is opened under it.
    fn blob(
        &self,
        loaded: &Loaded,
        number: u64,
        entry: &TreeEntry,
        blob: (&TreeDigest, u64, Option<&BlobSource>),
    ) -> Result<File, Errno> {
        match self.cache_file(blob) {
            Ok(file) => {
                self.pin_once(loaded, blob.0);
                Ok(file)
            }
            Err(reason) => {
                self.failed((loaded, number), entry.index, blob.0, reason);
                Err(Errno::Io)
            }
        }
    }

    /// Reads up to `size` bytes at `offset` of the open file `fh`; fewer at the end of the file.
    ///
    /// # Errors
    /// [`Errno::Inval`] for a handle that is not open, [`Errno::Io`] for a read that fails.
    pub(super) fn read(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        #[cfg(test)]
        self.note("read");
        let handle = locked(&self.handles)
            .get(&fh)
            .cloned()
            .ok_or(Errno::Inval)?;
        let file = match handle.as_ref() {
            Handle::Cached(file) | Handle::Spilled(file, _) => file,
            Handle::Empty => return Ok(Vec::new()),
            // The kernel reads a passthrough file itself: a request for one is a fault.
            Handle::Backed(_) => return Err(Errno::Io),
        };
        read_file(file, offset, size as usize)
    }

    /// Closes the open file `fh`.
    pub(super) fn release(&self, fh: u64) {
        let handle = locked(&self.handles).remove(&fh);
        if let (Some(Handle::Backed(blob) | Handle::Spilled(_, blob)), Some(table)) =
            (handle.as_deref(), self.backing.get())
        {
            table.release(blob);
        }
    }

    /// The number of files open now.
    #[cfg(test)]
    pub(super) fn open_files(&self) -> usize {
        locked(&self.handles).len()
    }

    /// The trees in memory.
    #[cfg(test)]
    pub(super) fn trees_in_memory(&self) -> usize {
        locked(&self.state).loaded.len()
    }
}

/// The bytes of the manifest file at `path`, at most [`MAX_MANIFEST_BYTES`] and one more byte, so
/// that a larger file is refused by its size and not read whole.
fn read_manifest(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    let _ = File::open(path)?
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The path of the entry `index` in its tree, from the root: `/a/b/c`.
fn path_of(tree: &Tree, index: u32) -> String {
    let mut names = Vec::new();
    let mut at = tree.entry(index);
    while let Some(entry) = at {
        if entry.parent.is_some() {
            names.push(entry.name.to_string());
        }
        at = entry.parent.and_then(|parent| tree.entry(parent));
    }
    names.reverse();
    format!("/{}", names.join("/"))
}

mod passthrough;
mod prefetch;
mod read;

pub(super) use read::read_file;

#[cfg(test)]
#[path = "core_tests.rs"]
mod tests;
