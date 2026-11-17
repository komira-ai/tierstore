//! The one walk of a version 2 tree list: each record checked, its span handed to the caller to
//! read, and its entry put in the tree; then the directories hashed, children first. See
//! [`tree_list_v2`](crate::tree_list_v2).

use std::collections::HashSet;
use std::sync::Arc;

use crate::tree_list::EMPTY_SHA256;
use crate::tree_list_v2::{
    KIND_DIR, KIND_FILE, KIND_SYMLINK, NONE, RecordFault, StringFault, TreeListV2, TreeListV2Error,
    le_u64, read_u32, valid_name, valid_target,
};
use crate::tree_manifest::{BlobSource, TreeDigest};
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

/// One record, its fields read and not yet judged.
struct Fields<'a> {
    kind: u8,
    mode: u16,
    name: u32,
    parent: u32,
    key: u32,
    size: u64,
    offset: u64,
    sha256: &'a [u8],
}

impl<'a> Fields<'a> {
    /// The fields of a record of at least 64 bytes.
    fn of(record: &'a [u8]) -> Self {
        let word = |at| read_u32(record, at).unwrap_or(NONE);
        Self {
            kind: record[0],
            mode: u16::from_le_bytes([record[2], record[3]]),
            name: word(4),
            parent: word(8),
            key: word(12),
            size: le_u64(&record[16..24]),
            offset: le_u64(&record[24..32]),
            sha256: &record[32..64],
        }
    }
}

/// A child of a directory while the tree is built, named by its name's number.
enum Child {
    /// A directory, by its number among the directories (the root is 0).
    Dir(usize),
    File {
        mode: Mode,
        size: u64,
        sha256: TreeDigest,
    },
    Symlink(u32),
}

/// A directory while the tree is built: the root is 0, the directory of record `i` is `i + 1`.
struct Dir {
    mode: Mode,
    /// The directory it is in, and its name; none for the root.
    parent: Option<(usize, u32)>,
    /// How many names from the root.
    depth: usize,
    children: Vec<(u32, Child)>,
}

/// A file of the list, until the names are checked and its path can be made.
struct File {
    dir: usize,
    name: u32,
    size: u64,
    sha256: TreeDigest,
    source: Option<BlobSource>,
}

/// The walk: what the records so far have made.
struct Walk<'l, 'a> {
    list: &'l TreeListV2<'a>,
    limits: &'l TreeLimits,
    dirs: Vec<Dir>,
    files: Vec<File>,
    /// The digests whose span was handed out.
    spanned: HashSet<[u8; 32]>,
    /// The kind of the record before, and the key and offset of the file before.
    last_kind: u8,
    last_place: (u32, u64),
}

impl<'a> TreeListV2<'a> {
    /// Walks the records once, in order: checks each, calls `on_span` with the span of every file
    /// with bytes whose digest it has not met before (pack by pack, ascending offset), and puts
    /// each entry in the tree. Then checks the name and target tables, and hashes the directories.
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
        if u64::from(self.count) > limits.max_entries {
            return Err(TreeListV2Error::Tree(RecordError::TooManyEntries(
                limits.max_entries,
            )));
        }
        let mut walk = Walk {
            list: self,
            limits,
            dirs: vec![Dir {
                mode: Mode::DIR,
                parent: None,
                depth: 0,
                children: Vec::new(),
            }],
            files: Vec::with_capacity(self.len()),
            spanned: HashSet::new(),
            last_kind: KIND_DIR,
            last_place: (0, 0),
        };
        for (index, record) in self.records.chunks_exact(self.record_len).enumerate() {
            // `open` bounded the count to a `u32`.
            let index = index as u32;
            walk.record(&Fields::of(record), &mut on_span)
                .map_err(|fault| TreeListV2Error::Record { index, fault })?;
        }
        self.names.check(valid_name, StringFault::BadName)?;
        self.targets.check(valid_target, StringFault::BadTarget)?;
        walk.finish()
    }
}

impl Walk<'_, '_> {
    /// Checks one record and puts its entry in the tree.
    fn record(
        &mut self,
        fields: &Fields<'_>,
        on_span: &mut impl FnMut(ListSpan),
    ) -> Result<(), RecordFault> {
        let kind = fields.kind;
        if !(KIND_DIR..=KIND_FILE).contains(&kind) {
            return Err(RecordFault::UnknownKind(kind));
        }
        if kind < self.last_kind {
            return Err(RecordFault::KindOrder);
        }
        self.last_kind = kind;
        if fields.name >= self.list.names.len() {
            return Err(RecordFault::Name(fields.name));
        }
        // The directories come first, each after its parent: a parent is a directory exactly
        // when its number is below the count of directories so far.
        let dir = match fields.parent {
            NONE => 0,
            parent if (parent as usize) < self.dirs.len() - 1 => parent as usize + 1,
            parent => return Err(RecordFault::Parent(parent)),
        };
        let child = match kind {
            KIND_DIR => {
                let mode = mode_of(fields.mode)?;
                self.dirs.push(Dir {
                    mode,
                    parent: Some((dir, fields.name)),
                    depth: self.dirs[dir].depth + 1,
                    children: Vec::new(),
                });
                Child::Dir(self.dirs.len() - 1)
            }
            KIND_SYMLINK => {
                if fields.key >= self.list.targets.len() {
                    return Err(RecordFault::Target(fields.key));
                }
                Child::Symlink(fields.key)
            }
            _ => self.file(dir, fields, on_span)?,
        };
        self.dirs[dir].children.push((fields.name, child));
        Ok(())
    }

