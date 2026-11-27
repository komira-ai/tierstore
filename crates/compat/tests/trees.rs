//! One cache directory shared by this branch's tree store and the tree store of the release before
//! it (`tierstore_trees_prev`, the real `tierstore` library of that release's commit): the trees
//! and placements one wrote, `trees/<T>/` and `placements/<id>`, are read by the other. That is
//! what happens on every hand-over of a build node, where the trees the mount provider placed are
//! in the directory when a process of the other release starts over it.
//!
//! Each release writes the two kinds of tree: a tree of records (`records` and `sources`, what a
//! version 1 tree list becomes) placed at a bucket prefix and a mount path, and an image (`image`,
//! a version 2 tree list as it is) placed at a mount path the way the mount provider places one:
//! staged ahead, created, its ranges filled and then marked ready. The other release opens the
//! directory with an upstream that holds nothing, so every byte it serves is one the writer left
//! in the cache directory, and a ready tree it keeps is one whose ranges it found there.
//!
//! What a release does over the directory is one macro (`release!`), expanded once with each
//! release's crate, and the tests are another (`direction!`), expanded once for each direction, so
//! that both directions run the same steps and check the same answers.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tierstore_compat::patterned;
use tierstore_testkit::dir::TestDir;

/// The pack the trees' files are spans of, and the bucket of the S3 location.
const PACK: &str = "cas/pack/one";
const BUCKET: &str = "build-cas";
/// The sizes of the three blobs of the pack, one after the other.
const BLOBS: [usize; 3] = [1000, 2000, 3000];
/// The lease of every placement; the reader of the lease test starts after it has ended.
const TTL: Duration = Duration::from_secs(3600);

/// The paths every check reads, with the ones that are not in the trees and one below a file.
const PATHS: [&str; 11] = [
    "",
    "bin",
    "bin/tool",
    "bin/other",
    "lib",
    "lib/deep/data",
    "empty",
    "link",
    "zero",
    "missing",
    "bin/tool/below",
];

/// What a path of a placement is, as either release answers it: the same type for both, so that
/// the answers are compared.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Dir {
        mode: u16,
        entries: usize,
        names: Vec<String>,
    },
    File {
        mode: u16,
        size: u64,
        sha256: String,
        span: Option<(String, u64)>,
        bytes: Vec<u8>,
    },
    Symlink {
        target: String,
    },
    /// The path was refused: what the store said.
    Refused(String),
}

/// What a placement is, as either release answers it.
#[derive(Debug, PartialEq, Eq)]
struct Placed {
    id: String,
    tree: String,
    locations: Vec<String>,
    created_ms: u64,
    expires_ms: u64,
    ready: bool,
}

/// What the writer placed: the placements, by what they hold.
#[derive(Debug)]
struct Written {
    records: Placed,
    image: Placed,
    /// The bytes of the image, which its tree directory holds as `image`.
    image_bytes: Vec<u8>,
    /// The paths of the records tree and of the image, as the writer answered them.
    records_seen: Vec<Seen>,
    image_seen: Vec<Seen>,
}

/// The digest, offset and size of each blob of the pack, and the pack's bytes.
fn pack() -> (Vec<(String, u64, u64)>, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut blobs = Vec::new();
    for (seed, size) in BLOBS.iter().enumerate() {
        let blob = patterned(seed as u8 + 1, *size);
        let sha = tierstore::TreeDigest::of(&blob).to_string();
        blobs.push((sha, bytes.len() as u64, *size as u64));
        bytes.extend_from_slice(&blob);
    }
    (blobs, bytes)
}

/// `size` bytes of the file `path` from `offset`, as a reader of a span reads them.
fn read_span(path: &Path, offset: u64, size: u64) -> Vec<u8> {
    let mut file = std::fs::File::open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut bytes = vec![0; size as usize];
    file.read_exact(&mut bytes).unwrap();
    bytes
}

/// The directories of the scratch directory of a test: the upstream that holds the pack (the
/// writer's), an upstream that holds nothing (the reader's), and the cache directory.
struct Dirs {
    _dir: TestDir,
    upstream: PathBuf,
    empty: PathBuf,
    cache: PathBuf,
}

