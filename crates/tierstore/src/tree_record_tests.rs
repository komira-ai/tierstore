//! The tree record and the builder: the canonical bytes, every refusal of the parser by name, and
//! what the tree digest does and does not depend on.

use crate::tree_record::MAX_DIR_ENTRIES;
use crate::{
    BlobSource, BuiltFile, MAX_DEPTH, MAX_ENTRIES, Mode, NewEntry, NewKind, RecordChild,
    RecordError, TreeDigest, TreeLimits, TreeRecord, build_tree,
};

fn sha(text: &str) -> TreeDigest {
    TreeDigest::of(text.as_bytes())
}

fn source(key: &str, offset: u64) -> Option<BlobSource> {
    Some(BlobSource {
        key: key.into(),
        offset,
    })
}

fn file(path: &str, content: &str) -> NewEntry {
    NewEntry {
        path: path.into(),
        kind: NewKind::File {
            mode: None,
            sha256: sha(content),
            size: content.len() as u64,
            from: None,
        },
    }
}

fn file_with(path: &str, content: &str, mode: Option<Mode>, from: Option<BlobSource>) -> NewEntry {
    NewEntry {
        path: path.into(),
        kind: NewKind::File {
            mode,
            sha256: sha(content),
            size: content.len() as u64,
            from,
        },
    }
}

fn dir(path: &str, mode: Option<Mode>) -> NewEntry {
    NewEntry {
        path: path.into(),
        kind: NewKind::Dir { mode },
    }
}

fn link(path: &str, target: &str) -> NewEntry {
    NewEntry {
        path: path.into(),
        kind: NewKind::Symlink {
            target: target.into(),
        },
    }
}

fn subtree(path: &str, digest: TreeDigest) -> NewEntry {
    NewEntry {
        path: path.into(),
        kind: NewKind::Tree { digest },
    }
}

fn mode(text: &str) -> Option<Mode> {
    Some(Mode::parse_octal(text).unwrap())
}

fn root(entries: Vec<NewEntry>) -> TreeDigest {
    build_tree(None, entries, &TreeLimits::default())
        .unwrap()
        .root
}

fn refused(entries: Vec<NewEntry>) -> RecordError {
    build_tree(None, entries, &TreeLimits::default()).unwrap_err()
}

fn refused_under(limits: TreeLimits, entries: Vec<NewEntry>) -> RecordError {
    build_tree(None, entries, &limits).unwrap_err()
}

fn file_child(name: &str, mode: u16, size: u64) -> RecordChild {
    RecordChild::File {
        name: name.into(),
        mode: Mode::new(mode).unwrap(),
        size,
        sha256: sha(name),
    }
}

/// A record by hand: a header, then the given bytes as children.
fn record_bytes(mode: u16, count: &[u8], children: &[u8]) -> Vec<u8> {
    let mut out = b"TSTR\x01".to_vec();
    out.extend_from_slice(&mode.to_be_bytes());
    out.extend_from_slice(count);
    out.extend_from_slice(children);
    out
}

fn name_bytes(name: &[u8]) -> Vec<u8> {
    let mut out = vec![name.len() as u8];
    out.extend_from_slice(name);
    out
}

/// A symlink child by hand.
fn link_bytes(name: &[u8], target: &[u8]) -> Vec<u8> {
    let mut out = vec![3];
    out.extend(name_bytes(name));
    out.extend(name_bytes(target));
    out
}

// ---- the digest of a tree

