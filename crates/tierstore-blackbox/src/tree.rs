//! A tree as a CAS service writes one into the upstream, and the check of a mount of it.
//!
//! A tree of `files` files of `file_bytes` bytes each, every file's bytes its own (its number
//! first), all in one pack, `cas/p/<SHA-256 of the pack>`; a symbolic link `link` to the first
//! file and an empty file `empty` beside them. Its version 2 tree list is stored under its key,
//! `cas/l/<SHA-256 of the list>`, the ref a Prepare names, with its L2 sidecar,
//! `cas/l2/<the same id>`, as a CAS service stores a tree. The files are `d<n>/f<m>`, 256 to a
//! directory.
//!
//! Every pack is under [`PACK_PREFIX`], the lists and sidecars are not: a stall of that prefix
//! holds the reads of the files' bytes and lets a node read the tree's list and place it.

use std::path::Path;
use std::sync::Arc;

use tierstore::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList, tree_list_key};
use tierstore_list::{PackCells, PackInfo, sidecar_from_v2, sidecar_key};

/// The start of the key of every pack.
pub(crate) const PACK_PREFIX: &str = "cas/p/";

/// The most files a tree may have.
pub(crate) const MOST_FILES: usize = 1_000_000;

/// The fewest bytes of a file: its number, so that no two files share bytes.
pub(crate) const LEAST_FILE_BYTES: usize = 8;

/// The files a directory of the tree holds.
const PER_DIR: usize = 256;

/// The path of the link, and of the empty file.
const LINK: &str = "link";
const EMPTY: &str = "empty";

