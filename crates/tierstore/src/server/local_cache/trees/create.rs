//! Making a tree and its placement.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::super::PinAnswer;
use super::TreeStore;
use super::files::{
    At, Place, PlacementFile, READY, RECORDS, SOURCES, STAGING, Source, Sources, encode_records,
    placement_file, tree_dir, trees_dir, write_new,
};
use super::location::{Location, Meets, PlacementId, Table, meets};
use super::state::{Placement, Shared, TreeInfo, closure};
use crate::{BuiltFile, BuiltTree, RecordChild, TreeDigest};

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
    /// The files of the entries of the request (not of the trees it holds by digest).
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
    /// The tree holds a tree by digest that this node does not have.
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
    /// Writes the directory of a new tree under a staging name and renames it to its own; gives
    /// where each of its records is in the records file.
    fn write_tree(
        &self,
        built: &BuiltTree,
        sources: &Sources,
        ready: bool,
    ) -> Result<Vec<(TreeDigest, Place)>, CreateError> {
        let number = self.staged.fetch_add(1, Ordering::Relaxed);
        let staging = trees_dir(&self.root).join(format!("{STAGING}{number}"));
        let (records, places) = encode_records(&built.records);
        let written = (|| {
            std::fs::create_dir_all(&staging)?;
            std::fs::write(staging.join(RECORDS), records)?;
            std::fs::write(staging.join(SOURCES), sources.encode())?;
            if ready {
                std::fs::write(staging.join(READY), b"")?;
            }
            std::fs::rename(&staging, tree_dir(&self.root, &built.root))
        })();
        match written {
            Ok(()) => Ok(places),
            Err(why) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(io_error(&why))
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
        let shared = &self.shared;
        let config = &shared.config;
        if !config.enabled {
            return Err(CreateError::Disabled);
        }
        let CreateRequest {
            built,
            at,
            ttl,
            ready,
        } = request;
        if at.is_empty() {
            return Err(CreateError::NoLocation);
        }
        let bytes = built
            .files
            .iter()
            .fold(0u64, |total, file| total.saturating_add(file.size));
        if bytes > config.max_bytes {
            let max = config.max_bytes;
            return Err(CreateError::TooLarge { bytes, max });
        }
        let id = PlacementId::of(&built.root, &at);
        let mut guard = shared.lock();
        let state = &mut *guard;
        if !shared.writable(state) {
            return Err(CreateError::ReadOnly);
        }
        let now = shared.now();
        shared.purge(state, now);
        let children = children_of(&built);
        if let Some(missing) = children
            .iter()
            .find(|child| !state.trees.contains_key(child))
        {
            return Err(CreateError::UnknownChild(*missing));
        }
        let ttl = ttl.unwrap_or(config.ttl).min(config.ttl_max);
        let expires_ms = now.saturating_add(millis(ttl));
        let sources = sources_of(&built.files);
        let summary = Created {
            tree: built.root,
            placement: id.clone(),
            created: true,
            new_tree: false,
            expires_ms,
            files: built.files.len() as u64,
            bytes,
            packs: sources.packs.len(),
            pins: PinAnswer::default(),
        };
        if let Some(placement) = state.placements.get_mut(&id).filter(|p| !p.released) {
            // The same tree at the same places: a refresh.
            placement.expires_ms = expires_ms;
            shared.write_placement(placement)?;
            if let Some(info) = state.trees.get(&built.root) {
                self.set_ready(info, ready);
            }
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
            tree: built.root,
            locations: at,
            created_ms,
            expires_ms,
            released: false,
            attachments,
            reads,
        };
        let new_tree = !state.trees.contains_key(&built.root);
        let mut pins = PinAnswer::default();
        if new_tree {
            let places = shared.write_tree(&built, &sources, ready)?;
            let info = TreeInfo {
                root: built.root,
                closure: closure(&state.trees, built.root, &children),
                records: places.into_iter().collect(),
                sources,
                ready: AtomicBool::new(ready),
            };
            pins = shared.pin(&info);
            state.trees.insert(built.root, Arc::new(info));
        } else if let Some(info) = state.trees.get(&built.root) {
            self.set_ready(info, ready);
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
