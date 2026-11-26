//! Placements at fs locations (`docs/design/node-local-trees.md` section 6.2): the tree of a
//! placement made by the node's trees API appears at `<mount>/<path>` of its fs location, and is
//! read with no manifest.
//!
//! A lookup of a name in the mount's root, or in a directory above a location, asks the node's
//! tree store over the fill socket where the path is (`place`). A path that is a location is the
//! root of its placement's tree: the tree store holds the placement for this mount, and the core
//! reads the tree whole (`image`, or `flat` from a node of a release before the images), numbers
//! it under the placement's key in the table of tree numbers
//! and attaches it as a lookup attaches a tree of `trees/`: implicitly, until it is idle for
//! `fs.tree_idle_secs`, when the hold of the tree store is given back (`unplace`). A path that a
//! location is below is a directory the mount passes through, of mode 0711 as the root is, which
//! nobody lists. Anything else is `ENOENT`. Every answer of the namespace is given with a timeout
//! of zero, so a placement that is released stops resolving at the next lookup.
//!
//! A tree is numbered under its placement and location, not its digest: one tree placed at two
//! locations, by two placements or by one, is two trees of the mount, so no directory has two
//! paths. The key is the placement's sixteen bytes of id, the eight bytes [`PLACED`], and the first
//! eight bytes of the SHA-256 of the location's path; a digest of a tree of manifests has those
//! eight bytes in that place with a chance of one in 2^64. A process that reads the tree again
//! finds the location among the placement's by the hash. A directory above the locations is numbered by the hash of its path
//! (`ino::placed_dir`); a process that took the mount over and does not know the path of such a
//! number asks the tree store for the paths of the locations (`mounted`) and finds it there.
//!
//! A file of a placed tree is a span of a cached pack: an open asks the fill socket for the span
//! (`span`), which fills it if the cache does not hold it, and reads the pack's own file at the
//! span's offset. It is never answered with a backing id an earlier release registered
//! (`backing.rs`), since the kernel would map the whole pack onto the file, and the core pins nothing for it: the tree store pins the packs of its trees.

use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use crate::server::local_cache::trees::{FlatKind, FlatTree, Location, image_path};
use crate::tree_manifest::{
    BlobSource, EntryKind, MAX_MANIFEST_BYTES, TreeDigest, TreeEntry, TreeError, flatten_in_waves,
};

use super::super::fill_client::{AskError, Place, PlacedTree, Until};
use super::super::fills::Report;
use super::super::ino;
use super::super::tree::Tree;
use super::fetched::{Fetched, TreeAsked};
use super::loaded::Loaded;
use super::said::Failing;
use super::{Core, Errno, Looked, locked};
use crate::server::local_cache::{FILL_SOCKET_CONCURRENCY, manifest_path};

/// The bytes 16 to 24 of the key a placement's tree at a location is numbered under; the first
/// sixteen are the placement's id, the last eight the start of the SHA-256 of the location's path.
const PLACED: &[u8; 8] = b"tsplaced";

/// The answer of the fill socket for an object the upstream does not have.
const ABSENT: &str = "upstream 404";

/// The longest one read of a tree's manifests may take, every manifest it fills through the fill
/// socket together. A job agent gives up on a lookup of `trees/<digest>` 60 s after it asked, and
/// that time includes the wait for one of the agent's few lookup threads; a lookup that needed
/// longer was never answered in time, and it kept the agent's thread in the kernel until it ended,
/// so the lookups behind it timed out too. Half of that bound is the read's, so the lookup is
/// answered, `EIO` at worst, while the agent still waits for it. It bounds every read of a tree's
/// manifests: a lookup's, an attach's, and a start's that makes the attachments again.
pub(in crate::server::treefs) const TREE_READ: Duration = Duration::from_secs(30);

/// The most entries a tree of the mount may have: what the index of an inode number counts.
const MOST_ENTRIES: usize = 1 << 24;

/// The most manifests of a tree one read fetches at once, each level of the tree before the next
/// (`tree_manifest/waves.rs`): as many fills as the node's fill socket runs at once by default, so
/// that a read of a tree whose manifests the cache does not hold costs a round trip for each level
/// of the tree and not one for each directory, where the node did not fill them first
/// (`fill-tree`, [`Core::read_tree`]).
const MANIFESTS_AT_ONCE: usize = FILL_SOCKET_CONCURRENCY;

