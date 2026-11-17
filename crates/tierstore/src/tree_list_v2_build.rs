//! The one walk of a version 2 tree list: each directory's children checked, the span of each new
//! digest handed to the caller as its file is checked; then the directories hashed, children
//! first. See [`tree_list_v2`](crate::tree_list_v2).

use std::collections::HashSet;
use std::sync::Arc;

use crate::tree_list::{EMPTY_SHA256, MAX_PATH_BYTES, MAX_TARGET_BYTES};
use crate::tree_list_v2::{
    EntryFault, KIND_DIR, KIND_FILE, KIND_SYMLINK, RECORD_LEN, TreeListV2, TreeListV2Error, le_u16,
    le_u32, le_u64,
};
use crate::tree_manifest::{BlobSource, TreeDigest, is_valid_name};
use crate::tree_record::{
    BuiltFile, BuiltTree, Mode, RecordChild, RecordError, TreeLimits, TreeRecord,
};

/// A span of a pack to read: the bytes of a file of the list, the first file of its digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListSpan {
    /// The key of the object that holds the bytes.
    pub key: Arc<str>,
    /// The offset of the first byte in that object.
    pub offset: u64,
    /// The length, above 0.
    pub size: u64,
    /// The SHA-256 of the bytes.
    pub sha256: TreeDigest,
}

/// The fields of one 32-byte entry record.
#[derive(Clone, Copy)]
struct Record {
    parent: u32,
    name_off: u32,
    /// A file's key index, a directory's first child.
    x: u32,
    kind: u8,
    mode: u16,
    name_len: u8,
    /// A file's offset, a directory's child count, a link target's offset.
    a: u64,
    /// A file's size, a link target's length.
    b: u64,
}

impl Record {
    fn of(bytes: &[u8]) -> Self {
        let kind_mode = le_u16(bytes, 12);
        Self {
            parent: le_u32(bytes, 0),
            name_off: le_u32(bytes, 4),
            x: le_u32(bytes, 8),
            kind: (kind_mode >> 12) as u8,
            mode: kind_mode & 0o7777,
            name_len: bytes[14],
            a: le_u64(bytes, 16),
            b: le_u64(bytes, 24),
        }
    }

    /// The mode: twelve bits are never above 07777.
    fn mode(self) -> Mode {
        Mode::new(self.mode).unwrap_or(Mode::FILE)
    }
}

impl<'a> TreeListV2<'a> {
    fn record(&self, index: usize) -> Record {
        Record::of(&self.entries[index * RECORD_LEN..(index + 1) * RECORD_LEN])
    }

    fn digest(&self, index: usize) -> [u8; 32] {
        let mut digest = [0; 32];
        digest.copy_from_slice(&self.digests[index * RECORD_LEN..(index + 1) * RECORD_LEN]);
        digest
    }

