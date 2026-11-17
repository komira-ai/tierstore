//! Making a tree and its placement.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;

use super::super::PinAnswer;
use super::TreeStore;
use super::files::{
    At, IMAGE, PlacementFile, READY, RECORDS, SOURCES, STAGING, Source, Sources, encode_records,
    placement_file, tree_dir, trees_dir, write_new,
};
use super::location::{Location, Meets, PlacementId, Table, meets};
use super::state::{Content, Placement, Shared, TreeInfo, closure};
use crate::{BuiltFile, BuiltTree, RecordChild, TreeDigest, TreeImage};

/// What [`TreeStore::create`] makes a tree and a placement of.
#[derive(Debug)]
pub struct CreateRequest {
    /// The records and files of the tree, as [`build_tree`](crate::build_tree) made them.
    pub built: BuiltTree,
    /// Where the tree appears: at least one location.
    pub at: Vec<Location>,
    /// How long the placement lasts after this create; none is `trees.ttl`. A longer one than
    /// `trees.ttl_max` is taken as that.
    pub ttl: Option<Duration>,
    /// Whether every range of the tree's files is in the cache already, as the caller has seen.
    pub ready: bool,
}

/// What [`TreeStore::create_image`] makes a tree and a placement of: a tree served straight from
/// its version 2 tree list.
#[derive(Debug)]
pub struct ImageRequest {
    /// The tree list, opened and walked.
    pub image: Arc<TreeImage>,
    /// The SHA-256 of the image's bytes, which names the tree. The caller has checked it: the
    /// mount provider, against the key the list is stored under, on a thread of its own while it
    /// walks the list. The store does not hash the image again.
    pub digest: TreeDigest,
    /// Where the tree appears: at least one location.
    pub at: Vec<Location>,
    /// How long the placement lasts after this create, as [`CreateRequest::ttl`].
    pub ttl: Option<Duration>,
    /// Whether every range of the tree's files is in the cache already, as the caller has seen.
    pub ready: bool,
    /// The image written ahead by [`TreeStore::stage_image`], if it was: a create of a tree the
    /// node does not have renames it into place rather than writing the image, if it holds the
    /// image's own bytes (the same buffer). Otherwise it is removed.
    pub staged: Option<StagedImage>,
}

/// An image written to the store's directory under a staging name before its create
/// ([`TreeStore::stage_image`]), so that the write runs beside the walk of the list: the create
/// that takes it renames it into place. One that no create takes is removed when it is dropped.
pub struct StagedImage {
    /// The bytes written.
    bytes: Bytes,
    /// The staging directory, until a create takes it.
    dir: Option<PathBuf>,
}

impl std::fmt::Debug for StagedImage {
    /// Where the image is and its size, not its bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedImage")
            .field("bytes", &self.bytes.len())
            .field("dir", &self.dir)
            .finish()
    }
}

impl StagedImage {
    /// The staging directory, if it holds `bytes`: the same buffer, which the image was opened
    /// from.
    fn take_for(&mut self, bytes: &Bytes) -> Option<PathBuf> {
        let same = self.bytes.as_ptr() == bytes.as_ptr() && self.bytes.len() == bytes.len();
        same.then(|| self.dir.take()).flatten()
    }
}

impl Drop for StagedImage {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// What a create made.
#[derive(Debug, PartialEq, Eq)]
pub struct Created {
    /// The digest of the tree.
    pub tree: TreeDigest,
    /// The placement: the tree at these locations.
    pub placement: PlacementId,
    /// Whether the placement is new. False: the same tree at the same locations was placed
    /// already, and the call refreshed its lease.
    pub created: bool,
    /// Whether the node had no copy of the tree before.
    pub new_tree: bool,
    /// When the lease ends, in milliseconds since the Unix epoch.
    pub expires_ms: u64,
    /// The files of the entries of the request (not of the trees it holds by digest), of any
    /// size.
    pub files: u64,
    /// Their bytes.
    pub bytes: u64,
    /// The packs they are fetched from.
    pub packs: usize,
    /// What pinning the packs did: nothing for a tree the node had.
    pub pins: PinAnswer,
}

/// Why a tree was not created.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CreateError {
    /// `trees.enabled` is false.
    #[error("trees are off on this node")]
    Disabled,
    /// The cache does not hold the lock of its directory, or has not read it since it took it.
    #[error("this node does not hold the lock of its cache directory")]
    ReadOnly,
    /// The request has no location.
    #[error("a tree needs at least one location")]
    NoLocation,
    /// A location is the same as another of the request, or is equal to the place of another
    /// placement: the place is taken.
    #[error("location taken: {0}")]
    Taken(Location),
    /// A location is inside, or around, another of the request or of a placement.
    #[error("location overlaps: {0}")]
    Overlaps(Location),
    /// The tree holds a tree by digest that this node does not have, or has only as an image
    /// (which has no records to hold).
    #[error("unknown child tree {0}")]
    UnknownChild(TreeDigest),
    /// The files of the tree are more bytes than `trees.max_bytes`.
    #[error("tree too large: {bytes} bytes of files, at most {max}")]
    TooLarge {
        /// The bytes of the files.
        bytes: u64,
        /// The limit.
        max: u64,
    },
    /// `trees.max_placements` placements are alive.
    #[error("too many placements: {0} are alive")]
    TooMany(usize),
    /// A file could not be written.
    #[error("writing the tree: {0}")]
    Io(String),
}

