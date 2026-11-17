//! The writer of a version 2 tree list: [`TreeList::encode_v2`]. See
//! [`tree_list_v2`](crate::tree_list_v2) and the reference for the layout.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::tree_list::{EMPTY_SHA256, ListKind, MAX_TREE_LIST_BYTES, TreeList, TreeListError};
use crate::tree_list_v2::{
    HEADER_LEN, KIND_DIR, KIND_FILE, KIND_SYMLINK, MAJOR, RANGE_LEN, RECORD_LEN, TREE_LIST_V2_MAGIC,
};
use crate::tree_manifest::{BlobSource, TreeDigest};
use crate::tree_record::Mode;

/// The gap across which the writer merges the ranges of one pack: two files of a pack whose bytes
/// are at most this far apart are one range.
pub(crate) const RANGE_MERGE_GAP: u64 = 64 * 1024;

/// What an entry of the tree is, as the writer lays it out.
#[derive(Clone, Copy)]
enum Item<'a> {
    Dir(u16),
    File {
        mode: Mode,
        size: u64,
        source: Option<&'a BlobSource>,
        sha256: Option<&'a TreeDigest>,
    },
    Link(&'a str),
}

/// An entry below the root: its path, its name (the last of the path) and what it is.
#[derive(Clone, Copy)]
struct Child<'a> {
    path: &'a str,
    name: &'a str,
    item: Item<'a>,
}

/// An entry in the order of the object: the root (no child) or a child, its parent's number,
/// and, for a directory, where its children begin and how many there are.
struct Placed<'a> {
    child: Option<Child<'a>>,
    parent: u32,
    first: u32,
    count: u32,
}

/// The parent path (empty for the root) and the name of `path`.
fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Puts the entry at `path` among the children of its parent.
fn add<'a>(children: &mut BTreeMap<&'a str, Vec<Child<'a>>>, path: &'a str, item: Item<'a>) {
    let (parent, name) = split(path);
    children
        .entry(parent)
        .or_default()
        .push(Child { path, name, item });
}

/// `at` rounded up to a multiple of 8.
fn align8(at: usize) -> usize {
    at.div_ceil(8) * 8
}

