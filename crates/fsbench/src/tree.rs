//! The tree of a file list, as the null file system serves it ([`crate::tsnull`] on `tsfuse`):
//! inodes, lookups, directory listings and the span of a read.
//!
//! The inode of the root is 1 and every other inode is its node's index in the tree plus 1.
//! Each file also has a place in a *data file*, the file [`crate::tsnull`] reads its replies
//! from: the files of the list one after another in list order, each starting on a 4 KiB
//! boundary (as the files of a pack sit in one file of the node cache), so the data file is
//! [`Tree::data_len`] bytes long.

use std::collections::BTreeMap;

use crate::Error;
use crate::list::FileList;

/// The boundary every file of the data file starts on.
pub const DATA_ALIGN: u64 = 4096;

/// Why a request about the tree has no answer: the errno a file system replies with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Miss {
    /// No such inode, or no such name in the directory (`ENOENT`).
    NotFound,
    /// A lookup or a listing of a file (`ENOTDIR`).
    NotDir,
    /// A read of a directory (`EISDIR`).
    IsDir,
}

impl Miss {
    /// The errno (positive).
    #[must_use]
    pub fn errno(self) -> i32 {
        match self {
            Self::NotFound => 2,
            Self::NotDir => 20,
            Self::IsDir => 21,
        }
    }
}

/// A node of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// A directory: its parent's inode and its entries by name.
    Dir {
        /// The parent's inode (the root's is itself).
        parent: u64,
        /// The entries, by name.
        entries: BTreeMap<String, u64>,
    },
    /// A file of `size` bytes, at byte `at` of the data file.
    File {
        /// The size in bytes.
        size: u64,
        /// Where its bytes start in the data file.
        at: u64,
    },
}

/// The bytes a read answers: `len` bytes from byte `at` of the data file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// The first byte, in the data file.
    pub at: u64,
    /// How many bytes.
    pub len: u64,
}

/// The files of a list, as inodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    /// The node of inode `i` is `nodes[i - 1]`.
    nodes: Vec<Node>,
    /// The length of the data file.
    data_len: u64,
}

impl Tree {
    /// The tree of the files of `list`, and of the directories their paths name.
    ///
    /// # Errors
    /// A path that names a file as a directory (`a` and `a/b` both listed).
    pub fn of(list: &FileList) -> Result<Self, Error> {
        let mut nodes = vec![Node::Dir {
            parent: 1,
            entries: BTreeMap::new(),
        }];
        let mut at = 0u64;
        for file in &list.files {
            let mut dir = 1u64;
            let mut parts = file.path.split('/').peekable();
            while let Some(part) = parts.next() {
                let last = parts.peek().is_none();
                let next = nodes.len() as u64 + 1;
                let Node::Dir { entries, .. } = &mut nodes[(dir - 1) as usize] else {
                    return Err(Error::Usage(format!(
                        "{}: a file of the list is named as a directory",
                        file.path
                    )));
                };
                match entries.get(part) {
                    Some(_) if last => {
                        return Err(Error::Usage(format!(
                            "{}: a directory of the list is named as a file",
                            file.path
                        )));
                    }
                    Some(&ino) => dir = ino,
                    None => {
                        entries.insert(part.to_owned(), next);
                        nodes.push(if last {
                            let node = Node::File {
                                size: file.size,
                                at,
                            };
                            at += file.size.next_multiple_of(DATA_ALIGN);
                            node
                        } else {
                            Node::Dir {
                                parent: dir,
                                entries: BTreeMap::new(),
                            }
                        });
                        dir = next;
                    }
                }
            }
        }
        Ok(Self {
            nodes,
            data_len: at,
        })
    }

    /// The length of the data file: every file, each rounded up to [`DATA_ALIGN`].
    #[must_use]
    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    /// How many nodes the tree has, the root included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the tree has no node (never: it has its root).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node of inode `ino`.
    ///
    /// # Errors
    /// [`Miss::NotFound`].
    pub fn node(&self, ino: u64) -> Result<&Node, Miss> {
        usize::try_from(ino)
            .ok()
            .and_then(|i| self.nodes.get(i.checked_sub(1)?))
            .ok_or(Miss::NotFound)
    }

    /// The inode and node of `name` in directory `parent`.
    ///
    /// # Errors
    /// [`Miss::NotFound`] (no such directory or name; a name that is not UTF-8 is never there),
    /// [`Miss::NotDir`] when `parent` is a file.
    pub fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, &Node), Miss> {
        let Node::Dir { entries, .. } = self.node(parent)? else {
            return Err(Miss::NotDir);
        };
        let ino = *std::str::from_utf8(name)
            .ok()
            .and_then(|name| entries.get(name))
            .ok_or(Miss::NotFound)?;
        Ok((ino, self.node(ino)?))
    }

    /// The bytes a read of at most `size` bytes at `offset` of file `ino` answers: up to the end
    /// of the file, none at or past it.
    ///
    /// # Errors
    /// [`Miss::NotFound`], or [`Miss::IsDir`] for a directory.
    pub fn read(&self, ino: u64, offset: u64, size: u32) -> Result<Span, Miss> {
        let Node::File { size: length, at } = *self.node(ino)? else {
            return Err(Miss::IsDir);
        };
        let left = length.saturating_sub(offset);
        Ok(Span {
            at: at + offset.min(length),
            len: left.min(u64::from(size)),
        })
    }

    /// The entries of directory `ino`, `.` and `..` first: inode, name and node.
    ///
    /// # Errors
    /// [`Miss::NotFound`], or [`Miss::NotDir`] for a file.
    pub fn entries(&self, ino: u64) -> Result<Vec<(u64, &str, &Node)>, Miss> {
        let dir = self.node(ino)?;
        let Node::Dir { parent, entries } = dir else {
            return Err(Miss::NotDir);
        };
        let mut all = vec![(ino, ".", dir), (*parent, "..", self.node(*parent)?)];
        for (name, &child) in entries {
            all.push((child, name.as_str(), self.node(child)?));
        }
        Ok(all)
    }
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
