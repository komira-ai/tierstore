//! The drive walk over bucket directories made here: what it lists, how walks share the
//! objects, and what it counts instead of failing.

use std::collections::BTreeSet;
use std::path::Path;

use tierstore_testkit::dir::TestDir;

use super::*;
use crate::xlmeta::tests::{Enc, object, xl};

/// Writes an object directory `rel` under `root` holding `xl.meta` of `size` bytes, and a data
/// directory with a part file (the walk must not go into it).
fn put(root: &Path, rel: &str, size: i64) {
    let dir = root.join(rel);
    std::fs::create_dir_all(dir.join("1fbc8984-uuid")).unwrap();
    std::fs::write(dir.join("1fbc8984-uuid/part.1"), b"shard").unwrap();
    std::fs::write(dir.join("xl.meta"), xl(&object(size, false, 1), 1)).unwrap();
}

/// Walks `root` with `share` and returns the listing text, the erasure text and the result.
fn walk_all(root: &Path, share: Share) -> (String, String, Walked) {
    let (mut listing, mut meta) = (Vec::new(), Vec::new());
    let walked = {
        let mut sinks = Sinks {
            listing: &mut listing,
            meta: Some(&mut meta),
        };
        walk(root, "kbf-cas", share, &mut sinks).unwrap()
    };
    (
        String::from_utf8(listing).unwrap(),
        String::from_utf8(meta).unwrap(),
        walked,
    )
}

/// One walk lists every object once with its size, time and tag, not the part files below
/// an object, and writes its erasure facts.
#[test]
fn one_walk_lists_every_object_once() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-walk-one");
    let root = dir.path().join("kbf-cas");
    put(&root, "cas/b/1-2/aa/pack", 10);
    put(&root, "kbf/2932/cas/0001/0002", 20);
    put(&root, "top", 30);
    let (listing, meta, walked) = walk_all(&root, Share { index: 0, of: 1 });
    let keys: Vec<&str> = listing
        .lines()
        .map(|l| l.split('\t').nth(1).unwrap())
        .collect();
    let keys: BTreeSet<&str> = keys.into_iter().collect();
    assert_eq!(
        keys,
        BTreeSet::from(["cas/b/1-2/aa/pack", "kbf/2932/cas/0001/0002", "top"])
    );
    assert!(
        listing.contains("kbf-cas\ttop\t30\t1790000000123\tabc-2\n"),
        "{listing}"
    );
    assert!(meta.contains("kbf-cas\ttop\t8\t4\t1\t0\t1\n"), "{meta}");
    assert_eq!((walked.objects, walked.bytes), (3, 60));
    assert_eq!((walked.deleted, walked.versioned), (0, 0));
    assert!(walked.errors.is_empty());
    // root, cas, cas/b, cas/b/1-2, cas/b/1-2/aa, kbf, kbf/2932, kbf/2932/cas, kbf/2932/cas/0001
    assert_eq!(walked.dirs, 9);
}

/// Three walks over a tree with wide and narrow directories list each object exactly once
/// between them, and each lists some.
#[test]
fn shares_list_each_object_exactly_once() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-walk-shares");
    let root = dir.path().join("kbf-cas");
    let mut all = BTreeSet::new();
    // A wide directory of builds (split), each with a narrow run directory (shared below it).
    for b in 0..40 {
        let rel = format!("cas/b/{b}/run/pack");
        put(&root, &rel, 1);
        all.insert(rel);
    }
    // A narrow tree: objects whose parents have few subdirectories stay shared to the object.
    for s in 0..5 {
        let rel = format!("kbf/2932/cas/0001/{s:04}");
        put(&root, &rel, 1);
        all.insert(rel);
    }
    let mut seen = Vec::new();
    for index in 0..3 {
        let (listing, _, walked) = walk_all(&root, Share { index, of: 3 });
        assert!(walked.objects > 0, "share {index} listed nothing");
        seen.extend(
            listing
                .lines()
                .map(|l| l.split('\t').nth(1).unwrap().to_owned()),
        );
    }
    assert_eq!(seen.len(), all.len());
    assert_eq!(seen.into_iter().collect::<BTreeSet<_>>(), all);
}