/// The packs of `files`, and where each file's bytes are in them. A file with no `from` is the
/// whole object `cas/l/<sha256>`; one of size 0 has no bytes to fetch.
fn sources_of(files: &[BuiltFile]) -> Sources {
    let mut sources = Sources::default();
    let mut packs: HashMap<String, u32> = HashMap::new();
    for file in files.iter().filter(|file| file.size > 0) {
        let (key, offset) = match &file.source {
            Some(source) => (source.key.to_string(), source.offset),
            None => (format!("cas/l/{}", file.sha256), 0),
        };
        let next = packs.len() as u32;
        let pack = *packs.entry(key.clone()).or_insert_with(|| {
            sources.packs.push(key);
            next
        });
        sources
            .files
            .entry(*file.sha256.as_bytes())
            .or_insert(Source {
                size: file.size,
                pack,
                offset,
            });
    }
    sources
}

/// The trees `built` holds by digest: the directories of its records that none of its records is.
fn children_of(built: &BuiltTree) -> Vec<TreeDigest> {
    let own: HashSet<TreeDigest> = built.records.iter().map(|(digest, _)| *digest).collect();
    let held: HashSet<TreeDigest> = built
        .records
        .iter()
        .flat_map(|(_, record)| record.children())
        .filter_map(|child| match child {
            RecordChild::Dir { digest, .. } => Some(*digest),
            RecordChild::File { .. } | RecordChild::Symlink { .. } => None,
        })
        .filter(|digest| !own.contains(digest))
        .collect();
    let mut children: Vec<TreeDigest> = held.into_iter().collect();
    children.sort_by_key(|digest| *digest.as_bytes());
    children
}

/// A tree a create makes, if the node does not have it.
enum NewTree {
    /// Records and files, as [`build_tree`](crate::build_tree) made them, and the sources of
    /// the files.
    Built(BuiltTree, Sources),
    /// A version 2 tree list, named by the SHA-256 of its bytes, and its file if it was written
    /// ahead.
    Image {
        digest: TreeDigest,
        image: Arc<TreeImage>,
        staged: Option<StagedImage>,
    },
}

impl NewTree {
    /// The digest that names the tree.
    fn root(&self) -> TreeDigest {
        match self {
            Self::Built(built, _) => built.root,
            Self::Image { digest, .. } => *digest,
        }
    }

    /// The number of the files and their bytes together.
    fn files(&self) -> (u64, u64) {
        match self {
            Self::Built(built, _) => {
                let bytes = built
                    .files
                    .iter()
                    .fold(0u64, |total, file| total.saturating_add(file.size));
                (built.files.len() as u64, bytes)
            }
            Self::Image { image, .. } => (image.files(), image.file_bytes()),
        }
    }

    /// The number of the packs the files are read from.
    fn packs(&self) -> usize {
        match self {
            Self::Built(_, sources) => sources.packs.len(),
            Self::Image { image, .. } => image.keys().len(),
        }
    }
}

/// The first of the locations whose key meets another's, or a placement's in `table`.
fn clash(table: &Table, at: &[Location]) -> Option<CreateError> {
    let mut own = Table::new();
    let id = PlacementId(String::new());
    for location in at {
        let key = location.key();
        let met = meets(&own, &key).or_else(|| meets(table, &key));
        match met {
            Some(Meets::Equal(_)) => return Some(CreateError::Taken(location.clone())),
            Some(Meets::Overlaps(_)) => return Some(CreateError::Overlaps(location.clone())),
            None => {
                own.insert(key, id.clone());
            }
        }
    }
    None
}

fn io_error(why: &std::io::Error) -> CreateError {
    CreateError::Io(why.to_string())
}

/// The milliseconds of `ttl`, which a clock in milliseconds cannot pass.
fn millis(ttl: Duration) -> u64 {
    u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX)
}