    /// Checks a file record, hands out its span if its digest is new, and keeps it.
    fn file(
        &mut self,
        dir: usize,
        fields: &Fields<'_>,
        on_span: &mut impl FnMut(ListSpan),
    ) -> Result<Child, RecordFault> {
        let mode = mode_of(fields.mode)?;
        let mut digest = [0; 32];
        digest.copy_from_slice(fields.sha256);
        let place = (fields.key, fields.offset);
        // The first file compares with (0, 0), which no file sorts before.
        if place < self.last_place {
            return Err(RecordFault::Unsorted);
        }
        self.last_place = place;
        let source = match (fields.key, fields.size) {
            (NONE, 0) => {
                if digest != EMPTY_SHA256 {
                    return Err(RecordFault::EmptyDigest);
                }
                None
            }
            (NONE, _) | (_, 0) => return Err(RecordFault::KeyAndSize),
            (key, size) => {
                let key = self
                    .list
                    .keys
                    .get(key as usize)
                    .ok_or(RecordFault::Key(key))?;
                if fields.offset.checked_add(size).is_none() {
                    return Err(RecordFault::RangeOverflow);
                }
                if self.spanned.insert(digest) {
                    on_span(ListSpan {
                        key: key.clone(),
                        offset: fields.offset,
                        size,
                        sha256: TreeDigest::from_bytes(digest),
                    });
                }
                Some(BlobSource {
                    key: key.clone(),
                    offset: fields.offset,
                })
            }
        };
        let sha256 = TreeDigest::from_bytes(digest);
        self.files.push(File {
            dir,
            name: fields.name,
            size: fields.size,
            sha256,
            source,
        });
        Ok(Child::File {
            mode,
            size: fields.size,
            sha256,
        })
    }

    /// The path of directory `dir`, from the names, which are checked: for a refusal or a file.
    fn path(&self, dir: usize, name: Option<u32>) -> String {
        let mut names = Vec::new();
        names.extend(name);
        let mut at = dir;
        while let Some((parent, name)) = self.dirs[at].parent {
            names.push(name);
            at = parent;
        }
        let names: Vec<&str> = names
            .iter()
            .rev()
            .map(|n| self.list.names.get(*n))
            .collect();
        names.join("/")
    }

    /// Checks the depth and size of every directory and that no name is twice in one, and hashes
    /// the directories, children before parents and the root last.
    fn finish(mut self) -> Result<BuiltTree, TreeListV2Error> {
        let refuse = |error| Err(TreeListV2Error::Tree(error));
        let mut digests = vec![TreeDigest::from_bytes([0; 32]); self.dirs.len()];
        let mut records = Vec::new();
        let mut seen = HashSet::new();
        for at in (0..self.dirs.len()).rev() {
            let mut children = std::mem::take(&mut self.dirs[at].children);
            children.sort_unstable_by_key(|(name, _)| *name);
            let deepest = self.dirs[at].depth + 1;
            if !children.is_empty() && deepest > self.limits.max_depth {
                return refuse(RecordError::TooDeep(self.path(at, Some(children[0].0))));
            }
            if children.len() > self.limits.max_dir_entries {
                return refuse(RecordError::DirectoryTooLarge(self.path(at, None)));
            }
            if let Some(pair) = children.windows(2).find(|pair| pair[0].0 == pair[1].0) {
                return refuse(RecordError::Duplicate(self.path(at, Some(pair[0].0))));
            }
            let names = &self.list.names;
            let targets = &self.list.targets;
            let children = children
                .into_iter()
                .map(|(name, child)| {
                    let name = names.get(name).to_owned();
                    match child {
                        Child::Dir(dir) => RecordChild::Dir {
                            name,
                            digest: digests[dir],
                        },
                        Child::File { mode, size, sha256 } => RecordChild::File {
                            name,
                            mode,
                            size,
                            sha256,
                        },
                        Child::Symlink(target) => RecordChild::Symlink {
                            name,
                            target: targets.get(target).to_owned(),
                        },
                    }
                })
                .collect();
            let record = TreeRecord::from_sorted(self.dirs[at].mode, children);
            let digest = record.digest();
            digests[at] = digest;
            if seen.insert(digest) {
                records.push((digest, record));
            }
        }
        let files = std::mem::take(&mut self.files)
            .into_iter()
            .map(|file| BuiltFile {
                path: self.path(file.dir, Some(file.name)),
                sha256: file.sha256,
                size: file.size,
                source: file.source,
            })
            .collect();
        Ok(BuiltTree {
            root: digests[0],
            records,
            files,
        })
    }
}

/// The mode of `bits`, if it has no bit above 07777.
fn mode_of(bits: u16) -> Result<Mode, RecordFault> {
    Mode::new(bits).map_err(|_| RecordFault::BadMode(bits))
}
