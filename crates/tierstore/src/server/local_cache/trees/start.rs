//! Reading the directory back: what a start (and the cache taking the lock) makes of `trees/`
//! and `placements/`.
//!
//! A tree is kept only if its records file reads and every record hashes to the digest it is
//! named by, its root among them, its sources file reads, every tree it holds by digest is kept
//! and, if it was ready, the cache still holds every range of its files. A placement is kept only
//! if its file reads, it is named by the id of what it holds, its tree is kept, its lease has not
//! ended and no location of it meets one kept before (in the order of the ids). A tree no kept
//! placement reaches is dropped. The holder of the lock removes what it drops; a cache that holds
//! no lock only leaves it out of memory.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::TreeStore;
use super::files::{
    At, PlacementFile, READY, RECORDS, SOURCES, STAGING, Sources, parse_records, placement_file,
    placements_dir, tree_dir, trees_dir,
};
use super::location::{Location, PlacementId, meets};
use super::state::{Placement, Shared, State, TreeInfo};
use crate::{RecordChild, TreeDigest, TreeRecord};

/// The entries of the directory `dir`, by name: none if it is not there.
fn entries(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(why) => {
            tracing::warn!("tierstore: trees: {} was not read: {why}", dir.display());
            return Vec::new();
        }
    };
    let mut found: Vec<_> = listing
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some((name, entry.path()))
        })
        .collect();
    found.sort();
    found
}

/// Removes a directory or a file the store has no use for, if the store may.
pub(super) fn discard(owned: bool, path: &Path) {
    if !owned {
        return;
    }
    let removed = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    if let Err(why) = removed {
        tracing::warn!(
            "tierstore: trees: {} was not removed: {why}",
            path.display()
        );
    }
}

/// A tree as its directory holds it, before it is checked against the others.
struct Read {
    root: TreeDigest,
    children: Vec<TreeDigest>,
    records: HashMap<TreeDigest, super::files::Place>,
    sources: Sources,
    ready: bool,
}

/// Reads the directory of the tree `root`, or says why it is not a tree.
fn read_tree(dir: &Path, root: TreeDigest) -> Result<Read, String> {
    let bytes = std::fs::read(dir.join(RECORDS)).map_err(|why| format!("records: {why}"))?;
    let index = parse_records(&bytes).map_err(|why| format!("records: {why}"))?;
    let mut records = HashMap::new();
    let mut holds = HashSet::new();
    for (digest, place) in &index {
        let start = place.offset as usize;
        let body = &bytes[start..start + place.len as usize];
        if TreeDigest::of(body) != *digest {
            return Err("a record does not hash to its name".to_owned());
        }
        let record = TreeRecord::parse(body).map_err(|why| format!("a record: {why}"))?;
        for child in record.children() {
            if let RecordChild::Dir { digest, .. } = child {
                holds.insert(*digest);
            }
        }
        if records.insert(*digest, *place).is_some() {
            return Err("a record is listed twice".to_owned());
        }
    }
    if !records.contains_key(&root) {
        return Err("the root's record is not there".to_owned());
    }
    let mut children: Vec<TreeDigest> = holds
        .into_iter()
        .filter(|digest| !records.contains_key(digest))
        .collect();
    children.sort_by_key(|digest| *digest.as_bytes());
    let text = std::fs::read(dir.join(SOURCES)).map_err(|why| format!("sources: {why}"))?;
    let sources = Sources::parse(&text).map_err(|why| format!("sources: {why}"))?;
    Ok(Read {
        root,
        children,
        records,
        sources,
        ready: dir.join(READY).exists(),
    })
}

/// `root` and every tree below it by digest, `root` first, each once.
fn below(read: &HashMap<TreeDigest, Read>, root: TreeDigest) -> Vec<TreeDigest> {
    let mut out = vec![root];
    let mut next = 0;
    while let Some(tree) = out.get(next).and_then(|digest| read.get(digest)) {
        for child in &tree.children {
            if !out.contains(child) {
                out.push(*child);
            }
        }
        next += 1;
    }
    out
}

/// A placement file that was read and passed its checks.
struct Kept {
    id: PlacementId,
    tree: TreeDigest,
    at: Vec<Location>,
    created_ms: u64,
    expires_ms: u64,
}