/// The same entries with other `from` locations, in another order, and the same tree sent flat
/// and as a child tree by digest give one digest.
#[test]
fn tree_digest_ignores_source_and_grouping() {
    let flat = vec![
        file_with("bin/cc", "compiler", mode("0755"), source("cas/d/a", 0)),
        file("bin/ld", "linker"),
        file("include/empty.h", ""),
        link("lib", "bin"),
    ];
    let expected = root(flat.clone());

    let mut reversed = flat.clone();
    reversed.reverse();
    assert_eq!(root(reversed), expected);

    let elsewhere = vec![
        file_with(
            "bin/cc",
            "compiler",
            mode("0755"),
            source("cas/d/other", 4096),
        ),
        file_with("bin/ld", "linker", None, source("cas/d/other", 9)),
        file("include/empty.h", ""),
        link("lib", "bin"),
    ];
    assert_eq!(root(elsewhere), expected);

    let bin = build_tree(
        None,
        vec![
            file_with("cc", "compiler", mode("0755"), None),
            file("ld", "linker"),
        ],
        &TreeLimits::default(),
    )
    .unwrap()
    .root;
    let grouped = vec![
        subtree("bin", bin),
        file("include/empty.h", ""),
        link("lib", "bin"),
    ];
    assert_eq!(root(grouped), expected);
}

/// The digest covers every name, kind, mode, target, content digest and size.
#[test]
fn tree_digest_covers_names_kinds_modes_targets_digests_and_sizes() {
    let base = root(vec![file("a", "x"), link("l", "t"), dir("d", None)]);
    let changed = [
        vec![file("b", "x"), link("l", "t"), dir("d", None)],
        vec![file("a", "y"), link("l", "t"), dir("d", None)],
        vec![
            file_with("a", "x", mode("0600"), None),
            link("l", "t"),
            dir("d", None),
        ],
        vec![file("a", "x"), link("l", "u"), dir("d", None)],
        vec![file("a", "x"), link("m", "t"), dir("d", None)],
        vec![file("a", "x"), link("l", "t"), dir("d", mode("0700"))],
        vec![file("a", "x"), link("l", "t"), dir("e", None)],
        vec![file("a", "x"), link("l", "t"), file("d", "")],
        vec![file("a", "x"), link("l", "t")],
    ];
    for entries in changed {
        assert_ne!(root(entries.clone()), base, "{entries:?}");
    }
    let wrong_size = NewEntry {
        path: "a".into(),
        kind: NewKind::File {
            mode: None,
            sha256: sha("x"),
            size: 2,
            from: None,
        },
    };
    assert_ne!(root(vec![wrong_size, link("l", "t"), dir("d", None)]), base);
    let with_root_mode = build_tree(
        Some(Mode::new(0o700).unwrap()),
        vec![file("a", "x"), link("l", "t"), dir("d", None)],
        &TreeLimits::default(),
    )
    .unwrap();
    assert_ne!(with_root_mode.root, base);
}

/// A mode left out is the default, so writing the default does not change the digest; and a
/// directory the paths imply is 0755, as is the root.
#[test]
fn mode_defaults_apply_before_hashing() {
    let plain = root(vec![file("d/f", "x")]);
    let written = build_tree(
        Some(Mode::DIR),
        vec![
            dir("d", mode("0755")),
            file_with("d/f", "x", mode("0644"), None),
        ],
        &TreeLimits::default(),
    )
    .unwrap();
    assert_eq!(written.root, plain);

    let built = build_tree(None, vec![file("d/f", "x")], &TreeLimits::default()).unwrap();
    let (_, root_record) = built.records.last().unwrap();
    assert_eq!(root_record.mode(), Mode::new(0o755).unwrap());
    let (_, child_record) = &built.records[0];
    assert_eq!(child_record.mode(), Mode::new(0o755).unwrap());
    assert_eq!(
        child_record.children(),
        &[RecordChild::File {
            name: "f".into(),
            mode: Mode::new(0o644).unwrap(),
            size: 1,
            sha256: sha("x"),
        }]
    );
}

