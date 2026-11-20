use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{Value, json};
use tierstore::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList};
use tierstore_list::{PackCells, PackInfo, PackRange, TreeListL2, sidecar_from_v2};

use super::{Child, Fetch, encode_v2, manifest_key, parse, walk};
use crate::Error;

const KEY: &str = "cas/l/k";

fn hex_of(byte: u8) -> String {
    hex::encode([byte; 32])
}

fn file(name: &str, size: u64, pack: &str, offset: u64, exec: bool) -> Value {
    json!({"name": name, "kind": "file", "sha256": hex_of(1), "size": size, "exec": exec,
        "source": {"key": pack, "offset": offset}})
}

fn dir(name: &str, sha: &str) -> Value {
    json!({"name": name, "kind": "dir", "sha256": sha, "size": 137})
}

fn manifest(children: &[Value]) -> Vec<u8> {
    json!({"v": 1, "children": children})
        .to_string()
        .into_bytes()
}

fn parse_err(children: Value) -> String {
    let bytes = json!({"v": 1, "children": [children]}).to_string();
    parse(KEY, bytes.as_bytes()).unwrap_err().to_string()
}

/// An in-memory store of objects that counts what is asked of it and how many requests were in
/// flight together.
#[derive(Default)]
struct Store {
    objects: BTreeMap<String, Vec<u8>>,
    asked: Mutex<Vec<String>>,
    flight: AtomicUsize,
    peak: AtomicUsize,
}

impl Store {
    /// Stores a directory manifest of `children` and returns its digest in hex.
    fn put(&mut self, children: &[Value]) -> String {
        let bytes = manifest(children);
        let sha = TreeDigest::of(&bytes).to_string();
        self.objects.insert(manifest_key(&sha), bytes);
        sha
    }

    fn put_raw(&mut self, sha: &str, bytes: &[u8]) {
        self.objects.insert(manifest_key(sha), bytes.to_vec());
    }

