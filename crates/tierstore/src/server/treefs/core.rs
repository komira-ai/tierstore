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
//! The namespace is the mount's root (`ino::ROOT`), `trees/` (`ino::TREES`) and the directories
//! above the fs locations of the node's placements (`core/placed.rs`); below `trees/`
//! are the roots of the trees that some run has attached, and a lookup of any other digest reads
//! that tree and attaches it with no run (`core/implicit.rs`), until it has been idle for
//! `fs.tree_idle_secs`. Every inode number of a tree is derivable (`ino.rs`), so a tree that is
//! not in memory is read again, from the table of tree numbers and the manifests, whenever a
//! request names one of its inodes. The trees of the node's placements appear at their fs
//! locations, below the root, and are read from the node's tree store (`core/placed.rs`).

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", test))]
use std::sync::OnceLock;
#[cfg(any(target_os = "linux", test))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, mpsc};
use std::time::Duration;

use crate::Clock;

use crate::server::local_cache::blob_path;
use crate::tree_manifest::{BlobSource, EntryKind, TreeDigest, TreeEntry};

use super::attach::{Attachments, Failure, Snapshot, is_run};
#[cfg(any(target_os = "linux", test))]
use super::backing::{Adopted, Guard};
use super::fill_client::FillClient;
#[cfg(any(target_os = "linux", test))]
use super::handle;
use super::ino;
use super::locked;
#[cfg(target_os = "linux")]
use super::put::space::PutSpace;
use super::table::{TableError, TreeTable};
use super::tree::Tree;
#[cfg(any(target_os = "linux", test))]
use handles::Handles;
use loaded::{Loaded, Trees};
use memory::Reads;
use pins::PinLocks;

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
    #[cfg(any(target_os = "linux", test))]
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

/// An open file.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug)]
enum Handle {
    /// A file of no bytes: nothing to read.
    Empty,
    /// The cache file of a blob, or the span of a pack that a file of a placed tree is.
    Cached(FileBytes),
    /// A blob the kernel reads itself, from the backing id an earlier release registered on this
    /// connection (`backing.rs`): reads never reach this process.
    Adopted,
}

/// What an open is answered with.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug)]
pub(super) enum Opened {
    /// A cached open: the page cache is kept, and every read it misses reaches [`Core::read`].
    Cached(u64),
    /// An open of a blob that has a backing id from an earlier release (`backing.rs`): answered
    /// with that id.
    Adopted {
        /// The handle of the open.
        fh: u64,
        /// The frontend's handle on the id.
        id: Guard,
    },
}

/// What the state lock guards: the attachments. A request that names a tree in memory does not
/// take it (`core/loaded.rs`).
#[derive(Debug)]
struct State {
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
    /// A successor is taking over (`Core::hold`): it keeps the table of tree numbers and the
    /// attachments file from now on.
    #[error("this instance is being replaced by its successor: ask its control socket")]
    Replaced,
}

/// The settings of a core that are not the other parts it is made of.
#[derive(Clone)]
pub(super) struct Settings {
    pub owner: (u32, u32),
    pub cache_path: PathBuf,
    pub mountpoint: PathBuf,
    /// The id of this mount, which the pins are made under.
    pub mount_id: String,
    /// How many processes have served this connection, this one included (`init_record`): in the
    /// numbers of the file handles this core gives out.
    #[cfg(any(target_os = "linux", test))]
    pub generation: u32,
    /// Whether the lookups of a tree's inodes are counted and a detached tree stays in memory
    /// until they are forgotten (a frontend that is told of forgets). If not, a tree is dropped
    /// when it is detached.
    pub count_lookups: bool,
    /// How long a tree a lookup attached stays attached with no request naming it
    /// (`fs.tree_idle_secs`).
    pub tree_idle: Duration,
    /// The clock the use of a tree is told by, in milliseconds: only its differences count.
    pub clock: Arc<dyn Clock>,
}