/// An explicit directory entry gives the mode of a directory the paths below it make, whichever
/// comes first, and a directory entry alone is an empty directory.
#[test]
fn an_explicit_directory_sets_the_mode_before_or_after_its_children() {
    let before = root(vec![dir("d", mode("1777")), file("d/f", "x")]);
    let after = root(vec![file("d/f", "x"), dir("d", mode("1777"))]);
    assert_eq!(before, after);
    assert_ne!(before, root(vec![file("d/f", "x")]));

    let built = build_tree(
        None,
        vec![dir("empty", mode("0700"))],
        &TreeLimits::default(),
    )
    .unwrap();
    assert_eq!(built.records.len(), 2);
    assert_eq!(built.records[0].1.children(), &[]);
    assert_eq!(built.records[0].1.mode().bits(), 0o700);
}

/// No entries is an empty root.
#[test]
fn no_entries_is_an_empty_root() {
    let built = build_tree(None, vec![], &TreeLimits::default()).unwrap();
    assert_eq!(built.records.len(), 1);
    assert_eq!(built.records[0].0, built.root);
    assert_eq!(built.records[0].1.children(), &[]);
    assert_eq!(built.files, vec![]);
}

/// Records come children first with the root's last, each distinct record once, and each parses
/// back to itself; files are listed by path with their sources.
#[test]
fn built_records_are_listed_bottom_up_once_each_and_parse_back() {
    let built = build_tree(
        None,
        vec![
            file_with("b/two", "2", None, source("cas/d/p", 10)),
            file("a/one", "1"),
            file("c/one", "1"),
            file("c/deeper/x", "x"),
            file("z", "zed"),
        ],
        &TreeLimits::default(),
    )
    .unwrap();
    let digests: Vec<_> = built.records.iter().map(|(d, _)| *d).collect();
    let mut unique = digests.clone();
    unique.sort_by_key(|d| *d.as_bytes());
    unique.dedup();
    assert_eq!(unique.len(), digests.len());
    assert_eq!(*digests.last().unwrap(), built.root);
    for (digest, record) in &built.records {
        assert_eq!(record.digest(), *digest);
        assert_eq!(&TreeRecord::parse(&record.encode()).unwrap(), record);
    }
    let paths: Vec<_> = built.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a/one", "b/two", "c/deeper/x", "c/one", "z"]);
    assert_eq!(
        built.files[1],
        BuiltFile {
            path: "b/two".into(),
            sha256: sha("2"),
            size: 1,
            source: source("cas/d/p", 10),
        }
    );
}

/// Two directories with equal contents have one record, listed once, though two names hold it.
#[test]
fn equal_directories_share_one_record() {
    let built = build_tree(
        None,
        vec![file("a/f", "x"), file("b/f", "x")],
        &TreeLimits::default(),
    )
    .unwrap();
    assert_eq!(built.records.len(), 2);
    let RecordChild::Dir { digest: a, .. } = &built.records[1].1.children()[0] else {
        panic!("a is a directory");
    };
    let RecordChild::Dir { digest: b, .. } = &built.records[1].1.children()[1] else {
        panic!("b is a directory");
    };
    assert_eq!(a, b);
}

/// A child tree is a directory of the new tree by its digest, and its record is not rebuilt.
#[test]
fn a_child_tree_is_a_directory_by_digest_and_has_no_record_here() {
    let child = sha("some earlier tree");
    let built = build_tree(
        None,
        vec![subtree("toolchain", child)],
        &TreeLimits::default(),
    )
    .unwrap();
    assert_eq!(built.records.len(), 1);
    assert_eq!(
        built.records[0].1.children(),
        &[RecordChild::Dir {
            name: "toolchain".into(),
            digest: child
        }]
    );
}

/// Names sort bytewise, not by locale or by character.
#[test]
fn children_sort_bytewise() {
    let built = build_tree(
        None,
        vec![
            file("b", "1"),
            file("a", "1"),
            file("B", "1"),
            file("é", "1"),
            file("z", "1"),
        ],
        &TreeLimits::default(),
    )
    .unwrap();
    let names: Vec<_> = built.records[0]
        .1
        .children()
        .iter()
        .map(RecordChild::name)
        .collect();
    assert_eq!(names, ["B", "a", "b", "z", "é"]);
}