    fn asked(&self) -> Vec<String> {
        let mut asked = self.asked.lock().unwrap().clone();
        asked.sort();
        asked
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Fetch for Store {
    async fn get(&self, key: &str) -> Result<Bytes, Error> {
        self.asked.lock().unwrap().push(key.to_owned());
        let now = self.flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::task::yield_now().await;
        self.flight.fetch_sub(1, Ordering::SeqCst);
        self.objects
            .get(key)
            .map(|bytes| Bytes::from(bytes.clone()))
            .ok_or_else(|| Error::Http {
                url: key.to_owned(),
                why: "status 404".to_owned(),
            })
    }
}

/// The tree of the tests: `a.txt` and an executable `run.sh` in one pack, an empty file, a link,
/// the same directory `src` under two names, and an empty directory with an empty directory in it.
fn sample_store() -> (Store, String) {
    let mut store = Store::default();
    let src = store.put(&[
        file("lib.rs", 3_000_000, "cas/b/p2", 0, false),
        json!({"name": "deep", "kind": "symlink", "target": "lib.rs"}),
    ]);
    let inner = store.put(&[]);
    let void = store.put(&[dir("inner", &inner)]);
    let root = store.put(&[
        file("a.txt", 5, "cas/b/p1", 0, false),
        file("run.sh", 7, "cas/b/p1", 5, true),
        json!({"name": "empty", "kind": "file", "sha256": hex_of(2), "size": 0}),
        json!({"name": "link", "kind": "symlink", "target": "../x"}),
        dir("src", &src),
        dir("copy", &src),
        dir("void", &void),
    ]);
    (store, root)
}

fn paths(list: &TreeList) -> Vec<(&str, &ListKind)> {
    let mut all: Vec<_> = list
        .entries
        .iter()
        .map(|e| (e.path.as_str(), &e.kind))
        .collect();
    all.sort_by_key(|(path, _)| *path);
    all
}

fn file_kind(mode: Mode, size: u64, pack: Option<(&str, u64)>, sha: Option<u8>) -> ListKind {
    ListKind::File {
        mode,
        size,
        source: pack.map(|(key, offset)| BlobSource {
            key: key.into(),
            offset,
        }),
        sha256: sha.map(|sha| TreeDigest::from_bytes([sha; 32])),
    }
}

/// A manifest of all three kinds is read in order; fields the reader does not know are ignored; a
/// file of size 0 needs no source and no sha256 (it gets the SHA-256 of no bytes), a link needs no
/// target, and a file's `exec` defaults to false.
#[test]
fn a_manifest_of_every_kind_is_read_and_unknown_fields_are_ignored() {
    let text = json!({"v": 1, "extra": [1], "children": [
        {"name": "Cargo.lock", "kind": "file", "sha256": hex_of(3), "size": 49047, "exec": false,
         "mtime": 5, "source": {"key": "cas/b/190405-0/pack", "offset": 10373, "x": 1}},
        {"name": "bin", "kind": "file", "sha256": hex_of(4), "size": 9, "exec": true,
         "source": {"key": "cas/b/q", "offset": 0}},
        {"name": "src", "kind": "dir", "sha256": hex_of(5), "size": 137},
        {"name": "link", "kind": "symlink", "target": "../x"},
        {"name": "bare", "kind": "symlink"},
        {"name": "zero", "kind": "file", "size": 0},
        {"name": "plain", "kind": "file", "size": 4, "sha256": hex_of(6),
         "source": {"key": "cas/b/q", "offset": 9}}
    ]})
    .to_string();
    let children = parse(KEY, text.as_bytes()).unwrap();
    let want = vec![
        Child::File {
            name: "Cargo.lock".into(),
            sha256: [3; 32],
            size: 49047,
            exec: false,
            source: Some(("cas/b/190405-0/pack".into(), 10373)),
        },
        Child::File {
            name: "bin".into(),
            sha256: [4; 32],
            size: 9,
            exec: true,
            source: Some(("cas/b/q".into(), 0)),
        },
        Child::Dir {
            name: "src".into(),
            sha256: [5; 32],
        },
        Child::Symlink {
            name: "link".into(),
            target: "../x".into(),
        },
        Child::Symlink {
            name: "bare".into(),
            target: String::new(),
        },
        Child::File {
            name: "zero".into(),
            sha256: *TreeDigest::of(&[]).as_bytes(),
            size: 0,
            exec: false,
            source: None,
        },
        Child::File {
            name: "plain".into(),
            sha256: [6; 32],
            size: 4,
            exec: false,
            source: Some(("cas/b/q".into(), 9)),
        },
    ];
    assert_eq!(children, want);
}

/// A manifest that is not JSON, has another version, or lacks its version or children is refused
/// with the key of the object in the message.
#[test]
fn a_manifest_of_the_wrong_shape_is_refused_naming_its_key() {
    let err = parse(KEY, b"not json").unwrap_err().to_string();
    assert_eq!(err, "cas/l/k: expected ident at line 1 column 2", "{err}");
    let err = parse(KEY, br#"{"v":2,"children":[]}"#)
        .unwrap_err()
        .to_string();
    assert_eq!(err, "cas/l/k: manifest version 2, only 1 is read");
    let err = parse(KEY, br#"{"children":[]}"#).unwrap_err().to_string();
    assert_eq!(err, "cas/l/k: no manifest version");
    let err = parse(KEY, br#"{"v":1}"#).unwrap_err().to_string();
    assert_eq!(err, "cas/l/k: no children list");
}

/// Each defect of a child names the child and the key: a missing name, kind, size, sha256 or
/// source, a name that is not a file name, a bad digest, and a kind other than the three.
#[test]
fn a_defective_child_is_refused_naming_it_and_the_key() {
    assert_eq!(
        parse_err(json!({"kind": "file"})),
        "cas/l/k: a child has no name"
    );
    assert_eq!(
        parse_err(json!({"name": "", "kind": "dir"})),
        "cas/l/k: child name \"\" is not a file name"
    );
    assert_eq!(
        parse_err(json!({"name": "a/b", "kind": "dir"})),
        "cas/l/k: child name \"a/b\" is not a file name"
    );
    assert_eq!(
        parse_err(json!({"name": "n"})),
        "cas/l/k: child \"n\" has no kind"
    );
    assert_eq!(
        parse_err(json!({"name": "n", "kind": "fifo"})),
        "cas/l/k: child \"n\" has the unknown kind \"fifo\""
    );
    assert_eq!(
        parse_err(json!({"name": "n", "kind": "file"})),
        "cas/l/k: file \"n\" has no size"
    );
    assert_eq!(
        parse_err(json!({"name": "n", "kind": "file", "size": 3})),
        "cas/l/k: file \"n\" has no sha256"
    );
    assert_eq!(
        parse_err(json!({"name": "n", "kind": "file", "size": 0, "sha256": "ABC"})),
        "cas/l/k: sha256 of \"n\" is not 64 lowercase hex digits"
    );
    assert_eq!(
        parse_err(json!({"name": "d", "kind": "dir"})),
        "cas/l/k: directory \"d\" has no sha256"
    );
    assert_eq!(
        parse_err(json!({"name": "d", "kind": "dir", "sha256": hex_of(0xab).to_uppercase()})),
        "cas/l/k: sha256 of \"d\" is not 64 lowercase hex digits"
    );
}

/// The key of a manifest is `cas/l/` and its digest.
#[test]
fn the_key_of_a_manifest_is_cas_l_and_its_digest() {
    assert_eq!(manifest_key("ab"), "cas/l/ab");
}

/// A walk lists every file and link by its path from the root with its mode (0755 for an
/// executable file, else 0644), its size, source and digest, and every empty directory; a
/// directory with something in it is not listed on its own.
#[tokio::test]
async fn a_walk_lists_files_links_and_empty_directories_by_path() {
    let (store, root) = sample_store();
    let walked = walk(&root, &store, 4).await.unwrap();
    let p1 = |offset| Some(("cas/b/p1", offset));
    let lib = file_kind(Mode::FILE, 3_000_000, Some(("cas/b/p2", 0)), Some(1));
    let link = ListKind::Symlink {
        target: "lib.rs".into(),
    };
    let want = vec![
        ("a.txt", file_kind(Mode::FILE, 5, p1(0), Some(1))),
        ("copy/deep", link.clone()),
        ("copy/lib.rs", lib.clone()),
        ("empty", file_kind(Mode::FILE, 0, None, None)),
        (
            "link",
            ListKind::Symlink {
                target: "../x".into(),
            },
        ),
        ("run.sh", file_kind(Mode::DIR, 7, p1(5), Some(1))),
        ("src/deep", link),
        ("src/lib.rs", lib),
        ("void/inner", ListKind::Dir { mode: Mode::DIR }),
    ];
    let got: Vec<(&str, ListKind)> = paths(&walked.list)
        .into_iter()
        .map(|(path, kind)| (path, kind.clone()))
        .collect();
    assert_eq!(got, want);
}

/// A walk counts the manifests fetched (the directory named twice once), the files above size 0
/// (twice for the directory named twice), their bytes and the distinct packs; and it asks the
/// store for each manifest exactly once.
#[tokio::test]
async fn a_walk_counts_manifests_files_bytes_and_packs_and_fetches_each_directory_once() {
    let (store, root) = sample_store();
    let walked = walk(&root, &store, 4).await.unwrap();
    assert_eq!(walked.manifests, 4);
    assert_eq!(walked.files, 2 + 2);
    assert_eq!(walked.bytes, 12 + 2 * 3_000_000);
    assert_eq!(walked.packs, 2);
    let asked = store.asked();
    assert_eq!(asked.len(), 4);
    assert!(asked.contains(&manifest_key(&root)));
    let mut unique = asked.clone();
    unique.dedup();
    assert_eq!(unique, asked);
}

/// A file of size 0 that names a pack is listed without the source and does not count as a file
/// or as naming the pack.
#[tokio::test]
async fn an_empty_file_with_a_source_is_listed_without_it_and_counts_nothing() {
    let mut store = Store::default();
    let root = store.put(&[file("e", 0, "cas/b/none", 4, false)]);
    let walked = walk(&root, &store, 1).await.unwrap();
    assert_eq!(
        walked.list.entries,
        vec![ListEntry {
            path: "e".into(),
            kind: file_kind(Mode::FILE, 0, None, None),
        }]
    );
    assert_eq!((walked.files, walked.bytes, walked.packs), (0, 0, 0));
    assert_eq!(walked.manifests, 1);
}

/// A tree whose root has no children is an empty list, read from one manifest.
#[tokio::test]
async fn an_empty_root_is_an_empty_list() {
    let mut store = Store::default();
    let root = store.put(&[]);
    let walked = walk(&root, &store, 1).await.unwrap();
    assert!(walked.list.entries.is_empty());
    assert_eq!(walked.manifests, 1);
}

/// With four directories at one level and a limit of 2 requests are never more than 2 in flight,
/// and 2 are in flight together; a limit of 0 is read as 1; a limit above the count allows all.
#[tokio::test]
async fn a_walk_keeps_at_most_parallel_fetches_in_flight() {
    for (parallel, peak) in [(2, 2), (0, 1), (1, 1), (16, 4)] {
        let mut store = Store::default();
        let dirs: Vec<Value> = (0u8..4)
            .map(|i| {
                let sha = store.put(&[file("f", 1, "cas/b/p", u64::from(i), false)]);
                dir(&format!("d{i}"), &sha)
            })
            .collect();
        let root = store.put(&dirs);
        let walked = walk(&root, &store, parallel).await.unwrap();
        assert_eq!(walked.manifests, 5);
        assert_eq!(walked.files, 4);
        assert_eq!(store.peak(), peak.max(1), "parallel {parallel}");
    }
}

/// A manifest the store does not have fails the walk with the store's error, naming the key.
#[tokio::test]
async fn a_missing_manifest_fails_the_walk_naming_its_key() {
    let mut store = Store::default();
    let gone = hex_of(9);
    let root = store.put(&[dir("gone", &gone)]);
    let err = walk(&root, &store, 2).await.unwrap_err();
    assert_eq!(err.to_string(), format!("cas/l/{gone}: status 404"));
}

/// A manifest that does not parse fails the walk with the parse error naming its key.
#[tokio::test]
async fn a_bad_manifest_fails_the_walk_naming_its_key() {
    let mut store = Store::default();
    let bad = hex_of(8);
    store.put_raw(&bad, b"{\"v\":3,\"children\":[]}");
    let root = store.put(&[dir("bad", &bad)]);
    let err = walk(&root, &store, 2).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("cas/l/{bad}: manifest version 3, only 1 is read")
    );
}

/// A tree id that is not 64 lowercase hex digits is refused before anything is fetched.
#[tokio::test]
async fn a_root_that_is_not_hex_is_refused_before_any_fetch() {
    let store = Store::default();
    let err = walk("xyz", &store, 2).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "xyz: the tree id is not 64 lowercase hex digits"
    );
    assert!(store.asked().is_empty());
}

/// A directory whose manifest names itself would never end; the walk stops at the depth a tree
/// may have and names the root's manifest.
#[tokio::test]
async fn a_directory_that_contains_itself_is_refused() {
    let mut store = Store::default();
    let me = hex_of(7);
    store.put_raw(&me, &manifest(&[dir("again", &me)]));
    let err = walk(&me, &store, 2).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("cas/l/{me}: tree deeper than 4096 levels")
    );
    assert_eq!(store.asked().len(), 1);
}

/// The list of a walk, as version 2 bytes, is accepted by the sidecar writer given cells of the
/// packs' sizes, and the sidecar opens: its packs are the two keys in order, its ranges merge the
/// neighbouring files of a pack and the file listed under two names.
#[tokio::test]
async fn the_version_2_list_of_a_walk_makes_a_sidecar_that_opens() {
    let (store, root) = sample_store();
    let walked = walk(&root, &store, 4).await.unwrap();
    let v2 = encode_v2(&walked.list).unwrap();
    let sizes: BTreeMap<&str, u64> = [("cas/b/p1", 12), ("cas/b/p2", 3_000_000)].into();
    let sidecar = sidecar_from_v2(&v2, |key| {
        let size = *sizes.get(key)?;
        Some(PackInfo {
            cells: PackCells {
                size,
                digests: vec![[7; 32]; size.div_ceil(1 << 20) as usize],
            },
            ..PackInfo::default()
        })
    })
    .unwrap();
    let l2 = TreeListL2::open(sidecar).unwrap();
    assert_eq!(l2.pack_key(0), Some("cas/b/p1"));
    assert_eq!(l2.pack_key(1), Some("cas/b/p2"));
    assert_eq!(l2.pack_key(2), None);
    let ranges: Vec<PackRange> = l2.ranges().collect();
    assert_eq!(
        ranges,
        vec![
            PackRange {
                pack: 0,
                offset: 0,
                len: 12
            },
            PackRange {
                pack: 1,
                offset: 0,
                len: 3_000_000
            },
        ]
    );
}

/// A list with a file above size 0 and no digest cannot be encoded; the error says which file.
#[test]
fn a_list_the_encoder_refuses_is_an_error_naming_the_file() {
    let list = TreeList {
        entries: vec![ListEntry {
            path: "x/y".into(),
            kind: ListKind::File {
                mode: Mode::FILE,
                size: 3,
                source: None,
                sha256: None,
            },
        }],
    };
    let err = encode_v2(&list).unwrap_err();
    assert!(
        matches!(
            err,
            Error::ReadPath {
                what: "encode the version 2 tree list",
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("x/y"), "{err}");
}

#[test]
fn a_file_with_bytes_and_no_source_is_the_loose_blob_of_its_digest() {
    let manifest = json!({"v": 1, "children": [
        {"name": "n", "kind": "file", "size": 3, "sha256": hex_of(1)}
    ]});
    let children = parse("cas/l/k", manifest.to_string().as_bytes()).unwrap();
    assert_eq!(
        children,
        vec![Child::File {
            name: "n".to_owned(),
            sha256: [1; 32],
            size: 3,
            exec: false,
            source: Some((format!("cas/l/{}", hex_of(1)), 0)),
        }]
    );
}