/// The core.
pub(in crate::server) struct Core {
    settings: Settings,
    fills: FillClient,
    table: TreeTable,
    /// The attachments, for short looks and changes: never held across a write of the attachments
    /// file or a round trip on the fill socket.
    state: Mutex<State>,
    /// The trees in memory, read with no lock (`core/loaded.rs`).
    trees: Trees,
    /// The path of each directory above the fs locations that a lookup found, by inode number.
    dirs: Mutex<HashMap<u64, String>>,
    /// Held by an attach, a restore and a detach for the work that changes the table of tree
    /// numbers or the attachments (and their file), and by a lookup that attaches a tree: so that
    /// `Core::hold` waits for that work and none starts after it, and two of them never write the
    /// attachments file at once. Not held across a round trip on the fill socket, nor while a
    /// manifest is read, but by the restore at a start, before any request is served.
    lifecycle: Mutex<()>,
    /// The locks of the pins and unpins of the trees (`core/pins.rs`): a pin is never made after the
    /// unpin of a tree it saw attached, and a tree attached again is not unpinned.
    pins: PinLocks,
    /// Read by a detach from its decision under the lifecycle lock to the end of its unpins,
    /// which it makes after it released that lock, and written by `Core::hold`: so a hold waits
    /// for the unpins of the detaches already decided, and no unpin of this process follows the
    /// pins of a successor. Only the threads that detach and hold take it.
    freeing: RwLock<()>,
    /// Whether a successor is taking over: no attach or detach is done, so that this process
    /// writes neither the table of tree numbers nor the attachments file (`Core::hold`).
    held: AtomicBool,
    /// Whether the fill socket is still to be told the mount's pins and placements, or is being
    /// told them (`core/live.rs`).
    live: live::Announcement,
    /// The trees a thread is filling now for an attach that asked for `prefetch = "all"`, each
    /// with the token of its thread: a detach removes the entry, and a thread whose token is no
    /// longer the tree's has been replaced and stops.
    prefetching: Mutex<HashMap<u64, Arc<()>>>,
    /// The reads of trees that run now, which every request for the same tree waits for
    /// (`core/memory.rs`).
    reads: Reads,
    /// The end of the channel the thread that detaches idle trees waits on: dropped with the
    /// core, which ends the thread (`core/implicit.rs`).
    sweeper: Mutex<Option<mpsc::Sender<()>>>,
    /// The backing ids an earlier release left on the connection, where this process took over
    /// one that has them (`backing.rs`). Set once, before the first request; read with no lock.
    #[cfg(any(target_os = "linux", test))]
    adopted: OnceLock<Adopted>,
    /// The put directory the frontend serves as `put/`, where `fs.put` is enabled.
    #[cfg(target_os = "linux")]
    put: OnceLock<Arc<PutSpace>>,
    /// The open files, in maps split by handle (`core/handles.rs`).
    #[cfg(any(target_os = "linux", test))]
    handles: Handles,
    #[cfg(any(target_os = "linux", test))]
    next_handle: AtomicU64,
    /// The requests served, by name, for the tests that count what reaches the core.
    #[cfg(test)]
    pub(super) served: Mutex<Vec<&'static str>>,
    /// The times the state lock was taken.
    #[cfg(test)]
    state_locks: AtomicU64,
    /// What the tests run inside a write of the attachments file, and between the mark of a tree
    /// as removed and its removal from the map.
    #[cfg(test)]
    pub(super) hooks: Hooks,
}

/// What a test runs at a point of the core's work, with the core.
#[cfg(test)]
type Hook = Box<dyn Fn(&Core) + Send + Sync>;

