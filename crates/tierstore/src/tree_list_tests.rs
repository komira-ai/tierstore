//! The tree list: every refusal by name and line, the limits at their exact values, the bytes a
//! writer stores and the entries a tree is built from.

use crate::tree_list::{
    LineFault, ListEntry, ListKind, MAX_NAME_BYTES, MAX_PATH_BYTES, MAX_TARGET_BYTES,
    MAX_TREE_LIST_BYTES, TREE_LIST_PREFIX, TreeList, TreeListError, parse_tree_list,
    tree_list_digest, tree_list_key,
};
use crate::{BlobSource, Mode, NewEntry, NewKind, TreeDigest, TreeLimits, build_tree};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const HEADER: &str = r#"{"format":"tierstore.tree-list","version":1}"#;

/// The bytes of a list of `lines` after the header, each ended by a newline.
fn list(lines: &[&str]) -> Vec<u8> {
    let mut text = format!("{HEADER}\n");
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    text.into_bytes()
}

fn parse(bytes: &[u8]) -> Result<TreeList, TreeListError> {
    parse_tree_list(bytes, &TreeDigest::of(bytes))
}

/// The fault of line `line` that reading `bytes` gives.
fn fault(bytes: &[u8], line: usize) -> LineFault {
    match parse(bytes) {
        Err(TreeListError::Line { line: at, fault }) if at == line => fault,
        other => panic!("expected a fault of line {line}, got {other:?}"),
    }
}

fn digest(text: &str) -> TreeDigest {
    TreeDigest::from_hex(text).unwrap()
}

fn mode(bits: u16) -> Mode {
    Mode::new(bits).unwrap()
}

fn file(path: &str, size: u64, source: Option<(&str, u64)>, sha256: Option<&str>) -> ListEntry {
    ListEntry {
        path: path.into(),
        kind: ListKind::File {
            mode: mode(0o644),
            size,
            source: source.map(|(key, offset)| BlobSource {
                key: key.into(),
                offset,
            }),
            sha256: sha256.map(digest),
        },
    }
}

/// The prefix and the bounds are the designed values.
#[test]
fn the_prefix_and_the_bounds_are_the_designed_values() {
    assert_eq!(TREE_LIST_PREFIX, "cas/l/");
    assert_eq!(MAX_TREE_LIST_BYTES, 268_435_456);
    assert_eq!(MAX_PATH_BYTES, 4096);
    assert_eq!(MAX_NAME_BYTES, 255);
    assert_eq!(MAX_TARGET_BYTES, 4096);
}

/// Each kind is read with its members: a file with a source and a digest, a file of size 0 with
/// neither, a directory with its mode and a link with its target.
#[test]
fn every_kind_is_read_with_its_members() {
    let bytes = list(&[
        &format!(
            r#"{{"path":"bin/cc","kind":"file","mode":493,"size":10,"key":"out/a1/0","offset":4096,"sha256":"{SHA_A}"}}"#
        ),
        r#"{"path":"e","kind":"file","mode":420,"size":0}"#,
        r#"{"path":"lib","kind":"symlink","target":"bin"}"#,
        r#"{"path":"tmp","kind":"dir","mode":1023}"#,
    ]);
    let read = parse(&bytes).unwrap();
    let cc = ListEntry {
        path: "bin/cc".into(),
        kind: ListKind::File {
            mode: mode(0o755),
            size: 10,
            source: Some(BlobSource {
                key: "out/a1/0".into(),
                offset: 4096,
            }),
            sha256: Some(digest(SHA_A)),
        },
    };
    assert_eq!(
        read.entries,
        vec![
            cc,
            file("e", 0, None, None),
            ListEntry {
                path: "lib".into(),
                kind: ListKind::Symlink {
                    target: "bin".into()
                },
            },
            ListEntry {
                path: "tmp".into(),
                kind: ListKind::Dir { mode: mode(0o1777) },
            },
        ]
    );
}

