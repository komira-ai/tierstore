//! The writer of a version 2 tree list: [`TreeList::encode_v2`]. See
//! [`tree_list_v2`](crate::tree_list_v2) and the reference for the layout.

use std::collections::{BTreeMap, BTreeSet};

use crate::tree_list::{EMPTY_SHA256, ListKind, MAX_TREE_LIST_BYTES, TreeList, TreeListError};
use crate::tree_list_v2::{
    HEADER_LEN, KIND_DIR, KIND_FILE, KIND_SYMLINK, NONE, RECORD_LEN, TREE_LIST_V2_MAGIC, VERSION,
};
use crate::tree_manifest::{BlobSource, TreeDigest};
use crate::tree_record::Mode;

/// One record before it is written.
struct Out<'a> {
    kind: u8,
    mode: u16,
    path: &'a str,
    key: u32,
    size: u64,
    offset: u64,
    sha256: [u8; 32],
}

/// The parent path and the name of `path`.
fn split(path: &str) -> (Option<&str>, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (Some(parent), name),
        None => (None, path),
    }
}

/// A sorted table of distinct strings, and the number of each.
struct Table<'a>(Vec<&'a str>);

impl<'a> Table<'a> {
    fn of(strings: BTreeSet<&'a str>) -> Self {
        Self(strings.into_iter().collect())
    }

    fn index(&self, text: &str) -> u32 {
        // Every string looked up was put in the table.
        self.0.binary_search(&text).unwrap_or_default() as u32
    }

    /// The bytes of the table: the count, the end of each string, the strings.
    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.0.len() as u32).to_le_bytes());
        let mut end = 0u32;
        for text in &self.0 {
            end = end.wrapping_add(text.len() as u32);
            out.extend_from_slice(&end.to_le_bytes());
        }
        for text in &self.0 {
            out.extend_from_slice(text.as_bytes());
        }
        out
    }
}