    /// The name of an entry whose name the walk has checked.
    fn name(&self, record: Record) -> &'a str {
        let start = record.name_off as usize;
        let bytes = &self.names[start..start + record.name_len as usize];
        std::str::from_utf8(bytes).unwrap_or_default()
    }

    /// The path of an entry whose ancestors the walk has checked: for a refusal.
    fn path(&self, index: usize) -> String {
        let mut names = Vec::new();
        let mut at = index;
        while at != 0 {
            let record = self.record(at);
            names.push(self.name(record));
            at = record.parent as usize;
        }
        names.reverse();
        names.join("/")
    }

    /// Walks the entries once, each directory's children in order: checks each child, calls
    /// `on_span` with the span of every file of size above 0 whose digest it has not met before,
    /// and records each entry's depth. Then hashes the directories, children before parents.
    ///
    /// The files of the result are in the order of the list. The span handed out for a digest is
    /// that of its first file in that order, which is the span the tree store keeps for the digest
    /// (`sources_of`), so the read it starts is the one an open of any file of the digest joins.
    ///
    /// The spans handed out before a fault are not taken back: a caller that reads them reads
    /// bytes it checks against their digest, so a refused list costs reads, never a wrong byte.
    pub fn build(
        &self,
        limits: &TreeLimits,
        mut on_span: impl FnMut(ListSpan),
    ) -> Result<BuiltTree, TreeListV2Error> {
        let count = self.len();
        let refuse = |error| Err(TreeListV2Error::Tree(error));
        if (count - 1) as u64 > limits.max_entries {
            return refuse(RecordError::TooManyEntries(limits.max_entries));
        }
        let root = self.record(0);
        if root.kind != KIND_DIR || root.parent != 0 || root.name_len != 0 {
            return Err(TreeListV2Error::Entry {
                index: 0,
                fault: EntryFault::Root,
            });
        }
        // The depth and the path length of each entry, set as its directory is walked.
        let mut depths = vec![(0u32, 0u32); count];
        let mut spanned = HashSet::new();
        let mut next = 1;
        for dir in 0..count {
            let record = self.record(dir);
            if record.kind != KIND_DIR {
                continue;
            }
            let fault = |fault| TreeListV2Error::Entry {
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
            next += record.a as usize;
            if next - first > limits.max_dir_entries {
                return refuse(RecordError::DirectoryTooLarge(self.path(dir)));
            }
            let mut previous: &[u8] = &[];
            for child in first..next {
                self.child(dir, child, &mut previous, &mut depths)
                    .map_err(|fault| TreeListV2Error::Entry {
                        index: child as u32,
                        fault,
                    })?;
                if depths[child].0 as usize > limits.max_depth {
                    return refuse(RecordError::TooDeep(self.path(child)));
                }
                let record = self.record(child);
                if record.kind == KIND_FILE && record.b > 0 {
                    let digest = self.digest(child);
                    if spanned.insert(digest) {
                        on_span(ListSpan {
                            key: self.keys[record.x as usize].clone(),
                            offset: record.a,
                            size: record.b,
                            sha256: TreeDigest::from_bytes(digest),
                        });
                    }
                }
            }
        }
        if next != count {
            return Err(TreeListV2Error::Orphans(next as u32));
        }
        Ok(self.finish())
    }

    /// Checks entry `child` of directory `dir`, whose name must sort after `previous`.
    fn child(
        &self,
        dir: usize,
        child: usize,
        previous: &mut &'a [u8],
        depths: &mut [(u32, u32)],
    ) -> Result<(), EntryFault> {
        let record = self.record(child);
        if record.parent as usize != dir {
            return Err(EntryFault::Parent(record.parent));
        }
        if record.kind > KIND_SYMLINK {
            return Err(EntryFault::UnknownKind(record.kind));
        }
        let start = record.name_off as usize;
        let name = self
            .names
            .get(start..start + record.name_len as usize)
            .ok_or(EntryFault::NameBounds)?;
        let text = std::str::from_utf8(name).map_err(|_| EntryFault::BadName)?;
        if !is_valid_name(text) {
            return Err(EntryFault::BadName);
        }
        if *previous >= name {
            return Err(EntryFault::NotAscending);
        }
        *previous = name;
        let (depth, len) = depths[dir];
        // A path is at most 4096 bytes and a name 255, so neither sum overflows.
        let len = match dir {
            0 => name.len() as u32,
            _ => len + 1 + name.len() as u32,
        };
        if len as usize > MAX_PATH_BYTES {
            return Err(EntryFault::PathTooLong);
        }
        depths[child] = (depth + 1, len);
        match record.kind {
            KIND_FILE if record.b == 0 && self.digest(child) != EMPTY_SHA256 => {
                return Err(EntryFault::EmptyDigest);
            }
            KIND_FILE if record.b == 0 => {}
            KIND_FILE => {
                if record.x as usize >= self.keys.len() {
                    return Err(EntryFault::Key(record.x));
                }
                if record.a.checked_add(record.b).is_none() {
                    return Err(EntryFault::RangeOverflow);
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
        Ok(())
    }

    /// The tree of the walked entries: the directories hashed from the last to the root, the
    /// files in the order of the list.
    fn finish(&self) -> BuiltTree {
        let count = self.len();
        let mut digests = vec![TreeDigest::from_bytes([0; 32]); count];
        let mut records = Vec::new();
        let mut seen = HashSet::new();
        for dir in (0..count).rev() {
            let record = self.record(dir);
            if record.kind != KIND_DIR {
                continue;
            }
            let first = record.x as usize;
            let children = (first..first + record.a as usize)
                .map(|index| {
                    let child = self.record(index);
                    let name = self.name(child).to_owned();
                    match child.kind {
                        KIND_DIR => RecordChild::Dir {
                            name,
                            digest: digests[index],
                        },
                        KIND_FILE => RecordChild::File {
                            name,
                            mode: child.mode(),
                            size: child.b,
                            sha256: TreeDigest::from_bytes(self.digest(index)),
                        },
                        _ => RecordChild::Symlink {
                            name,
                            target: self.target(child).to_owned(),
                        },
                    }
                })
                .collect();
            let tree = TreeRecord::from_sorted(record.mode(), children);
            let digest = tree.digest();
            digests[dir] = digest;
            if seen.insert(digest) {
                records.push((digest, tree));
            }
        }
        let mut paths = vec![String::new(); count];
        let mut files = Vec::new();
        for dir in 0..count {
            let record = self.record(dir);
            if record.kind != KIND_DIR {
                continue;
            }
            let first = record.x as usize;
            for index in first..first + record.a as usize {
                let child = self.record(index);
                let path = match dir {
                    0 => self.name(child).to_owned(),
                    _ => format!("{}/{}", paths[dir], self.name(child)),
                };
                match child.kind {
                    KIND_DIR => paths[index] = path,
                    KIND_FILE => files.push(BuiltFile {
                        path,
                        sha256: TreeDigest::from_bytes(self.digest(index)),
                        size: child.b,
                        source: (child.b > 0).then(|| BlobSource {
                            key: self.keys[child.x as usize].clone(),
                            offset: child.a,
                        }),
                    }),
                    _ => {}
                }
            }
        }
        BuiltTree {
            root: digests[0],
            records,
            files,
        }
    }

    /// The target of a link the walk has checked.
    fn target(&self, record: Record) -> &'a str {
        let start = record.a as usize;
        let bytes = &self.names[start..start + record.b as usize];
        std::str::from_utf8(bytes).unwrap_or_default()
    }
}
