//! The one walk of an L2 list's entries at open: each directory's children checked in the
//! breadth-first order the format says, by the rules of a version 2 list, and each file's bytes
//! checked to lie within its pack.

use crate::error::{EntryFault, Malformed};
use crate::layout::{
    EMPTY_SHA256, KIND_DIR, KIND_FILE, KIND_SYMLINK, MAX_PATH_BYTES, MAX_TARGET_BYTES, RECORD_LEN,
    Record, Section, digest_at,
};
use crate::open::{Header, pack_size};

/// What the walk learnt of the tree: what a caller holds a tree to its own limits by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    /// The entries, the root included.
    pub entries: u32,
    /// The files, of any size.
    pub files: u32,
    /// Their bytes together, at most `u64::MAX`.
    pub file_bytes: u64,
    /// The depth of the deepest entry (the root's children are at depth 1).
    pub max_depth: u32,
    /// The most children of one directory.
    pub max_children: u32,
    /// The bytes of every entry's path together: names are shared, so a small list can name
    /// paths of many more bytes than it holds.
    pub path_bytes: u64,
}

/// The names checked so far, by where they begin in the names: a list's names are each stored
/// once and shared, so most entries name one already checked.
struct Names<'a> {
    blob: &'a [u8],
    /// For each offset of the blob, the length of the name checked there, or 0: a name is 1 to
    /// 255 bytes.
    at: Vec<u8>,
}

impl<'a> Names<'a> {
    fn new(blob: &'a [u8]) -> Self {
        Self {
            blob,
            at: vec![0; blob.len()],
        }
    }

    /// The `len` bytes at `start`, if they are a name an entry may have: not empty, not `.` or
    /// `..`, UTF-8, with no `/` and no NUL.
    fn get(&mut self, start: usize, len: u8) -> Result<&'a [u8], EntryFault> {
        let bytes = self
            .blob
            .get(start..start + usize::from(len))
            .ok_or(EntryFault::NameBounds)?;
        if len > 0 && self.at[start] == len {
            return Ok(bytes);
        }
        let valid = std::str::from_utf8(bytes).is_ok_and(|name| {
            !(name.is_empty()
                || name == "."
                || name == ".."
                || name.bytes().any(|byte| byte == b'/' || byte == 0))
        });
        if !valid {
            return Err(EntryFault::BadName);
        }
        // Not empty: a valid name has a byte, so `start` is within the blob.
        self.at[start] = len;
        Ok(bytes)
    }
}

/// The sections the walk reads.
struct Walk<'a> {
    entries: &'a [u8],
    digests: &'a [u8],
    packs: &'a [u8],
    pack_count: u32,
    names: &'a [u8],
}

impl Walk<'_> {
    fn record(&self, index: usize) -> Record {
        Record::of(&self.entries[index * RECORD_LEN..(index + 1) * RECORD_LEN])
    }

    /// Checks entry `child` of directory `dir`, whose name must sort after `previous`, and gives
    /// its depth and path length.
    fn child<'n>(
        &self,
        dir: usize,
        child: usize,
        previous: &mut &'n [u8],
        parent: (u32, u32),
        names: &mut Names<'n>,
    ) -> Result<(u32, u32), EntryFault> {
        let record = self.record(child);
        if record.parent as usize != dir {
            return Err(EntryFault::Parent(record.parent));
        }
        if record.kind > KIND_SYMLINK {
            return Err(EntryFault::UnknownKind(record.kind));
        }
        let name = names.get(record.name_off as usize, record.name_len)?;
        if *previous >= name {
            return Err(EntryFault::NotAscending);
        }
        *previous = name;
        let (depth, len) = parent;
        // A path is at most 4096 bytes and a name 255, so neither sum overflows.
        let len = match dir {
            0 => name.len() as u32,
            _ => len + 1 + name.len() as u32,
        };
        if len as usize > MAX_PATH_BYTES {
            return Err(EntryFault::PathTooLong);
        }
        match record.kind {
            KIND_FILE
                if record.b == 0 && digest_at(self.digests, child * RECORD_LEN) != EMPTY_SHA256 =>
            {
                return Err(EntryFault::EmptyDigest);
            }
            KIND_FILE if record.b == 0 => {}
            KIND_FILE => {
                if record.x >= self.pack_count {
                    return Err(EntryFault::Pack(record.x));
                }
                let size = pack_size(self.packs, record.x);
                if record.a.checked_add(record.b).is_none_or(|end| end > size) {
                    return Err(EntryFault::OutsidePack);
                }
            }
            KIND_SYMLINK => {
                let target = usize::try_from(record.a)
                    .ok()
                    .zip(usize::try_from(record.b).ok())
                    .filter(|(_, len)| (1..=MAX_TARGET_BYTES).contains(len))
                    .and_then(|(start, len)| self.names.get(start..start.checked_add(len)?))
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .filter(|target| !target.contains('\0'));
                if target.is_none() {
                    return Err(EntryFault::Target);
                }
            }
            _ => {}
        }
        Ok((depth + 1, len))
    }
}

/// Walks the entries of a list whose header, packs and ranges are checked: every directory's
/// children are the entries `first..first + count`, right after the children of the directories
/// before it, each with the directory as its parent, sorted by name; every name, target, pack
/// number and file span is within the list. Gives the tree's [`Shape`].
pub(crate) fn walk(bytes: &[u8], header: &Header) -> Result<Shape, Malformed> {
    let packs = header.place(Section::Packs);
    let walk = Walk {
        entries: header.place(Section::Entries).of(bytes),
        digests: header.place(Section::Digests).of(bytes),
        packs: packs.of(bytes),
        pack_count: packs.count,
        names: header.place(Section::Names).of(bytes),
    };
    let count = header.place(Section::Entries).count as usize;
    let root = walk.record(0);
    if root.kind != KIND_DIR || root.parent != 0 || root.name_len != 0 {
        return Err(Malformed::Entry {
            index: 0,
            fault: EntryFault::Root,
        });
    }
    let mut shape = Shape {
        entries: count as u32,
        ..Shape::default()
    };
    // The depth and path length of each entry, pushed as its directory is walked: the children of
    // the directories in order are the entries in order.
    let mut depths: Vec<(u32, u32)> = Vec::with_capacity(count);
    depths.push((0, 0));
    let mut names = Names::new(walk.names);
    let mut next = 1usize;
    for dir in 0..count {
        let record = walk.record(dir);
        if record.kind != KIND_DIR {
            continue;
        }
        let fault = |fault| Malformed::Entry {
            index: dir as u32,
            fault,
        };
        if next <= dir {
            return Err(fault(EntryFault::Unreachable));
        }
        if record.x as usize != next || record.a > (count - next) as u64 {
            return Err(fault(EntryFault::Children));
        }
        let first = next;
        // At most the entries left: below 2^32.
        next += record.a as usize;
        shape.max_children = shape.max_children.max(record.a as u32);
        let mut previous: &[u8] = &[];
        for child in first..next {
            let (depth, len) = walk
                .child(dir, child, &mut previous, depths[dir], &mut names)
                .map_err(|fault| Malformed::Entry {
                    index: child as u32,
                    fault,
                })?;
            depths.push((depth, len));
            shape.max_depth = shape.max_depth.max(depth);
            shape.path_bytes += u64::from(len);
            let record = walk.record(child);
            if record.kind == KIND_FILE {
                shape.files += 1;
                shape.file_bytes = shape.file_bytes.saturating_add(record.b);
            }
        }
    }
    if next != count {
        return Err(Malformed::Orphans(next as u32));
    }
    Ok(shape)
}