/// A list of the header alone is an empty tree, and members the reader does not know are ignored
/// in the header and in every kind, so a later writer may add them.
#[test]
fn a_header_alone_is_empty_and_unknown_members_are_ignored() {
    assert_eq!(parse(&list(&[])).unwrap(), TreeList::default());
    let bytes = format!(
        concat!(
            r#"{{"format":"tierstore.tree-list","version":1,"writer":"kbf"}}"#,
            "\n",
            r#"{{"path":"a","kind":"file","mode":420,"size":1,"key":"k","offset":0,"sha256":"{}","xattr":{{}}}}"#,
            "\n",
            r#"{{"path":"d","kind":"dir","mode":493,"owner":"x"}}"#,
            "\n",
            r#"{{"path":"l","kind":"symlink","target":"a","mode":511}}"#,
            "\n"
        ),
        SHA_A
    );
    let read = parse(bytes.as_bytes()).unwrap();
    assert_eq!(read.entries.len(), 3);
    assert_eq!(read.entries[0], file("a", 1, Some(("k", 0)), Some(SHA_A)));
}

/// The bytes are judged by size before they are hashed: one byte over the bound is refused, and a
/// list of exactly the bound is read.
#[test]
fn the_size_bound_is_exact() {
    let over = vec![b' '; MAX_TREE_LIST_BYTES + 1];
    assert_eq!(
        parse_tree_list(&over, &TreeDigest::of(b"anything")),
        Err(TreeListError::TooLarge {
            size: MAX_TREE_LIST_BYTES + 1
        })
    );
    let mut at = HEADER.as_bytes().to_vec();
    at.resize(MAX_TREE_LIST_BYTES - 1, b' ');
    at.push(b'\n');
    assert_eq!(parse(&at), Ok(TreeList::default()));
    assert_eq!(
        parse_tree_list(&over, &TreeDigest::of(b"x"))
            .unwrap_err()
            .to_string(),
        "tree list too large: 268435457 bytes, at most 268435456"
    );
}

/// Bytes that do not hash to the digest their key names are refused, naming both digests.
#[test]
fn bytes_under_another_digest_are_refused() {
    let bytes = list(&[]);
    let other = digest(SHA_A);
    let refused = parse_tree_list(&bytes, &other).unwrap_err();
    assert_eq!(
        refused,
        TreeListError::DigestMismatch {
            expected: other,
            actual: TreeDigest::of(&bytes)
        }
    );
    assert_eq!(
        refused.to_string(),
        format!(
            "tree list digest mismatch: expected {SHA_A}, the bytes hash to {}",
            TreeDigest::of(&bytes)
        )
    );
}