/// A delete marker is counted and not listed; a second version is counted; a damaged
/// `xl.meta` and an `xl.meta` that is a directory are named in the errors; a symbolic link
/// is not followed.
#[test]
fn deleted_versioned_and_unreadable_objects_are_counted() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-walk-odd");
    let root = dir.path().join("b");
    put(&root, "live", 5);
    let mut marker = Enc::default();
    marker.map(1).str("Type").int(2);
    std::fs::create_dir_all(root.join("gone")).unwrap();
    std::fs::write(root.join("gone/xl.meta"), xl(&marker.0, 2)).unwrap();
    std::fs::create_dir_all(root.join("two")).unwrap();
    std::fs::write(root.join("two/xl.meta"), xl(&object(7, true, 1), 2)).unwrap();
    std::fs::create_dir_all(root.join("bad")).unwrap();
    std::fs::write(root.join("bad/xl.meta"), b"XL1 nope").unwrap();
    std::fs::create_dir_all(root.join("odd/xl.meta")).unwrap();
    std::os::unix::fs::symlink(root.join("live"), root.join("link")).unwrap();
    let (listing, meta, walked) = walk_all(&root, Share { index: 0, of: 1 });
    assert_eq!(listing.lines().count(), 2, "{listing}");
    assert!(meta.contains("kbf-cas\ttwo\t8\t4\t1\t1\t2\n"), "{meta}");
    assert_eq!((walked.objects, walked.bytes), (2, 12));
    assert_eq!((walked.deleted, walked.versioned), (1, 1));
    let names: Vec<&str> = walked.errors.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(names, ["bad/xl.meta", "odd/xl.meta"]);
    assert_eq!(
        walked.errors[0].1,
        format!(
            "{}:0: not an xl.meta (no XL2 magic)",
            root.join("bad/xl.meta").display()
        )
    );
}

/// A bucket directory that is not there is an error naming it; a sink that fails is an error.
#[test]
fn a_missing_root_and_a_failing_sink_are_errors() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-walk-err");
    let mut sink = Vec::new();
    let mut sinks = Sinks {
        listing: &mut sink,
        meta: None,
    };
    let missing = dir.path().join("absent");
    assert!(matches!(
        walk(&missing, "b", Share { index: 0, of: 1 }, &mut sinks),
        Err(Error::Io { what: "stat", .. })
    ));
    let file = dir.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(matches!(
        walk(&file, "b", Share { index: 0, of: 1 }, &mut sinks),
        Err(Error::Io { what: "read", .. })
    ));
    let root = dir.path().join("b");
    put(&root, "k", 1);
    let mut full = Full;
    let mut sinks = Sinks {
        listing: &mut full,
        meta: None,
    };
    let err = walk(&root, "b", Share { index: 0, of: 1 }, &mut sinks).unwrap_err();
    assert_eq!(err.to_string(), "write the listing: full");
    let mut ok = Vec::new();
    let mut full = Full;
    let mut sinks = Sinks {
        listing: &mut ok,
        meta: Some(&mut full),
    };
    let err = walk(&root, "b", Share { index: 0, of: 1 }, &mut sinks).unwrap_err();
    assert_eq!(err.to_string(), "write the erasure facts: full");
}

/// A writer that is always full.
struct Full;

impl Write for Full {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("full"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("full"))
    }
}

/// `i/n` parses when i is below n; anything else is refused with its reason.
#[test]
fn a_share_is_i_of_n() {
    assert_eq!(parse_share("2/12"), Ok(Share { index: 2, of: 12 }));
    assert_eq!(
        parse_share("12/12").unwrap_err(),
        "share 12/12: i must be below n"
    );
    assert_eq!(parse_share("3").unwrap_err(), "\"3\" is not i/n");
    assert_eq!(parse_share("x/2").unwrap_err(), "\"x\" is not a number");
    assert_eq!(parse_share("1/y").unwrap_err(), "\"y\" is not a number");
    assert!(Share { index: 0, of: 1 }.owns("anything"));
}

