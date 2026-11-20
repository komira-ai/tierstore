//! A job's tree as the store holds it: one JSON manifest per directory under `cas/l/<sha256>`,
//! each file naming its bytes in a pack. [`parse`] reads one manifest, [`walk`] reads the root and
//! every directory below it and makes the tree's [`TreeList`], and [`encode_v2`] writes that list
//! as version 2 bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, StreamExt as _, TryStreamExt as _};
use serde::Deserialize;
use tierstore::{BlobSource, ListEntry, ListKind, MAX_DEPTH, Mode, TreeDigest, TreeList};

use crate::Error;

/// One child of a directory manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Child {
    /// A regular file.
    File {
        /// The name in its directory.
        name: String,
        /// The SHA-256 of the contents.
        sha256: [u8; 32],
        /// The length of the contents.
        size: u64,
        /// Whether the file is executable.
        exec: bool,
        /// The pack key and the offset of the file's first byte in it; none for a file of
        /// size 0 that names no pack. A file with bytes and no `source` in its manifest is a
        /// loose blob: the object `cas/l/<sha256>`, from offset 0.
        source: Option<(String, u64)>,
    },
    /// A directory.
    Dir {
        /// The name in its directory.
        name: String,
        /// The SHA-256 of the directory's own manifest.
        sha256: [u8; 32],
    },
    /// A symbolic link.
    Symlink {
        /// The name in its directory.
        name: String,
        /// What the link says (empty when the manifest gives none).
        target: String,
    },
}

#[derive(Deserialize)]
struct RawManifest {
    v: Option<u64>,
    children: Option<Vec<RawChild>>,
}

#[derive(Deserialize)]
struct RawChild {
    name: Option<String>,
    kind: Option<String>,
    sha256: Option<String>,
    size: Option<u64>,
    exec: Option<bool>,
    source: Option<RawSource>,
    target: Option<String>,
}

#[derive(Deserialize)]
struct RawSource {
    key: String,
    offset: u64,
}

/// The children of the version 1 manifest `bytes`, read from the object `key` (named in every
/// error). Unknown fields are ignored. A file needs its size and, above size 0, its SHA-256; its
/// `source` is optional: a file with bytes and no `source` is a loose blob, read whole from the
/// object `cas/l/<its SHA-256>` at offset 0. A directory needs its SHA-256; a link without a
/// target has an empty one.
pub fn parse(key: &str, bytes: &[u8]) -> Result<Vec<Child>, Error> {
    let bad = |why: String| Error::parse(key, why);
    let raw: RawManifest = serde_json::from_slice(bytes).map_err(|e| bad(e.to_string()))?;
    match raw.v {
        Some(1) => {}
        Some(v) => return Err(bad(format!("manifest version {v}, only 1 is read"))),
        None => return Err(bad("no manifest version".to_owned())),
    }
    let children = raw
        .children
        .ok_or_else(|| bad("no children list".to_owned()))?;
    children
        .into_iter()
        .map(|child| parse_child(child).map_err(bad))
        .collect()
}

fn parse_child(raw: RawChild) -> Result<Child, String> {
    let name = raw.name.ok_or("a child has no name")?;
    if name.is_empty() || name.contains('/') {
        return Err(format!("child name {name:?} is not a file name"));
    }
    let kind = raw
        .kind
        .ok_or_else(|| format!("child {name:?} has no kind"))?;
    match kind.as_str() {
        "file" => {
            let size = raw
                .size
                .ok_or_else(|| format!("file {name:?} has no size"))?;
            let sha256 = match (raw.sha256, size) {
                (Some(text), _) => digest(&name, &text)?,
                (None, 0) => *TreeDigest::of(&[]).as_bytes(),
                (None, _) => return Err(format!("file {name:?} has no sha256")),
            };
            let source = match (raw.source, size) {
                (Some(source), _) => Some((source.key, source.offset)),
                (None, 0) => None,
                // A file stored alone (a loose blob): its bytes are the whole object named by its
                // digest, which is where the shipped mount reads them.
                (None, _) => Some((manifest_key(&hex::encode(sha256)), 0)),
            };
            Ok(Child::File {
                name,
                sha256,
                size,
                exec: raw.exec.unwrap_or(false),
                source,
            })
        }
        "dir" => {
            let text = raw
                .sha256
                .ok_or_else(|| format!("directory {name:?} has no sha256"))?;
            let sha256 = digest(&name, &text)?;
            Ok(Child::Dir { name, sha256 })
        }
        "symlink" => Ok(Child::Symlink {
            name,
            target: raw.target.unwrap_or_default(),
        }),
        other => Err(format!("child {name:?} has the unknown kind {other:?}")),
    }
}

fn digest(name: &str, text: &str) -> Result<[u8; 32], String> {
    TreeDigest::from_hex(text)
        .map(|d| *d.as_bytes())
        .ok_or_else(|| format!("sha256 of {name:?} is not 64 lowercase hex digits"))
}

