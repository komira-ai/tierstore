//! The backing table: for each blob that some open file is served from by kernel passthrough, the
//! backing id the kernel was given for it and how many opens use it (`docs/design/virtual-build-fs.md`
//! sections 6.4 and 7.1).
//!
//! The kernel binds an inode with an open passthrough file to one backing file for as long as any
//! open of it lives, and fails a second open that comes with another id. So a blob has one id for
//! as long as any open of it lives, in whatever tree and under whatever name, and the table is
//! where that is decided: an open finds the live id and counts itself on it, or is told to register
//! one (the first open), and every other open of that blob waits until it has. The last release
//! drops the id, which closes it.
//!
//! The table is written to `<runtime dir>/backing`, one line `<sha256> <id> <opens>` for each live
//! blob, by a rename, at each first open and each last release and without `fsync`: it has to
//! outlive the process and not the machine. A successor that takes over the connection needs it
//! before its first open (an inode open across the hand-over would otherwise get a new id and the
//! open would fail with `EIO`; linux E6, case 3b). This release only writes it: the connection ends
//! with the process, so a start makes a new, empty one. The `opens` of a line is the count at that
//! write.
//!
//! The ids are limited to `fs.max_backing_ids`. An open of a blob with no live id, while that many
//! ids are live or being registered, does not wait for a release (the release is a request that
//! needs a free worker, and every worker might be parked in such an open): it is a cached open,
//! counted in `spilled`. While any such open of the blob lives, every other open of it is cached
//! too, so that no inode has a passthrough open and a cached one at once.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use crate::tree_manifest::TreeDigest;

use super::locked;

/// The name of the file in the runtime directory.
pub(super) const FILE: &str = "backing";

/// What closes the backing id when the last reference to it is dropped.
pub(super) type Guard = Arc<dyn Any + Send + Sync>;

/// A backing id the kernel was given, as an open is answered with it.
#[derive(Clone)]
pub(super) struct Backing {
    /// The number the kernel gave.
    pub id: u32,
    guard: Guard,
}

impl std::fmt::Debug for Backing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backing").field("id", &self.id).finish()
    }
}

impl Backing {
    /// What closes the id, if it is a `T`: the frontend gives it a type when it registers the id.
    pub(super) fn guard<T: Any>(&self) -> Option<&T> {
        self.guard.downcast_ref()
    }
}

/// What an open calls to register the id of a cache file with the kernel.
pub(super) type Registrar<'a> = dyn FnMut(&File) -> io::Result<Registered> + 'a;

/// A backing id just registered.
pub(super) struct Registered {
    /// The number the kernel gave.
    pub id: u32,
    /// Closes the id when dropped.
    pub guard: Guard,
}

/// A blob with a live id.
struct Live {
    id: u32,
    guard: Guard,
    opens: u32,
}

#[derive(Default)]
struct Inner {
    live: HashMap<TreeDigest, Live>,
    /// The blobs whose id is being registered by an open that was told to.
    registering: HashSet<TreeDigest>,
    /// The cached opens of blobs that had no id when the limit was reached, by blob.
    spilled: HashMap<TreeDigest, u32>,
    /// The opens that wait for a registration.
    waiting: usize,
}

/// The table.
pub(super) struct BackingTable {
    file: PathBuf,
    most: usize,
    inner: Mutex<Inner>,
    changed: Condvar,
}

/// What an open of a blob is told to do.
pub(super) enum Slot {
    /// The blob has a live id, and this open is counted on it.
    Live(Backing),
    /// The blob has none: register one, and give it to [`Ticket::registered`].
    Register(Ticket),
    /// The blob has none and the limit is reached (or cached opens of it live): open it cached.
    /// This open is counted, and ended by [`BackingTable::release`] like a passthrough one.
    Spill,
}

/// The right to register the id of one blob, held until it is registered; dropped, it lets the
/// opens that wait for the blob go on.
pub(super) struct Ticket {
    table: Arc<BackingTable>,
    blob: TreeDigest,
}

impl BackingTable {
    /// An empty table, at most `most` ids, kept in `file`, which is written empty now.
    pub(super) fn new(file: PathBuf, most: usize) -> Arc<Self> {
        let table = Arc::new(Self {
            file,
            most,
            inner: Mutex::new(Inner::default()),
            changed: Condvar::new(),
        });
        table.write(&locked(&table.inner));
        table
    }