/// The verb writes the listing and the erasure facts, names the bucket after its directory
/// unless told, and prints a summary and each error; its failures name what failed.
#[test]
fn the_verb_writes_its_files_and_summary() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-walk-verb");
    let root = dir.path().join("kbf-cas");
    put(&root, "a/k", 4);
    std::fs::create_dir_all(root.join("bad")).unwrap();
    std::fs::write(root.join("bad/xl.meta"), b"no").unwrap();
    let out = dir.path().join("l.tsv");
    let meta_out = dir.path().join("m.tsv");
    let args = Args {
        bucket_dir: root.clone(),
        bucket: None,
        share: Share { index: 0, of: 1 },
        out: out.clone(),
        meta_out: Some(meta_out.clone()),
    };
    let mut stdout = Vec::new();
    run(&args, &mut stdout).unwrap();
    let text = String::from_utf8(stdout).unwrap();
    assert!(
        text.starts_with(&format!(
            "1 objects, 4 bytes, 0 deleted, 0 with versions, 2 directories, 1 errors in {}\nerror bad/xl.meta: ",
            out.display()
        )),
        "{text}"
    );
    let objects = crate::listing::read(&out).unwrap();
    assert_eq!(objects[0].bucket, "kbf-cas");
    assert_eq!(objects[0].key, "a/k");
    assert_eq!(
        std::fs::read_to_string(&meta_out).unwrap(),
        "kbf-cas\ta/k\t8\t4\t1\t0\t1\n"
    );

    let named = Args {
        bucket: Some("other".into()),
        meta_out: None,
        ..args
    };
    run(&named, &mut Vec::new()).unwrap();
    assert_eq!(crate::listing::read(&out).unwrap()[0].bucket, "other");

    let no_name = Args {
        bucket_dir: std::path::PathBuf::from("/"),
        bucket: None,
        share: Share { index: 0, of: 1 },
        out: out.clone(),
        meta_out: None,
    };
    assert!(matches!(
        run(&no_name, &mut Vec::new()),
        Err(Error::Usage(_))
    ));
    let bad_out = Args {
        bucket_dir: root.clone(),
        bucket: None,
        share: Share { index: 0, of: 1 },
        out: dir.path().join("no/dir/l.tsv"),
        meta_out: None,
    };
    assert!(matches!(
        run(&bad_out, &mut Vec::new()),
        Err(Error::Io { what: "create", .. })
    ));
    let bad_meta = Args {
        bucket_dir: root.clone(),
        bucket: None,
        share: Share { index: 0, of: 1 },
        out: out.clone(),
        meta_out: Some(dir.path().join("no/dir/m.tsv")),
    };
    assert!(matches!(
        run(&bad_meta, &mut Vec::new()),
        Err(Error::Io { what: "create", .. })
    ));
    let quiet = Args {
        bucket_dir: root,
        bucket: None,
        share: Share { index: 0, of: 1 },
        out,
        meta_out: None,
    };
    let err = run(&quiet, &mut Full).unwrap_err();
    assert_eq!(err.to_string(), "write standard output: full");
}

/// A bucket directory reached through a symbolic link is walked like the directory itself.
#[test]
fn a_linked_bucket_directory_is_walked() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-walk-link");
    let real = dir.path().join("real");
    put(&real, "a/k", 3);
    let link = dir.path().join("kbf-cas");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let (listing, _, walked) = walk_all(&link, Share { index: 0, of: 1 });
    assert_eq!(walked.objects, 1);
    assert!(listing.starts_with("kbf-cas\ta/k\t3\t"), "{listing}");
}

/// A damaged object is named only by the walk it belongs to: the other walks do not read it.
#[test]
fn only_the_owning_walk_reads_and_names_a_damaged_object() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-walk-owner");
    let root = dir.path().join("b");
    std::fs::create_dir_all(root.join("bad")).unwrap();
    std::fs::write(root.join("bad/xl.meta"), b"no").unwrap();
    std::fs::create_dir_all(root.join("odd/xl.meta")).unwrap();
    let owner = (0..3)
        .find(|i| Share { index: *i, of: 3 }.owns("bad"))
        .unwrap();
    let odd_owner = (0..3)
        .find(|i| Share { index: *i, of: 3 }.owns("odd"))
        .unwrap();
    for index in 0..3 {
        let (_, _, walked) = walk_all(&root, Share { index, of: 3 });
        let named: Vec<&str> = walked.errors.iter().map(|(p, _)| p.as_str()).collect();
        let mut want = Vec::new();
        if index == owner {
            want.push("bad/xl.meta");
        }
        if index == odd_owner {
            want.push("odd/xl.meta");
        }
        want.sort_unstable();
        assert_eq!(named, want, "share {index}");
    }
}