impl Shared {
    /// Writes `files` to a new directory under a staging name; its path.
    fn stage(&self, files: &[(&str, &[u8])]) -> Result<PathBuf, CreateError> {
        let number = self.staged.fetch_add(1, Ordering::Relaxed);
        let staging = trees_dir(&self.root).join(format!("{STAGING}{number}"));
        let written = (|| {
            std::fs::create_dir_all(&staging)?;
            for (name, bytes) in files {
                std::fs::write(staging.join(name), bytes)?;
            }
            Ok(())
        })();
        match written {
            Ok(()) => Ok(staging),
            Err(why) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(io_error(&why))
            }
        }
    }

    /// Renames the staging directory `staging` to the directory of the tree `root`, with its
    /// `ready` marker if `ready`; removes it if it cannot.
    fn settle(&self, staging: &Path, root: &TreeDigest, ready: bool) -> Result<(), CreateError> {
        let settled = (|| {
            if ready {
                std::fs::write(staging.join(READY), b"")?;
            }
            std::fs::rename(staging, tree_dir(&self.root, root))
        })();
        settled.map_err(|why| {
            let _ = std::fs::remove_dir_all(staging);
            io_error(&why)
        })
    }

    /// Writes the directory of a new tree, its `files` and its `ready` marker if `ready`, under a
    /// staging name, and renames it to its own.
    fn write_tree(
        &self,
        root: &TreeDigest,
        files: &[(&str, &[u8])],
        ready: bool,
    ) -> Result<(), CreateError> {
        let staging = self.stage(files)?;
        self.settle(&staging, root, ready)
    }

    /// Writes the directory of the new tree `tree` and gives what the store keeps of it.
    fn write_new_tree(&self, tree: NewTree, ready: bool) -> Result<Content, CreateError> {
        let root = tree.root();
        match tree {
            NewTree::Built(built, sources) => {
                let (records, places) = encode_records(&built.records);
                let files = [(RECORDS, &records[..]), (SOURCES, &sources.encode()[..])];
                self.write_tree(&root, &files, ready)?;
                Ok(Content::Records {
                    records: places.into_iter().collect(),
                    sources,
                })
            }
            NewTree::Image { image, staged, .. } => {
                match staged.and_then(|mut staged| staged.take_for(image.bytes())) {
                    Some(staging) => self.settle(&staging, &root, ready)?,
                    None => self.write_tree(&root, &[(IMAGE, &image.bytes()[..])], ready)?,
                }
                Ok(Content::Image(image))
            }
        }
    }

    /// Writes the file of a placement.
    fn write_placement(&self, placement: &Placement) -> Result<(), CreateError> {
        let file = PlacementFile {
            v: 1,
            tree: placement.tree.to_string(),
            at: placement.locations.iter().map(At::of).collect(),
            created_ms: placement.created_ms,
            expires_ms: placement.expires_ms,
        };
        let text = serde_json::to_vec(&file).map_err(|why| CreateError::Io(why.to_string()))?;
        write_new(&placement_file(&self.root, &placement.id), &text).map_err(|why| io_error(&why))
    }
}

impl TreeStore {
    /// Makes the tree of `request` and places it at its locations, or refreshes the placement
    /// that is there: the same tree at the same locations is one placement, and a create of it
    /// again only moves its lease. A call blocks on the files it writes: make it on a thread that
    /// may.
    ///
    /// # Errors
    /// See [`CreateError`]; nothing is made when one is given.
    pub fn create(&self, request: CreateRequest) -> Result<Created, CreateError> {
        let CreateRequest {
            built,
            at,
            ttl,
            ready,
        } = request;
        let sources = sources_of(&built.files);
        self.make(NewTree::Built(built, sources), at, ttl, ready)
    }

    /// Makes the tree of the version 2 tree list of `request` and places it at its locations, as
    /// [`TreeStore::create`] does: the tree is named by the digest of the request and is the list
    /// itself, written to the tree's directory as it is (`image`) and read in place; it has no
    /// directory records and holds no tree by digest.
    ///
    /// # Errors
    /// As [`TreeStore::create`].
    pub fn create_image(&self, request: ImageRequest) -> Result<Created, CreateError> {
        let ImageRequest {
            image,
            digest,
            at,
            ttl,
            ready,
            staged,
        } = request;
        let tree = NewTree::Image {
            digest,
            image,
            staged,
        };
        self.make(tree, at, ttl, ready)
    }

    /// Writes the image `bytes` under a staging name, for the create of its tree to rename into
    /// place ([`ImageRequest::staged`]): a caller that has the bytes of a version 2 tree list
    /// before it has walked it writes them beside the walk. A call blocks on the file it writes.
    ///
    /// # Errors
    /// [`CreateError::Disabled`], [`CreateError::ReadOnly`], and [`CreateError::Io`] for a file
    /// that could not be written; nothing is left in the directory when one is given.
    pub fn stage_image(&self, bytes: Bytes) -> Result<StagedImage, CreateError> {
        let shared = &self.shared;
        if !shared.config.enabled {
            return Err(CreateError::Disabled);
        }
        if !shared.writable(&shared.lock()) {
            return Err(CreateError::ReadOnly);
        }
        let dir = shared.stage(&[(IMAGE, &bytes[..])])?;
        Ok(StagedImage {
            bytes,
            dir: Some(dir),
        })
    }

