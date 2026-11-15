//! Where a tree appears: the locations of a placement, the table that finds the placement a key
//! or a path falls under, and the rules that keep two placements from answering one key.
//!
//! A location is a bucket and key prefix of the S3 front, or a path of names under the mount of
//! `tierstore fs`. The table is keyed by one string for each location, its interface, its bucket
//! and its prefix ending in `/`, so that "this key falls under a placement" is the test of the
//! key's own `/` boundaries, a handful of map lookups, and "another placement is equal to, inside
//! or around this location" is the test of one prefix and one range.

use std::collections::BTreeMap;
use std::fmt;

use crate::TreeDigest;
use crate::tree_manifest::is_valid_name;

/// The longest key of S3.
const MAX_KEY_BYTES: usize = 1024;

/// The names that are directories of the mount, not of any tree's namespace.
const MOUNT_NAMES: [&str; 3] = ["trees", "views", "put"];

/// Why a location was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LocationError {
    /// The bucket is empty or has a `/` or a NUL.
    #[error("bad location: the bucket is empty or has a / or a NUL")]
    Bucket,
    /// The prefix of an S3 location is empty, does not end in `/`, has a NUL, begins with `/`, or
    /// is longer than a key can be.
    #[error(
        "bad location: a prefix is not empty, begins not with /, ends in / and is at most 1024 bytes"
    )]
    Prefix,
    /// A path of the mount that has no name, an invalid one, or one of the mount's own first.
    #[error("bad location: a path is names of the usual rules, none of trees, views or put first")]
    Path,
}

/// A place a tree appears at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// The keys under `prefix` of the bucket `bucket` of the S3 front.
    S3 {
        /// The bucket the node serves.
        bucket: String,
        /// The key prefix: ends in `/`.
        prefix: String,
    },
    /// The path `path` under the mount of `tierstore fs`: names joined by `/`.
    Fs {
        /// The path, with no `/` at either end.
        path: String,
    },
}

impl Location {
    /// An S3 location.
    ///
    /// # Errors
    /// [`LocationError::Bucket`] or [`LocationError::Prefix`].
    pub fn s3(bucket: &str, prefix: &str) -> Result<Self, LocationError> {
        if bucket.is_empty() || bucket.contains(['/', '\0']) {
            return Err(LocationError::Bucket);
        }
        let bad = prefix.len() > MAX_KEY_BYTES
            || !prefix.ends_with('/')
            || prefix.starts_with('/')
            || prefix.contains('\0');
        if bad {
            return Err(LocationError::Prefix);
        }
        Ok(Self::S3 {
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
        })
    }

    /// A location under the mount.
    ///
    /// # Errors
    /// [`LocationError::Path`].
    pub fn fs(path: &str) -> Result<Self, LocationError> {
        let first = path.split('/').next().unwrap_or_default();
        if MOUNT_NAMES.contains(&first) || !path.split('/').all(is_valid_name) {
            return Err(LocationError::Path);
        }
        Ok(Self::Fs {
            path: path.to_owned(),
        })
    }

    /// The key of the location in the table of a node: its interface, bucket and prefix.
    pub(super) fn key(&self) -> String {
        match self {
            Self::S3 { bucket, prefix } => format!("s3\0{bucket}\0{prefix}"),
            Self::Fs { path } => format!("fs\0\0{path}/"),
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::S3 { bucket, prefix } => write!(f, "s3 {bucket}/{prefix}"),
            Self::Fs { path } => write!(f, "fs {path}"),
        }
    }
}

/// The identity of a placement: a tree at a set of locations. The same tree at the same locations
/// is the same placement, so a create of it again is a refresh.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlacementId(pub(super) String);

impl PlacementId {
    /// The thirty-two lower-case hex digits of the id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id of `tree` at `locations`, whatever their order.
    pub(super) fn of(tree: &TreeDigest, locations: &[Location]) -> Self {
        use sha2::{Digest as _, Sha256};
        let mut keys: Vec<String> = locations.iter().map(Location::key).collect();
        keys.sort();
        let mut hash = Sha256::new();
        hash.update(tree.as_bytes());
        for key in &keys {
            hash.update((key.len() as u64).to_be_bytes());
            hash.update(key.as_bytes());
        }
        let digest = hash.finalize();
        Self(hex::encode(&digest[..16]))
    }

    /// Reads thirty-two lower-case hex digits; anything else is none.
    pub(super) fn parse(text: &str) -> Option<Self> {
        let hex = text.len() == 32 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        hex.then(|| Self(text.to_owned()))
    }
}

impl fmt::Display for PlacementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The placement a bucket and key, or a mount path, fall under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    /// The placement.
    pub placement: PlacementId,
    /// The tree it places.
    pub tree: TreeDigest,
    /// The path below the root of the tree: empty for the root itself; no `/` at its start.
    pub path: String,
}

/// How a new location meets a placement already in the table.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Meets {
    /// The same location.
    Equal(PlacementId),
    /// Inside, or around, the location of the placement.
    Overlaps(PlacementId),
}

/// The table of the locations of a node, keyed by [`Location::key`].
pub(super) type Table = BTreeMap<String, PlacementId>;

/// Which placement of `table`, if any, `key` (a [`Location::key`]) is equal to, inside or around.
pub(super) fn meets(table: &Table, key: &str) -> Option<Meets> {
    if let Some(equal) = table.get(key) {
        return Some(Meets::Equal(equal.clone()));
    }
    // Around: a prefix of the key, at one of its `/`, is a location.
    let around = key
        .match_indices('/')
        .find_map(|(at, _)| table.get(&key[..=at]));
    if let Some(around) = around {
        return Some(Meets::Overlaps(around.clone()));
    }
    // Inside: a location that begins with the key.
    let inside = table
        .range(key.to_owned()..)
        .next()
        .filter(|(held, _)| held.starts_with(key));
    inside.map(|(_, id)| Meets::Overlaps(id.clone()))
}

/// The placement `table` has for the key `rest` under `scope` (`"s3\0<bucket>\0"` or `"fs\0\0"`),
/// and what is left of `rest` below it.
pub(super) fn under<'a>(
    table: &Table,
    scope: &str,
    rest: &'a str,
) -> Option<(PlacementId, &'a str)> {
    rest.match_indices('/').find_map(|(at, _)| {
        let key = format!("{scope}{}", &rest[..=at]);
        table.get(&key).map(|id| (id.clone(), &rest[at + 1..]))
    })
}
