//! The outputs of a farm action, as files on this machine: what the Command declared
//! (`output_paths`, `--output`), as the `ActionResult` names it, written below the checkout at
//! the same paths.
//!
//! The result is the farm's, and what it names is published (the macOS release files), so it is
//! held to the declaration before anything is written:
//! - every path is a plain relative path (no `..`, no `.`, no empty or absolute part), and is a
//!   declared path or below one;
//! - every declared path is in the result: the command made it;
//! - a symlink is refused: the files are copied out as files, never as links that could point
//!   elsewhere;
//! - each output directory's `Tree` holds every directory its nodes name, by the digest of the
//!   bytes the farm sent (a re-encoded message can differ from them), and each name in it is one
//!   plain component.
//!
//! [`plan`] makes the list of what to write from the result and the trees; [`super::client`]
//! reads each file's blob, held to its digest, and [`super::fetch_outputs`] writes it.

#[cfg(test)]
#[path = "outputs_tests.rs"]
mod tests;

use std::collections::BTreeMap;

use prost::Message as _;

use super::proto::reapi::{ActionResult, Digest, Directory};
use super::tree::{components, digest_of};

/// Why the outputs of a result are not what the Command declared.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutputError {
    #[error("the output path {0:?} is not a relative path of plain components")]
    BadPath(String),
    #[error("the farm returned {0:?}, which is not a declared output nor below one")]
    Undeclared(String),
    #[error("the command made no {0:?}, a declared output")]
    Missing(String),
    #[error("the output {0:?} is a symlink; outputs are files and directories")]
    Symlink(String),
    #[error("the output {0:?} is given twice")]
    Twice(String),
    #[error("the output {path:?} has no digest")]
    NoDigest { path: String },
    #[error("the tree of {path:?} does not decode: {why}")]
    Tree { path: String, why: String },
    #[error("the tree of {path:?} lacks the directory {hash}")]
    NoDirectory { path: String, hash: String },
    #[error("the tree of {path:?} names {name:?}, which is not one plain component")]
    BadName { path: String, name: String },
}

/// One thing to make below the checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A directory (an output directory, or one inside it, empty or not).
    Dir { path: String },
    /// A file: its content is the blob `digest`, or `inline` when the farm sent it inline.
    File {
        path: String,
        digest: Digest,
        inline: Option<Vec<u8>>,
        executable: bool,
    },
}

impl Item {
    /// The path of the item, relative to the checkout.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Item::Dir { path } | Item::File { path, .. } => path,
        }
    }
}

/// A `Tree` as the bytes the farm sent: its root and its children kept as encoded, so each
/// child's digest is the one the directory nodes name. (A length-delimited field reads as bytes
/// as well as a message.)
#[derive(Clone, PartialEq, prost::Message)]
pub struct RawTree {
    #[prost(bytes = "vec", tag = "1")]
    pub root: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub children: Vec<Vec<u8>>,
}

/// Whether `path` is `declared` or below it.
pub(crate) fn within(path: &str, declared: &str) -> bool {
    path == declared
        || path
            .strip_prefix(declared)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `path` checked: plain, and a declared output or below one.
fn checked<'a>(path: &'a str, declared: &[String]) -> Result<&'a str, OutputError> {
    components(path).map_err(|_| OutputError::BadPath(path.to_owned()))?;
    if !declared.iter().any(|d| within(path, d)) {
        return Err(OutputError::Undeclared(path.to_owned()));
    }
    Ok(path)
}

/// The digests of the trees of `result`'s output directories: what to read before [`plan`].
///
/// # Errors
/// [`OutputError::NoDigest`] for an output directory with no tree.
pub fn tree_digests(result: &ActionResult) -> Result<Vec<Digest>, OutputError> {
    result
        .output_directories
        .iter()
        .map(|d| {
            d.tree_digest.clone().ok_or_else(|| OutputError::NoDigest {
                path: d.path.clone(),
            })
        })
        .collect()
}