/// A store that gives the bytes of an object by its key.
#[async_trait]
pub trait Fetch: Send + Sync {
    /// The whole object `key`.
    async fn get(&self, key: &str) -> Result<Bytes, Error>;
}

/// A tree read from its manifests.
#[derive(Debug)]
pub struct Walked {
    /// The tree's entries: every file and link by its path from the root, and every empty
    /// directory.
    pub list: TreeList,
    /// How many manifests were fetched (each directory once, however often it is named).
    pub manifests: u64,
    /// How many files have a size above 0.
    pub files: u64,
    /// The sum of those files' sizes.
    pub bytes: u64,
    /// How many distinct packs those files name.
    pub packs: u64,
}

/// The object key of the manifest of sha256 `sha_hex`: `cas/l/<hex>`.
#[must_use]
pub fn manifest_key(sha_hex: &str) -> String {
    format!("cas/l/{sha_hex}")
}

/// Reads the manifest of the tree `root` (64 lowercase hex digits) and of every directory
/// below it, one level at a time with at most `parallel` (at least 1) fetches in flight, each
/// directory's manifest once however many places name it, and lists the tree.
pub async fn walk(root: &str, fetch: &dyn Fetch, parallel: usize) -> Result<Walked, Error> {
    let root_digest = TreeDigest::from_hex(root)
        .map(|d| *d.as_bytes())
        .ok_or_else(|| Error::parse(root, "the tree id is not 64 lowercase hex digits"))?;
    let mut cache: BTreeMap<[u8; 32], Arc<Vec<Child>>> = BTreeMap::new();
    let mut entries = Vec::new();
    let mut packs: BTreeSet<String> = BTreeSet::new();
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut level: Vec<(String, [u8; 32])> = vec![(String::new(), root_digest)];
    let mut depth = 0usize;
    while !level.is_empty() {
        if depth > MAX_DEPTH {
            return Err(Error::parse(
                manifest_key(root),
                format!("tree deeper than {MAX_DEPTH} levels"),
            ));
        }
        depth += 1;
        let wanted: BTreeSet<[u8; 32]> = level
            .iter()
            .map(|(_, d)| *d)
            .filter(|d| !cache.contains_key(d))
            .collect();
        let fetched: Vec<([u8; 32], Vec<Child>)> = stream::iter(wanted)
            .map(|d| async move {
                let key = manifest_key(&hex::encode(d));
                let body = fetch.get(&key).await?;
                Ok::<_, Error>((d, parse(&key, &body)?))
            })
            .buffer_unordered(parallel.max(1))
            .try_collect()
            .await?;
        for (d, children) in fetched {
            cache.insert(d, Arc::new(children));
        }
        let mut next = Vec::new();
        for (prefix, d) in &level {
            let children = Arc::clone(&cache[d]);
            if children.is_empty() && !prefix.is_empty() {
                entries.push(ListEntry {
                    path: prefix.clone(),
                    kind: ListKind::Dir { mode: Mode::DIR },
                });
            }
            for child in children.iter() {
                let join = |name: &str| {
                    if prefix.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{prefix}/{name}")
                    }
                };
                match child {
                    Child::Dir { name, sha256 } => next.push((join(name), *sha256)),
                    Child::Symlink { name, target } => entries.push(ListEntry {
                        path: join(name),
                        kind: ListKind::Symlink {
                            target: target.clone(),
                        },
                    }),
                    Child::File {
                        name,
                        sha256,
                        size,
                        exec,
                        source,
                    } => {
                        let source = if *size > 0 { source.as_ref() } else { None };
                        if let Some((key, _)) = source {
                            packs.insert(key.clone());
                        }
                        if *size > 0 {
                            files += 1;
                            bytes = bytes.saturating_add(*size);
                        }
                        entries.push(ListEntry {
                            path: join(name),
                            kind: ListKind::File {
                                mode: if *exec { Mode::DIR } else { Mode::FILE },
                                size: *size,
                                source: source.map(|(key, offset)| BlobSource {
                                    key: Arc::from(key.as_str()),
                                    offset: *offset,
                                }),
                                // A file of size 0 has the digest of no bytes, which the list leaves out.
                                sha256: (*size > 0).then(|| TreeDigest::from_bytes(*sha256)),
                            },
                        });
                    }
                }
            }
        }
        level = next;
    }
    Ok(Walked {
        list: TreeList { entries },
        manifests: cache.len() as u64,
        files,
        bytes,
        packs: packs.len() as u64,
    })
}

/// The version 2 bytes of `list`.
pub fn encode_v2(list: &TreeList) -> Result<Vec<u8>, Error> {
    list.encode_v2().map_err(|e| Error::ReadPath {
        what: "encode the version 2 tree list",
        why: e.to_string(),
    })
}

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod tests;
