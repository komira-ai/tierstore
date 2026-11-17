//! The one walk of a version 2 tree list: each directory's children checked, the span of each new
//! digest handed to the caller as its file is checked; then the directories hashed, children
//! first. See [`tree_list_v2`](crate::tree_list_v2).

use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher, RandomState};

use crate::tree_list::{EMPTY_SHA256, MAX_PATH_BYTES, MAX_TARGET_BYTES, MAX_TREE_LIST_BYTES};
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
    /// The number of the key of the object that holds the bytes, in
    /// [`TreeListV2::keys`](crate::TreeListV2::keys).
    pub key: u32,
    /// The offset of the first byte in that object.
    pub offset: u64,
    /// The length, above 0.
    pub size: u64,
    /// The SHA-256 of the bytes.
    pub sha256: TreeDigest,
}

/// The keys of a set of SHA-256 digests. A digest is uniform already, so its words are mixed with
/// keys drawn once per set rather than hashed again: cheap, and, the keys being unknown to whoever
/// wrote the list, not a set an author of the list can fill with colliding digests on purpose.
#[derive(Clone, Copy)]
struct DigestKeys([u64; 4]);

impl DigestKeys {
    fn new() -> Self {
        let state = RandomState::new();
        Self([0u64, 1, 2, 3].map(|word| state.hash_one(word)))
    }
}

impl BuildHasher for DigestKeys {
    type Hasher = DigestHasher;

    fn build_hasher(&self) -> DigestHasher {
        DigestHasher {
            keys: self.0,
            state: 0,
        }
    }
}

/// The hasher of [`DigestKeys`].
struct DigestHasher {
    keys: [u64; 4],
    state: u64,
}

impl Hasher for DigestHasher {
    fn write(&mut self, bytes: &[u8]) {
        for (at, chunk) in bytes.chunks(8).enumerate() {
            let mut word = [0; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            let word = u64::from_le_bytes(word) ^ self.keys[at % 4];
            self.state = (self.state ^ word)
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .rotate_left(29);
        }
    }

    fn finish(&self) -> u64 {
        self.state
    }
}

/// The name, and the depth and path length, of every entry, in the order of the list.
pub(crate) type Walked<'a> = (Vec<&'a str>, Vec<(u32, u32)>);

/// A list [`TreeListV2::check`] walked and found good, with what the walk learnt of each entry.
pub struct Checked<'l, 'a> {
    list: &'l TreeListV2<'a>,
    names: Vec<&'a str>,
    depths: Vec<(u32, u32)>,
}

impl Checked<'_, '_> {
    /// The tree of the list: the directories hashed, children before parents, into the records
    /// the tree store keeps, and the files in the order of the list.
    #[must_use]
    pub fn build(self) -> BuiltTree {
        self.list.finish(&self.names, &self.depths)
    }
}

/// The names checked so far, by where they begin in the names: a list's names are each stored
/// once and shared, so most entries name one already checked.
struct Names<'a> {
    blob: &'a [u8],
    /// For each offset of the blob (and its end), one more than the number in `checked` of the
    /// name checked there, or 0.
    at: Vec<u32>,
    checked: Vec<&'a str>,
}

impl<'a> Names<'a> {
    fn new(blob: &'a [u8]) -> Self {
        Self {
            blob,
            at: vec![0; blob.len() + 1],
            checked: Vec::new(),
        }
    }

    /// The `len` bytes at `start` as a name an entry may have.
    fn get(&mut self, start: usize, len: usize) -> Result<&'a str, EntryFault> {
        let bytes = self
            .blob
            .get(start..start + len)
            .ok_or(EntryFault::NameBounds)?;
        let known = self.at[start] as usize;
        if known > 0 && self.checked[known - 1].len() == len {
            return Ok(self.checked[known - 1]);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| EntryFault::BadName)?;
        if !is_valid_name(text) {
            return Err(EntryFault::BadName);
        }
        self.checked.push(text);
        self.at[start] = self.checked.len() as u32;
        Ok(text)
    }
}

