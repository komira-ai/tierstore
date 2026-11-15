//! The files of the tree store, in the cache directory.
//!
//! ```text
//! <path>/trees/<T>/records     every directory record of the tree T made by its entries
//! <path>/trees/<T>/sources     where each file's bytes are fetched from
//! <path>/trees/<T>/ready       present once every range of the tree was in the cache and pinned
//! <path>/placements/<id>       one placement, JSON
//! ```
//!
//! `<T>` is the digest of the tree, sixty-four lower-case hex digits; `<id>` is a placement's id,
//! thirty-two. Each file is written beside its name and renamed into place (a tree's directory is
//! written under `trees/.new-<n>` and renamed whole), and none is synced: a file that a crash left
//! short is found at the next start, which drops what does not read or hash to its name.
//! `docs/stored-formats.md` has the layouts.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::location::{Location, LocationError, PlacementId};
use crate::{TreeDigest, TreeRecord};

/// The directory of the trees, in the cache directory.
const TREES: &str = "trees";

/// The directory of the placements, in the cache directory.
const PLACEMENTS: &str = "placements";

/// The file of a tree's records.
pub(super) const RECORDS: &str = "records";

/// The file of a tree's sources.
pub(super) const SOURCES: &str = "sources";

/// The file that says a tree's ranges were all in the cache.
pub(super) const READY: &str = "ready";

/// What a tree's directory is called while it is written.
pub(super) const STAGING: &str = ".new-";

/// The records file's magic.
const RECORDS_MAGIC: &[u8; 4] = b"TSRS";

/// The sources file's magic.
const SOURCES_MAGIC: &[u8; 4] = b"TSSR";

/// The version of every file here.
const VERSION: u8 = 1;

/// The `trees` directory of the cache directory `root`.
pub(super) fn trees_dir(root: &Path) -> PathBuf {
    root.join(TREES)
}

/// The directory of the tree `tree`.
pub(super) fn tree_dir(root: &Path, tree: &TreeDigest) -> PathBuf {
    trees_dir(root).join(tree.to_string())
}

/// The `placements` directory of the cache directory `root`.
pub(super) fn placements_dir(root: &Path) -> PathBuf {
    root.join(PLACEMENTS)
}

/// The file of the placement `id`.
pub(super) fn placement_file(root: &Path, id: &PlacementId) -> PathBuf {
    placements_dir(root).join(id.as_str())
}

/// Writes `bytes` to `path` by writing beside it and renaming.
pub(super) fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.as_os_str().to_owned();
    name.push(".new");
    let beside = PathBuf::from(name);
    std::fs::write(&beside, bytes)?;
    std::fs::rename(&beside, path)
}

/// Why a file of the store does not read.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub(super) struct Corrupt(pub &'static str);

/// The bytes left to read of a file.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], Corrupt> {
        if len > self.0.len() {
            return Err(Corrupt("truncated"));
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, Corrupt> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, Corrupt> {
        let bytes = self.take(8)?;
        let mut eight = [0; 8];
        eight.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(eight))
    }

    fn digest(&mut self) -> Result<[u8; 32], Corrupt> {
        let mut out = [0; 32];
        out.copy_from_slice(self.take(32)?);
        Ok(out)
    }

    fn header(&mut self, magic: &[u8; 4]) -> Result<(), Corrupt> {
        if self.take(4)? != magic {
            return Err(Corrupt("bad magic"));
        }
        if self.take(1)? != [VERSION] {
            return Err(Corrupt("unsupported version"));
        }
        Ok(())
    }
}

/// Where a record is in a records file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Place {
    /// The first byte of the record in the file.
    pub offset: u64,
    /// Its length.
    pub len: u32,
}

/// The bytes of the records file of a tree: its directory records, each with the digest it is
/// named by, as an index and then the bodies in the order of the index; and where each record is
/// in those bytes.
pub(super) fn encode_records(
    records: &[(TreeDigest, TreeRecord)],
) -> (Vec<u8>, Vec<(TreeDigest, Place)>) {
    let bodies: Vec<Vec<u8>> = records.iter().map(|(_, record)| record.encode()).collect();
    let mut out = Vec::new();
    out.extend_from_slice(RECORDS_MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&(records.len() as u32).to_be_bytes());
    for ((digest, _), body) in records.iter().zip(&bodies) {
        out.extend_from_slice(digest.as_bytes());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    }
    let mut places = Vec::with_capacity(records.len());
    for ((digest, _), body) in records.iter().zip(&bodies) {
        let place = Place {
            offset: out.len() as u64,
            len: body.len() as u32,
        };
        places.push((*digest, place));
        out.extend_from_slice(body);
    }
    (out, places)
}