/// The bytes of the manifest file at `path`, at most [`MAX_MANIFEST_BYTES`] and one more byte, so
/// that a larger file is refused by its size and not read whole.
pub(super) fn read_manifest(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    let _ = File::open(path)?
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Why a tree was not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server::treefs) enum Unread {
    /// There is no such tree: the upstream does not have the root manifest, or the placement has
    /// ended; why, in words.
    Gone(String),
    /// The tree cannot be read now: why.
    Failed(String),
}

impl std::fmt::Display for Unread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}

/// The start of the SHA-256 of `path`.
fn path_hash(path: &str) -> [u8; 8] {
    let digest = Sha256::digest(path.as_bytes());
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest[..8]);
    first
}

/// The key the tree of the placement `id` (thirty-two lower-case hex digits) at the fs location
/// `path` is numbered under.
pub(in crate::server::treefs) fn key_of(id: &str, path: &str) -> Option<TreeDigest> {
    let lower = id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let bytes = hex::decode(id).ok().filter(|b| b.len() == 16 && lower)?;
    let mut key = [0_u8; 32];
    key[..16].copy_from_slice(&bytes);
    key[16..24].copy_from_slice(PLACED);
    key[24..].copy_from_slice(&path_hash(path));
    Some(TreeDigest::from_bytes(key))
}

/// The id of the placement whose tree at a location `digest` is the key of, and the hash of the
/// location's path; none for a tree of manifests.
pub(in crate::server::treefs) fn placement_of(digest: &TreeDigest) -> Option<(String, [u8; 8])> {
    let bytes = digest.as_bytes();
    let mut hash = [0_u8; 8];
    hash.copy_from_slice(&bytes[24..]);
    (bytes[16..24] == PLACED[..]).then(|| (hex::encode(&bytes[..16]), hash))
}

/// The inode number of the directory `path` above the fs locations.
pub(in crate::server::treefs) fn dir_ino(path: &str) -> u64 {
    ino::placed_dir(u64::from_le_bytes(path_hash(path)))
}

/// The tree of the mount that `flat` is at its location `path`: its entries, as a tree of
/// manifests has them, and the stored modes. Refused unless the root comes first, every other
/// entry follows the directory that holds it, and every file names a pack of the list.
fn tree_of(flat: &FlatTree, path: String) -> Result<Tree, String> {
    let damaged = |why: &str| format!("the flattened tree at {path} is damaged: {why}");
    if flat.entries.len() > MOST_ENTRIES {
        return Err(damaged("too many entries"));
    }
    if flat.entries.is_empty() {
        return Err(damaged("no root"));
    }
    let packs: Vec<Arc<str>> = flat
        .packs
        .iter()
        .map(|pack| Arc::from(pack.as_str()))
        .collect();
    let mut entries: Vec<TreeEntry> = Vec::with_capacity(flat.entries.len());
    let mut modes = Vec::with_capacity(flat.entries.len());
    for (at, entry) in flat.entries.iter().enumerate() {
        // At most `MOST_ENTRIES`, which fits 32 bits.
        let index = at as u32;
        let holder = entry.parent.and_then(|parent| entries.get(parent as usize));
        let placed = match (index, holder) {
            (0, None) => entry.parent.is_none() && entry.kind == FlatKind::Dir,
            (_, Some(holder)) => matches!(holder.kind, EntryKind::Dir { .. }),
            _ => false,
        };
        if !placed {
            return Err(damaged("an entry is not below a directory before it"));
        }
        let kind = match &entry.kind {
            FlatKind::Dir => EntryKind::Dir {
                digest: TreeDigest::from_bytes([0; 32]),
                size: 0,
            },
            FlatKind::File {
                size,
                sha256,
                pack,
                offset,
            } => {
                let source = match pack {
                    Some(pack) => {
                        let key = packs.get(*pack as usize).ok_or_else(|| damaged("a pack"))?;
                        Some(BlobSource {
                            key: Arc::clone(key),
                            offset: *offset,
                        })
                    }
                    None => None,
                };
                EntryKind::File {
                    digest: TreeDigest::from_hex(sha256).ok_or_else(|| damaged("a digest"))?,
                    size: *size,
                    exec: entry.mode & 0o111 != 0,
                    source,
                }
            }
            FlatKind::Symlink { target } => EntryKind::Symlink {
                target: Arc::from(target.as_str()),
            },
        };
        entries.push(TreeEntry {
            index,
            parent: entry.parent,
            name: Arc::from(entry.name.as_str()),
            kind,
        });
        modes.push(entry.mode);
    }
    Ok(Tree::placed(entries, modes, path))
}

