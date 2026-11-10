//! The reading of a tree manifest and the flattening of a tree: every refusal by name, the
//! limits at their exact values, and the numbering of the entries.

use std::cell::Cell;
use std::collections::HashMap;

use crate::tree_manifest::count_entries;
use crate::{
    BlobSource, EntryKind, MAX_DEPTH, MAX_ENTRIES, MAX_MANIFEST_BYTES, TreeDigest, TreeError,
    TreeManifest, flatten, parse_manifest,
};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn json(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

fn dir(name: &str, digest: &TreeDigest) -> String {
    let name = json(name);
    format!(r#"{{"name":{name},"kind":"dir","sha256":"{digest}","size":1}}"#)
}

fn file(name: &str, size: u64) -> String {
    let name = json(name);
    format!(r#"{{"name":{name},"kind":"file","sha256":"{SHA_A}","size":{size}}}"#)
}

fn link(name: &str, target: &str) -> String {
    let (name, target) = (json(name), json(target));
    format!(r#"{{"name":{name},"kind":"symlink","target":{target}}}"#)
}

fn manifest(children: &[String]) -> String {
    format!(r#"{{"v":1,"children":[{}]}}"#, children.join(","))
}

fn parse(text: &str) -> Result<TreeManifest, TreeError> {
    parse_manifest(text.as_bytes(), &TreeDigest::of(text.as_bytes()))
}

/// The manifests a test describes a tree with, and how many times each was asked for.
#[derive(Default)]
struct Store {
    blobs: HashMap<TreeDigest, Vec<u8>>,
    asked: Cell<usize>,
}

impl Store {
    fn put(&mut self, text: String) -> TreeDigest {
        let digest = TreeDigest::of(text.as_bytes());
        self.blobs.insert(digest, text.into_bytes());
        digest
    }

    fn get(&self, digest: &TreeDigest) -> Result<Vec<u8>, String> {
        self.asked.set(self.asked.get() + 1);
        self.blobs
            .get(digest)
            .cloned()
            .ok_or_else(|| format!("no object {digest}"))
    }

    fn flatten(&self, root: &TreeDigest) -> Result<Vec<crate::TreeEntry>, TreeError> {
        flatten(root, |d| self.get(d))
    }

    fn count(&self, root: &TreeDigest) -> Result<u64, TreeError> {
        count_entries(root, |d| self.get(d))
    }

    /// A chain of `levels` manifests, each holding one directory `d` that is the next, the last
    /// holding the file `f` or nothing.
    fn chain(&mut self, levels: usize, bottom_file: bool) -> TreeDigest {
        let bottom = if bottom_file {
            manifest(&[file("f", 1)])
        } else {
            manifest(&[])
        };
        let mut top = self.put(bottom);
        for _ in 1..levels {
            top = self.put(manifest(&[dir("d", &top)]));
        }
        top
    }
}

fn names(entries: &[crate::TreeEntry]) -> Vec<(u32, Option<u32>, &str)> {
    entries
        .iter()
        .map(|e| (e.index, e.parent, e.name.as_ref()))
        .collect()
}

/// The limits are the numbers of the design.
#[test]
fn the_limits_are_the_designed_values() {
    assert_eq!(MAX_ENTRIES, 16_777_215);
    assert_eq!(MAX_MANIFEST_BYTES, 67_108_864);
    assert_eq!(MAX_DEPTH, 4096);
}

/// Each kind is read with its attributes: a directory's digest and size, a file's digest, size,
/// executable bit and source, a symlink's target.
#[test]
fn every_kind_is_read_with_its_attributes() {
    let child = TreeDigest::of(b"child");
    let text = format!(
        r#"{{"v":1,"children":[
          {{"name":"bin","kind":"dir","sha256":"{child}","size":812}},
          {{"name":"cc","kind":"file","exec":true,"sha256":"{SHA_A}","size":1048576,
            "source":{{"key":"cas/d/n7/b2/0000000000000041","offset":524288}}}},
          {{"name":"empty.h","kind":"file","sha256":"{EMPTY_SHA}","size":0}},
          {{"name":"lib","kind":"symlink","target":"bin"}}]}}"#
    );
    let parsed = parse(&text).unwrap();
    assert_eq!(parsed.size(), text.len() as u64);
    let seen: Vec<_> = parsed
        .children
        .iter()
        .map(|c| (c.name.as_ref(), c.kind.clone()))
        .collect();
    let sha_a = TreeDigest::from_hex(SHA_A).unwrap();
    assert_eq!(
        seen,
        vec![
            (
                "bin",
                EntryKind::Dir {
                    digest: child,
                    size: 812
                }
            ),
            (
                "cc",
                EntryKind::File {
                    digest: sha_a,
                    size: 1_048_576,
                    exec: true,
                    source: Some(BlobSource {
                        key: "cas/d/n7/b2/0000000000000041".to_string(),
                        offset: 524_288
                    }),
                }
            ),
            (
                "empty.h",
                EntryKind::File {
                    digest: TreeDigest::from_hex(EMPTY_SHA).unwrap(),
                    size: 0,
                    exec: false,
                    source: None,
                }
            ),
            (
                "lib",
                EntryKind::Symlink {
                    target: "bin".to_string()
                }
            ),
        ]
    );
}

/// `exec` is false when it is absent, false or null, and true when it is true.
#[test]
fn exec_defaults_to_false() {
    for (field, expected) in [
        ("", false),
        (r#","exec":false"#, false),
        (r#","exec":null"#, false),
        (r#","exec":true"#, true),
    ] {
        let text = format!(
            r#"{{"v":1,"children":[{{"name":"f","kind":"file","sha256":"{SHA_A}","size":3{field}}}]}}"#
        );
        let parsed = parse(&text).unwrap();
        let EntryKind::File { exec, .. } = parsed.children[0].kind else {
            panic!("not a file");
        };
        assert_eq!(exec, expected, "{field}");
    }
}

/// A field the reader does not know is ignored wherever it is: at the top, in each kind of
/// child, and in a source.
#[test]
fn unknown_fields_are_ignored() {
    let text = format!(
        r#"{{"v":1,"future":{{"a":[1,2]}},"children":[
          {{"name":"a","kind":"dir","sha256":"{SHA_A}","size":1,"acl":"x"}},
          {{"name":"b","kind":"file","sha256":"{SHA_A}","size":1,"mtime":5,
            "source":{{"key":"k","offset":0,"pack":7}}}},
          {{"name":"c","kind":"symlink","target":"a","flags":[]}}]}}"#
    );
    let parsed = parse(&text).unwrap();
    assert_eq!(parsed.children.len(), 3);
}

/// A kind the reader does not know is refused by name, even when its other fields have shapes
/// no known kind has.
#[test]
fn an_unknown_kind_is_refused_by_name() {
    for child in [
        r#"{"name":"a","kind":"device","major":1}"#,
        r#"{"name":"a","kind":"hardlink","size":"big","target":3}"#,
        r#"{"kind":"fifo"}"#,
    ] {
        let err = parse(&format!(r#"{{"v":1,"children":[{child}]}}"#)).unwrap_err();
        assert_eq!(err, TreeError::UnsupportedKind, "{child}");
        assert_eq!(err.to_string(), "unsupported entry kind");
    }
}

/// A version other than the number 1 is refused by name before the children are looked at,
/// whatever their shape.
#[test]
fn an_unknown_version_is_refused_by_name() {
    for text in [
        r#"{"v":2,"children":[{"name":"a","kind":"newkind"}]}"#,
        r#"{"v":2,"entries":"another shape"}"#,
        r#"{"v":0,"children":[]}"#,
        r#"{"v":"1","children":[]}"#,
        r#"{"v":1.5,"children":[]}"#,
        r#"{"v":null,"children":[]}"#,
    ] {
        let err = parse(text).unwrap_err();
        assert_eq!(err, TreeError::UnsupportedVersion, "{text}");
        assert_eq!(err.to_string(), "unsupported manifest version");
    }
}

/// Bytes that are not a manifest of any version are malformed: not JSON, not an object, no `v`,
/// no `children`, a child of the wrong type, a field of the wrong type, no kind, trailing bytes.
#[test]
fn bytes_that_are_not_a_manifest_are_malformed() {
    for text in [
        "",
        "not json",
        "[]",
        r#"{"children":[]}"#,
        r#"{"v":1}"#,
        r#"{"v":1,"children":{}}"#,
        r#"{"v":1,"children":[1]}"#,
        r#"{"v":1,"children":[{"name":"a"}]}"#,
        r#"{"v":1,"children":[{"name":"a","kind":"file","sha256":"aa","size":"3"}]}"#,
        r#"{"v":1,"children":[{"name":"a","kind":"file","sha256":"aa","size":-1}]}"#,
        r#"{"v":1,"children":[{"name":"a","kind":"symlink"}]}"#,
        r#"{"v":1,"children":[{"name":"a","kind":"symlink","name":"b","target":"t"}]}"#,
        r#"{"v":1,"children":[]} x"#,
        r#"{"v":1,"children":[]}{"v":1,"children":[]}"#,
    ] {
        let err = parse(text).unwrap_err();
        assert!(
            matches!(&err, TreeError::Malformed(m) if err.to_string() == format!("malformed manifest: {m}")),
            "{text}: {err:?}"
        );
    }
    let invalid_utf8 =
        b"{\"v\":1,\"children\":[{\"name\":\"\xff\",\"kind\":\"symlink\",\"target\":\"t\"}]}";
    let digest = TreeDigest::of(invalid_utf8);
    assert!(matches!(
        parse_manifest(invalid_utf8, &digest),
        Err(TreeError::Malformed(_))
    ));
}

/// A name that is empty, `.`, `..`, has a `/` or a NUL, or begins `.wh.` is refused by name in a
/// child of every kind.
#[test]
fn an_unsupported_name_is_refused_in_every_kind() {
    let sub = TreeDigest::of(b"x");
    for bad in [
        "",
        ".",
        "..",
        "a/b",
        "/",
        "a/",
        "a\u{0}b",
        "\u{0}",
        ".wh.",
        ".wh.x",
        ".wh..wh.y",
    ] {
        for child in [dir(bad, &sub), file(bad, 1), link(bad, "t")] {
            let err = parse(&manifest(&[child])).unwrap_err();
            assert_eq!(err, TreeError::UnsupportedName(bad.to_string()), "{bad:?}");
            assert!(err.to_string().starts_with("unsupported entry name"));
        }
    }
}

/// Names that look like the forbidden ones but are not are accepted.
#[test]
fn names_near_the_forbidden_ones_are_accepted() {
    let all: Vec<String> = ["...", ".w", ".wh", ".whx", "a b", "a\\b", "é", "~"]
        .iter()
        .map(|n| link(n, "t"))
        .collect();
    // They are not in order as listed; sort the manifest's way.
    let mut sorted = all.clone();
    sorted.sort_by_key(|c| {
        let value: serde_json::Value = serde_json::from_str(c).unwrap();
        value["name"].as_str().unwrap().to_string()
    });
    let parsed = parse(&manifest(&sorted)).unwrap();
    assert_eq!(parsed.children.len(), 8);
}

/// Children must be sorted bytewise: a name that sorts before the one ahead is refused, and so
/// is a repeat of it, each by its own name.
#[test]
fn unsorted_and_duplicate_names_are_refused() {
    let err = parse(&manifest(&[link("b", "t"), link("a", "t")])).unwrap_err();
    assert_eq!(
        err,
        TreeError::Unsorted {
            previous: "b".to_string(),
            name: "a".to_string()
        }
    );
    assert!(err.to_string().starts_with("entries not sorted"));
    let err = parse(&manifest(&[link("a", "t"), file("a", 1)])).unwrap_err();
    assert_eq!(err, TreeError::Duplicate("a".to_string()));
    assert!(err.to_string().starts_with("duplicate entry name"));
    // A duplicate that is not adjacent in the file is out of order, so it is refused as that.
    let err = parse(&manifest(&[link("a", "t"), link("b", "t"), link("a", "t")])).unwrap_err();
    assert!(matches!(err, TreeError::Unsorted { .. }));
}

/// Order is by bytes: uppercase before lowercase, a prefix before its extensions, and the UTF-8
/// bytes of a name decide, not its characters.
#[test]
fn order_is_bytewise() {
    let ok = manifest(&[
        link("B", "t"),
        link("a", "t"),
        link("a.b", "t"),
        link("ab", "t"),
        link("é", "t"),
    ]);
    assert_eq!(parse(&ok).unwrap().children.len(), 5);
    let err = parse(&manifest(&[link("a", "t"), link("B", "t")])).unwrap_err();
    assert!(matches!(err, TreeError::Unsorted { .. }));
    // U+FF5E (bytes EF BD 9E) sorts after U+00E9 (C3 A9) by bytes, and so by characters too, but
    // U+10000 (F0 ...) sorts after U+FF5E by bytes where UTF-16 would put it before.
    let ok = manifest(&[link("\u{ff5e}", "t"), link("\u{10000}", "t")]);
    assert_eq!(parse(&ok).unwrap().children.len(), 2);
}

/// A `sha256` that is not 64 lowercase hex digits is refused, in a directory and in a file.
#[test]
fn a_malformed_digest_is_refused() {
    for bad in [
        "",
        "aa",
        &"A".repeat(64),
        &"g".repeat(64),
        &"a".repeat(63),
        &"a".repeat(65),
        &format!("{}é", "a".repeat(62)),
    ] {
        for kind in ["dir", "file"] {
            let text = format!(
                r#"{{"v":1,"children":[{{"name":"x","kind":"{kind}","sha256":{bad:?},"size":1}}]}}"#
            );
            let err = parse(&text).unwrap_err();
            assert_eq!(err, TreeError::MalformedDigest("x".to_string()), "{bad:?}");
            assert!(err.to_string().starts_with("malformed digest"));
        }
    }
}

/// A file of size 0 has no source; a file of size 1 may have one.
#[test]
fn an_empty_file_has_no_source() {
    let with = |size: u64| {
        format!(
            r#"{{"v":1,"children":[{{"name":"f","kind":"file","sha256":"{SHA_A}","size":{size},"source":{{"key":"k","offset":0}}}}]}}"#
        )
    };
    let err = parse(&with(0)).unwrap_err();
    assert_eq!(err, TreeError::EmptyFileWithSource("f".to_string()));
    assert!(err.to_string().starts_with("file \"f\" has size 0"));
    assert!(parse(&with(1)).is_ok());
}

/// A source whose end does not fit in 64 bits is refused; one that ends at 2^64 - 1 is not.
#[test]
fn a_source_past_the_end_of_the_offsets_is_refused() {
    let with = |offset: u64, size: u64| {
        format!(
            r#"{{"v":1,"children":[{{"name":"f","kind":"file","sha256":"{SHA_A}","size":{size},"source":{{"key":"k","offset":{offset}}}}}]}}"#
        )
    };
    let err = parse(&with(u64::MAX, 1)).unwrap_err();
    assert_eq!(err, TreeError::SourceOverflow("f".to_string()));
    assert!(err.to_string().starts_with("source of file"));
    assert!(parse(&with(u64::MAX - 1, 1)).is_ok());
}

/// Bytes that do not hash to the digest they were asked for are refused, naming both.
#[test]
fn a_digest_mismatch_is_refused() {
    let text = manifest(&[]);
    let wrong = TreeDigest::of(b"other");
    let err = parse_manifest(text.as_bytes(), &wrong).unwrap_err();
    let right = TreeDigest::of(text.as_bytes());
    assert_eq!(
        err,
        TreeError::DigestMismatch {
            expected: wrong,
            actual: right
        }
    );
    assert_eq!(
        err.to_string(),
        format!("manifest digest mismatch: expected {wrong}, the bytes hash to {right}")
    );
}

/// A manifest of exactly 64 MiB is read; one byte more is refused by name, judged by its length
/// before it is hashed (so whatever digest it is given).
#[test]
fn the_manifest_size_limit_is_64_mib() {
    let head = manifest(&[]);
    let pad = |len: usize| {
        let mut bytes = head.clone().into_bytes();
        bytes.resize(len, b' ');
        bytes
    };
    let at = pad(MAX_MANIFEST_BYTES);
    let parsed = parse_manifest(&at, &TreeDigest::of(&at)).unwrap();
    assert_eq!(parsed.size(), 67_108_864);
    let over = pad(MAX_MANIFEST_BYTES + 1);
    let err = parse_manifest(&over, &TreeDigest::of(b"unrelated")).unwrap_err();
    assert_eq!(
        err,
        TreeError::ManifestTooLarge {
            size: MAX_MANIFEST_BYTES + 1
        }
    );
    assert_eq!(
        err.to_string(),
        "manifest too large: 67108865 bytes, at most 67108864"
    );
}

/// A digest reads from 64 lowercase hex digits, prints as them, and refuses all else.
#[test]
fn a_digest_reads_and_prints_as_hex() {
    let digest = TreeDigest::of(b"");
    assert_eq!(digest.to_string(), EMPTY_SHA);
    assert_eq!(TreeDigest::from_hex(EMPTY_SHA), Some(digest));
    assert_eq!(format!("{digest:?}"), format!("TreeDigest({EMPTY_SHA})"));
    assert_eq!(TreeDigest::from_hex(&EMPTY_SHA.to_uppercase()), None);
    assert_eq!(TreeDigest::from_hex(&EMPTY_SHA[1..]), None);
}

/// The root is 0; the rest are numbered depth first in manifest order, and each entry names the
/// index of its directory.
#[test]
fn entries_are_numbered_depth_first_in_manifest_order() {
    let mut store = Store::default();
    let inner = store.put(manifest(&[file("x", 1), link("y", "x")]));
    let lib = store.put(manifest(&[dir("inner", &inner), file("z", 2)]));
    let empty = store.put(manifest(&[]));
    let root = store.put(manifest(&[
        dir("a", &lib),
        file("b", 1),
        dir("c", &empty),
        link("d", "b"),
    ]));
    let entries = store.flatten(&root).unwrap();
    assert_eq!(
        names(&entries),
        vec![
            (0, None, ""),
            (1, Some(0), "a"),
            (2, Some(1), "inner"),
            (3, Some(2), "x"),
            (4, Some(2), "y"),
            (5, Some(1), "z"),
            (6, Some(0), "b"),
            (7, Some(0), "c"),
            (8, Some(0), "d"),
        ]
    );
    assert_eq!(
        entries[0].kind,
        EntryKind::Dir {
            digest: root,
            size: store.blobs[&root].len() as u64
        }
    );
    assert_eq!(
        entries[8].kind,
        EntryKind::Symlink {
            target: "b".to_string()
        }
    );
}

/// A root with nothing in it is the one entry 0.
#[test]
fn an_empty_tree_is_the_root_alone() {
    let mut store = Store::default();
    let root = store.put(manifest(&[]));
    assert_eq!(names(&store.flatten(&root).unwrap()), vec![(0, None, "")]);
    assert_eq!(store.count(&root), Ok(0));
}

/// The same manifests give the same entries, whatever order they were written or asked for in,
/// and a second flattening is equal to the first.
#[test]
fn indices_are_deterministic() {
    let build = |reverse: bool| {
        let mut store = Store::default();
        let mut leaves = vec![
            manifest(&[file("one", 1)]),
            manifest(&[file("two", 2), file("two2", 3)]),
        ];
        if reverse {
            leaves.reverse();
        }
        let digests: Vec<_> = leaves.into_iter().map(|t| store.put(t)).collect();
        let one = TreeDigest::of(manifest(&[file("one", 1)]).as_bytes());
        let two = *digests.iter().find(|d| **d != one).unwrap();
        let root = store.put(manifest(&[dir("p", &one), dir("q", &two), file("r", 1)]));
        (store, root)
    };
    let (first, root_first) = build(false);
    let (second, root_second) = build(true);
    assert_eq!(root_first, root_second);
    let a = first.flatten(&root_first).unwrap();
    assert_eq!(a, first.flatten(&root_first).unwrap());
    assert_eq!(a, second.flatten(&root_second).unwrap());
    assert_eq!(a.len(), 7);
}

/// A directory that occurs in several places is listed in each with fresh indices and its own
/// parent, and its manifest is asked for once.
#[test]
fn a_shared_subtree_is_listed_at_each_place_and_fetched_once() {
    let mut store = Store::default();
    let shared = store.put(manifest(&[file("f", 1), file("g", 1)]));
    let root = store.put(manifest(&[
        dir("a", &shared),
        dir("b", &shared),
        dir("c", &shared),
    ]));
    let entries = store.flatten(&root).unwrap();
    assert_eq!(
        names(&entries),
        vec![
            (0, None, ""),
            (1, Some(0), "a"),
            (2, Some(1), "f"),
            (3, Some(1), "g"),
            (4, Some(0), "b"),
            (5, Some(4), "f"),
            (6, Some(4), "g"),
            (7, Some(0), "c"),
            (8, Some(7), "f"),
            (9, Some(7), "g"),
        ]
    );
    assert_eq!(
        store.asked.get(),
        2,
        "the root and the shared manifest, once each"
    );
}

/// A function that cannot give a manifest, and one that gives the wrong bytes, are reported
/// with the digest asked for, for the root and for a child.
#[test]
fn fetch_failures_and_wrong_bytes_are_reported() {
    let mut store = Store::default();
    let missing = TreeDigest::of(b"missing");
    let err = store.flatten(&missing).unwrap_err();
    assert_eq!(
        err,
        TreeError::Fetch {
            digest: missing,
            reason: format!("no object {missing}")
        }
    );
    assert_eq!(
        err.to_string(),
        format!("cannot fetch manifest {missing}: no object {missing}")
    );
    let root = store.put(manifest(&[dir("a", &missing)]));
    assert!(matches!(
        store.flatten(&root),
        Err(TreeError::Fetch { digest, .. }) if digest == missing
    ));
    // Bytes under the wrong digest.
    let other = store.put(manifest(&[file("x", 1)]));
    let wrong_bytes = store.blobs[&other].clone();
    store.blobs.insert(missing, wrong_bytes);
    assert!(matches!(
        store.flatten(&root),
        Err(TreeError::DigestMismatch { expected, .. }) if expected == missing
    ));
    // A child that is itself refused is refused for what it is.
    let bad = store.put(r#"{"v":2,"children":[]}"#.to_string());
    let root = store.put(manifest(&[dir("a", &bad)]));
    assert_eq!(store.flatten(&root), Err(TreeError::UnsupportedVersion));
}

/// One manifest of 4096 files.
fn full_leaf(store: &mut Store) -> TreeDigest {
    let names: Vec<String> = (0..4096).map(|i| format!("{i:05}")).collect();
    store.put(manifest(
        &names.iter().map(|n| file(n, 1)).collect::<Vec<_>>(),
    ))
}

/// 4095 directories that are all the one leaf of 4096 files: 4095 + 4095 * 4096 = 2^24 - 1
/// entries below the root, described by two distinct manifests.
fn full_tree(store: &mut Store) -> TreeDigest {
    let leaf = full_leaf(store);
    let dirs: Vec<String> = (0..4095).map(|i| dir(&format!("{i:05}"), &leaf)).collect();
    store.put(manifest(&dirs))
}

/// A tree of exactly 2^24 - 1 entries below its root is accepted. The walk counts each distinct
/// manifest once, so the count is reached without making the entries (`flatten` of this tree
/// would make 16 million; no test does, and the entries of smaller trees are tested by name).
#[test]
fn a_tree_of_2_to_the_24_minus_1_entries_is_accepted() {
    let mut store = Store::default();
    let root = full_tree(&mut store);
    assert_eq!(store.count(&root), Ok(MAX_ENTRIES));
    assert_eq!(store.asked.get(), 2, "two distinct manifests describe it");
}

/// One entry more is refused by name, however the extra entry is reached: beside the directories
/// (found as the shared leaf is met again), or as a whole tree of the full size below one more
/// directory (found as that directory is finished). `flatten` refuses before it makes an entry.
#[test]
fn a_tree_of_2_to_the_24_entries_is_refused() {
    let mut store = Store::default();
    let full = full_tree(&mut store);
    let leaf = full_leaf(&mut store);
    let mut children: Vec<String> = (0..4095).map(|i| dir(&format!("{i:05}"), &leaf)).collect();
    children.push(file("zzzzz", 1));
    let shared_met_again = store.put(manifest(&children));
    assert_eq!(store.count(&shared_met_again), Err(TreeError::TreeTooLarge));
    assert_eq!(
        store.flatten(&shared_met_again),
        Err(TreeError::TreeTooLarge)
    );

    let one_more = store.put(manifest(&[dir("a", &full)]));
    assert_eq!(store.count(&one_more), Err(TreeError::TreeTooLarge));
    assert_eq!(store.flatten(&one_more), Err(TreeError::TreeTooLarge));
    assert_eq!(
        TreeError::TreeTooLarge.to_string(),
        "tree too large: more than 16777215 entries"
    );
}

/// A chain of 4096 nested names is accepted, with the file at its bottom at depth 4096.
#[test]
fn depth_4096_is_accepted() {
    let mut store = Store::default();
    let root = store.chain(4096, true);
    let entries = store.flatten(&root).unwrap();
    assert_eq!(entries.len(), 4097);
    assert_eq!(entries[4096].name.as_ref(), "f");
    assert_eq!(entries[4096].parent, Some(4095));
}

/// A chain one deeper is refused by name.
#[test]
fn depth_4097_is_refused() {
    let mut store = Store::default();
    let root = store.chain(4097, true);
    assert_eq!(store.flatten(&root), Err(TreeError::TreeTooDeep));
    assert_eq!(
        TreeError::TreeTooDeep.to_string(),
        "tree too deep: more than 4096 levels"
    );
}

/// An empty directory at depth 4096 is accepted: nothing below it is deeper.
#[test]
fn an_empty_directory_at_depth_4096_is_accepted() {
    let mut store = Store::default();
    let root = store.chain(4097, false);
    let entries = store.flatten(&root).unwrap();
    assert_eq!(entries.len(), 4097);
    assert_eq!(entries[4096].name.as_ref(), "d");
}

/// The walk stops at the first level over the limit and does not ask for the rest of a chain
/// that is far deeper.
#[test]
fn a_deep_chain_is_refused_without_fetching_all_of_it() {
    let mut store = Store::default();
    let root = store.chain(6000, true);
    assert_eq!(store.flatten(&root), Err(TreeError::TreeTooDeep));
    assert_eq!(store.asked.get(), 4097);
}

/// A subtree already measured at a shallow place and met again deeper is refused when the two
/// together are over the depth: the first place is fine on its own.
#[test]
fn a_shared_subtree_met_deeper_is_refused_for_the_depth_it_has_there() {
    let mut store = Store::default();
    let tall = store.chain(4095, true);
    let beside = store.put(manifest(&[dir("x", &tall)]));
    let fine = store.put(manifest(&[dir("a", &tall)]));
    assert!(store.flatten(&fine).is_ok());
    let root = store.put(manifest(&[dir("a", &tall), dir("b", &beside)]));
    assert_eq!(store.flatten(&root), Err(TreeError::TreeTooDeep));
}