impl TreeList {
    /// The bytes of the list as version 2. Every directory the paths imply is an entry of its own
    /// (mode 0755 unless the list gives one); the entries are laid out breadth first from the
    /// root, each directory's children together and sorted by name; each name is in the name
    /// blob once, each link's target after it; the keys are sorted; the ranges are the files'
    /// bytes sorted by key and offset, merged within [`RANGE_MERGE_GAP`]; every ignored field and
    /// padding byte is zero. The same entries in any order are therefore the same bytes and the
    /// same key.
    ///
    /// Nothing is judged here that the reader judges: a path twice, a name a tree may not have, a
    /// file with a path below it, make bytes the reader refuses. A file of size above 0 with no
    /// `sha256` is refused ([`TreeListError::MissingSha256`]), a name over 255 bytes, which a
    /// record cannot hold ([`TreeListError::NameTooLong`]), and bytes over
    /// [`MAX_TREE_LIST_BYTES`] ([`TreeListError::TooLarge`]).
    pub fn encode_v2(&self) -> Result<Vec<u8>, TreeListError> {
        let mut dirs: BTreeMap<&str, u16> = BTreeMap::new();
        for entry in &self.entries {
            let mut path = entry.path.as_str();
            while let Some((parent, _)) = path.rsplit_once('/') {
                dirs.entry(parent).or_insert(Mode::DIR.bits());
                path = parent;
            }
        }
        let mut children: BTreeMap<&str, Vec<Child<'_>>> = BTreeMap::new();
        let mut keys = BTreeSet::new();
        for entry in &self.entries {
            match &entry.kind {
                ListKind::Dir { mode } => {
                    dirs.insert(&entry.path, mode.bits());
                }
                ListKind::Symlink { target } => add(&mut children, &entry.path, Item::Link(target)),
                ListKind::File {
                    mode,
                    size,
                    source,
                    sha256,
                } => {
                    if *size > 0 && sha256.is_none() {
                        return Err(TreeListError::MissingSha256(entry.path.clone()));
                    }
                    keys.extend(source.as_ref().map(|source| &*source.key));
                    let (source, sha256) = (source.as_ref(), sha256.as_ref());
                    let (mode, size) = (*mode, *size);
                    add(
                        &mut children,
                        &entry.path,
                        Item::File {
                            mode,
                            size,
                            source,
                            sha256,
                        },
                    );
                }
            }
        }
        for (path, mode) in &dirs {
            add(&mut children, path, Item::Dir(*mode));
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| a.name.cmp(b.name));
        }
        let placed = breadth_first(&children);
        let keys: Vec<&str> = keys.into_iter().collect();
        let key_index = |key: &str| keys.binary_search(&key).unwrap_or_default() as u32;
        let mut names = Vec::new();
        let mut interned: HashMap<&str, u32> = HashMap::new();
        let mut entries = Vec::with_capacity(placed.len() * RECORD_LEN);
        let mut digests = Vec::with_capacity(placed.len() * RECORD_LEN);
        let mut spans = Vec::new();
        for node in &placed {
            let mut record = [0u8; RECORD_LEN];
            let mut digest = [0u8; 32];
            record[0..4].copy_from_slice(&node.parent.to_le_bytes());
            let (kind, mode) = match node.child {
                None => (KIND_DIR, Mode::DIR.bits()),
                Some(child) => {
                    if child.name.len() > 255 {
                        return Err(TreeListError::NameTooLong(child.path.to_owned()));
                    }
                    let name_off = *interned.entry(child.name).or_insert_with(|| {
                        let at = names.len() as u32;
                        names.extend_from_slice(child.name.as_bytes());
                        at
                    });
                    record[4..8].copy_from_slice(&name_off.to_le_bytes());
                    record[14] = child.name.len() as u8;
                    match child.item {
                        Item::Dir(mode) => (KIND_DIR, mode),
                        Item::File {
                            mode,
                            size,
                            source,
                            sha256,
                        } => {
                            digest = sha256.map_or(EMPTY_SHA256, |sha256| *sha256.as_bytes());
                            // A file of size 0 has no key: its key and offset stay 0.
                            if let Some(source) = source.filter(|_| size > 0) {
                                let key = key_index(&source.key);
                                record[8..12].copy_from_slice(&key.to_le_bytes());
                                record[16..24].copy_from_slice(&source.offset.to_le_bytes());
                                spans.push((key, source.offset, size));
                            }
                            record[24..32].copy_from_slice(&size.to_le_bytes());
                            (KIND_FILE, mode.bits())
                        }
                        Item::Link(target) => {
                            record[16..24].copy_from_slice(&(names.len() as u64).to_le_bytes());
                            record[24..32].copy_from_slice(&(target.len() as u64).to_le_bytes());
                            names.extend_from_slice(target.as_bytes());
                            (KIND_SYMLINK, 0)
                        }
                    }
                }
            };
            if kind == KIND_DIR {
                record[8..12].copy_from_slice(&node.first.to_le_bytes());
                record[16..24].copy_from_slice(&u64::from(node.count).to_le_bytes());
            }
            let kind_mode = (u16::from(kind) << 12) | mode;
            record[12..14].copy_from_slice(&kind_mode.to_le_bytes());
            entries.extend_from_slice(&record);
            digests.extend_from_slice(&digest);
        }
        let ranges = merged(spans);
        layout(&entries, &digests, &keys, &names, &ranges, placed.len())
    }
}