// ---- refusals of the builder

#[test]
fn a_tree_entry_at_the_root_is_refused() {
    assert_eq!(refused(vec![subtree("", sha("t"))]), RecordError::RootTree);
}

/// Each bad name is refused with the path of the entry, whether it is the last name or one above.
#[test]
fn bad_names_are_refused_with_the_path() {
    for path in [
        "",
        "/",
        "/a",
        "a/",
        "a//b",
        ".",
        "..",
        "a/./b",
        "a/../b",
        ".wh.x",
        "a/.wh.x/b",
        "a\0b",
    ] {
        assert_eq!(
            refused(vec![file(path, "x")]),
            RecordError::UnsupportedName(path.into()),
            "{path:?}"
        );
    }
}

#[test]
fn a_path_used_twice_is_a_duplicate() {
    for second in [
        file("a", "y"),
        link("a", "t"),
        subtree("a", sha("t")),
        dir("a", None),
    ] {
        assert_eq!(
            refused(vec![file("a", "x"), second]),
            RecordError::Duplicate("a".into())
        );
    }
    assert_eq!(
        refused(vec![dir("d", None), dir("d", None)]),
        RecordError::Duplicate("d".into())
    );
}

#[test]
fn a_file_that_is_also_a_directory_is_refused() {
    let cases = [
        vec![file("a", "x"), file("a/b", "y")],
        vec![file("a/b", "y"), file("a", "x")],
        vec![subtree("a", sha("t")), file("a/b", "y")],
        vec![link("a", "t"), file("a/b", "y")],
        vec![dir("a", None), file("a", "x")],
        vec![dir("a", None), subtree("a", sha("t"))],
        vec![file("a/b", "y"), link("a", "t")],
    ];
    for entries in cases {
        let error = refused(entries.clone());
        assert!(
            matches!(&error, RecordError::FileAndDirectory(p) if p == "a/b" || p == "a"),
            "{entries:?}: {error:?}"
        );
    }
}

#[test]
fn a_source_of_an_empty_file_is_refused() {
    assert_eq!(
        refused(vec![file_with("e", "", None, source("cas/d/p", 0))]),
        RecordError::EmptyFileWithSource("e".into())
    );
}

#[test]
fn a_source_range_past_2_to_the_64_is_refused_and_one_ending_there_is_not() {
    let at = |offset: u64| vec![file_with("f", "xy", None, source("cas/d/p", offset))];
    assert_eq!(
        refused(at(u64::MAX - 1)),
        RecordError::RangeOverflow("f".into())
    );
    assert!(build_tree(None, at(u64::MAX - 2), &TreeLimits::default()).is_ok());
}

#[test]
fn the_limits_are_those_of_the_format_by_default() {
    let limits = TreeLimits::default();
    assert_eq!(limits.max_entries, MAX_ENTRIES);
    assert_eq!(limits.max_depth, MAX_DEPTH);
    assert_eq!(limits.max_dir_entries, MAX_DIR_ENTRIES);
}

#[test]
fn depth_is_the_names_on_a_path() {
    let limits = TreeLimits {
        max_depth: 2,
        ..TreeLimits::default()
    };
    assert!(build_tree(None, vec![file("a/b", "x")], &limits).is_ok());
    assert_eq!(
        refused_under(limits, vec![file("a/b/c", "x")]),
        RecordError::TooDeep("a/b/c".into())
    );
}

/// Entries count the directories the paths imply.
#[test]
fn entries_count_implied_directories_and_stop_at_the_limit() {
    let limits = TreeLimits {
        max_entries: 3,
        ..TreeLimits::default()
    };
    // a, a/b, a/b/c: three.
    assert!(build_tree(None, vec![file("a/b/c", "x")], &limits).is_ok());
    assert_eq!(
        refused_under(limits, vec![file("a/b/c", "x"), file("a/b/d", "x")]),
        RecordError::TooManyEntries(3)
    );
}