/// The points of the core's work where a test may run something.
#[cfg(test)]
#[derive(Default)]
pub(super) struct Hooks {
    /// Inside a write of the attachments file, before the file is written.
    pub(super) saving: Mutex<Option<Hook>>,
    /// After a tree is marked as removed and before it is taken out of the map.
    pub(super) marked: Mutex<Option<Hook>>,
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
                attachments: Attachments::new(runtime_dir),
            }),
            trees: Trees::default(),
            dirs: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
            pins: PinLocks::new(),
            freeing: RwLock::new(()),
            held: AtomicBool::new(false),
            live: live::Announcement::default(),
            prefetching: Mutex::new(HashMap::new()),
            reads: Reads::default(),
            sweeper: Mutex::new(None),
            #[cfg(any(target_os = "linux", test))]
            adopted: OnceLock::new(),
            #[cfg(target_os = "linux")]
            put: OnceLock::new(),
            #[cfg(any(target_os = "linux", test))]
            handles: Handles::new(),
            #[cfg(any(target_os = "linux", test))]
            next_handle: AtomicU64::new(1),
            #[cfg(test)]
            served: Mutex::new(Vec::new()),
            #[cfg(test)]
            state_locks: AtomicU64::new(0),
            #[cfg(test)]
            hooks: Hooks::default(),
        }
    }

    /// The state, locked.
    fn state(&self) -> MutexGuard<'_, State> {
        #[cfg(test)]
        let _ = self.state_locks.fetch_add(1, Ordering::AcqRel);
        locked(&self.state)
    }

    /// Writes the attachments file from `snapshot`, which the caller took under the state lock and
    /// released it for. The caller holds the lifecycle lock, so no two writes of the file run at
    /// once.
    fn write_attachments(&self, snapshot: &Snapshot) -> io::Result<()> {
        #[cfg(test)]
        if let Some(hook) = locked(&self.hooks.saving).as_ref() {
            hook(self);
        }
        snapshot.write()
    }

    /// Serves `space` as `put/` from now on (once).
    #[cfg(target_os = "linux")]
    pub(super) fn serve_put(&self, space: Arc<PutSpace>) {
        let _ = self.put.set(space);
    }

    /// The put directory, where it is served.
    #[cfg(target_os = "linux")]
    pub(super) fn put(&self) -> Option<&Arc<PutSpace>> {
        self.put.get()
    }

    /// The settings of the core.
    #[cfg(test)]
    pub(super) fn settings(&self) -> &Settings {
        &self.settings
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
            // A placed tree's modes are served as stored.
            perm: tree.mode(entry.index).unwrap_or(perm),
            nlink,
            uid: self.settings.owner.0,
            gid: self.settings.owner.1,
        }
    }

    /// The time on the core's clock.
    fn now(&self) -> u64 {
        self.settings.clock.now()
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
        let digest = TreeDigest::from_hex(digest)
            .filter(|digest| placed::placement_of(digest).is_none())
            .ok_or(AttachError::Digest)?;
        if !is_run(run) {
            return Err(AttachError::Run);
        }
        if self.is_held() {
            return Err(AttachError::Replaced);
        }
        // Read before the lifecycle lock, as a lookup that attaches a tree reads it: a slow fill
        // holds up no attach, no detach and no lookup.
        let tree = self
            .read_ahead(&digest, &fetched::Fetched::default())
            .map_err(|why| AttachError::Tree(why.to_string()))?;
        let _lifecycle = locked(&self.lifecycle);
        if self.is_held() {
            return Err(AttachError::Replaced);
        }
        let number = self.table.number_of(&digest)?;
        if let Some(tree) = tree {
            let _ = self.keep(number, digest, tree);
            let _ = self.read_done(number);
        }
        let (added, snapshot) = {
            let mut state = self.state();
            let added = state.attachments.add(run, number, digest);
            (added, state.attachments.snapshot())
        };
        if let Err(error) = self.write_attachments(&snapshot) {
            if added {
                self.state().attachments.remove_one(run, number);
            }
            return Err(error.into());
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

    /// The tree `number` in memory, read if it is not.
    fn load_unless_loaded(
        &self,
        number: u64,
        digest: TreeDigest,
    ) -> Result<Arc<Loaded>, AttachError> {
        if let Some(loaded) = self.trees.get(number) {
            return Ok(loaded);
        }
        self.load(number, digest)
            .map_err(|why| AttachError::Tree(why.to_string()))
    }

    /// Ends every attachment of `run`: the failures its attachments recorded, or none if it
    /// attached nothing. A tree no run has now and no lookup attached is unpinned, with no lock of
    /// the core's held but that of its pins (`Core::free_tree`), and left memory when the kernel
    /// forgets its inodes.
    pub(super) fn detach(&self, run: &str) -> Option<Vec<Failure>> {
        let (freed, failures, _freeing) = {
            let _lifecycle = locked(&self.lifecycle);
            if self.is_held() {
                return None;
            }
            let freeing = self.freeing();
            let (freed, failures, snapshot) = {
                let mut state = self.state();
                let trees = state.attachments.remove(run)?;
                let mut failures = Vec::new();
                let mut freed = Vec::new();
                for (number, digest, mut failed) in trees {
                    failures.append(&mut failed);
                    if !state.attachments.is_attached(number) {
                        freed.push((number, digest));
                    }
                }
                (freed, failures, state.attachments.snapshot())
            };
            let _ = self.write_attachments(&snapshot);
            for (number, _) in &freed {
                let _ = locked(&self.prefetching).remove(number);
            }
            (freed, failures, freeing)
        };
        for (number, digest) in freed {
            self.free_tree(number, &digest);
        }
        Some(failures)
    }

    /// Unpins the tree `number`, `digest`, which a detach has found that nothing attaches, and
    /// takes it out of memory if the kernel holds none of its inodes. The caller has released
    /// the lifecycle lock: the round trip to the cache holds only the lock of the tree's pins
    /// (`core/pins.rs`), and a tree that was attached again since the caller decided is left as
    /// it is, pinned.
    pub(super) fn free_tree(&self, number: u64, digest: &TreeDigest) {
        {
            let _unpinning = self.pins.unpin(digest);
            if self.attached(number) {
                return;
            }
            if let Some(loaded) = self.trees.get(number) {
                loaded.pinned.clear();
            }
            self.let_go(digest);
        }
        self.drop_if_forgotten(number);
    }

    /// Stops this process from changing the table of tree numbers and the attachments file
    /// (`on`), or lets it do so again. A successor that takes over reads both once it has taken
    /// the sockets, and this process must have written its last by then: the call waits for the
    /// attach or detach that is changing them, and for the unpins of the detaches decided
    /// before it, and every later one is refused (`AttachError::Replaced`, and, for a detach, an
    /// answer of none; ask `Core::is_held`). A hold also waits, with the lifecycle lock released,
    /// for the announcement of the mount's pins that is being sent, and none is sent after it
    /// (`core/live.rs`).
    pub(super) fn hold(&self, on: bool) {
        {
            let _lifecycle = locked(&self.lifecycle);
            self.held.store(on, Ordering::SeqCst);
            // The unpins of the detaches decided before, which run with the lifecycle lock
            // released.
            drop(self.freeing.write().unwrap_or_else(PoisonError::into_inner));
        }
        self.live.wait_sent();
    }

    /// Marks a detach whose unpins are to come, until the guard is dropped (`Core::hold`).
    pub(super) fn freeing(&self) -> RwLockReadGuard<'_, ()> {
        self.freeing.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether [`Core::hold`] is on.
    pub(super) fn is_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    /// Ends the attachments of every run that is not in `keep`; the number of runs.
    pub(super) fn detach_all_except(&self, keep: &[String]) -> usize {
        let runs = self.state().attachments.runs();
        runs.iter()
            .filter(|run| !keep.contains(run))
            .filter(|run| self.detach(run).is_some())
            .count()
    }

    /// Each attachment, as a tree digest and a run.
    pub(super) fn attachments(&self) -> Vec<(TreeDigest, String)> {
        self.state().attachments.listing()
    }

    /// Looks up `name` in the directory `parent`. A digest in `trees/` that no run has attached is
    /// attached by the lookup (`Core::resolve`); a name in the root or in a directory above the fs
    /// locations is asked of the node's tree store over the fill socket (`Core::lookup_placed`).
    ///
    /// # Errors
    /// [`Errno::NoEnt`] for a name the namespace does not have (a digest of a tree the upstream
    /// does not have, a path that is no location and above none), [`Errno::NotDir`] for a parent
    /// that is not a directory, [`Errno::Io`] for a tree that cannot be read or a fill socket that
    /// does not answer.
    pub(super) fn lookup(&self, parent: u64, name: &str) -> Result<Looked, Errno> {
        let (number, index) = ino::split(parent);
        let namespace = number == 0 || ino::is_placed_dir(parent);
        #[cfg(test)]
        self.note(if namespace { "namespace" } else { "lookup" });
        if namespace {
            return match (parent, name) {
                (ino::ROOT, "trees") => Ok(Looked::Found(self.namespace(ino::TREES))),
                (ino::TREES, digest) => {
                    let number = self.resolve(digest)?;
                    let loaded = self.counted(number)?;
                    let root = self.root_entry(number, &loaded, Duration::ZERO)?;
                    Ok(Looked::Found(root))
                }
                (ino::ROOT, _) => self.lookup_placed(parent, name),
                (dir, _) if ino::is_placed_dir(dir) => self.lookup_placed(dir, name),
                _ => Err(Errno::NoEnt),
            };
        }
        let loaded = self.tree(number)?;
        self.looked_into(&loaded);
        let dir = loaded.tree.entry(index).ok_or(Errno::NoEnt)?;
        if !matches!(dir.kind, EntryKind::Dir { .. }) {
            return Err(Errno::NotDir);
        }
        let found = loaded.tree.child(index, name);
        let Some(child) = found.and_then(|child| loaded.tree.entry(child)) else {
            return Ok(Looked::Absent(TREE_TTL));
        };
        self.count_lookup(number, &loaded);
        let attr = self.attr_of(number, &loaded.tree, &child);
        Ok(Looked::Found(Entry {
            attr,
            ttl: TREE_TTL,
        }))
    }

    /// The root directory of the tree `number`, with the timeout `ttl`.
    fn root_entry(&self, number: u64, loaded: &Loaded, ttl: Duration) -> Result<Entry, Errno> {
        let root = loaded.tree.entry(0).ok_or(Errno::NoEnt)?;
        Ok(Entry {
            attr: self.attr_of(number, &loaded.tree, &root),
            ttl,
        })
    }

    /// The attributes of `ino`. Every number of the range of the directories above the fs
    /// locations is such a directory: they are all alike, so none is looked up.
    ///
    /// # Errors
    /// [`Errno::NoEnt`] for an inode no tree and no namespace has.
    pub(super) fn getattr(&self, ino: u64) -> Result<Entry, Errno> {
        #[cfg(test)]
        self.note("getattr");
        let (number, index) = ino::split(ino);
        if number == 0 || ino::is_placed_dir(ino) {
            return match ino {
                ino::ROOT | ino::TREES => Ok(self.namespace(ino)),
                dir if ino::is_placed_dir(dir) => Ok(self.namespace(dir)),
                _ => Err(Errno::NoEnt),
            };
        }
        let loaded = self.tree(number)?;
        let entry = loaded.tree.entry(index).ok_or(Errno::NoEnt)?;
        Ok(Entry {
            attr: self.attr_of(number, &loaded.tree, &entry),
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
        if number == 0 || ino::is_placed_dir(ino) {
            return Err(Errno::Inval);
        }
        let loaded = self.tree(number)?;
        match loaded.tree.entry(index).map(|entry| entry.kind) {
            Some(EntryKind::Symlink { target }) => Ok(target),
            Some(_) => Err(Errno::Inval),
            None => Err(Errno::NoEnt),
        }
    }

    /// Opens the directory `ino` for listing.
    ///
    /// # Errors
    /// [`Errno::Acces`] for the mount's root, `trees/` and the directories above the fs
    /// locations, which nobody may list.
    pub(super) fn opendir(&self, ino: u64) -> Result<(), Errno> {
        let (number, index) = ino::split(ino);
        if number == 0 || ino::is_placed_dir(ino) {
            return Err(Errno::Acces);
        }
        let loaded = self.tree(number)?;
        match loaded.tree.entry(index).map(|entry| entry.kind) {
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
    #[cfg(any(target_os = "linux", test))]
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
        self.looked_into(&loaded);
        let parent = loaded.tree.entry(index).and_then(|entry| entry.parent);
        let parent_ino = parent.map_or_else(
            || self.root_parent(&loaded.tree),
            |parent| ino::of(number, parent),
        );
        let kids = loaded.tree.children(index);
        let first = usize::try_from(offset).unwrap_or(usize::MAX);
        // `.` and `..` are at the positions 0 and 1, the children from 2 on.
        for (position, (name, at)) in [(".", ino), ("..", parent_ino)].into_iter().enumerate() {
            if position < first {
                continue;
            }
            let item = DirItem {
                name: name.to_owned(),
                entry: self.getattr(at)?,
            };
            if !sink(position as u64 + 1, item) {
                return Ok(());
            }
        }
        let entries = kids
            .from(first.saturating_sub(2))
            .filter_map(|kid| loaded.tree.entry(kid));
        for (at, entry) in entries.enumerate() {
            let position = first.max(2).saturating_add(at);
            let item = DirItem {
                name: entry.name.to_string(),
                entry: Entry {
                    attr: self.attr_of(number, &loaded.tree, &entry),
                    ttl: TREE_TTL,
                },
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
    /// Every open is a cached one, but that of a blob an earlier release registered a backing id
    /// for on this connection (`Core::adopt_backing`), which is answered with that id. No lock the
    /// opens of other files take is held across a fill or a pin.
    ///
    /// # Errors
    /// [`Errno::RoFs`] for an open for writing, [`Errno::IsDir`] for a directory, [`Errno::Inval`]
    /// for a link, [`Errno::Io`] for a blob that cannot be had.
    #[cfg(any(target_os = "linux", test))]
    pub(super) fn open(&self, ino: u64, writing: bool) -> Result<Opened, Errno> {
        #[cfg(test)]
        self.note("open");
        if writing {
            return Err(Errno::RoFs);
        }
        let (number, index) = ino::split(ino);
        if number == 0 || ino::is_placed_dir(ino) {
            return Err(Errno::IsDir);
        }
        let loaded = self.tree(number)?;
        let entry = loaded.tree.entry(index).ok_or(Errno::NoEnt)?;
        // A file of a placed tree is a span of a pack: never opened by passthrough.
        let adopted = self
            .adopted
            .get()
            .filter(|_| loaded.tree.location().is_none());
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
                if let Some(id) = adopted.and_then(|adopted| adopted.get(digest)) {
                    self.pin_once((&loaded, number), digest);
                    let fh = self.new_handle(Handle::Adopted);
                    let id = Arc::clone(id);
                    return Ok(Opened::Adopted { fh, id });
                }
                let blob = (digest, *size, source.as_ref());
                Handle::Cached(self.blob(&loaded, number, &entry, blob)?)
            }
        };
        Ok(Opened::Cached(self.new_handle(handle)))
    }

    /// A handle for an open, which stays until [`Core::release`].
    #[cfg(any(target_os = "linux", test))]
    fn new_handle(&self, handle: Handle) -> u64 {
        let count = self.next_handle.fetch_add(1, Ordering::AcqRel);
        let fh = handle::make(self.settings.generation, count);
        self.handles.insert(fh, Arc::new(handle));
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
        self.state().attachments.fail(number, &failure);
    }

    /// Asks the cache to keep the blob for `loaded`, the tree `number`, the first time it is opened
    /// under it while the tree is attached: a tree no run and no lookup has attached is kept by
    /// recency alone, since nothing would take its pins away. A blob pinned already is seen without
    /// any lock the core shares; the pins of different opens are asked at once. The set of
    /// pinned blobs and the state are never locked together.
    fn pin_once(&self, (loaded, number): (&Loaded, u64), digest: &TreeDigest) {
        if loaded.pinned.contains(digest) {
            return;
        }
        let _pinning = self.pins.pin(&loaded.digest);
        if loaded.pinned.contains(digest) {
            return;
        }
        if !self.attached(number) {
            return;
        }
        let sha = digest.to_string();
        self.fills.touch(&[&sha]);
        let pinned = self.settled().and_then(|()| {
            self.fills
                .pin(&self.settings.mount_id, &loaded.digest.to_string(), &[&sha])
        });
        if pinned.is_ok_and(|answer| answer.starts_with("ok")) {
            loaded.pinned.insert(*digest);
        }
    }

    /// The bytes of a file: the cache file of its blob, filled if it must be, which is pinned for
    /// the tree the first time it is opened under it (`Core::pin_once`); for a placed tree, the
    /// span of the pack, which the tree store pins.
    fn blob(
        &self,
        loaded: &Loaded,
        number: u64,
        entry: &TreeEntry,
        blob: (&TreeDigest, u64, Option<&BlobSource>),
    ) -> Result<FileBytes, Errno> {
        let got = if loaded.tree.location().is_some() {
            self.span_file(blob)
        } else {
            self.cache_file(blob).map(|file| {
                self.pin_once((loaded, number), blob.0);
                FileBytes::whole(file, blob.1)
            })
        };
        match got {
            Ok(bytes) => Ok(bytes),
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
    #[cfg(any(target_os = "linux", test))]
    pub(super) fn read(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        #[cfg(test)]
        self.note("read");
        let handle = self.handles.get(fh).ok_or(Errno::Inval)?;
        let bytes = match handle.as_ref() {
            Handle::Cached(bytes) => bytes,
            Handle::Empty => return Ok(Vec::new()),
            // The kernel reads a passthrough file itself: a request for one is a fault.
            Handle::Adopted => return Err(Errno::Io),
        };
        bytes.read(offset, size as usize)
    }

    /// Closes the open file `fh`.
    #[cfg(any(target_os = "linux", test))]
    pub(super) fn release(&self, fh: u64) {
        self.handles.remove(fh);
    }

    /// The number of files open now.
    #[cfg(test)]
    pub(super) fn open_files(&self) -> usize {
        self.handles.len()
    }

    /// The trees in memory.
    #[cfg(test)]
    pub(super) fn trees_in_memory(&self) -> usize {
        self.trees.len()
    }
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

#[cfg(any(target_os = "linux", test))]
mod adopt;
mod fetched;
#[cfg(any(target_os = "linux", test))]
mod handles;
mod implicit;
mod live;
mod loaded;
mod memory;
mod pins;
mod placed;
mod prefetch;
mod read;
mod restore;

pub(super) use implicit::look_every;
pub(super) use read::FileBytes;

#[cfg(test)]
#[path = "core_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "core/lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
#[path = "core_whiteout_names_tests.rs"]
mod whiteout_names_tests;