impl TreeList {
    /// The bytes of the list as version 2: every directory the paths imply is a record of its own
    /// (mode 0755 unless the list gives one), the directories sorted by path, then the links
    /// sorted by path, then the files with bytes sorted by key, offset and path, then the files
    /// of size 0 sorted by path; each table sorted and each string in it once. The same entries in
    /// any order are therefore the same bytes and the same key.
    ///
    /// Nothing is judged here that the reader judges: a path twice, a name a tree may not have, a
    /// file with a path below it, make bytes the reader refuses. A file of size above 0 with no
    /// `sha256` is refused ([`TreeListError::MissingSha256`]), and so are bytes over
    /// [`MAX_TREE_LIST_BYTES`] ([`TreeListError::TooLarge`]).
    pub fn encode_v2(&self) -> Result<Vec<u8>, TreeListError> {
        let mut dirs: BTreeMap<&str, u16> = BTreeMap::new();
        for entry in &self.entries {
            let mut path = entry.path.as_str();
            while let (Some(parent), _) = split(path) {
                dirs.entry(parent).or_insert(Mode::DIR.bits());
                path = parent;
            }
        }
        for entry in &self.entries {
            if let ListKind::Dir { mode } = &entry.kind {
                dirs.insert(&entry.path, mode.bits());
            }
        }
        let mut names = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let mut targets = BTreeSet::new();
        names.extend(dirs.keys().map(|path| split(path).1));
        let mut links = Vec::new();
        let mut files = Vec::new();
        for entry in &self.entries {
            match &entry.kind {
                ListKind::Dir { .. } => {}
                ListKind::Symlink { target } => {
                    names.insert(split(&entry.path).1);
                    targets.insert(target.as_str());
                    links.push((entry.path.as_str(), target.as_str()));
                }
                ListKind::File {
                    mode,
                    size,
                    source,
                    sha256,
                } => {
                    names.insert(split(&entry.path).1);
                    keys.extend(source.as_ref().map(|source| &*source.key));
                    files.push(FileEntry {
                        path: &entry.path,
                        mode: *mode,
                        size: *size,
                        source: source.as_ref(),
                        sha256: sha256.as_ref(),
                    });
                }
            }
        }
        let (names, keys, targets) = (Table::of(names), Table::of(keys), Table::of(targets));
        let mut records: Vec<Out<'_>> = dirs
            .iter()
            .map(|(path, mode)| Out {
                kind: KIND_DIR,
                mode: *mode,
                path,
                key: NONE,
                size: 0,
                offset: 0,
                sha256: [0; 32],
            })
            .collect();
        links.sort_unstable();
        records.extend(links.iter().map(|(path, target)| Out {
            kind: KIND_SYMLINK,
            mode: 0,
            path,
            key: targets.index(target),
            size: 0,
            offset: 0,
            sha256: [0; 32],
        }));
        let mut file_records = files
            .iter()
            .map(|file| file_record(file, &keys))
            .collect::<Result<Vec<_>, _>>()?;
        file_records.sort_by(|a, b| (a.key, a.offset, a.path).cmp(&(b.key, b.offset, b.path)));
        records.extend(file_records);
        let dir_index: BTreeMap<&str, u32> = dirs
            .keys()
            .enumerate()
            .map(|(index, path)| (*path, index as u32))
            .collect();
        let (key_bytes, name_bytes, target_bytes) = (keys.bytes(), names.bytes(), targets.bytes());
        let size = HEADER_LEN as usize
            + key_bytes.len()
            + name_bytes.len()
            + target_bytes.len()
            + records.len() * RECORD_LEN as usize;
        if size > MAX_TREE_LIST_BYTES {
            return Err(TreeListError::TooLarge {
                size,
                max: MAX_TREE_LIST_BYTES,
            });
        }
        // Every offset and count below fits in 32 bits: the whole is at most 256 MiB.
        let keys_at = HEADER_LEN;
        let names_at = keys_at + key_bytes.len() as u32;
        let targets_at = names_at + name_bytes.len() as u32;
        let records_at = targets_at + target_bytes.len() as u32;
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&TREE_LIST_V2_MAGIC);
        for field in [
            VERSION,
            HEADER_LEN,
            RECORD_LEN,
            records.len() as u32,
            keys_at,
            names_at,
            targets_at,
            records_at,
        ] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        out.extend_from_slice(&key_bytes);
        out.extend_from_slice(&name_bytes);
        out.extend_from_slice(&target_bytes);
        for record in &records {
            let (parent, name) = split(record.path);
            let parent = parent.and_then(|parent| dir_index.get(parent).copied());
            let parent = parent.unwrap_or(NONE);
            out.push(record.kind);
            out.push(0);
            out.extend_from_slice(&record.mode.to_le_bytes());
            out.extend_from_slice(&names.index(name).to_le_bytes());
            out.extend_from_slice(&parent.to_le_bytes());
            out.extend_from_slice(&record.key.to_le_bytes());
            out.extend_from_slice(&record.size.to_le_bytes());
            out.extend_from_slice(&record.offset.to_le_bytes());
            out.extend_from_slice(&record.sha256);
        }
        Ok(out)
    }
}

/// A file entry of the list, its members borrowed.
struct FileEntry<'a> {
    path: &'a str,
    mode: Mode,
    size: u64,
    source: Option<&'a BlobSource>,
    sha256: Option<&'a TreeDigest>,
}

/// The record of a file entry.
fn file_record<'a>(file: &FileEntry<'a>, keys: &Table<'_>) -> Result<Out<'a>, TreeListError> {
    let sha256 = match (file.sha256, file.size) {
        (Some(sha256), _) => *sha256.as_bytes(),
        (None, 0) => EMPTY_SHA256,
        (None, _) => return Err(TreeListError::MissingSha256(file.path.to_owned())),
    };
    let (key, offset) = file
        .source
        .map_or((NONE, 0), |source| (keys.index(&source.key), source.offset));
    Ok(Out {
        kind: KIND_FILE,
        mode: file.mode.bits(),
        path: file.path,
        key,
        size: file.size,
        offset,
        sha256,
    })
}