#[test]
fn a_directory_has_at_most_the_limit_of_children() {
    let limits = TreeLimits {
        max_dir_entries: 2,
        ..TreeLimits::default()
    };
    assert!(build_tree(None, vec![file("a", "x"), file("b", "x")], &limits).is_ok());
    assert_eq!(
        refused_under(limits, vec![file("a", "x"), file("b", "x"), file("c", "x")]),
        RecordError::DirectoryTooLarge("c".into())
    );
    assert_eq!(
        refused_under(
            limits,
            vec![file("a/x", "x"), file("b/x", "x"), file("c/x", "x")]
        ),
        RecordError::DirectoryTooLarge("c/x".into())
    );
}

// ---- the record

#[test]
fn modes_are_0_to_07777() {
    assert_eq!(Mode::new(0o7777).unwrap().bits(), 0o7777);
    assert_eq!(Mode::new(0).unwrap().bits(), 0);
    assert_eq!(Mode::new(0o10000), Err(RecordError::ModeTooLarge(0o10000)));
    assert_eq!(
        Mode::new(u16::MAX),
        Err(RecordError::ModeTooLarge(u16::MAX))
    );
}

#[test]
fn octal_mode_texts_are_one_to_four_digits() {
    for (text, bits) in [
        ("0", 0),
        ("7", 7),
        ("644", 0o644),
        ("0644", 0o644),
        ("0755", 0o755),
        ("4755", 0o4755),
        ("1777", 0o1777),
        ("7777", 0o7777),
    ] {
        assert_eq!(Mode::parse_octal(text).unwrap().bits(), bits, "{text}");
    }
    for text in ["", "12345", "0800", "0x1f", "-1", " 644", "rwx", "٣"] {
        assert_eq!(
            Mode::parse_octal(text),
            Err(RecordError::BadMode(text.into())),
            "{text:?}"
        );
    }
}

/// Every kind, written by hand to the layout of the design and read back; a file of the largest
/// size, whose varint is ten bytes, and a setuid mode.
#[test]
fn tree_record_is_canonical() {
    let child_digest = TreeDigest::from_bytes([0x11; 32]);
    let record = TreeRecord::new(
        Mode::new(0o755).unwrap(),
        vec![
            RecordChild::Dir {
                name: "bin".into(),
                digest: child_digest,
            },
            file_child("cc", 0o4755, u64::MAX),
            RecordChild::Symlink {
                name: "lib".into(),
                target: "bin".into(),
            },
        ],
    )
    .unwrap();

    let mut expected = b"TSTR\x01\x01\xed\x03".to_vec();
    expected.extend_from_slice(&[2, 3]);
    expected.extend_from_slice(b"bin");
    expected.extend_from_slice(&[0x11; 32]);
    expected.extend_from_slice(&[1, 2]);
    expected.extend_from_slice(b"cc");
    expected.extend_from_slice(&[0x09, 0xed]);
    expected.extend_from_slice(&[0xff; 9]);
    expected.push(0x01);
    expected.extend_from_slice(sha("cc").as_bytes());
    expected.extend_from_slice(&[3, 3]);
    expected.extend_from_slice(b"lib");
    expected.extend_from_slice(&[3]);
    expected.extend_from_slice(b"bin");

    assert_eq!(record.encode(), expected);
    assert_eq!(TreeRecord::parse(&expected).unwrap(), record);
    assert_eq!(record.digest(), TreeDigest::of(&expected));
}