    /// Counts an open of `blob`: on its live id, or, if it has none and there is room for one, by
    /// the right to register it; as a cached open if there is no room. Waits only for another
    /// open's registration of the same blob, which ends with that open's own fill.
    pub(super) fn open(self: &Arc<Self>, blob: &TreeDigest) -> Slot {
        let mut inner = locked(&self.inner);
        loop {
            if let Some(live) = inner.live.get_mut(blob) {
                live.opens += 1;
                return Slot::Live(Backing {
                    id: live.id,
                    guard: Arc::clone(&live.guard),
                });
            }
            if let Some(count) = inner.spilled.get_mut(blob) {
                *count += 1;
                return Slot::Spill;
            }
            if !inner.registering.contains(blob) {
                if inner.live.len() + inner.registering.len() >= self.most {
                    let _ = inner.spilled.insert(*blob, 1);
                    return Slot::Spill;
                }
                let _ = inner.registering.insert(*blob);
                return Slot::Register(Ticket {
                    table: Arc::clone(self),
                    blob: *blob,
                });
            }
            inner.waiting += 1;
            inner = self
                .changed
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
            inner.waiting -= 1;
        }
    }

    /// Ends one open of `blob`; the last one drops its id.
    pub(super) fn release(&self, blob: &TreeDigest) {
        let mut inner = locked(&self.inner);
        if let Some(count) = inner.spilled.get_mut(blob) {
            *count -= 1;
            if *count == 0 {
                let _ = inner.spilled.remove(blob);
            }
            return;
        }
        let Some(live) = inner.live.get_mut(blob) else {
            return;
        };
        live.opens -= 1;
        if live.opens == 0 {
            // Dropping the guard closes the id.
            let _ = inner.live.remove(blob);
            self.write(&inner);
        }
    }

    /// Writes the file; a file that cannot be written is said and the table goes on.
    fn write(&self, inner: &Inner) {
        let mut lines: Vec<(String, u32, u32)> = inner
            .live
            .iter()
            .map(|(blob, live)| (blob.to_string(), live.id, live.opens))
            .collect();
        lines.sort();
        let mut text = String::new();
        for (blob, id, opens) in lines {
            let _ = writeln!(text, "{blob} {id} {opens}");
        }
        let mut new = self.file.clone().into_os_string();
        new.push(".new");
        let written = std::fs::write(&new, text).and_then(|()| std::fs::rename(&new, &self.file));
        if let Err(why) = written {
            let file = self.file.display();
            tracing::warn!("tierstore fs: cannot write the backing table {file}: {why}");
        }
    }

    /// The blobs with a live id.
    #[cfg(test)]
    pub(super) fn live(&self) -> usize {
        locked(&self.inner).live.len()
    }

    /// The cached opens of `blob` that the limit caused.
    #[cfg(test)]
    pub(super) fn spilled(&self, blob: &TreeDigest) -> u32 {
        locked(&self.inner).spilled.get(blob).copied().unwrap_or(0)
    }

    /// The opens that wait now.
    #[cfg(test)]
    pub(super) fn waiting(&self) -> usize {
        locked(&self.inner).waiting
    }

    /// The opens counted on `blob`.
    #[cfg(test)]
    pub(super) fn opens(&self, blob: &TreeDigest) -> u32 {
        locked(&self.inner)
            .live
            .get(blob)
            .map_or(0, |live| live.opens)
    }
}

impl Ticket {
    /// Records the id that was registered for the blob, counting this open on it.
    pub(super) fn registered(self, registered: Registered) -> Backing {
        let backing = Backing {
            id: registered.id,
            guard: Arc::clone(&registered.guard),
        };
        let mut inner = locked(&self.table.inner);
        let _ = inner.registering.remove(&self.blob);
        let _ = inner.live.insert(
            self.blob,
            Live {
                id: registered.id,
                guard: registered.guard,
                opens: 1,
            },
        );
        self.table.write(&inner);
        backing
    }
}

impl Drop for Ticket {
    /// Lets the opens that wait for the blob go on.
    fn drop(&mut self) {
        let _ = locked(&self.table.inner).registering.remove(&self.blob);
        self.table.changed.notify_all();
    }
}

#[cfg(test)]
#[path = "backing_tests.rs"]
mod tests;