impl Shared {
    /// Whether the cache holds every range of the files of `tree`.
    fn holds_all(&self, tree: &Read) -> bool {
        tree.sources.files.values().all(|source| {
            let pack = &tree.sources.packs[source.pack as usize];
            self.cache
                .span_of(pack, source.offset, source.size)
                .is_some()
        })
    }

    /// Reads the directory and checks it (see the module docs).
    pub(super) fn read_directory(&self, owned: bool, held: Vec<Placement>) -> State {
        let now = self.now();
        let mut read = HashMap::new();
        for (name, path) in entries(&trees_dir(&self.root)) {
            if name.starts_with(STAGING) {
                discard(owned, &path);
                continue;
            }
            let outcome = match TreeDigest::from_hex(&name) {
                Some(root) => read_tree(&path, root).map(|tree| (root, tree)),
                None => Err("not a digest".to_owned()),
            };
            match outcome {
                Ok((root, tree)) => {
                    read.insert(root, tree);
                }
                Err(why) => {
                    tracing::warn!("tierstore: trees: {name} is dropped: {why}");
                    discard(owned, &path);
                }
            }
        }
        // A ready tree whose bytes the cache lost is not ready: dropped.
        let lost: Vec<TreeDigest> = read
            .values()
            .filter(|tree| tree.ready && !self.holds_all(tree))
            .map(|tree| tree.root)
            .collect();
        for root in lost {
            tracing::warn!("tierstore: trees: {root} is dropped: the cache lost some of its bytes");
            read.remove(&root);
            discard(owned, &tree_dir(&self.root, &root));
        }
        // A tree whose child is gone is gone too, and so are the trees of that, in turn.
        loop {
            let orphans: Vec<TreeDigest> = read
                .values()
                .filter(|tree| tree.children.iter().any(|child| !read.contains_key(child)))
                .map(|tree| tree.root)
                .collect();
            if orphans.is_empty() {
                break;
            }
            for root in orphans {
                tracing::warn!("tierstore: trees: {root} is dropped: a tree it holds is gone");
                read.remove(&root);
                discard(owned, &tree_dir(&self.root, &root));
            }
        }
        let mut state = State {
            owned,
            generation: self.cache.inner.generation(),
            ..State::default()
        };
        let closures: HashMap<TreeDigest, Vec<TreeDigest>> = read
            .keys()
            .map(|root| (*root, below(&read, *root)))
            .collect();
        for (root, tree) in read {
            let info = TreeInfo {
                root,
                closure: closures[&root].clone(),
                records: tree.records,
                sources: tree.sources,
                ready: AtomicBool::new(tree.ready),
            };
            state.trees.insert(root, Arc::new(info));
        }
        self.keep_held(&mut state, held);
        self.read_placements(&mut state, now);
        self.drop_unreached(&mut state);
        state
    }

    /// Puts back the placements that something held before the directory was read, as they were:
    /// a lease that ended or a release does not take a placement from its holders. One whose tree
    /// the directory no longer has cannot be served and is let go.
    fn keep_held(&self, state: &mut State, held: Vec<Placement>) {
        for placement in held {
            if !state.trees.contains_key(&placement.tree) {
                continue;
            }
            let keys: Vec<String> = placement.locations.iter().map(Location::key).collect();
            let taken = !placement.released
                && keys.iter().any(|key| meets(&state.table, key).is_some());
            if taken {
                continue;
            }
            if !placement.released {
                for key in keys {
                    state.table.insert(key, placement.id.clone());
                }
            }
            state.placements.insert(placement.id.clone(), placement);
        }
    }