/// A writer that takes `left` writes and then fails, and whose flush fails when `bad_flush`.
struct Flaky {
    left: usize,
    bad_flush: bool,
}

impl Write for Flaky {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Err(std::io::Error::other("full"));
        }
        self.left -= 1;
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.bad_flush {
            Err(std::io::Error::other("flush"))
        } else {
            Ok(())
        }
    }
}

/// A listing or erasure writer whose flush fails, and a standard output that fails after the
/// summary line, are errors naming what was written.
#[test]
fn failing_flushes_and_a_failing_error_line_are_errors() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-walk-flush");
    let root = dir.path().join("kbf-cas");
    put(&root, "k", 1);
    std::fs::create_dir_all(root.join("bad")).unwrap();
    std::fs::write(root.join("bad/xl.meta"), b"no").unwrap();
    let args = Args {
        bucket_dir: root,
        bucket: None,
        share: Share { index: 0, of: 1 },
        out: dir.path().join("l.tsv"),
        meta_out: Some(dir.path().join("m.tsv")),
    };
    let ok = || Flaky {
        left: usize::MAX,
        bad_flush: false,
    };
    let bad = || Flaky {
        left: usize::MAX,
        bad_flush: true,
    };
    let err = walk_into(&args, &mut bad(), Some(&mut ok()), &mut Vec::new()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("write {}: flush", args.out.display())
    );
    let err = walk_into(&args, &mut ok(), Some(&mut bad()), &mut Vec::new()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("write {}: flush", dir.path().join("m.tsv").display())
    );
    let mut one_line = Flaky {
        left: 1,
        bad_flush: false,
    };
    let err = walk_into(&args, &mut ok(), None, &mut one_line).unwrap_err();
    assert_eq!(err.to_string(), "write standard output: full");
}

#[cfg(target_os = "linux")]
/// Makes a chain of directories below `root`, each name 200 bytes, until the path of
/// `<last>/xl.meta` is longer than the system's `PATH_MAX` (4096 bytes on Linux), and returns
/// the last directory's path relative to `root`. The chain is made one directory at a time
/// through directory descriptors, so no call is given the long path.
fn too_long_chain(root: &Path) -> String {
    use rustix::fs::{Mode, OFlags, mkdirat, openat};
    let name = "d".repeat(200);
    let mut dir = openat(
        rustix::fs::CWD,
        root,
        OFlags::DIRECTORY | OFlags::RDONLY,
        Mode::empty(),
    )
    .unwrap();
    let mut rel = String::new();
    while root.as_os_str().len() + 1 + rel.len() + "/xl.meta".len() <= 4096 {
        mkdirat(&dir, name.as_str(), Mode::from_raw_mode(0o755)).unwrap();
        dir = openat(
            &dir,
            name.as_str(),
            OFlags::DIRECTORY | OFlags::RDONLY,
            Mode::empty(),
        )
        .unwrap();
        rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
    }
    rel
}

/// A directory whose `xl.meta` cannot be looked at (here its path is too long for the system,
/// an error other than "not found") is not walked into: the walk that owns it names it among
/// the errors, a walk that does not own it says nothing, and the rest of the tree is listed.
#[test]
// Linux only: `PATH_MAX` and the error's text are Linux's (macOS has 1024 and another code).
#[cfg(target_os = "linux")]
fn an_object_whose_xl_meta_cannot_be_looked_at_is_named_by_its_owner_only() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-drivewalk-long");
    put(dir.path(), "cas/l/a", 7);
    let deep = too_long_chain(dir.path());
    let (listing, _, all) = walk_all(dir.path(), Share { index: 0, of: 1 });
    assert_eq!(listing.lines().count(), 1, "{listing}");
    assert_eq!(all.errors.len(), 1, "{:?}", all.errors);
    assert_eq!(all.errors[0].0, format!("{deep}/xl.meta"));
    assert_eq!(all.errors[0].1, "File name too long (os error 36)");

    let named: Vec<usize> = (0..2)
        .map(|index| walk_all(dir.path(), Share { index, of: 2 }).2.errors.len())
        .collect();
    let mut sorted = named.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        [0, 1],
        "exactly one of two walks names it: {named:?}"
    );
}