impl Core {
    /// Reads the tree `digest`: a placement's from the node's tree store, whole (an image from
    /// the tree's directory in the cache directory, which the placement this mount holds keeps
    /// there); any other's from its manifests, fetched through the fill socket where the cache
    /// does not hold them, a level of the tree at a time and up to [`MANIFESTS_AT_ONCE`] at once,
    /// all of them by [`TREE_READ`] after the read began, and flattened. The manifests it fetches
    /// are counted in `fetched`, which the caller makes for this read alone, and reported to the
    /// node (`fetched`, then `levels`) when the read ends.
    ///
    /// A tree whose root manifest the cache does not hold is new, and so are the manifests of the
    /// chain of directories that changed below it, which a level at a time would cost a round
    /// trip of the socket each: the node is first asked to fill them itself (`fill-tree`), going
    /// down the tree in the cache and answering once. Whatever it answers, the tree is then read as
    /// above, so a manifest the node did not fill (it failed, or it is of a release before the
    /// request) is filled by the read itself, and fails the read in the words it did before.
    pub(in crate::server::treefs) fn read_tree(
        &self,
        digest: &TreeDigest,
        fetched: &Fetched,
    ) -> Result<Tree, Unread> {
        if let Some((id, hash)) = placement_of(digest) {
            let gone = || Unread::Gone(format!("no placement {id} at that location"));
            let placed = match self.fills.image(&id) {
                Ok(placed) => placed,
                Err(answer) if answer == "none" => return Err(gone()),
                Err(answer) => return Err(Unread::Failed(answer)),
            };
            let paths = match &placed {
                PlacedTree::Image { paths, .. } => paths,
                PlacedTree::Flat(flat) => &flat.paths,
            };
            let path = paths.iter().find(|path| path_hash(path) == hash);
            let path = path.cloned().ok_or_else(gone)?;
            return match placed {
                PlacedTree::Image { tree, .. } => self.image_of(&tree, path),
                PlacedTree::Flat(flat) => tree_of(&flat, path).map_err(Unread::Failed),
            };
        }
        let until = Until {
            clock: &*self.settings.clock,
            end_ms: self
                .now()
                .saturating_add(u64::try_from(TREE_READ.as_millis()).unwrap_or(u64::MAX)),
        };
        let root = manifest_path(&self.settings.cache_path, &digest.to_string());
        if std::fs::metadata(root).is_err() {
            let start = self.now();
            let answer = self.fills.fill_tree(digest, until);
            let ms = self.now().saturating_sub(start);
            fetched.asked_tree(TreeAsked { answer, ms });
        }
        let fetch = |wanted: &TreeDigest, level: usize| -> Result<Vec<u8>, String> {
            let hex = wanted.to_string();
            let path = manifest_path(&self.settings.cache_path, &hex);
            if let Ok(bytes) = read_manifest(&path) {
                fetched.cached(level);
                return Ok(bytes);
            }
            let read = |()| read_manifest(&path).map_err(|error| error.to_string());
            let start = self.now();
            let filled = self.fills.fill_manifest(&hex, until);
            let ms = self.now().saturating_sub(start);
            let had = filled.and_then(read);
            fetched.filled(level, ms, had.is_ok());
            had
        };
        let flattened = flatten_in_waves(digest, MANIFESTS_AT_ONCE, fetch);
        self.fills.report(Report::Fetched(fetched.counts()));
        self.fills.report(Report::Levels(fetched.levels()));
        flattened.map(Tree::new).map_err(|why| {
            let gone = matches!(
                &why,
                TreeError::Fetch { digest: missing, reason } if missing == digest && reason == ABSENT
            );
            if gone {
                Unread::Gone(why.to_string())
            } else {
                Unread::Failed(why.to_string())
            }
        })
    }