/// The entries in breadth-first order from the root: each directory's children, which
/// `children` holds by the directory's path, sorted, follow the children of the directories
/// before it.
fn breadth_first<'a>(children: &BTreeMap<&str, Vec<Child<'a>>>) -> Vec<Placed<'a>> {
    let mut placed = vec![Placed {
        child: None,
        parent: 0,
        first: 0,
        count: 0,
    }];
    let mut at = 0;
    while at < placed.len() {
        let path = match placed[at].child {
            None => Some(""),
            Some(Child {
                path,
                item: Item::Dir(_),
                ..
            }) => Some(path),
            Some(_) => None,
        };
        if let Some(path) = path {
            let kids = children.get(path).map_or(&[][..], Vec::as_slice);
            placed[at].first = placed.len() as u32;
            placed[at].count = kids.len() as u32;
            placed.extend(kids.iter().map(|kid| Placed {
                child: Some(*kid),
                parent: at as u32,
                first: 0,
                count: 0,
            }));
        }
        at += 1;
    }
    placed
}

/// The ranges of `spans` ({key, offset, size} of each file with bytes): sorted, and merged where
/// a span of a key begins at most [`RANGE_MERGE_GAP`] after the end of the range before it.
fn merged(mut spans: Vec<(u32, u64, u64)>) -> Vec<(u32, u64, u64)> {
    spans.sort_unstable();
    let mut ranges: Vec<(u32, u64, u64)> = Vec::new();
    for (key, offset, size) in spans {
        let end = offset.saturating_add(size);
        match ranges.last_mut() {
            Some(last) if last.0 == key && offset <= last.2.saturating_add(RANGE_MERGE_GAP) => {
                last.2 = last.2.max(end);
            }
            _ => ranges.push((key, offset, end)),
        }
    }
    ranges
}

/// The object: the header, then each section at the next multiple of 8, zero between them.
fn layout(
    entries: &[u8],
    digests: &[u8],
    keys: &[&str],
    names: &[u8],
    ranges: &[(u32, u64, u64)],
    count: usize,
) -> Result<Vec<u8>, TreeListError> {
    let mut offsets = Vec::with_capacity((keys.len() + 1) * 4);
    let mut end = 0u64;
    offsets.extend_from_slice(&0u32.to_le_bytes());
    for key in keys {
        end += key.len() as u64;
        offsets.extend_from_slice(&(end as u32).to_le_bytes());
    }
    let mut blob = Vec::with_capacity(end as usize);
    for key in keys {
        blob.extend_from_slice(key.as_bytes());
    }
    let mut range_bytes = Vec::with_capacity(ranges.len() * RANGE_LEN);
    for (key, start, end) in ranges {
        range_bytes.extend_from_slice(&key.to_le_bytes());
        range_bytes.extend_from_slice(&0u32.to_le_bytes());
        range_bytes.extend_from_slice(&start.to_le_bytes());
        range_bytes.extend_from_slice(&(end - start).to_le_bytes());
    }
    let sections: [(&[u8], usize); 6] = [
        (entries, count),
        (digests, count),
        (&offsets, keys.len()),
        (&blob, blob.len()),
        (names, names.len()),
        (&range_bytes, ranges.len()),
    ];
    let mut at = HEADER_LEN;
    let mut places = [0usize; 6];
    for (place, (bytes, _)) in places.iter_mut().zip(&sections) {
        *place = align8(at);
        at = *place + bytes.len();
    }
    if at > MAX_TREE_LIST_BYTES {
        return Err(TreeListError::TooLarge {
            size: at,
            max: MAX_TREE_LIST_BYTES,
        });
    }
    // Every offset and count below fits in 32 bits: the whole is at most 256 MiB.
    let mut out = Vec::with_capacity(at);
    out.extend_from_slice(&TREE_LIST_V2_MAGIC);
    out.extend_from_slice(&MAJOR.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for (place, (_, count)) in places.iter().zip(&sections) {
        out.extend_from_slice(&(*place as u32).to_le_bytes());
        out.extend_from_slice(&(*count as u32).to_le_bytes());
    }
    for (place, (bytes, _)) in places.iter().zip(&sections) {
        out.resize(*place, 0);
        out.extend_from_slice(bytes);
    }
    Ok(out)
}
