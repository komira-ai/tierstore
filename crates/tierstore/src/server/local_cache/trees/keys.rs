//! The keys the placements of a bucket show to a listing of the S3 front, in key order.
//!
//! A placement at the prefix `L` shows a key `L<path>` for each file and each symbolic link of its
//! tree; a directory is no key (S3 has no directory objects), so an empty directory shows nothing.
//! A listing of the prefix `P` meets the placements whose `L` begins with `P` (the listing is
//! above them) and the one whose `L` is a prefix of `P` (the listing is inside it); no two
//! placements meet one key, so their keys never collide.
//!
//! Key order is not the order of a walk by names: the names `a` (a directory) and `a-b` (a file)
//! are in that order in the record, but the key `L/a-b` sorts before every key `L/a/...`, since
//! `-` is below `/`. So the children of a directory are taken in the order of their keys: a file
//! by its name, a directory by its name and a `/`. The keys of one directory are then one run of
//! the order, and a directory whose run is wholly before the start, or outside the prefix, is not
//! read.

use super::TreeStore;
use super::location::PlacementId;
use super::read::ReadError;
use crate::{RecordChild, TreeDigest};

/// The most children of a directory read at a time.
const PAGE: usize = 1000;

/// A key a placement shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeKey {
    /// The key: the placement's prefix and the path below its root.
    pub key: String,
    /// The length of the file; 0 for a symbolic link, whose read has no bytes.
    pub size: u64,
    /// The SHA-256 of what a read of the key gives.
    pub sha256: TreeDigest,
    /// When the placement was made, in milliseconds since the Unix epoch.
    pub created_ms: u64,
}

/// A placement a listing meets: its id, its prefix and when it was made.
struct Meeting {
    id: PlacementId,
    at: String,
    created_ms: u64,
}

/// The walk of one listing: what it keeps and how many keys it wants.
struct Walk<'a> {
    prefix: &'a str,
    after: Option<&'a str>,
    limit: usize,
    created_ms: u64,
    out: Vec<TreeKey>,
}

impl Walk<'_> {
    /// Whether a key of the run of keys that begin with `run` can be in the listing.
    fn may_hold(&self, run: &str) -> bool {
        let meets = run.starts_with(self.prefix) || self.prefix.starts_with(run);
        // A start that is at or past `run` and not inside it is past every key of the run.
        let passed = self
            .after
            .is_some_and(|after| after >= run && !after.starts_with(run));
        meets && !passed
    }

    /// Whether the key `key` is in the listing.
    fn takes(&self, key: &str) -> bool {
        key.starts_with(self.prefix) && self.after.is_none_or(|after| key > after)
    }

    fn full(&self) -> bool {
        self.out.len() >= self.limit
    }
}

impl TreeStore {
    /// Whether some placement alive has a location in the bucket `bucket`.
    #[must_use]
    pub fn places_in(&self, bucket: &str) -> bool {
        let scope = format!("s3\0{bucket}\0");
        let state = self.shared.lock();
        let now = self.shared.now();
        let mut held = state.table.range(scope.clone()..);
        held.any(|(key, id)| {
            key.starts_with(&scope)
                && state
                    .placements
                    .get(id)
                    .is_some_and(|placement| placement.locatable(now))
        })
    }

    /// At most `limit` keys that the placements of the bucket `bucket` show, that begin with
    /// `prefix` and sort after `after` (if given), in key order: the first `limit` such keys
    /// there are. A placement that ends while it is walked shows none.
    ///
    /// # Errors
    /// [`ReadError::Corrupt`] if a placement's records cannot be read.
    pub fn s3_keys(
        &self,
        bucket: &str,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TreeKey>, ReadError> {
        let mut keys = Vec::new();
        for meeting in self.meeting(bucket, prefix) {
            let mut walk = Walk {
                prefix,
                after,
                limit,
                created_ms: meeting.created_ms,
                out: Vec::new(),
            };
            if walk.may_hold(&meeting.at) {
                match self.walk_dir(&meeting.id, "", &meeting.at, &mut walk) {
                    Ok(()) | Err(ReadError::NoPlacement) => {}
                    Err(error) => return Err(error),
                }
            }
            keys.append(&mut walk.out);
        }
        keys.sort_by(|a, b| a.key.cmp(&b.key));
        keys.truncate(limit);
        Ok(keys)
    }

    /// The placements of `bucket`, alive and not released, whose prefix begins with `prefix` or
    /// is a prefix of it.
    fn meeting(&self, bucket: &str, prefix: &str) -> Vec<Meeting> {
        let scope = format!("s3\0{bucket}\0");
        let state = self.shared.lock();
        let now = self.shared.now();
        let first = format!("{scope}{prefix}");
        let inside = state
            .table
            .range(first.clone()..)
            .take_while(|(key, _)| key.starts_with(&first));
        let around = prefix
            .match_indices('/')
            .filter_map(|(at, _)| {
                let key = format!("{scope}{}", &prefix[..=at]);
                state.table.get_key_value(&key)
            })
            .filter(|(key, _)| key.len() < first.len());
        around
            .chain(inside)
            .filter_map(|(key, id)| {
                let placement = state.placements.get(id)?;
                placement.locatable(now).then(|| Meeting {
                    id: id.clone(),
                    at: key[scope.len()..].to_owned(),
                    created_ms: placement.created_ms,
                })
            })
            .collect()
    }

    /// Walks the directory `path` of the placement `id`, whose keys begin with `run`, in key
    /// order, until `walk` has its keys.
    fn walk_dir(
        &self,
        id: &PlacementId,
        path: &str,
        run: &str,
        walk: &mut Walk<'_>,
    ) -> Result<(), ReadError> {
        let mut children = self.children(id, path)?;
        children.sort_by_cached_key(|child| match child {
            RecordChild::Dir { name, .. } => format!("{name}/"),
            other => other.name().to_owned(),
        });
        for child in children {
            if walk.full() {
                return Ok(());
            }
            let name = child.name();
            let below = if path.is_empty() {
                name.to_owned()
            } else {
                format!("{path}/{name}")
            };
            let (size, sha256) = match &child {
                RecordChild::Dir { .. } => {
                    let inner = format!("{run}{name}/");
                    if walk.may_hold(&inner) {
                        self.walk_dir(id, &below, &inner, walk)?;
                    }
                    continue;
                }
                RecordChild::File { size, sha256, .. } => (*size, *sha256),
                RecordChild::Symlink { .. } => (0, TreeDigest::of(b"")),
            };
            let key = format!("{run}{name}");
            if walk.takes(&key) {
                walk.out.push(TreeKey {
                    key,
                    size,
                    sha256,
                    created_ms: walk.created_ms,
                });
            }
        }
        Ok(())
    }

    /// Every child of the directory `path` of the placement `id`, read a page at a time.
    fn children(&self, id: &PlacementId, path: &str) -> Result<Vec<RecordChild>, ReadError> {
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = self.list(id, path, after.as_deref(), PAGE)?;
            all.extend(page.entries);
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(all),
            }
        }
    }
}