    /// Reads `placements/` into `state`, keeping what the module docs say.
    fn read_placements(&self, state: &mut State, now: u64) {
        let mut kept: Vec<Kept> = Vec::new();
        for (name, path) in entries(&placements_dir(&self.root)) {
            let Some(id) = PlacementId::parse(&name) else {
                discard(state.owned, &path);
                continue;
            };
            if state.placements.contains_key(&id) {
                // Held in memory, as it was: the file says no more than that.
                continue;
            }
            match self.read_placement(state, now, &id, &path) {
                Ok(Some(placement)) => kept.push(placement),
                // A file of a later release's version or interface: left where it is.
                Ok(None) => {}
                Err(why) => {
                    tracing::warn!("tierstore: trees: placement {name} is dropped: {why}");
                    discard(state.owned, &path);
                }
            }
        }
        for placement in kept {
            let keys: Vec<String> = placement.at.iter().map(Location::key).collect();
            let conflict = keys
                .iter()
                .enumerate()
                .any(|(i, key)| keys[..i].contains(key) || meets(&state.table, key).is_some());
            if conflict {
                let id = &placement.id;
                tracing::warn!("tierstore: trees: placement {id} is dropped: a location is taken");
                discard(state.owned, &placement_file(&self.root, id));
                continue;
            }
            for key in keys {
                state.table.insert(key, placement.id.clone());
            }
            let id = placement.id.clone();
            let placement = Placement {
                id: placement.id,
                tree: placement.tree,
                locations: placement.at,
                created_ms: placement.created_ms,
                expires_ms: placement.expires_ms,
                released: false,
                attachments: BTreeSet::new(),
                reads: 0,
            };
            state.placements.insert(id, placement);
        }
    }

    /// The placement file at `path` named `id`, none if it is of a version, or has a location of an
    /// interface, this release does not know, or why it is not kept.
    fn read_placement(
        &self,
        state: &State,
        now: u64,
        id: &PlacementId,
        path: &Path,
    ) -> Result<Option<Kept>, String> {
        let text = std::fs::read(path).map_err(|why| why.to_string())?;
        let file: PlacementFile = serde_json::from_slice(&text).map_err(|why| why.to_string())?;
        if file.v != 1 {
            return Ok(None);
        }
        let tree = TreeDigest::from_hex(&file.tree).ok_or("the tree is not a digest")?;
        if !state.trees.contains_key(&tree) {
            return Err("its tree is gone".to_owned());
        }
        let at: Result<Vec<Option<Location>>, _> = file.at.iter().map(At::location).collect();
        let at = at.map_err(|why| why.to_string())?;
        // A location of an interface a later release added: the file is left where it is.
        let Some(at) = at.into_iter().collect::<Option<Vec<Location>>>() else {
            return Ok(None);
        };
        if at.is_empty() || PlacementId::of(&tree, &at) != *id {
            return Err("it is not named by what it holds".to_owned());
        }
        if file.expires_ms <= now {
            return Err("its lease ended".to_owned());
        }
        Ok(Some(Kept {
            id: id.clone(),
            tree,
            at,
            created_ms: file.created_ms,
            expires_ms: file.expires_ms,
        }))
    }

    /// Takes the trees no placement reaches out of `state`, and their directories off the disk.
    pub(super) fn drop_unreached(&self, state: &mut State) {
        let reached: HashSet<TreeDigest> = state
            .placements
            .values()
            .filter_map(|placement| state.trees.get(&placement.tree))
            .flat_map(|info| info.closure.iter().copied())
            .collect();
        let unreached: Vec<TreeDigest> = state
            .trees
            .keys()
            .filter(|root| !reached.contains(root))
            .copied()
            .collect();
        for root in unreached {
            state.trees.remove(&root);
            discard(state.owned, &tree_dir(&self.root, &root));
            self.unpin(&root);
        }
    }
}

impl TreeStore {
    /// Reads the directory into the store and, if the cache holds its lock, pins every tree. A
    /// store that holds no lock reads and removes nothing.
    pub(super) fn load(&self) -> io::Result<()> {
        let shared = &self.shared;
        let owned = shared.cache.inner.read_write();
        if owned {
            std::fs::create_dir_all(trees_dir(&shared.root))?;
            std::fs::create_dir_all(placements_dir(&shared.root))?;
        }
        // One hold of the lock from the first look at what is held to the swap: an attach or a
        // read that comes meanwhile waits for the new state and finds the placement in it.
        let mut guard = shared.lock();
        // What holds a placement now goes on holding it, with its lease or released as it is: the
        // directory knows nothing of its holders.
        let held: Vec<Placement> = guard
            .placements
            .extract_if(|_, placement| placement.held())
            .map(|(_, placement)| placement)
            .collect();
        let state = shared.read_directory(owned, held);
        if owned {
            for info in state.trees.values() {
                let _ = shared.pin(info);
            }
        }
        *guard = state;
        Ok(())
    }
}