impl Dirs {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let (upstream, empty) = (dir.path().join("upstream"), dir.path().join("empty"));
        std::fs::create_dir_all(&upstream).unwrap();
        std::fs::create_dir_all(&empty).unwrap();
        let cache = dir.path().join("cache");
        Self {
            _dir: dir,
            upstream,
            empty,
            cache,
        }
    }

    /// The names in `trees/` and in `placements/` of the cache directory.
    fn names(&self) -> (Vec<String>, Vec<String>) {
        let names = |dir: &str| {
            let mut names: Vec<String> = std::fs::read_dir(self.cache.join(dir))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        (names("trees"), names("placements"))
    }
}

/// A disk whose free space is always ample, so that no fill depends on the host's disk; links and
/// times are the system's. One type for both releases' `Disk`.
#[derive(Clone, Copy, Debug, Default)]
struct AmpleDisk;

/// `AmpleDisk` as the `Disk` of the crate `$krate`.
macro_rules! ample_disk {
    ($krate:ident) => {
        impl $krate::server::local_cache::Disk for AmpleDisk {
            fn space(&self, _path: &Path) -> std::io::Result<$krate::server::local_cache::Space> {
                Ok($krate::server::local_cache::Space {
                    total: 1 << 40,
                    free: 1 << 40,
                })
            }

            fn link(&self, from: &Path, to: &Path) -> std::io::Result<()> {
                $krate::server::local_cache::Disk::link(
                    &$krate::server::local_cache::SystemDisk,
                    from,
                    to,
                )
            }

            fn links(&self, path: &Path) -> std::io::Result<u64> {
                $krate::server::local_cache::Disk::links(
                    &$krate::server::local_cache::SystemDisk,
                    path,
                )
            }

            fn touch(&self, path: &Path, millis: u64) -> std::io::Result<()> {
                $krate::server::local_cache::Disk::touch(
                    &$krate::server::local_cache::SystemDisk,
                    path,
                    millis,
                )
            }
        }
    };
}

ample_disk!(tierstore);
ample_disk!(tierstore_trees_prev);