/// A digest folded to 64 bits, for the set of digests whose span was handed out. Two digests that
/// fold alike leave the second unread until it is opened, never a wrong byte read: each read is
/// checked against its own digest.
fn fold(digest: &[u8; 32]) -> u64 {
    // Each word by its own odd multiplier, so that no pattern of equal words cancels out.
    let word = |at: usize, by: u64| le_u64(digest, at).wrapping_mul(by);
    word(0, 0x9e37_79b9_7f4a_7c15)
        ^ word(8, 0xc2b2_ae3d_27d4_eb4f)
        ^ word(16, 0x1656_67b1_9e37_79f9)
        ^ word(24, 0xff51_afd7_ed55_8ccd)
}

/// A set of digests.
fn digest_set<T>(capacity: usize) -> HashSet<T, DigestKeys> {
    HashSet::with_capacity_and_hasher(capacity, DigestKeys::new())
}

/// Which spans a walk hands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Spans {
    /// One for each digest, its first file's: the span the tree store keeps for a digest of a
    /// tree of records.
    PerDigest,
    /// One for each distinct span of a digest: a tree served from its list reads each file at
    /// its own span, so every span a file has is read once.
    PerSpan,
    /// None: the list is only checked.
    None,
}

/// What a walk counted of the files of a list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counted {
    /// The files, of any size.
    pub files: u64,
    /// Their bytes together, at most `u64::MAX`.
    pub bytes: u64,
}

/// The fields of one 32-byte entry record.
#[derive(Clone, Copy)]
pub(crate) struct Record {
    pub(crate) parent: u32,
    pub(crate) name_off: u32,
    /// A file's key index, a directory's first child.
    pub(crate) x: u32,
    pub(crate) kind: u8,
    mode: u16,
    pub(crate) name_len: u8,
    /// A file's offset, a directory's child count, a link target's offset.
    pub(crate) a: u64,
    /// A file's size, a link target's length.
    pub(crate) b: u64,
}