/// A list that does not end with a newline is refused at its last line, an empty object at line 1.
#[test]
fn the_last_line_needs_its_newline() {
    assert_eq!(fault(b"", 1), LineFault::NoEnd);
    assert_eq!(fault(HEADER.as_bytes(), 1), LineFault::NoEnd);
    let mut cut = list(&[r#"{"path":"d","kind":"dir","mode":493}"#]);
    cut.pop();
    assert_eq!(fault(&cut, 2), LineFault::NoEnd);
    assert_eq!(
        parse(&cut).unwrap_err().to_string(),
        "tree list line 2: no newline at the end"
    );
}

/// The header must name the format and version 1; another format, another version, a version that
/// is not a number, either member absent, a per-directory manifest and a line that is not JSON are
/// each refused by name at line 1.
#[test]
fn the_header_is_refused_by_name() {
    let header = |text: &str| fault(format!("{text}\n").as_bytes(), 1);
    assert_eq!(
        header(r#"{"format":"tierstore.tree","version":1}"#),
        LineFault::UnknownFormat("tierstore.tree".into())
    );
    for version in ["2", "0", r#""1""#] {
        let text = format!(r#"{{"format":"tierstore.tree-list","version":{version}}}"#);
        assert_eq!(header(&text), LineFault::UnsupportedVersion, "{version}");
    }
    assert_eq!(
        header(r#"{"version":1}"#),
        LineFault::Missing {
            what: "header",
            member: "format"
        }
    );
    assert_eq!(
        header(r#"{"format":"tierstore.tree-list"}"#),
        LineFault::Missing {
            what: "header",
            member: "version"
        }
    );
    assert_eq!(
        header(r#"{"v":1,"children":[]}"#),
        LineFault::Missing {
            what: "header",
            member: "format"
        }
    );
    assert!(matches!(header("not json"), LineFault::Malformed(_)));
    assert!(matches!(header(""), LineFault::Malformed(_)));
    let refused = parse(b"{\"format\":\"x\",\"version\":1}\n").unwrap_err();
    assert_eq!(
        refused.to_string(),
        r#"tree list line 1: unknown format "x""#
    );
}

/// An entry that is not JSON, a blank line, a kind this reader does not know, and an entry without
/// a member its kind needs are refused naming the line.
#[test]
fn entries_of_the_wrong_shape_are_refused_naming_the_line() {
    let at2 = |line: &str| fault(&list(&[line]), 2);
    assert!(matches!(at2("[]"), LineFault::Malformed(_)));
    assert!(matches!(at2(""), LineFault::Malformed(_)));
    assert!(matches!(at2(r#"{"path":"a"}"#), LineFault::Malformed(_)));
    assert!(matches!(
        at2(r#"{"path":"a","kind":"file","mode":"420","size":0}"#),
        LineFault::Malformed(_)
    ));
    assert_eq!(
        at2(r#"{"path":"a","kind":"fifo"}"#),
        LineFault::UnknownKind("fifo".into())
    );
    let missing = |what, member| LineFault::Missing { what, member };
    assert_eq!(
        at2(r#"{"path":"a","kind":"file","size":0}"#),
        missing("file", "mode")
    );
    assert_eq!(
        at2(r#"{"path":"a","kind":"file","mode":420}"#),
        missing("file", "size")
    );
    assert_eq!(at2(r#"{"path":"a","kind":"dir"}"#), missing("dir", "mode"));
    assert_eq!(
        at2(r#"{"path":"a","kind":"symlink"}"#),
        missing("symlink", "target")
    );
    let refused = parse(&list(&[r#"{"path":"a","kind":"dir"}"#])).unwrap_err();
    assert_eq!(refused.to_string(), "tree list line 2: dir without mode");
}

/// A path is refused when it is empty, over 4096 bytes, has a name over 255 bytes, or has an empty,
/// `.`, `..`, NUL-holding or `.wh.` name; the longest path and name are read.
#[test]
fn paths_are_judged_by_their_names_and_lengths() {
    let dir = |path: &str| {
        let path = serde_json::to_string(path).unwrap();
        format!(r#"{{"path":{path},"kind":"dir","mode":493}}"#)
    };
    let names = format!(
        "{}/{}",
        "n".repeat(MAX_NAME_BYTES),
        vec!["m".repeat(250); 15].join("/")
    );
    let longest = format!("{names}/{}", "x".repeat(MAX_PATH_BYTES - names.len() - 1));
    assert_eq!(longest.len(), MAX_PATH_BYTES);
    assert!(parse(&list(&[&dir(&longest)])).is_ok());
    let too_long = format!("{longest}m");
    let long_name = "n".repeat(MAX_NAME_BYTES + 1);
    for path in [
        "",
        too_long.as_str(),
        long_name.as_str(),
        "a//b",
        "/a",
        "a/",
        "a/./b",
        "../a",
        "a\0b",
        "x/.wh.y",
    ] {
        assert_eq!(
            fault(&list(&[&dir(path)]), 2),
            LineFault::BadPath(path.into()),
            "{path:?}"
        );
    }
}

/// A target is refused when it is empty, over 4096 bytes or holds a NUL; the longest is read.
#[test]
fn targets_are_judged_by_length_and_nul() {
    let link = |target: &str| {
        let target = serde_json::to_string(target).unwrap();
        format!(r#"{{"path":"l","kind":"symlink","target":{target}}}"#)
    };
    let longest = "t".repeat(MAX_TARGET_BYTES);
    assert!(parse(&list(&[&link(&longest)])).is_ok());
    let too_long = format!("{longest}t");
    for target in ["", too_long.as_str(), "a\0b"] {
        assert_eq!(
            fault(&list(&[&link(target)]), 2),
            LineFault::BadTarget("l".into())
        );
    }
    let refused = parse(&list(&[&link("")])).unwrap_err();
    assert_eq!(
        refused.to_string(),
        r#"tree list line 2: bad target of "l""#
    );
}

/// A mode of 07777 is read; a bit above it, and a number past 16 bits, are refused with the mode as
/// written, in a file and in a directory.
#[test]
fn a_mode_above_07777_is_refused() {
    let read = parse(&list(&[r#"{"path":"d","kind":"dir","mode":4095}"#])).unwrap();
    assert_eq!(read.entries[0].kind, ListKind::Dir { mode: mode(0o7777) });
    assert_eq!(
        fault(&list(&[r#"{"path":"d","kind":"dir","mode":4096}"#]), 2),
        LineFault::BadMode(4096)
    );
    assert_eq!(
        fault(
            &list(&[r#"{"path":"f","kind":"file","mode":65536,"size":0}"#]),
            2
        ),
        LineFault::BadMode(65536)
    );
}

/// A digest that is not 64 lowercase hex digits is refused.
#[test]
fn a_malformed_digest_is_refused() {
    let upper = SHA_A.to_uppercase();
    for sha in [upper.as_str(), "aa", ""] {
        let line = format!(r#"{{"path":"f","kind":"file","mode":420,"size":0,"sha256":"{sha}"}}"#);
        assert_eq!(
            fault(&list(&[&line]), 2),
            LineFault::MalformedDigest,
            "{sha}"
        );
    }
}

/// A file of size 0 has neither key nor offset, and a larger one has both: every other mix is
/// refused.
#[test]
fn keys_offsets_and_sizes_must_agree() {
    for line in [
        r#"{"path":"f","kind":"file","mode":420,"size":0,"key":"k"}"#,
        r#"{"path":"f","kind":"file","mode":420,"size":0,"offset":0}"#,
        r#"{"path":"f","kind":"file","mode":420,"size":0,"key":"k","offset":0}"#,
        r#"{"path":"f","kind":"file","mode":420,"size":1}"#,
        r#"{"path":"f","kind":"file","mode":420,"size":1,"key":"k"}"#,
        r#"{"path":"f","kind":"file","mode":420,"size":1,"offset":0}"#,
    ] {
        assert_eq!(fault(&list(&[line]), 2), LineFault::KeyAndOffset, "{line}");
    }
}

/// An offset plus a size that just fits in 64 bits is read; one more byte is refused.
#[test]
fn a_range_past_2_to_the_64_is_refused() {
    let line = |size: u64| {
        format!(
            r#"{{"path":"f","kind":"file","mode":420,"size":{size},"key":"k","offset":{}}}"#,
            u64::MAX - 1
        )
    };
    assert!(parse(&list(&[&line(1)])).is_ok());
    assert_eq!(fault(&list(&[&line(2)]), 2), LineFault::RangeOverflow);
}

/// The entries must be sorted by path bytewise with no path twice; the line that breaks the order
/// is named with both paths.
#[test]
fn the_order_is_strict() {
    let dir = |path: &str| format!(r#"{{"path":"{path}","kind":"dir","mode":493}}"#);
    let (a, b, upper) = (dir("a"), dir("b"), dir("B"));
    assert!(parse(&list(&[&upper, &a, &b])).is_ok());
    assert_eq!(
        fault(&list(&[&a, &b, &b]), 4),
        LineFault::Duplicate("b".into())
    );
    assert_eq!(
        fault(&list(&[&a, &b, &upper]), 4),
        LineFault::Unsorted {
            previous: "b".into(),
            path: "B".into()
        }
    );
    let refused = parse(&list(&[&b, &a])).unwrap_err();
    assert_eq!(
        refused.to_string(),
        r#"tree list line 3: not sorted: "a" follows "b""#
    );
}

/// The entries of a tree: each kind maps to the entry `build_tree` takes, a file of size 0 with no
/// digest gets the digest of no bytes, and the entries build.
#[test]
fn the_entries_build_a_tree() {
    let listed = TreeList {
        entries: vec![
            file("bin/cc", 10, Some(("out/a1/0", 7)), Some(SHA_A)),
            file("e", 0, None, None),
            ListEntry {
                path: "lib".into(),
                kind: ListKind::Symlink {
                    target: "bin".into(),
                },
            },
            ListEntry {
                path: "tmp".into(),
                kind: ListKind::Dir { mode: mode(0o1777) },
            },
        ],
    };
    let entries = listed.into_entries().unwrap();
    assert_eq!(
        entries,
        vec![
            NewEntry {
                path: "bin/cc".into(),
                kind: NewKind::File {
                    mode: Some(mode(0o644)),
                    sha256: digest(SHA_A),
                    size: 10,
                    from: Some(BlobSource {
                        key: "out/a1/0".into(),
                        offset: 7
                    }),
                },
            },
            NewEntry {
                path: "e".into(),
                kind: NewKind::File {
                    mode: Some(mode(0o644)),
                    sha256: digest(EMPTY_SHA),
                    size: 0,
                    from: None,
                },
            },
            NewEntry {
                path: "lib".into(),
                kind: NewKind::Symlink {
                    target: "bin".into()
                },
            },
            NewEntry {
                path: "tmp".into(),
                kind: NewKind::Dir {
                    mode: Some(mode(0o1777))
                },
            },
        ]
    );
    let built = build_tree(None, entries, &TreeLimits::default()).unwrap();
    assert_eq!(built.files.len(), 2);
}

/// A file with bytes and no digest cannot be a file of a node-local tree: it is refused naming the
/// path.
#[test]
fn a_file_with_bytes_and_no_digest_is_refused_for_a_tree() {
    let listed = TreeList {
        entries: vec![file("src/a.c", 5, Some(("k", 0)), None)],
    };
    let refused = listed.into_entries().unwrap_err();
    assert_eq!(refused, TreeListError::MissingSha256("src/a.c".into()));
    assert_eq!(refused.to_string(), r#"file "src/a.c" has no sha256"#);
}

/// The encoding writes the header, then the entries sorted by path, each with its members in the
/// order of the reference and what is not set left out, escaping what JSON needs; it reads back to
/// the same entries, and the same entries in another order are the same bytes.
#[test]
fn the_encoding_is_sorted_compact_and_reads_back() {
    let listed = TreeList {
        entries: vec![
            ListEntry {
                path: "m".into(),
                kind: ListKind::Symlink {
                    target: "\n\"\\".into(),
                },
            },
            file("b", 3, Some(("k\"\\", 1)), None),
            ListEntry {
                path: "d".into(),
                kind: ListKind::Dir { mode: mode(0o700) },
            },
            file("a\u{1}é", 3, Some(("k", 9)), Some(SHA_A)),
            file("e", 0, None, None),
        ],
    };
    let bytes = listed.encode();
    let expected = format!(
        concat!(
            r#"{{"format":"tierstore.tree-list","version":1}}"#,
            "\n",
            r#"{{"path":"a\u0001é","kind":"file","mode":420,"size":3,"key":"k","offset":9,"sha256":"{sha}"}}"#,
            "\n",
            r#"{{"path":"b","kind":"file","mode":420,"size":3,"key":"k\"\\","offset":1}}"#,
            "\n",
            r#"{{"path":"d","kind":"dir","mode":448}}"#,
            "\n",
            r#"{{"path":"e","kind":"file","mode":420,"size":0}}"#,
            "\n",
            r#"{{"path":"m","kind":"symlink","target":"\u000a\"\\"}}"#,
            "\n"
        ),
        sha = SHA_A
    );
    assert_eq!(String::from_utf8(bytes.clone()).unwrap(), expected);
    let back = parse(&bytes).unwrap();
    assert_eq!(back.encode(), bytes);
    let mut reversed = listed.clone();
    reversed.entries.reverse();
    assert_eq!(reversed.encode(), bytes);
    assert_eq!(
        TreeList::default().encode(),
        format!("{HEADER}\n").into_bytes()
    );
}

/// The key of a list is the prefix and the digest of its bytes, and only a key of exactly that form
/// names a digest.
#[test]
fn the_key_names_the_digest_of_the_bytes() {
    let bytes = TreeList::default().encode();
    let key = tree_list_key(&bytes);
    assert_eq!(key, format!("cas/l/{}", TreeDigest::of(&bytes)));
    assert_eq!(tree_list_digest(&key), Some(TreeDigest::of(&bytes)));
    let upper = format!("cas/l/{}", SHA_A.to_uppercase());
    let others = [
        format!("cas/t/{SHA_A}"),
        format!("cas/l/{SHA_A}/"),
        format!("x/cas/l/{SHA_A}"),
        upper,
        "cas/l/aa".to_owned(),
        "cas/l/".to_owned(),
        String::new(),
    ];
    for other in &others {
        assert_eq!(tree_list_digest(other), None, "{other}");
    }
}