/// What to write for `result`, whose Command declared `declared`: every output file and every
/// file and directory of every output directory, each `trees` (the bytes of the trees, by hash)
/// holds, parents before children.
///
/// # Errors
/// [`OutputError`] for a result that is not what the Command declared, or a tree that is not
/// whole.
pub fn plan(
    result: &ActionResult,
    declared: &[String],
    trees: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<Item>, OutputError> {
    let mut symlinks = result
        .output_symlinks
        .iter()
        .chain(&result.output_file_symlinks)
        .chain(&result.output_directory_symlinks);
    if let Some(link) = symlinks.next() {
        return Err(OutputError::Symlink(link.path.clone()));
    }
    let mut items = Vec::new();
    for file in &result.output_files {
        let path = checked(&file.path, declared)?;
        let digest = file.digest.clone().ok_or_else(|| OutputError::NoDigest {
            path: path.to_owned(),
        })?;
        // A farm may send a small file inline; the bytes must still be the digest's.
        let inline = (!file.contents.is_empty()).then(|| file.contents.clone());
        items.push(Item::File {
            path: path.to_owned(),
            digest,
            inline,
            executable: file.is_executable,
        });
    }
    for dir in &result.output_directories {
        let path = checked(&dir.path, declared)?;
        let digest = dir
            .tree_digest
            .as_ref()
            .ok_or_else(|| OutputError::NoDigest {
                path: path.to_owned(),
            })?;
        let raw = trees
            .get(&digest.hash)
            .ok_or_else(|| OutputError::NoDirectory {
                path: path.to_owned(),
                hash: digest.hash.clone(),
            })?;
        let tree_error = |e: prost::DecodeError| OutputError::Tree {
            path: path.to_owned(),
            why: e.to_string(),
        };
        let raw = RawTree::decode(raw.as_slice()).map_err(tree_error)?;
        let children: BTreeMap<String, &[u8]> = raw
            .children
            .iter()
            .map(|c| (digest_of(c).hash, c.as_slice()))
            .collect();
        let root = Directory::decode(raw.root.as_slice()).map_err(tree_error)?;
        walk(path, path, &root, &children, &mut items)?;
    }
    for declared in declared {
        if !items.iter().any(|i| i.path() == declared) {
            return Err(OutputError::Missing(declared.clone()));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for item in &items {
        if !seen.insert(item.path()) {
            return Err(OutputError::Twice(item.path().to_owned()));
        }
    }
    Ok(items)
}

/// The items of the directory `dir` at `at`, of the output directory `output`, into `items`.
fn walk(
    output: &str,
    at: &str,
    dir: &Directory,
    children: &BTreeMap<String, &[u8]>,
    items: &mut Vec<Item>,
) -> Result<(), OutputError> {
    items.push(Item::Dir {
        path: at.to_owned(),
    });
    let name_of = |name: &str| {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            Err(OutputError::BadName {
                path: output.to_owned(),
                name: name.to_owned(),
            })
        } else {
            Ok(format!("{at}/{name}"))
        }
    };
    if let Some(link) = dir.symlinks.first() {
        return Err(OutputError::Symlink(name_of(&link.name)?));
    }
    for file in &dir.files {
        let path = name_of(&file.name)?;
        let digest = file
            .digest
            .clone()
            .ok_or_else(|| OutputError::NoDigest { path: path.clone() })?;
        items.push(Item::File {
            path,
            digest,
            inline: None,
            executable: file.is_executable,
        });
    }
    for sub in &dir.directories {
        let path = name_of(&sub.name)?;
        let hash = sub
            .digest
            .as_ref()
            .map(|d| d.hash.clone())
            .unwrap_or_default();
        let bytes = children
            .get(&hash)
            .ok_or_else(|| OutputError::NoDirectory {
                path: output.to_owned(),
                hash: hash.clone(),
            })?;
        let child = Directory::decode(*bytes).map_err(|e| OutputError::Tree {
            path: output.to_owned(),
            why: e.to_string(),
        })?;
        walk(output, &path, &child, children, items)?;
    }
    Ok(())
}