/// The index of a records file: the digest each record is named by and where it is. The file
/// holds exactly the bodies the index says.
pub(super) fn parse_records(bytes: &[u8]) -> Result<Vec<(TreeDigest, Place)>, Corrupt> {
    let mut cursor = Cursor(bytes);
    cursor.header(RECORDS_MAGIC)?;
    let count = cursor.u32()? as usize;
    // Each index entry is 36 bytes: a count the file cannot hold is refused before a vector is made.
    if count > cursor.0.len() / 36 {
        return Err(Corrupt("truncated"));
    }
    let mut index = Vec::with_capacity(count);
    let mut offset = (bytes.len() - cursor.0.len() + count * 36) as u64;
    for _ in 0..count {
        let digest = TreeDigest::from_bytes(cursor.digest()?);
        let len = cursor.u32()?;
        index.push((digest, Place { offset, len }));
        offset += u64::from(len);
    }
    if offset != bytes.len() as u64 {
        return Err(Corrupt("the records are not the length the index says"));
    }
    Ok(index)
}

/// Where the bytes of a file are: a range of an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Source {
    /// The file's length.
    pub size: u64,
    /// Which of the packs of [`Sources`] holds the bytes.
    pub pack: u32,
    /// The offset of the first byte in the pack.
    pub offset: u64,
}

/// The sources of the files of one tree, by the SHA-256 of their bytes: a file's bytes are the same
/// wherever they are, so one source serves every file of the digest.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Sources {
    /// The keys of the objects that hold the bytes.
    pub packs: Vec<String>,
    /// Each file's source.
    pub files: BTreeMap<[u8; 32], Source>,
}

impl Sources {
    /// The bytes of the sources file: the packs, then the files in the order of their digests.
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(SOURCES_MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&(self.packs.len() as u32).to_be_bytes());
        for pack in &self.packs {
            out.extend_from_slice(&(pack.len() as u32).to_be_bytes());
            out.extend_from_slice(pack.as_bytes());
        }
        out.extend_from_slice(&(self.files.len() as u32).to_be_bytes());
        for (sha256, source) in &self.files {
            out.extend_from_slice(sha256);
            out.extend_from_slice(&source.size.to_be_bytes());
            out.extend_from_slice(&source.pack.to_be_bytes());
            out.extend_from_slice(&source.offset.to_be_bytes());
        }
        out
    }

    /// Reads a sources file: the digests strictly ascending, each pack named by a file in range,
    /// nothing after the last.
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, Corrupt> {
        let mut cursor = Cursor(bytes);
        cursor.header(SOURCES_MAGIC)?;
        let count = cursor.u32()?;
        let mut packs = Vec::new();
        for _ in 0..count {
            let len = cursor.u32()? as usize;
            let text = std::str::from_utf8(cursor.take(len)?);
            packs.push(
                text.map_err(|_| Corrupt("a pack key that is not UTF-8"))?
                    .to_owned(),
            );
        }
        let files = cursor.u32()?;
        let mut sources = BTreeMap::new();
        let mut last: Option<[u8; 32]> = None;
        for _ in 0..files {
            let sha256 = cursor.digest()?;
            let source = Source {
                size: cursor.u64()?,
                pack: cursor.u32()?,
                offset: cursor.u64()?,
            };
            if last.is_some_and(|last| last >= sha256) {
                return Err(Corrupt("the digests are not in order"));
            }
            if source.pack as usize >= packs.len() {
                return Err(Corrupt("a source names a pack that is not listed"));
            }
            last = Some(sha256);
            sources.insert(sha256, source);
        }
        if !cursor.0.is_empty() {
            return Err(Corrupt("bytes after the last source"));
        }
        Ok(Self {
            packs,
            files: sources,
        })
    }
}

/// The JSON of a placement's file. A key a later release adds is ignored.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PlacementFile {
    /// The version of the file: 1.
    pub v: u32,
    /// The digest of the tree placed.
    pub tree: String,
    /// The locations.
    pub at: Vec<At>,
    /// When the placement was first made, in milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// When the lease ends, in milliseconds since the Unix epoch.
    pub expires_ms: u64,
}

/// One location in the JSON of a placement.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "interface", rename_all = "lowercase")]
pub(super) enum At {
    S3 { bucket: String, prefix: String },
    Fs { path: String },
    /// An interface a later release added: the file is left where it is, not read.
    #[serde(other)]
    Other,
}

impl At {
    /// The location written.
    pub(super) fn of(location: &Location) -> Self {
        match location {
            Location::S3 { bucket, prefix } => Self::S3 {
                bucket: bucket.clone(),
                prefix: prefix.clone(),
            },
            Location::Fs { path } => Self::Fs { path: path.clone() },
        }
    }

    /// The location, if what was written is one; none for an interface this release does not
    /// know.
    pub(super) fn location(&self) -> Result<Option<Location>, LocationError> {
        match self {
            Self::S3 { bucket, prefix } => Location::s3(bucket, prefix).map(Some),
            Self::Fs { path } => Location::fs(path).map(Some),
            Self::Other => Ok(None),
        }
    }
}