    /// The tree at the fs location `path` that is the image of the tree `tree`: the list in
    /// memory already (the tree store's, or another location's), else mapped from the tree's
    /// directory and checked, its shape, not its digest, which the tree store checked when it
    /// took the image and checks at each start. Room is made under `fs.tree_list_ram_bytes` first,
    /// unless another tree of the mount shows the list already, and a list that does not fit is
    /// mapped all the same, and counted (`core/lists.rs`).
    fn image_of(&self, tree: &TreeDigest, path: String) -> Result<Tree, Unread> {
        let file = image_path(&self.settings.cache_path, tree);
        let failed = |why: String| Unread::Failed(format!("the image of {tree} at {path}: {why}"));
        // A list another tree of the mount shows already adds nothing: no room is made for it,
        // and it is never mapped past the cap. One the tree store mapped, which no tree of the
        // mount shows yet, is made room for: the mount holds it from now on.
        let need = self.lists.mapped.need(tree, &file);
        let fits = (need == 0 && self.shown(tree)) || self.make_room(need);
        let image = self
            .lists
            .mapped
            .open(tree, &file)
            .map_err(|why| failed(why.to_string()))?;
        if !fits {
            self.over_cap(tree);
        }
        Ok(Tree::image(image, path))
    }

    /// Looks up `name` in `parent`, the mount's root or a directory above the fs locations: the
    /// root of a placement's tree, a directory above a location, or `ENOENT`.
    ///
    /// # Errors
    /// [`Errno::NoEnt`] for a path that is no location and is above none (and for a name that
    /// cannot be one), [`Errno::Io`] for a fill socket that does not answer or a tree that cannot
    /// be read.
    pub(in crate::server::treefs) fn lookup_placed(
        &self,
        parent: u64,
        name: &str,
    ) -> Result<Looked, Errno> {
        let path = if parent == ino::ROOT {
            name.to_owned()
        } else {
            format!("{}/{name}", self.dir_path(parent)?)
        };
        // A name of a tree has no newline in the fill socket's lines, and the mount's own names
        // (`trees`, `views`, `put`) are no location.
        if path.contains('\n') || Location::fs(&path).is_err() {
            return Err(Errno::NoEnt);
        }
        let place = self
            .settled()
            .map_err(|why| why.to_string())
            .and_then(|()| self.fills.place(&self.settings.mount_id, &path));
        match place.map_err(|why| self.asked(&path, &why))? {
            Place::Tree(id) => {
                let key = key_of(&id, &path).ok_or_else(|| self.asked(&path, &id))?;
                let found = self.resolve_digest(key).and_then(|number| {
                    let loaded = self.counted(number)?;
                    self.root_entry(number, &loaded, Duration::ZERO)
                });
                self.settle_hold(&key, &path);
                found.map(Looked::Found)
            }
            Place::Dir => {
                let ino = dir_ino(&path);
                let _ = locked(&self.dirs).insert(ino, path);
                Ok(Looked::Found(self.namespace(ino)))
            }
            Place::Nothing => Err(Errno::NoEnt),
        }
    }

    /// Tells the node, the first time while the tree `loaded` is in memory, that a lookup in one
    /// of its directories or a listing of one was served, if it is the tree of a placement: the
    /// start of the command of the run whose source it is (`tierstore_request_to_start_seconds`).
    /// Neither the agent that opens the tree's root nor the kernel that mounts it under the job's
    /// overlay reads below the root, so the first such request is the container runtime making the
    /// job's root, or the command itself. The report leaves on a thread of its own: this does not
    /// wait for it.
    pub(super) fn looked_into(&self, loaded: &Loaded) {
        let Some(path) = loaded.tree.location() else {
            return;
        };
        // Read first, so that every request after the first only reads the flag.
        let first = !loaded.looked_into.load(Ordering::Acquire)
            && !loaded.looked_into.swap(true, Ordering::AcqRel);
        if first {
            self.fills.report(Report::Started(path.to_owned()));
        }
    }

    /// Makes the node's hold of the placement whose tree at `path` has the key `key` agree with
    /// the attachment, under the lock of the key's pins, which an idle detach gives holds back
    /// under (`Core::free_tree`): held again if the tree is attached (a detach between the
    /// lookup's `place` and its attach gave it back), given back if it is not (the lookup failed,
    /// or the tree went idle since). The `place` is made under the lock of the placement, as an
    /// `unplace` is (`core/pins.rs`). The lookups of other paths do not wait for it. A settling
    /// in progress is waited for by `Core::hold`, and none is made after it: the successor
    /// settles the holds.
    fn settle_hold(&self, key: &TreeDigest, path: &str) {
        let _freeing = self.freeing();
        if self.is_held() {
            return;
        }
        let _settling = self.pins.unpin(key);
        let number = self.table.find(key);
        let attached = number.is_some_and(|n| self.attached(n));
        if attached {
            let _placing = self.pins.placement(key);
            let _ = self
                .settled()
                .map(|()| self.fills.place(&self.settings.mount_id, path));
        } else {
            self.let_go(key);
        }
    }