impl Record {
    pub(crate) fn of(bytes: &[u8]) -> Self {
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
    pub(crate) fn mode(self) -> Mode {
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

    /// Walks the entries once and makes the tree of them: [`check`](Self::check), then the
    /// directories hashed, children before parents, into the records the tree store keeps.
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
        on_span: impl FnMut(ListSpan),
    ) -> Result<BuiltTree, TreeListV2Error> {
        Ok(self.check(limits, on_span)?.build())
    }

    /// Walks the entries once, each directory's children in order, and checks each child; calls
    /// `on_span` with the span of every file of size above 0 whose digest it has not met before,
    /// as soon as the file is checked. The list is then a tree as it is: an entry's number names
    /// it, a directory's children are a slice of the entries, and a name is found among them by a
    /// binary search. The spans handed out before a fault are not taken back. What it gives makes
    /// the tree store's records of the tree ([`Checked::build`]).
    pub fn check(
        &self,
        limits: &TreeLimits,
        on_span: impl FnMut(ListSpan),
    ) -> Result<Checked<'_, 'a>, TreeListV2Error> {
        let ((names, depths), _) = self.walk(limits, Spans::PerDigest, on_span)?;
        Ok(Checked {
            list: self,
            names,
            depths,
        })
    }

    /// The walk of [`check`](Self::check), giving the name, and the depth and path length, of
    /// every entry, in the order of the list, and the count of the files and their bytes; it
    /// hands out the spans `spans` asks for. A list it accepts is one whose entries may be read in
    /// place: every directory's children are the entries `first..first + count`, sorted by name,
    /// and every name, target, key number and span is within the list.
    pub(crate) fn walk(
        &self,
        limits: &TreeLimits,
        spans: Spans,
        mut on_span: impl FnMut(ListSpan),
    ) -> Result<(Walked<'a>, Counted), TreeListV2Error> {
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
        // The depth and path length, and the name, of each entry, pushed as its directory is
        // walked: the children of the directories in order are the entries in order.
        let mut depths = Vec::with_capacity(count);
        depths.push((0u32, 0u32));
        let mut names = Vec::with_capacity(count);
        names.push("");
        let mut spanned = digest_set::<u64>(if spans == Spans::None { 0 } else { count });
        let mut counted = Counted::default();
        let mut checked = Names::new(self.names);
        // The bytes of every path together: names are shared, so a small object could name far
        // more path bytes than it holds, which `finish` makes into strings.
        let mut path_bytes = 0usize;
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
                self.child(
                    dir,
                    child,
                    &mut previous,
                    (&mut depths, &mut names),
                    &mut checked,
                )
                .map_err(|fault| TreeListV2Error::Entry {
                    index: child as u32,
                    fault,
                })?;
                path_bytes += depths[child].1 as usize;
                if path_bytes > MAX_TREE_LIST_BYTES {
                    return Err(TreeListV2Error::PathBytes {
                        max: MAX_TREE_LIST_BYTES,
                    });
                }
                if depths[child].0 as usize > limits.max_depth {
                    return refuse(RecordError::TooDeep(self.path(child)));
                }
                let record = self.record(child);
                if record.kind != KIND_FILE {
                    continue;
                }
                counted.files += 1;
                counted.bytes = counted.bytes.saturating_add(record.b);
                if record.b > 0 && spans != Spans::None {
                    let digest = self.digest(child);
                    let key = match spans {
                        // Each of the two by its own odd multiplier, as in `fold`.
                        Spans::PerSpan => {
                            fold(&digest)
                                ^ u64::from(record.x).wrapping_mul(0xd6e8_feb8_6659_fd93)
                                ^ record.a.wrapping_mul(0xa076_1d64_78bd_642f)
                        }
                        _ => fold(&digest),
                    };
                    if spanned.insert(key) {
                        on_span(ListSpan {
                            key: record.x,
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
        Ok(((names, depths), counted))
    }

    /// Checks entry `child` of directory `dir`, whose name must sort after `previous`.
    fn child(
        &self,
        dir: usize,
        child: usize,
        previous: &mut &'a [u8],
        (depths, names): (&mut Vec<(u32, u32)>, &mut Vec<&'a str>),
        checked: &mut Names<'a>,
    ) -> Result<(), EntryFault> {
        let record = self.record(child);
        if record.parent as usize != dir {
            return Err(EntryFault::Parent(record.parent));
        }
        if record.kind > KIND_SYMLINK {
            return Err(EntryFault::UnknownKind(record.kind));
        }
        let text = checked.get(record.name_off as usize, record.name_len as usize)?;
        let name = text.as_bytes();
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
        depths.push((depth + 1, len));
        names.push(text);
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
    fn finish(&self, names: &[&'a str], depths: &[(u32, u32)]) -> BuiltTree {
        let count = self.len();
        let mut digests = vec![[0u8; 32]; count];
        let mut records = Vec::new();
        let mut seen = digest_set(count / 8);
        for dir in (0..count).rev() {
            let record = self.record(dir);
            if record.kind != KIND_DIR {
                continue;
            }
            let first = record.x as usize;
            let children = (first..first + record.a as usize)
                .map(|index| {
                    let child = self.record(index);
                    let name = names[index].to_owned();
                    match child.kind {
                        KIND_DIR => RecordChild::Dir {
                            name,
                            digest: TreeDigest::from_bytes(digests[index]),
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
            digests[dir] = *digest.as_bytes();
            if seen.insert(digest) {
                records.push((digest, tree));
            }
        }
        // The path of each directory, as where it is in one string of them all.
        let mut dir_paths = String::new();
        let (mut path_at, mut path_len) = (vec![0u32; count], vec![0u32; count]);
        let mut files = Vec::new();
        for dir in 0..count {
            let record = self.record(dir);
            if record.kind != KIND_DIR {
                continue;
            }
            let first = record.x as usize;
            for index in first..first + record.a as usize {
                let child = self.record(index);
                let start = path_at[dir] as usize;
                let parent = &dir_paths[start..start + path_len[dir] as usize];
                let mut path = String::with_capacity(depths[index].1 as usize);
                if dir != 0 {
                    path.push_str(parent);
                    path.push('/');
                }
                path.push_str(names[index]);
                match child.kind {
                    KIND_DIR => {
                        // The whole of every path is at most 256 MiB: the walk refused more.
                        path_at[index] = dir_paths.len() as u32;
                        path_len[index] = path.len() as u32;
                        dir_paths.push_str(&path);
                    }
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
            root: TreeDigest::from_bytes(digests[0]),
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