/// The functions of one release over a cache directory, in the module `$name`, written with the
/// crate `$krate`: open a cache and its tree store, place the two trees, and answer what a
/// placement and its paths are.
macro_rules! release {
    ($name:ident, $krate:ident) => {
        mod $name {
            use std::path::Path;
            use std::sync::Arc;

            use bytes::Bytes;
            use $krate::server::local_cache::trees::{
                CreateRequest, ImageRequest, Location, PlacementId, Resolved, TreeStore,
                TreesConfig,
            };
            use $krate::server::local_cache::{CacheConfig, LocalCache};
            use $krate::{
                Backend, BlobSource, DirBackend, ListEntry, ListKind, Mode, NewEntry, NewKind,
                TestClock, TreeDigest, TreeImage, TreeLimits, TreeList, build_tree,
            };

            use super::{AmpleDisk, BUCKET, PACK, PATHS, Placed, Seen, TTL, Written};

            /// A cache over `cache` with the upstream `upstream` and the clock `clock`: one that
            /// takes the directory's lock if `locked` (and is read-only while another holds it),
            /// else one that takes none and is read-write.
            pub fn cache(
                cache: &Path,
                upstream: &Path,
                clock: &Arc<TestClock>,
                locked: bool,
            ) -> LocalCache {
                let config = CacheConfig::new(cache, 1 << 20, vec![("cas/".to_owned(), true)]);
                let upstream: Arc<dyn Backend> = Arc::new(DirBackend::open(upstream).unwrap());
                let (disk, clock) = (Arc::new(AmpleDisk), clock.clone());
                if locked {
                    LocalCache::open_locked(config, upstream, disk, clock).unwrap()
                } else {
                    LocalCache::open(config, upstream, disk, clock).unwrap()
                }
            }

            /// The tree store over `cache`, as a start opens it.
            pub fn store(cache: LocalCache) -> TreeStore {
                TreeStore::open(cache, TreesConfig::default()).unwrap()
            }

            /// A clock at `now` milliseconds.
            pub fn clock(now: u64) -> Arc<TestClock> {
                let clock = Arc::new(TestClock::new());
                clock.set(now);
                clock
            }

            /// Puts the pack in the upstream at `upstream`.
            pub async fn put_pack(upstream: &Path, bytes: &[u8]) {
                let backend = DirBackend::open(upstream).unwrap();
                backend
                    .put(PACK, Bytes::copy_from_slice(bytes))
                    .await
                    .unwrap();
            }

            fn sha(hex: &str) -> TreeDigest {
                TreeDigest::from_hex(hex).unwrap()
            }

            fn source(offset: u64) -> Option<BlobSource> {
                Some(BlobSource {
                    key: Arc::from(PACK),
                    offset,
                })
            }

            /// The tree of the paths of `PATHS` as records: two directories of the pack's three
            /// blobs (one of them setuid), an empty directory, a link and an empty file.
            fn records_entries(blobs: &[(String, u64, u64)]) -> Vec<NewEntry> {
                let file = |path: &str, blob: usize, mode: Option<u16>| {
                    let (hex, offset, size) = &blobs[blob];
                    NewEntry {
                        path: path.to_owned(),
                        kind: NewKind::File {
                            mode: mode.map(|bits| Mode::new(bits).unwrap()),
                            sha256: sha(hex),
                            size: *size,
                            from: source(*offset),
                        },
                    }
                };
                vec![
                    file("bin/tool", 0, Some(0o4755)),
                    file("bin/other", 1, None),
                    file("lib/deep/data", 2, None),
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

            /// The same tree as a version 2 tree list, with `other_mode` the mode of `bin/other`,
            /// so that two modes are two images.
            pub fn image_bytes(blobs: &[(String, u64, u64)], other_mode: u16) -> Vec<u8> {
                let file = |path: &str, blob: usize, mode: u16| {
                    let (hex, offset, size) = &blobs[blob];
                    ListEntry {
                        path: path.to_owned(),
                        kind: ListKind::File {
                            mode: Mode::new(mode).unwrap(),
                            size: *size,
                            source: source(*offset),
                            sha256: Some(sha(hex)),
                        },
                    }
                };
                let list = TreeList {
                    entries: vec![
                        file("bin/tool", 0, 0o4755),
                        file("bin/other", 1, other_mode),
                        file("lib/deep/data", 2, 0o644),
                        ListEntry {
                            path: "empty".to_owned(),
                            kind: ListKind::Dir { mode: Mode::DIR },
                        },
                        ListEntry {
                            path: "link".to_owned(),
                            kind: ListKind::Symlink {
                                target: "bin/tool".to_owned(),
                            },
                        },
                        ListEntry {
                            path: "zero".to_owned(),
                            kind: ListKind::File {
                                mode: Mode::new(0o644).unwrap(),
                                size: 0,
                                source: None,
                                sha256: Some(TreeDigest::of(b"")),
                            },
                        },
                    ],
                };
                list.encode_v2().unwrap()
            }

            /// Fills every blob of the pack into the cache of `store`.
            async fn fill(store: &TreeStore, blobs: &[(String, u64, u64)]) {
                let parts: Vec<(u64, u64, TreeDigest)> = blobs
                    .iter()
                    .map(|(hex, offset, size)| (*offset, *size, sha(hex)))
                    .collect();
                store.fetch_parts(PACK, &parts).await.unwrap();
            }

            /// Places an image of `bytes` at the mount path `at` as the mount provider does: the
            /// image staged ahead, the tree created with it, and marked ready once its ranges are
            /// held (`fill` first).
            pub async fn place_image(
                store: &TreeStore,
                blobs: &[(String, u64, u64)],
                bytes: &[u8],
                at: &str,
            ) -> PlacementId {
                let bytes = Bytes::copy_from_slice(bytes);
                let staged = store.stage_image(bytes.clone()).unwrap();
                let image = TreeImage::open(bytes.clone(), &TreeLimits::default(), |_| {}).unwrap();
                let made = store
                    .create_image(ImageRequest {
                        image: Arc::new(image),
                        digest: TreeDigest::of(&bytes),
                        at: vec![Location::fs(at).unwrap()],
                        ttl: Some(TTL),
                        ready: false,
                        staged: Some(staged),
                    })
                    .unwrap();
                fill(store, blobs).await;
                let pins = store.mark_ready(&made.tree).unwrap();
                assert_eq!((pins.refused, pins.absent), (0, 0), "every pack held");
                made.placement
            }

            /// Places the records tree at the bucket prefix `rec/` and the mount path `rec`, as a
            /// version 1 list is placed: created, then marked ready once its ranges are held.
            pub async fn place_records(
                store: &TreeStore,
                blobs: &[(String, u64, u64)],
            ) -> PlacementId {
                let built = build_tree(None, records_entries(blobs), &TreeLimits::default());
                let made = store
                    .create(CreateRequest {
                        built: built.unwrap(),
                        at: vec![
                            Location::s3(BUCKET, "rec/").unwrap(),
                            Location::fs("rec").unwrap(),
                        ],
                        ttl: Some(TTL),
                        ready: false,
                    })
                    .unwrap();
                fill(store, blobs).await;
                store.mark_ready(&made.tree).unwrap();
                made.placement
            }

            /// Leaves an image staged and never created, as a preparation cut short by the end of
            /// its process does: the handle, whose drop would remove the staging directory, is
            /// forgotten, as a process that is killed runs no destructor.
            pub fn leave_staged(store: &TreeStore, bytes: &[u8]) {
                let staged = store.stage_image(Bytes::copy_from_slice(bytes)).unwrap();
                std::mem::forget(staged);
            }

            /// Writes both trees into the cache of `store`, and what the writer answers of them.
            pub async fn write(store: &TreeStore, blobs: &[(String, u64, u64)]) -> Written {
                let image_bytes = image_bytes(blobs, 0o644);
                let image = place_image(store, blobs, &image_bytes, "img").await;
                let records = place_records(store, blobs).await;
                Written {
                    records: placed(store, &records),
                    image: placed(store, &image),
                    image_bytes,
                    records_seen: seen(store, &records).await,
                    image_seen: seen(store, &image).await,
                }
            }

            /// The id of the placement the bucket key `key` falls under, and its path.
            pub fn locate_s3(store: &TreeStore, key: &str) -> Option<(String, String)> {
                let located = store.locate_s3(BUCKET, key)?;
                Some((located.placement.as_str().to_owned(), located.path))
            }

            /// The id of the placement the mount path `path` falls under, and its path.
            pub fn locate_fs(store: &TreeStore, path: &str) -> Option<(String, String)> {
                let located = store.locate_fs(path)?;
                Some((located.placement.as_str().to_owned(), located.path))
            }

            /// The placement the mount path `path` falls under, which the store must have.
            pub fn at(store: &TreeStore, path: &str) -> PlacementId {
                store.locate_fs(path).unwrap().placement
            }

            /// What the store says of the placement `id`.
            pub fn placed(store: &TreeStore, id: &PlacementId) -> Placed {
                let info = store.placement(id).unwrap();
                let mut locations: Vec<String> =
                    info.locations.iter().map(|at| at.to_string()).collect();
                locations.sort();
                Placed {
                    id: id.as_str().to_owned(),
                    tree: info.tree.to_string(),
                    locations,
                    created_ms: info.created_ms,
                    expires_ms: info.expires_ms,
                    ready: info.ready,
                }
            }

            /// What the store says of each of `PATHS` of the placement `id`, with the bytes of each
            /// file read from the cache.
            pub async fn seen(store: &TreeStore, id: &PlacementId) -> Vec<Seen> {
                let mut out = Vec::new();
                for path in PATHS {
                    let answer = match store.resolve(id, path) {
                        Err(why) => Seen::Refused(why.to_string()),
                        Ok(Resolved::Dir { mode, entries }) => {
                            let listing = store.list(id, path, None, 1000).unwrap();
                            let names = listing
                                .entries
                                .iter()
                                .map(|child| child.name().to_owned())
                                .collect();
                            assert_eq!(listing.next, None, "{path}");
                            Seen::Dir {
                                mode: mode.bits(),
                                entries,
                                names,
                            }
                        }
                        Ok(Resolved::Symlink { target }) => Seen::Symlink { target },
                        Ok(Resolved::File {
                            mode,
                            size,
                            sha256,
                            span,
                        }) => {
                            let open = store.open_file(id, path).await.unwrap();
                            let bytes = match &open.span {
                                Some(held) => super::read_span(&held.path, held.offset, size),
                                None => Vec::new(),
                            };
                            assert_eq!(TreeDigest::of(&bytes), sha256, "{path}");
                            let stat = store.stat(id, path).unwrap();
                            assert_eq!((stat.size, stat.mode), (size, mode), "{path}");
                            Seen::File {
                                mode: mode.bits(),
                                size,
                                sha256: sha256.to_string(),
                                span: span.map(|span| (span.pack, span.offset)),
                                bytes,
                            }
                        }
                    };
                    out.push(answer);
                }
                out
            }

            /// Leaves what a writer leaves behind besides a live tree, with `clock` at the start: an
            /// image placed at `old`, then (half a lease later) the same tree with another mode at
            /// `img`, and a third image staged and never placed. Returns the placements at `img`
            /// and at `old`.
            pub async fn leave_ended(
                store: &TreeStore,
                blobs: &[(String, u64, u64)],
                clock: &TestClock,
            ) -> (Placed, Placed) {
                let ended = place_image(store, blobs, &image_bytes(blobs, 0o600), "old").await;
                clock.advance(TTL.as_millis() as u64 / 2);
                let live = place_image(store, blobs, &image_bytes(blobs, 0o644), "img").await;
                leave_staged(store, &image_bytes(blobs, 0o640));
                (placed(store, &live), placed(store, &ended))
            }

            /// Places the image `bytes` at `at` again, as the next preparation of the same source
            /// does: the same tree at the same location, which refreshes its lease.
            pub fn place_again(store: &TreeStore, bytes: &[u8], at: &str) -> (bool, bool, String) {
                let bytes = Bytes::copy_from_slice(bytes);
                let image = TreeImage::open(bytes.clone(), &TreeLimits::default(), |_| {}).unwrap();
                let made = store
                    .create_image(ImageRequest {
                        image: Arc::new(image),
                        digest: TreeDigest::of(&bytes),
                        at: vec![Location::fs(at).unwrap()],
                        ttl: Some(TTL),
                        ready: false,
                        staged: None,
                    })
                    .unwrap();
                (
                    made.created,
                    made.new_tree,
                    made.placement.as_str().to_owned(),
                )
            }
        }
    };
}

release!(now, tierstore);
release!(before, tierstore_trees_prev);

/// When the writer's trees are placed; the reader starts later.
const START: u64 = 1_000_000;
const TTL_MS: u64 = TTL.as_millis() as u64;

/// Asserts that the answers `seen` (of `PATHS`, in order) are those of the tree both kinds hold:
/// the bytes of each file are its blob of the pack, the empty file has no span, and what is not a
/// file of the tree is refused as the store says it.
fn assert_tree(seen: &[Seen], blobs: &[(String, u64, u64)], pack: &[u8]) {
    let blob = |i: usize| {
        let (sha, offset, size) = &blobs[i];
        let range = *offset as usize..(*offset + *size) as usize;
        (sha.clone(), *offset, *size, pack[range].to_vec())
    };
    let file = |mode: u16, i: usize| {
        let (sha256, offset, size, bytes) = blob(i);
        Seen::File {
            mode,
            size,
            sha256,
            span: Some((PACK.to_owned(), offset)),
            bytes,
        }
    };
    let names = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
    let expected = vec![
        Seen::Dir {
            mode: 0o755,
            entries: 5,
            names: names(&["bin", "empty", "lib", "link", "zero"]),
        },
        Seen::Dir {
            mode: 0o755,
            entries: 2,
            names: names(&["other", "tool"]),
        },
        file(0o4755, 0),
        file(0o644, 1),
        Seen::Dir {
            mode: 0o755,
            entries: 1,
            names: names(&["deep"]),
        },
        file(0o644, 2),
        Seen::Dir {
            mode: 0o755,
            entries: 0,
            names: Vec::new(),
        },
        Seen::Symlink {
            target: "bin/tool".to_owned(),
        },
        Seen::File {
            mode: 0o644,
            size: 0,
            sha256: tierstore::TreeDigest::of(b"").to_string(),
            span: None,
            bytes: Vec::new(),
        },
        Seen::Refused("no such path".to_owned()),
        Seen::Refused("not a directory".to_owned()),
    ];
    assert_eq!(seen, expected);
}

/// The tests of one direction: `$writer` writes the directory, `$reader` reads it.
macro_rules! direction {
    ($writer:ident, $reader:ident, $reads:ident, $drops:ident, $hand_over:ident) => {
        /// Asserts that a start of the reader over the directory the writer left keeps both trees
        /// and both placements (nothing in `trees/` or `placements/` is removed), finds each
        /// location the writer placed (the bucket prefix and both mount paths) at the same
        /// placement id, says of each placement what the writer said (tree, locations, lease,
        /// ready) and of every path what the writer said, with the bytes of every file read from
        /// the cache the writer filled (the reader's upstream holds nothing), and that the image
        /// file is the version 2 list's bytes as they are. Then the reader places the same image
        /// at the same path again (the next preparation of the source): it is a refresh, not a
        /// new placement or tree, and the writer, started again, reads the refreshed lease.
        #[tokio::test]
        async fn $reads() {
            let dirs = Dirs::new(concat!("compat-trees-", stringify!($reads)));
            let (blobs, pack) = pack();
            $writer::put_pack(&dirs.upstream, &pack).await;
            let written = {
                let clock = $writer::clock(START);
                let cache = $writer::cache(&dirs.cache, &dirs.upstream, &clock, false);
                $writer::write(&$writer::store(cache), &blobs).await
            };
            assert_tree(&written.records_seen, &blobs, &pack);
            assert_tree(&written.image_seen, &blobs, &pack);
            let image_dir = dirs.cache.join("trees").join(&written.image.tree);
            let image = std::fs::read(image_dir.join("image")).unwrap();
            assert_eq!(image, written.image_bytes, "the image is the list as it is");
            let mut trees = vec![written.image.tree.clone(), written.records.tree.clone()];
            trees.sort();
            let mut ids = vec![written.image.id.clone(), written.records.id.clone()];
            ids.sort();
            assert_eq!(dirs.names(), (trees.clone(), ids.clone()));

            let clock = $reader::clock(START + 1000);
            let reader = $reader::store($reader::cache(&dirs.cache, &dirs.empty, &clock, false));
            let at = |id: &str, path: &str| Some((id.to_owned(), path.to_owned()));
            let records = written.records.id.as_str();
            let image = written.image.id.as_str();
            assert_eq!(
                $reader::locate_fs(&reader, "img/bin/tool"),
                at(image, "bin/tool")
            );
            assert_eq!($reader::locate_fs(&reader, "rec/zero"), at(records, "zero"));
            assert_eq!(
                $reader::locate_s3(&reader, "rec/lib/deep/data"),
                at(records, "lib/deep/data")
            );
            assert_eq!($reader::locate_fs(&reader, "other"), None);
            let image_id = $reader::at(&reader, "img");
            let records_id = $reader::at(&reader, "rec");
            assert_eq!($reader::placed(&reader, &image_id), written.image);
            assert_eq!($reader::placed(&reader, &records_id), written.records);
            assert!(written.image.ready && written.records.ready);
            assert_eq!($reader::seen(&reader, &image_id).await, written.image_seen);
            assert_eq!(
                $reader::seen(&reader, &records_id).await,
                written.records_seen
            );
            assert_eq!(
                dirs.names(),
                (trees.clone(), ids.clone()),
                "the reader removed nothing"
            );

            let again = $reader::place_again(&reader, &written.image_bytes, "img");
            assert_eq!(again, (false, false, written.image.id.clone()), "a refresh");
            drop(reader);
            assert_eq!(dirs.names(), (trees, ids));
            let clock = $writer::clock(START + 2000);
            let writer = $writer::store($writer::cache(&dirs.cache, &dirs.empty, &clock, false));
            let refreshed = $writer::placed(&writer, &$writer::at(&writer, "img"));
            assert_eq!(refreshed.expires_ms, START + 1000 + TTL_MS);
            assert_eq!(refreshed.created_ms, written.image.created_ms);
            let seen = $writer::seen(&writer, &$writer::at(&writer, "img")).await;
            assert_eq!(seen, written.image_seen);
        }

        /// Asserts that a start of the reader, read-write over the directory (a cache that takes
        /// no lock, as one that holds the lock is), drops what the writer left that no longer
        /// holds a tree: an image staged and never placed (`trees/.new-<n>`), and a placement
        /// whose lease ended with the tree only it held (its file and its tree's directory
        /// removed), while it keeps and serves the placement whose lease has not ended.
        #[tokio::test]
        async fn $drops() {
            let dirs = Dirs::new(concat!("compat-trees-", stringify!($drops)));
            let (blobs, pack) = pack();
            $writer::put_pack(&dirs.upstream, &pack).await;
            let (live, ended) = {
                let clock = $writer::clock(START);
                let cache = $writer::cache(&dirs.cache, &dirs.upstream, &clock, false);
                $writer::leave_ended(&$writer::store(cache), &blobs, &clock).await
            };
            assert_left(&dirs, &live, &ended);

            let clock = $reader::clock(START + TTL_MS + 1);
            let reader = $reader::store($reader::cache(&dirs.cache, &dirs.empty, &clock, false));
            assert_eq!(
                $reader::locate_fs(&reader, "old/bin/tool"),
                None,
                "the lease ended"
            );
            let id = $reader::at(&reader, "img");
            assert_eq!($reader::placed(&reader, &id), live);
            assert_tree(&$reader::seen(&reader, &id).await, &blobs, &pack);
            assert_eq!(dirs.names(), (vec![live.tree], vec![live.id]));
        }

        /// Asserts that a reader started while the writer still holds the directory's lock (a
        /// hand-over: the new process starts before the old one ends) is read-only, serves the
        /// placement whose lease has not ended, and leaves out but removes nothing of what the
        /// writer left: the staged image, the ended placement and its tree stay in the directory.
        #[tokio::test]
        async fn $hand_over() {
            let dirs = Dirs::new(concat!("compat-trees-", stringify!($hand_over)));
            let (blobs, pack) = pack();
            $writer::put_pack(&dirs.upstream, &pack).await;
            let clock = $writer::clock(START);
            let cache = $writer::cache(&dirs.cache, &dirs.upstream, &clock, true);
            assert_eq!(cache.mode().as_str(), "read-write");
            let writer = $writer::store(cache);
            let (live, ended) = $writer::leave_ended(&writer, &blobs, &clock).await;
            let left = assert_left(&dirs, &live, &ended);

            let clock = $reader::clock(START + TTL_MS + 1);
            let cache = $reader::cache(&dirs.cache, &dirs.empty, &clock, true);
            assert_eq!(cache.mode().as_str(), "read-only");
            let reader = $reader::store(cache);
            assert_eq!(
                $reader::locate_fs(&reader, "old/bin/tool"),
                None,
                "the lease ended"
            );
            let id = $reader::at(&reader, "img");
            assert_eq!($reader::placed(&reader, &id), live);
            assert_tree(&$reader::seen(&reader, &id).await, &blobs, &pack);
            assert_eq!(
                dirs.names(),
                left,
                "a reader without the lock removed nothing"
            );
            drop(writer);
        }
    };
}

/// Asserts that the directory holds what `leave_ended` left: the trees of `live` and `ended` and
/// one staged image, and the two placements. Returns the names.
fn assert_left(dirs: &Dirs, live: &Placed, ended: &Placed) -> (Vec<String>, Vec<String>) {
    let (trees, placements) = dirs.names();
    let staged: Vec<&String> = trees
        .iter()
        .filter(|name| name.starts_with(".new-"))
        .collect();
    assert_eq!(staged.len(), 1, "{trees:?}");
    let mut expected = vec![live.tree.clone(), ended.tree.clone(), staged[0].clone()];
    expected.sort();
    assert_eq!(trees, expected);
    let mut ids = vec![live.id.clone(), ended.id.clone()];
    ids.sort();
    assert_eq!(placements, ids);
    (trees, placements)
}

direction!(
    now,
    before,
    the_previous_release_reads_the_trees_this_branch_placed,
    the_previous_release_drops_what_this_branch_left_ended_or_unfinished,
    the_previous_release_handed_over_to_reads_and_removes_nothing
);
direction!(
    before,
    now,
    this_branch_reads_the_trees_the_previous_release_placed,
    this_branch_drops_what_the_previous_release_left_ended_or_unfinished,
    this_branch_handed_over_to_reads_and_removes_nothing
);
