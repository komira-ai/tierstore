//! Fixtures of the tests of the tree store: a cache over an upstream with a pack in it, the entries
//! of a small tree whose files are members of the pack, and the requests that make it.

use std::sync::Arc;

use super::super::pack_testing::Pack;
use super::super::testing::Rig;
use super::super::{LocalCache, RangeWant};
use super::{CreateRequest, Location, TreeStore, TreesConfig};
use crate::{BlobSource, BuiltTree, Mode, NewEntry, NewKind, TreeDigest, TreeLimits, build_tree};

/// A cache of a mebibyte with the pack `cas/pack/one` of three blobs (1000, 2000 and 3000 bytes)
/// in its upstream, and a tree store over it.
pub(in crate::server) struct Bench {
    pub rig: Rig,
    pub pack: Pack,
    pub store: TreeStore,
}

impl Bench {
    pub(in crate::server) async fn new(name: &str) -> Self {
        Self::with(name, TreesConfig::default()).await
    }

    pub(in crate::server) async fn with(name: &str, config: TreesConfig) -> Self {
        let rig = Rig::new(name);
        let pack = Pack::put(&rig, "cas/pack/one", &[1000, 2000, 3000]).await;
        let store = TreeStore::open(rig.cache.clone(), config).expect("a store");
        Self { rig, pack, store }
    }

    /// The store again over the same directory, as after a restart, over `cache`.
    pub(in crate::server) fn reopen(cache: LocalCache) -> TreeStore {
        TreeStore::open(cache, TreesConfig::default()).expect("a store")
    }

    /// The file `path` whose bytes are the blob `blob` of the pack.
    pub(in crate::server) fn member(&self, path: &str, blob: usize) -> NewEntry {
        self.member_with(path, blob, None)
    }

    /// The same with the mode `mode`.
    fn member_with(&self, path: &str, blob: usize, mode: Option<Mode>) -> NewEntry {
        let (sha256, offset, size) = &self.pack.blobs[blob];
        NewEntry {
            path: path.to_owned(),
            kind: NewKind::File {
                mode,
                sha256: TreeDigest::from_hex(sha256).expect("a digest"),
                size: *size,
                from: Some(BlobSource {
                    key: Arc::from(self.pack.key.as_str()),
                    offset: *offset,
                }),
            },
        }
    }

    /// The members of the pack as a tree of two directories, a symbolic link and an empty
    /// directory, and one empty file.
    pub(in crate::server) fn entries(&self) -> Vec<NewEntry> {
        let shown = self.member_with("bin/tool", 0, Some(Mode::new(0o4755).expect("a mode")));
        vec![
            shown,
            self.member("bin/other", 1),
            self.member("lib/deep/data", 2),
            NewEntry {
                path: "empty".to_owned(),
                kind: NewKind::Dir { mode: None },
            },
            NewEntry {
                path: "link".to_owned(),
                kind: NewKind::Symlink {
                    target: "bin/tool".to_owned(),
                },
            },
            NewEntry {
                path: "zero".to_owned(),
                kind: NewKind::File {
                    mode: None,
                    sha256: TreeDigest::of(b""),
                    size: 0,
                    from: None,
                },
            },
        ]
    }

    pub(in crate::server) fn built(&self) -> BuiltTree {
        built(self.entries())
    }

    /// A request to place the small tree at `at`.
    pub(in crate::server) fn request(&self, at: Vec<Location>) -> CreateRequest {
        CreateRequest {
            built: self.built(),
            at,
            ttl: None,
            ready: false,
        }
    }

    /// Fills the range of the blob `blob` of the pack.
    pub(in crate::server) async fn fill(&self, blob: usize) {
        let (sha256, offset, size) = &self.pack.blobs[blob];
        let want = RangeWant {
            pack: &self.pack.key,
            known: None,
            offset: *offset,
            len: *size,
            sha256,
        };
        self.rig.cache.fill_range(&want).await.expect("a fill");
    }

    pub(in crate::server) async fn fill_all(&self) {
        for blob in 0..self.pack.blobs.len() {
            self.fill(blob).await;
        }
    }
}

/// The tree of `entries`.
pub(in crate::server) fn built(entries: Vec<NewEntry>) -> BuiltTree {
    build_tree(None, entries, &TreeLimits::default()).expect("a tree")
}

pub(in crate::server) fn fs_at(path: &str) -> Location {
    Location::fs(path).expect("a location")
}

pub(in crate::server) fn s3_at(prefix: &str) -> Location {
    Location::s3("build-cas", prefix).expect("a location")
}

/// The directory of the tree `tree` in the cache directory of `rig`.
pub(in crate::server) fn tree_path(rig: &Rig, tree: &TreeDigest) -> std::path::PathBuf {
    super::files::tree_dir(&rig.config.path, tree)
}