/// Every refusal of the parser, each by its name.
#[test]
fn tree_record_refusals_are_by_name() {
    let one_file = {
        let mut c = vec![1];
        c.extend(name_bytes(b"a"));
        c.extend_from_slice(&0o644u16.to_be_bytes());
        c.push(1);
        c.extend_from_slice(&[7; 32]);
        c
    };
    let good = record_bytes(0o755, &[1], &one_file);
    assert!(TreeRecord::parse(&good).is_ok());

    // Not a record: wrong magic, and bytes too few to hold one.
    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    assert_eq!(TreeRecord::parse(&bad_magic), Err(RecordError::BadMagic));
    assert_eq!(TreeRecord::parse(b"TST"), Err(RecordError::Truncated));
    assert_eq!(TreeRecord::parse(b""), Err(RecordError::Truncated));

    // Another version.
    let mut version2 = good.clone();
    version2[4] = 2;
    assert_eq!(
        TreeRecord::parse(&version2),
        Err(RecordError::UnsupportedVersion(2))
    );

    // Mode bits above 07777, on the directory and on a file.
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o10000, &[0], &[])),
        Err(RecordError::ModeTooLarge(0o10000))
    );
    let mut file_mode_high = vec![1];
    file_mode_high.extend(name_bytes(b"a"));
    file_mode_high.extend_from_slice(&0o20644u16.to_be_bytes());
    file_mode_high.push(1);
    file_mode_high.extend_from_slice(&[7; 32]);
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &[1], &file_mode_high)),
        Err(RecordError::ModeTooLarge(0o20644))
    );

    // Cut anywhere, a record is truncated; and what follows the last child is refused.
    for len in 0..good.len() {
        assert!(TreeRecord::parse(&good[..len]).is_err(), "cut at {len}");
    }
    assert_eq!(
        TreeRecord::parse(&good[..good.len() - 1]),
        Err(RecordError::Truncated)
    );
    let mut trailing = good.clone();
    trailing.push(0);
    assert_eq!(
        TreeRecord::parse(&trailing),
        Err(RecordError::TrailingBytes)
    );

    // A count the bytes cannot hold is truncation, not an allocation.
    let huge = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &huge, &[])),
        Err(RecordError::Truncated)
    );

    // A kind that is not 1, 2 or 3.
    for kind in [0u8, 4, 255] {
        let mut c = vec![kind];
        c.extend(name_bytes(b"a"));
        c.extend_from_slice(&[0; 40]);
        assert_eq!(
            TreeRecord::parse(&record_bytes(0o755, &[1], &c)),
            Err(RecordError::UnknownKind(kind))
        );
    }

    // Text that is not UTF-8: a name, and a target.
    let mut bad_name = vec![3];
    bad_name.extend(name_bytes(&[0xff]));
    bad_name.extend(name_bytes(b"t"));
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &[1], &bad_name)),
        Err(RecordError::NotUtf8)
    );
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &[1], &link_bytes(b"l", &[0xc3]))),
        Err(RecordError::NotUtf8)
    );

    // A name the format refuses.
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &[1], &link_bytes(b"..", b"t"))),
        Err(RecordError::UnsupportedName("..".into()))
    );

    // Children out of order, and a name twice.
    let ba = [link_bytes(b"b", b"t"), link_bytes(b"a", b"t")].concat();
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &[2], &ba)),
        Err(RecordError::Unsorted {
            previous: "b".into(),
            name: "a".into()
        })
    );
    let aa = [link_bytes(b"a", b"t"), link_bytes(b"a", b"u")].concat();
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &[2], &aa)),
        Err(RecordError::Unsorted {
            previous: "a".into(),
            name: "a".into()
        })
    );
}