    /// Tells the node's tree store which placements this mount holds, those of the trees of
    /// `attached` that are placed, so that it ends the mount's other holds.
    ///
    /// # Errors
    /// [`AskError`]: the socket did not answer.
    pub(in crate::server::treefs) fn places_live(
        &self,
        attached: &[TreeDigest],
    ) -> Result<(), AskError> {
        let ids: Vec<String> = attached
            .iter()
            .filter_map(placement_of)
            .map(|(id, _)| id)
            .collect();
        self.fills
            .places_live(&self.settings.mount_id, &ids)
            .map(drop)
    }

    /// The path of the directory `dir` above the fs locations: from the paths this core has
    /// looked up, else from the locations there are now.
    ///
    /// # Errors
    /// [`Errno::NoEnt`] for a number that is no such directory now, [`Errno::Io`] for a fill
    /// socket that does not answer.
    pub(in crate::server::treefs) fn dir_path(&self, dir: u64) -> Result<String, Errno> {
        if let Some(path) = locked(&self.dirs).get(&dir) {
            return Ok(path.clone());
        }
        let paths = self.fills.mounted().map_err(|why| self.asked("", &why))?;
        let mut dirs = locked(&self.dirs);
        for path in &paths {
            for (at, _) in path.match_indices('/') {
                let above = &path[..at];
                let _ = dirs.insert(dir_ino(above), above.to_owned());
            }
        }
        dirs.get(&dir).cloned().ok_or(Errno::NoEnt)
    }

    /// The inode of the directory that holds the root of `tree`: `trees/` for a tree of manifests;
    /// for a placed tree, the directory above its location, or the mount's root.
    pub(in crate::server::treefs) fn root_parent(&self, tree: &Tree) -> u64 {
        let Some(location) = tree.location() else {
            return ino::TREES;
        };
        let Some((above, _)) = location.rsplit_once('/') else {
            return ino::ROOT;
        };
        let ino = dir_ino(above);
        let _ = locked(&self.dirs)
            .entry(ino)
            .or_insert_with(|| above.to_owned());
        ino
    }

    /// The inode of the directory that holds `dir`, a directory above the fs locations.
    ///
    /// # Errors
    /// As [`Core::dir_path`].
    pub(in crate::server::treefs) fn placed_parent(&self, dir: u64) -> Result<u64, Errno> {
        let path = self.dir_path(dir)?;
        Ok(path
            .rsplit_once('/')
            .map_or(ino::ROOT, |(up, _)| dir_ino(up)))
    }

    /// Gives back the hold of the node's tree store on the placement whose tree at a location has
    /// the key `digest`, unless the tree of another of its locations is attached still; a tree of
    /// manifests is unpinned instead. The look at the other locations and the `unplace` are made
    /// under the lock of the placement (`core/pins.rs`).
    pub(in crate::server::treefs) fn let_go(&self, digest: &TreeDigest) {
        match placement_of(digest) {
            Some((id, _)) => {
                let _unplacing = self.pins.unplace(digest);
                let trees = self.state().attachments.trees();
                let held = trees
                    .iter()
                    .any(|other| placement_of(other).is_some_and(|(other, _)| other == id));
                if !held {
                    let _ = self
                        .settled()
                        .map(|()| self.fills.unplace(&self.settings.mount_id, &id));
                }
            }
            None => {
                let _ = self.settled().and_then(|()| {
                    self.fills
                        .unpin(&self.settings.mount_id, &digest.to_string())
                });
            }
        }
    }
}

impl Core {
    /// What a question to the fill socket about `path` that was not answered is: `EIO`, said on
    /// stderr (`Core::say_failed`).
    fn asked(&self, path: &str, why: &str) -> Errno {
        self.say_failed((0, Failing::Other), None, &|| {
            format!("tierstore fs: cannot look up the placement at {path:?}: {why}")
        });
        Errno::Io
    }
}

#[cfg(test)]
#[path = "placed_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "placed_flat_tests.rs"]
mod flat_tests;

#[cfg(test)]
#[path = "placed_image_tests.rs"]
mod image_tests;

#[cfg(test)]
#[path = "tree_read_tests.rs"]
mod tree_read_tests;

#[cfg(test)]
#[path = "tree_fill_tests.rs"]
mod tree_fill_tests;