/// Why a tree could not be made.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// A shape out of bounds: no file, more than [`MOST_FILES`], or files of fewer than
    /// [`LEAST_FILE_BYTES`] bytes.
    #[error(
        "a tree of {files} files of {file_bytes} bytes: 1 to {MOST_FILES} files of at least {LEAST_FILE_BYTES} bytes"
    )]
    Shape {
        /// The files asked for.
        files: usize,
        /// The bytes of each.
        file_bytes: usize,
    },
    /// The list could not be written as version 2.
    #[error("the tree list: {0}")]
    List(#[from] tierstore::TreeListError),
    /// The sidecar could not be written.
    #[error("the L2 sidecar: {0}")]
    Sidecar(#[from] tierstore_list::WriteError),
}

/// One file of the tree: where it is, where its bytes are in the pack, and their digest.
#[derive(Clone, Debug)]
struct File {
    path: String,
    offset: u64,
    sha256: TreeDigest,
}

/// A tree and the objects that hold it (module docs).
#[derive(Clone, Debug)]
pub struct Tree {
    key: String,
    objects: Vec<(String, Vec<u8>)>,
    files: Vec<File>,
    file_bytes: usize,
}

impl Tree {
    /// The tree `name` of `files` files of `file_bytes` bytes each.
    ///
    /// # Errors
    /// [`TreeError`]: the shape is out of bounds, or the list or the sidecar cannot be written.
    pub fn new(name: &str, files: usize, file_bytes: usize) -> Result<Self, TreeError> {
        if files == 0 || files > MOST_FILES || file_bytes < LEAST_FILE_BYTES {
            return Err(TreeError::Shape { files, file_bytes });
        }
        let mut pack = Vec::with_capacity(files * file_bytes);
        let mut made = Vec::with_capacity(files);
        for n in 0..files {
            let bytes = bytes_of(name, n, file_bytes);
            made.push(File {
                path: format!("d{:04}/f{n:07}", n / PER_DIR),
                offset: pack.len() as u64,
                sha256: TreeDigest::of(&bytes),
            });
            pack.extend_from_slice(&bytes);
        }
        let pack_key = format!("{PACK_PREFIX}{}", TreeDigest::of(&pack));
        let at: Arc<str> = Arc::from(pack_key.as_str());
        let mut entries: Vec<ListEntry> = made
            .iter()
            .map(|file| ListEntry {
                path: file.path.clone(),
                kind: ListKind::File {
                    mode: Mode::FILE,
                    size: file_bytes as u64,
                    source: Some(BlobSource {
                        key: Arc::clone(&at),
                        offset: file.offset,
                    }),
                    sha256: Some(file.sha256),
                },
            })
            .collect();
        entries.push(ListEntry {
            path: LINK.to_owned(),
            kind: ListKind::Symlink {
                target: made[0].path.clone(),
            },
        });
        entries.push(ListEntry {
            path: EMPTY.to_owned(),
            kind: ListKind::File {
                mode: Mode::FILE,
                size: 0,
                source: None,
                sha256: None,
            },
        });
        let v2 = TreeList { entries }.encode_v2()?;
        let cells = PackCells::of(&pack);
        let sidecar = sidecar_from_v2(&v2, |key| {
            (key == pack_key).then(|| PackInfo {
                cells: cells.clone(),
                ..PackInfo::default()
            })
        })?;
        let key = tree_list_key(&v2);
        let sidecar_key = sidecar_key(TreeDigest::of(&v2).as_bytes());
        Ok(Self {
            key: key.clone(),
            objects: vec![(pack_key, pack), (key, v2), (sidecar_key, sidecar)],
            files: made,
            file_bytes,
        })
    }

    /// The key of the tree's list: the ref of a Prepare.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The objects that hold the tree, each `(key, bytes)`: the pack, the list, the sidecar.
    #[must_use]
    pub fn objects(&self) -> &[(String, Vec<u8>)] {
        &self.objects
    }

    /// Reads the tree at `root`, a mount of it, as a job would: every file's bytes, the link's
    /// target and the empty file. The first thing that is not as the tree has it, if any.
    ///
    /// # Errors
    /// [`TreeFault`]: a file, link or empty file that cannot be read, or that differs from the
    /// tree's.
    pub fn check(&self, root: &Path) -> Result<(), TreeFault> {
        let unreadable = |path: &str| {
            let path = path.to_owned();
            move |why: std::io::Error| TreeFault::Unreadable {
                path,
                why: why.to_string(),
            }
        };
        for file in &self.files {
            let bytes = std::fs::read(root.join(&file.path)).map_err(unreadable(&file.path))?;
            if bytes.len() != self.file_bytes || TreeDigest::of(&bytes) != file.sha256 {
                return Err(TreeFault::Differs {
                    path: file.path.clone(),
                    what: format!(
                        "{} bytes that are not the tree's {}",
                        bytes.len(),
                        self.file_bytes
                    ),
                });
            }
        }
        let target = std::fs::read_link(root.join(LINK)).map_err(unreadable(LINK))?;
        if target != Path::new(&self.files[0].path) {
            return Err(TreeFault::Differs {
                path: LINK.to_owned(),
                what: format!("a link to {}", target.display()),
            });
        }
        let empty = std::fs::metadata(root.join(EMPTY)).map_err(unreadable(EMPTY))?;
        if !empty.is_file() || empty.len() != 0 {
            return Err(TreeFault::Differs {
                path: EMPTY.to_owned(),
                what: format!("{} bytes, not an empty file", empty.len()),
            });
        }
        Ok(())
    }
}

/// What a read of a mount found that is not the tree's ([`Tree::check`]).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreeFault {
    /// A path that could not be read: an error of the mount, which a job sees as one.
    #[error("{path}: {why}")]
    Unreadable {
        /// The path in the tree.
        path: String,
        /// The error of the read.
        why: String,
    },
    /// A path read with what the tree does not have: a wrong answer, which a job cannot tell.
    #[error("{path}: {what}")]
    Differs {
        /// The path in the tree.
        path: String,
        /// What was read.
        what: String,
    },
}

/// The `len` bytes of file `n` of the tree `name`: `n`, then the name's bytes over and over.
fn bytes_of(name: &str, n: usize, len: usize) -> Vec<u8> {
    let mut bytes = (n as u64).to_le_bytes().to_vec();
    bytes.extend(
        name.bytes()
            .chain(std::iter::once(b'/'))
            .cycle()
            .take(len - LEAST_FILE_BYTES),
    );
    bytes
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