    /// Makes `tree`, if the node does not have it, and places it at `at`.
    fn make(
        &self,
        tree: NewTree,
        at: Vec<Location>,
        ttl: Option<Duration>,
        ready: bool,
    ) -> Result<Created, CreateError> {
        let shared = &self.shared;
        let config = &shared.config;
        if !config.enabled {
            return Err(CreateError::Disabled);
        }
        if at.is_empty() {
            return Err(CreateError::NoLocation);
        }
        let (files, bytes) = tree.files();
        if bytes > config.max_bytes {
            let max = config.max_bytes;
            return Err(CreateError::TooLarge { bytes, max });
        }
        let root = tree.root();
        let id = PlacementId::of(&root, &at);
        let mut guard = shared.lock();
        let state = &mut *guard;
        if !shared.writable(state) {
            return Err(CreateError::ReadOnly);
        }
        let now = shared.now();
        shared.purge(state, now);
        let children = match &tree {
            NewTree::Built(built, _) => children_of(built),
            NewTree::Image { .. } => Vec::new(),
        };
        // A tree is held by its records: an image has none to hold.
        let held = |child: &TreeDigest| {
            state
                .trees
                .get(child)
                .is_some_and(|info| info.records().is_some())
        };
        if let Some(missing) = children.iter().find(|child| !held(child)) {
            return Err(CreateError::UnknownChild(*missing));
        }
        let ttl = ttl.unwrap_or(config.ttl).min(config.ttl_max);
        let expires_ms = now.saturating_add(millis(ttl));
        let summary = Created {
            tree: root,
            placement: id.clone(),
            created: true,
            new_tree: false,
            expires_ms,
            files,
            bytes,
            packs: tree.packs(),
            pins: PinAnswer::default(),
        };
        if let Some(placement) = state.placements.get_mut(&id).filter(|p| !p.released) {
            // The same tree at the same places: a refresh.
            placement.expires_ms = expires_ms;
            shared.write_placement(placement)?;
            state
                .trees
                .get(&root)
                .into_iter()
                .for_each(|info| self.set_ready(info, ready));
            return Ok(Created {
                created: false,
                ..summary
            });
        }
        if let Some(why) = clash(&state.table, &at) {
            return Err(why);
        }
        let old = state.placements.get(&id);
        if old.is_none() && state.placements.len() >= config.max_placements {
            return Err(CreateError::TooMany(state.placements.len()));
        }
        let (created_ms, attachments, reads) = old.map_or((now, Default::default(), 0), |old| {
            (old.created_ms, old.attachments.clone(), old.reads)
        });
        let placement = Placement {
            id: id.clone(),
            tree: root,
            locations: at,
            created_ms,
            expires_ms,
            released: false,
            attachments,
            reads,
        };
        let new_tree = !state.trees.contains_key(&root);
        let mut pins = PinAnswer::default();
        if new_tree {
            let content = shared.write_new_tree(tree, ready)?;
            let info = TreeInfo {
                root,
                closure: closure(&state.trees, root, &children),
                content,
                ready: AtomicBool::new(ready),
            };
            pins = shared.pin(&info);
            state.trees.insert(root, Arc::new(info));
        } else {
            state
                .trees
                .get(&root)
                .into_iter()
                .for_each(|info| self.set_ready(info, ready));
        }
        if let Err(why) = shared.write_placement(&placement) {
            // The tree this made, if any, has no placement: it goes.
            shared.drop_unreached(state);
            return Err(why);
        }
        for location in &placement.locations {
            state.table.insert(location.key(), id.clone());
        }
        state.placements.insert(id, placement);
        Ok(Created {
            new_tree,
            pins,
            ..summary
        })
    }

    /// Says every range of the tree was in the cache and pinned, if the caller says so.
    fn set_ready(&self, info: &TreeInfo, ready: bool) {
        if ready && !info.ready.swap(true, Ordering::SeqCst) {
            // A file that cannot be written leaves the tree as it was: the flag is a hint for the
            // next start, which reads a tree with no marker as one it holds no ranges of.
            let marker = tree_dir(&self.shared.root, &info.root).join(READY);
            if let Err(why) = std::fs::write(marker, b"") {
                tracing::warn!(
                    "tierstore: trees: {} was not marked ready: {why}",
                    info.root
                );
            }
            let _ = self.shared.pin(info);
        }
    }
}