/// A varint is read in its shortest form only, and at most 64 bits.
#[test]
fn tree_record_varints_are_minimal_and_fit_64_bits() {
    let link_a = link_bytes(b"a", b"t");
    // The count 1 written in two bytes, and in ten.
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &[0x81, 0x00], &link_a)),
        Err(RecordError::NonMinimalVarint)
    );
    let mut ten = vec![0x81];
    ten.extend_from_slice(&[0x80; 8]);
    ten.push(0x00);
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &ten, &link_a)),
        Err(RecordError::NonMinimalVarint)
    );
    // A name length written in two bytes.
    let mut long_form = vec![3, 0x81, 0x00, b'a'];
    long_form.extend(name_bytes(b"t"));
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &[1], &long_form)),
        Err(RecordError::NonMinimalVarint)
    );
    // The tenth byte may be 1 and no more; an eleventh byte cannot follow.
    let mut too_big = vec![0xff; 9];
    too_big.push(0x02);
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &too_big, &[])),
        Err(RecordError::VarintOverflow)
    );
    let mut continued = vec![0xff; 9];
    continued.push(0x81);
    assert_eq!(
        TreeRecord::parse(&record_bytes(0o755, &continued, &[])),
        Err(RecordError::VarintOverflow)
    );
    // The varint of 128 is two bytes and is read.
    let mut c = vec![1];
    c.extend(name_bytes(b"a"));
    c.extend_from_slice(&0o644u16.to_be_bytes());
    c.extend_from_slice(&[0x80, 0x01]);
    c.extend_from_slice(&[7; 32]);
    let record = TreeRecord::parse(&record_bytes(0o755, &[1], &c)).unwrap();
    assert_eq!(
        record.children(),
        &[RecordChild::File {
            name: "a".into(),
            mode: Mode::new(0o644).unwrap(),
            size: 128,
            sha256: TreeDigest::from_bytes([7; 32]),
        }]
    );
}

/// A name longer than 127 bytes has a two-byte length and round trips.
#[test]
fn long_names_round_trip() {
    let name = "n".repeat(300);
    let record = TreeRecord::new(Mode::DIR, vec![file_child(&name, 0o644, 300)]).unwrap();
    assert_eq!(TreeRecord::parse(&record.encode()).unwrap(), record);
}

#[test]
fn a_record_is_made_only_of_valid_sorted_children() {
    assert_eq!(
        TreeRecord::new(
            Mode::DIR,
            vec![file_child("a", 0o644, 1), file_child("a/b", 0o644, 1)]
        ),
        Err(RecordError::UnsupportedName("a/b".into()))
    );
    assert_eq!(
        TreeRecord::new(
            Mode::DIR,
            vec![file_child("b", 0o644, 1), file_child("a", 0o644, 1)]
        ),
        Err(RecordError::Unsorted {
            previous: "b".into(),
            name: "a".into()
        })
    );
    let empty = TreeRecord::new(Mode::DIR, vec![]).unwrap();
    assert_eq!(empty.encode(), b"TSTR\x01\x01\xed\x00");
}

#[test]
fn child_names_are_read_off_every_kind() {
    assert_eq!(file_child("f", 0o644, 0).name(), "f");
    assert_eq!(
        RecordChild::Dir {
            name: "d".into(),
            digest: sha("d")
        }
        .name(),
        "d"
    );
    assert_eq!(
        RecordChild::Symlink {
            name: "s".into(),
            target: "t".into()
        }
        .name(),
        "s"
    );
}

#[test]
fn errors_say_what_they_refused() {
    let texts = [
        (RecordError::BadMagic, "bad magic"),
        (
            RecordError::UnsupportedVersion(2),
            "unsupported tree version 2",
        ),
        (RecordError::ModeTooLarge(0o10000), "bad mode 10000"),
        (RecordError::BadMode("9".into()), "bad mode \"9\""),
        (RecordError::Duplicate("a".into()), "duplicate path \"a\""),
        (
            RecordError::FileAndDirectory("a".into()),
            "file and directory at one path \"a\"",
        ),
        (RecordError::RootTree, "tree entry may not be the root"),
        (RecordError::TooManyEntries(3), "tree too large"),
        (RecordError::TooDeep("a".into()), "tree too deep"),
    ];
    for (error, text) in texts {
        assert!(error.to_string().contains(text), "{error}");
    }
}
