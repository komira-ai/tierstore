//! The core against a fake fill socket and the sample tree: attach and detach, the namespace, the
//! lookups, listings and opens below a tree's root, the failures an open records, and the tree
//! leaving memory when nothing holds it.

use std::time::Duration;

use super::{AttachError, Attr, Core, DirItem, Entry, Errno, Failure, Kind, Looked, TREE_TTL};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Built, OWNER, Rig, file, sample, sha};

const RUN: &str = "run-1";

/// A rig over the sample tree with `RUN` attached; the tree is number 1.
fn attached(name: &str) -> Rig {
    let rig = Rig::new(name, sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    rig
}

fn root_ino() -> u64 {
    ino::of(1, 0)
}

fn found(core: &Core, parent: u64, name: &str) -> Entry {
    match core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => entry,
        other => panic!("{name}: {other:?}"),
    }
}

/// The names of a listing from `offset`, with the position after each.
fn listed(core: &Core, dir: u64, offset: u64) -> Vec<(u64, String)> {
    let mut items = Vec::new();
    core.readdir(dir, offset, &mut |next, item: DirItem| {
        items.push((next, item.name));
        true
    })
    .unwrap();
    items
}

fn blob_of(rig: &Rig, content_len: usize) -> String {
    let built = rig.upstream.built.lock().unwrap();
    built
        .blobs
        .iter()
        .find(|(_, bytes)| bytes.len() == content_len)
        .map(|(sha, _)| sha.clone())
        .unwrap()
}

/// Asserts that an attach reads the manifests of the tree through the fill socket, once each, and
/// answers the path of the tree below the mount; that a second run attaching it reads nothing
/// more; and that the tree is numbered 1 and its root's inode number is derived from it.
#[test]
fn an_attach_reads_the_manifests_once_and_answers_the_path() {
    let rig = Rig::new("tfs-core-attach", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let path = rig.core.attach(&root, "run-1").unwrap();
    assert_eq!(path, rig.mountpoint.join("trees").join(&root));
    assert_eq!(rig.upstream.asked("fill").len(), 3);
    assert_eq!(rig.upstream.asked("fill")[0], format!("fill {root}"));
    let _ = rig.core.attach(&root, "run-2").unwrap();
    assert_eq!(rig.upstream.asked("fill").len(), 3);
    assert_eq!(found(&rig.core, ino::TREES, &root).attr.ino, root_ino());
    let text = std::fs::read_to_string(rig.runtime.join("attachments")).unwrap();
    assert_eq!(text, format!("{root} run-1\n{root} run-2\n"));
}

/// Asserts each refusal of an attach: a digest that is not one, a run that is not a name, a tree
/// the upstream does not have, a manifest that does not parse, and an attachments file that cannot
/// be written, after which the run has nothing attached.
#[test]
fn an_attach_is_refused_by_name() {
    let rig = Rig::new("tfs-core-refused", sample());
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    assert_eq!(
        core.attach("nope", RUN).unwrap_err().to_string(),
        "not a sha256 digest"
    );
    assert_eq!(
        core.attach(&root.to_uppercase(), RUN)
            .unwrap_err()
            .to_string(),
        "not a sha256 digest"
    );
    assert_eq!(
        core.attach(&root, "").unwrap_err().to_string(),
        "not a usable run name"
    );
    assert_eq!(
        core.attach(&root, "a b").unwrap_err().to_string(),
        "not a usable run name"
    );
    let unknown = sha(b"no such manifest");
    let why = core.attach(&unknown, RUN).unwrap_err().to_string();
    assert!(why.contains("upstream 404"), "{why}");
    // Bytes that are not a manifest, under their own digest.
    let junk = b"this is not json".to_vec();
    let digest = sha(&junk);
    let _ = rig
        .upstream
        .built
        .lock()
        .unwrap()
        .manifests
        .insert(digest.clone(), junk);
    let why = core.attach(&digest, RUN).unwrap_err();
    assert!(matches!(why, AttachError::Tree(_)), "{why}");
    assert!(why.to_string().contains("malformed manifest"), "{why}");
    // A runtime directory that is gone: the attachment is not kept.
    std::fs::remove_dir_all(&rig.runtime).unwrap();
    let why = core.attach(&root, RUN).unwrap_err();
    assert!(matches!(why, AttachError::File(_)), "{why}");
    assert!(why.to_string().starts_with("the attachments file: "));
    assert!(core.attachments().is_empty());
    assert!(core.detach(RUN).is_none());
}

/// Asserts that a table with every number taken refuses the attach with the table's words.
#[test]
fn a_table_with_every_number_taken_refuses_the_attach() {
    let rig = Rig::new("tfs-core-table", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    rig.core.table.exhaust();
    let why = rig.core.attach(&root, RUN).unwrap_err();
    assert!(matches!(why, AttachError::Table(_)), "{why}");
    assert_eq!(why.to_string(), "the table of tree numbers is full");
}

/// Asserts the namespace: the root and `trees/` are directories of mode 0711 with no timeout, owned
/// by root, with the links of a directory; only `trees` is in the root; nobody lists either.
#[test]
fn the_namespace_is_two_directories_nobody_lists() {
    let rig = attached("tfs-core-namespace");
    let core = &rig.core;
    let root = core.getattr(ino::ROOT).unwrap();
    let trees = core.getattr(ino::TREES).unwrap();
    for entry in [root, trees] {
        assert_eq!(entry.ttl, Duration::ZERO);
        assert_eq!(
            (
                entry.attr.kind,
                entry.attr.perm,
                entry.attr.uid,
                entry.attr.gid
            ),
            (Kind::Dir, 0o711, 0, 0)
        );
    }
    assert_eq!((root.attr.ino, root.attr.nlink), (ino::ROOT, 3));
    assert_eq!((trees.attr.ino, trees.attr.nlink), (ino::TREES, 2));
    assert_eq!(found(core, ino::ROOT, "trees"), trees);
    for name in ["views", "", "other"] {
        assert_eq!(core.lookup(ino::ROOT, name), Err(Errno::NoEnt), "{name}");
    }
    assert_eq!(core.lookup(3, "x"), Err(Errno::NoEnt));
    assert_eq!(core.getattr(3), Err(Errno::NoEnt));
    assert_eq!(core.getattr(0), Err(Errno::NoEnt));
    for dir in [ino::ROOT, ino::TREES, 3] {
        assert_eq!(core.opendir(dir), Err(Errno::Acces));
        assert_eq!(core.readdir(dir, 0, &mut |_, _| true), Err(Errno::Acces));
    }
}

/// Asserts that `trees/<digest>` is found for an attached tree with no timeout, owned by the
/// owner of the mount, and is not found for a tree nobody has attached, a name that is not a
/// digest, or a tree after its last run detached.
#[test]
fn a_tree_is_found_by_digest_only_while_attached() {
    let rig = attached("tfs-core-trees");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let entry = found(core, ino::TREES, &root);
    assert_eq!(entry.ttl, Duration::ZERO);
    let attr = entry.attr;
    assert_eq!((attr.kind, attr.perm, attr.nlink), (Kind::Dir, 0o755, 4));
    assert_eq!((attr.uid, attr.gid), OWNER);
    assert_eq!(core.lookup(ino::TREES, &sha(b"other")), Err(Errno::NoEnt));
    assert_eq!(core.lookup(ino::TREES, "not-a-digest"), Err(Errno::NoEnt));
    assert_eq!(
        core.lookup(ino::TREES, &root.to_uppercase()),
        Err(Errno::NoEnt)
    );
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.lookup(ino::TREES, &root), Err(Errno::NoEnt));
    // Attached again: the same number.
    let _ = core.attach(&root, "run-2").unwrap();
    assert_eq!(found(core, ino::TREES, &root).attr.ino, root_ino());
}

/// Asserts the attributes below a tree's root: mode 0644, or 0755 for an executable or a
/// directory, 0777 for a link, the size of the contents or of the target, one link for a file, one
/// owner, and the longest timeout; and the inode number is the tree number and the index.
#[test]
fn the_entries_of_a_tree_report_their_modes_sizes_owner_and_timeout() {
    let rig = attached("tfs-core-attrs");
    let core = &rig.core;
    // Depth first in manifest order: root 0, big.bin 1, bin 2 (cc 3, tool 4), empty.h 5,
    // include 6 (a.h 7), lib 8.
    let cases = [
        ("big.bin", 1, Kind::File, 0o644, 300_000, 1),
        ("bin", 2, Kind::Dir, 0o755, 0, 2),
        ("empty.h", 5, Kind::File, 0o644, 0, 1),
        ("include", 6, Kind::Dir, 0o755, 0, 2),
        ("lib", 8, Kind::Symlink, 0o777, 3, 1),
    ];
    for (name, index, kind, perm, size, nlink) in cases {
        let entry = found(core, root_ino(), name);
        let want = Attr {
            ino: ino::of(1, index),
            kind,
            size,
            perm,
            nlink,
            uid: OWNER.0,
            gid: OWNER.1,
        };
        assert_eq!(
            entry,
            Entry {
                attr: want,
                ttl: TREE_TTL
            },
            "{name}"
        );
        assert_eq!(core.getattr(want.ino).unwrap(), entry);
    }
    let cc = found(core, ino::of(1, 2), "cc");
    assert_eq!(
        (cc.attr.ino, cc.attr.perm, cc.attr.size),
        (ino::of(1, 3), 0o755, 3000)
    );
    assert_eq!(found(core, ino::of(1, 2), "tool").attr.perm, 0o644);
    assert_eq!(found(core, ino::of(1, 6), "a.h").attr.size, 6);
}

/// Asserts that a name that is not below a tree is answered as absent with the longest timeout,
/// not as an error; that a file as parent is `ENOTDIR`; and that an inode of a tree or an entry that
/// does not exist is `ENOENT`.
#[test]
fn an_absent_name_is_a_cached_absence_and_a_bad_parent_is_an_error() {
    let rig = attached("tfs-core-absent");
    let core = &rig.core;
    assert_eq!(
        core.lookup(root_ino(), "nothing"),
        Ok(Looked::Absent(TREE_TTL))
    );
    assert_eq!(
        core.lookup(ino::of(1, 2), "nothing"),
        Ok(Looked::Absent(TREE_TTL))
    );
    assert_eq!(core.lookup(ino::of(1, 1), "x"), Err(Errno::NotDir));
    assert_eq!(core.lookup(ino::of(1, 8), "x"), Err(Errno::NotDir));
    assert_eq!(core.lookup(ino::of(1, 99), "x"), Err(Errno::NoEnt));
    assert_eq!(core.lookup(ino::of(9, 0), "x"), Err(Errno::NoEnt));
    assert_eq!(core.getattr(ino::of(1, 99)), Err(Errno::NoEnt));
    assert_eq!(core.getattr(ino::of(9, 0)), Err(Errno::NoEnt));
}

/// Asserts that a link says its target, and that anything else is not a link.
#[test]
fn a_link_says_its_target() {
    let rig = attached("tfs-core-readlink");
    let core = &rig.core;
    assert_eq!(&*core.readlink(ino::of(1, 8)).unwrap(), "bin");
    assert_eq!(core.readlink(ino::of(1, 1)), Err(Errno::Inval));
    assert_eq!(core.readlink(ino::of(1, 2)), Err(Errno::Inval));
    assert_eq!(core.readlink(ino::ROOT), Err(Errno::Inval));
    assert_eq!(core.readlink(ino::of(1, 99)), Err(Errno::NoEnt));
    assert_eq!(core.readlink(ino::of(9, 0)), Err(Errno::NoEnt));
}

/// Asserts a listing: `.` and `..` first, then the children in manifest order, each with the
/// position that follows it; a listing resumes from a position; the `..` of a tree's root is
/// `trees/`, and of a directory its parent; a sink that stops is not asked again; and what is not a
/// directory is not listed.
#[test]
fn a_listing_has_the_dots_then_the_children_and_resumes_from_a_position() {
    let rig = attached("tfs-core-readdir");
    let core = &rig.core;
    let names = |items: Vec<(u64, String)>| items.into_iter().map(|(_, n)| n).collect::<Vec<_>>();
    let all = listed(core, root_ino(), 0);
    assert_eq!(
        names(all.clone()),
        [".", "..", "big.bin", "bin", "empty.h", "include", "lib"]
    );
    assert_eq!(
        all.iter().map(|(next, _)| *next).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6, 7]
    );
    assert_eq!(
        names(listed(core, root_ino(), 4)),
        ["empty.h", "include", "lib"]
    );
    assert!(listed(core, root_ino(), 7).is_empty());
    assert!(listed(core, root_ino(), u64::MAX).is_empty());
    let mut dots = Vec::new();
    core.readdir(ino::of(1, 2), 0, &mut |_, item| {
        dots.push((item.name, item.entry.attr.ino));
        true
    })
    .unwrap();
    assert_eq!(dots[0], (".".to_owned(), ino::of(1, 2)));
    assert_eq!(dots[1], ("..".to_owned(), root_ino()));
    let mut above = 0;
    core.readdir(root_ino(), 1, &mut |_, item| {
        above = item.entry.attr.ino;
        false
    })
    .unwrap();
    assert_eq!(above, ino::TREES);
    let mut asked = 0;
    core.readdir(root_ino(), 0, &mut |_, _| {
        asked += 1;
        asked < 3
    })
    .unwrap();
    assert_eq!(asked, 3);
    assert_eq!(
        core.readdir(ino::of(1, 1), 0, &mut |_, _| true),
        Err(Errno::NotDir)
    );
    assert_eq!(core.opendir(ino::of(1, 8)), Err(Errno::NotDir));
    assert_eq!(core.opendir(ino::of(1, 99)), Err(Errno::NoEnt));
    assert_eq!(core.opendir(root_ino()), Ok(()));
}

/// Asserts an open: the blob is filled through the socket from its source, or from the object
/// `cas/l/<sha256>` where the manifest names none; the file reads back whole and in parts; a read
/// at the end gives fewer bytes and past it none; a file of no bytes opens with no fill; and a
/// release closes the file.
#[test]
fn an_open_fills_the_blob_and_reads_its_bytes() {
    let rig = attached("tfs-core-open");
    let core = &rig.core;
    let built = rig.upstream.built.lock().unwrap().blobs.clone();
    let big = built[&blob_of(&rig, 300_000)].clone();
    let fh = core.open(ino::of(1, 1), false).unwrap();
    let blob = blob_of(&rig, 300_000);
    assert_eq!(
        rig.upstream.asked("fill").last().unwrap(),
        &format!("fill {blob} 300000 from packs/one @ 4096")
    );
    assert_eq!(core.read(fh, 0, 300_000).unwrap(), big);
    assert_eq!(core.read(fh, 1000, 5).unwrap(), big[1000..1005]);
    assert_eq!(core.read(fh, 299_998, 100).unwrap(), big[299_998..]);
    assert!(core.read(fh, 300_000, 10).unwrap().is_empty());
    assert!(core.read(fh, 400_000, 10).unwrap().is_empty());
    assert!(core.read(fh, 0, 0).unwrap().is_empty());
    let tool = blob_of(&rig, 10);
    let fh2 = core.open(ino::of(1, 4), false).unwrap();
    assert_eq!(
        rig.upstream.asked("fill").last().unwrap(),
        &format!("fill {tool} 10 from cas/l/{tool} @ 0")
    );
    assert_eq!(core.read(fh2, 0, 100).unwrap(), b"tool bytes");
    let fills = rig.upstream.asked("fill").len();
    let empty = core.open(ino::of(1, 5), false).unwrap();
    assert!(core.read(empty, 0, 100).unwrap().is_empty());
    assert_eq!(rig.upstream.asked("fill").len(), fills);
    assert_eq!(core.open_files(), 3);
    for fh in [fh, fh2, empty] {
        core.release(fh);
    }
    assert_eq!(core.open_files(), 0);
    core.release(fh);
    assert_eq!(core.read(fh, 0, 1), Err(Errno::Inval));
}

/// Asserts that a blob already in the cache is not asked for again, by this open or the next, and
/// that it is pinned for the tree, and touched, once however many times it is opened, and again for
/// another blob.
#[test]
fn a_blob_in_the_cache_is_not_filled_again_and_is_pinned_once_per_tree() {
    let rig = attached("tfs-core-pin");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let cc = blob_of(&rig, 3000);
    let fills = |rig: &Rig| rig.upstream.asked("fill").len();
    let first = core.open(ino::of(1, 3), false).unwrap();
    let after_first = fills(&rig);
    let second = core.open(ino::of(1, 3), false).unwrap();
    assert_eq!(fills(&rig), after_first);
    assert_eq!(
        rig.upstream.asked("pin"),
        [format!("pin mount-1 {root} {cc}")]
    );
    assert_eq!(rig.upstream.asked("touch"), [format!("touch {cc}")]);
    let a = blob_of(&rig, 6);
    let third = core.open(ino::of(1, 7), false).unwrap();
    assert_eq!(rig.upstream.asked("pin").len(), 2);
    assert_eq!(
        rig.upstream.asked("pin")[1],
        format!("pin mount-1 {root} {a}")
    );
    for fh in [first, second, third] {
        core.release(fh);
    }
}

/// Asserts the refusals of an open: for writing (`EROFS`), a directory and the namespace
/// (`EISDIR`), a link (`EINVAL`), an entry that is not there (`ENOENT`).
#[test]
fn an_open_is_refused_for_writing_and_for_what_is_not_a_file() {
    let rig = attached("tfs-core-open-refused");
    let core = &rig.core;
    assert_eq!(core.open(ino::of(1, 1), true), Err(Errno::RoFs));
    assert_eq!(core.open(ino::of(1, 2), false), Err(Errno::IsDir));
    assert_eq!(core.open(ino::ROOT, false), Err(Errno::IsDir));
    assert_eq!(core.open(ino::of(1, 8), false), Err(Errno::Inval));
    assert_eq!(core.open(ino::of(1, 99), false), Err(Errno::NoEnt));
    assert_eq!(core.open_files(), 0);
    assert!(rig.upstream.asked("fill").len() == 3);
}

/// Asserts that a fill that fails fails the open with `EIO`, records the path, the digest and the
/// words of the failure on every run that has the tree, once for a path however often it is opened,
/// and gives them at `detach`; that a fill answered `ok` that left no file is a failure `io`, and a
/// blob the upstream does not have is `upstream 404`; and that the file opens once the upstream
/// has it again.
#[test]
fn a_failed_fill_fails_the_open_and_is_recorded_for_every_run_of_the_tree() {
    let rig = attached("tfs-core-fail");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = core.attach(&root, "run-2").unwrap();
    let cc = blob_of(&rig, 3000);
    let tool = blob_of(&rig, 10);
    let a = blob_of(&rig, 6);
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(cc.clone(), "no space".to_owned());
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(tool.clone(), "ok".to_owned());
    let held = rig.upstream.built.lock().unwrap().blobs.remove(&a).unwrap();
    assert_eq!(core.open(ino::of(1, 3), false), Err(Errno::Io));
    assert_eq!(core.open(ino::of(1, 3), false), Err(Errno::Io));
    assert_eq!(core.open(ino::of(1, 4), false), Err(Errno::Io));
    assert_eq!(core.open(ino::of(1, 7), false), Err(Errno::Io));
    assert_eq!(core.open_files(), 0);
    assert!(rig.upstream.asked("pin").is_empty());
    let want = vec![
        Failure {
            path: "/bin/cc".into(),
            digest: cc.clone(),
            reason: "no space".into(),
        },
        Failure {
            path: "/bin/tool".into(),
            digest: tool,
            reason: "io".into(),
        },
        Failure {
            path: "/include/a.h".into(),
            digest: a.clone(),
            reason: "upstream 404".into(),
        },
    ];
    let _ = rig.upstream.built.lock().unwrap().blobs.insert(a, held);
    assert_eq!(core.detach("run-1").unwrap(), want);
    rig.upstream.failing.lock().unwrap().clear();
    assert!(core.open(ino::of(1, 3), false).is_ok());
    assert_eq!(core.detach("run-2").unwrap(), want);
    assert!(core.detach("run-2").is_none());
}

/// Asserts that an open file keeps being served after the cache file is removed (evicted), and
/// that a later open fills it again.
#[test]
fn an_evicted_blob_that_is_open_is_still_read_and_filled_again_on_the_next_open() {
    let rig = attached("tfs-core-evicted");
    let core = &rig.core;
    let cc = blob_of(&rig, 3000);
    let fh = core.open(ino::of(1, 3), false).unwrap();
    let path = crate::server::local_cache::blob_path(&rig.cache, &cc);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(core.read(fh, 0, 3000).unwrap().len(), 3000);
    let fills = rig.upstream.asked("fill").len();
    let again = core.open(ino::of(1, 3), false).unwrap();
    assert_eq!(rig.upstream.asked("fill").len(), fills + 1);
    assert!(path.exists());
    core.release(fh);
    core.release(again);
}

/// Asserts that a read that fails (the handle is a directory's file descriptor) is `EIO`.
#[test]
fn a_read_that_the_system_fails_is_an_io_error() {
    let rig = attached("tfs-core-readfail");
    let core = &rig.core;
    let handle = core.open(ino::of(1, 3), false).unwrap();
    // Replace the open file with a directory: reading it fails.
    let directory = std::fs::File::open(&rig.cache).unwrap();
    let _ = super::locked(&core.handles).insert(
        handle,
        std::sync::Arc::new(super::Handle::Cached(directory)),
    );
    assert_eq!(core.read(handle, 0, 10), Err(Errno::Io));
}

/// Asserts the detach: a run's failures come back, a tree that another run has stays attached and
/// pinned, the last run's detach unpins the tree, a run that attached nothing is `None`, and the
/// attachments file follows.
#[test]
fn a_detach_unpins_a_tree_only_when_its_last_run_goes() {
    let rig = attached("tfs-core-detach");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = core.attach(&root, "run-2").unwrap();
    assert_eq!(core.detach("run-1"), Some(Vec::new()));
    assert!(rig.upstream.asked("unpin").is_empty());
    assert_eq!(core.attachments().len(), 1);
    assert_eq!(core.detach("run-1"), None);
    assert_eq!(core.detach("run-2"), Some(Vec::new()));
    assert_eq!(
        rig.upstream.asked("unpin"),
        [format!("unpin mount-1 {root}")]
    );
    assert_eq!(
        std::fs::read_to_string(rig.runtime.join("attachments")).unwrap(),
        ""
    );
}

/// Asserts that `detach-all-except` ends the runs that are not named and counts them, and keeps the
/// ones that are.
#[test]
fn detach_all_except_ends_the_runs_not_named() {
    let rig = attached("tfs-core-except");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    for run in ["run-2", "run-3"] {
        let _ = core.attach(&root, run).unwrap();
    }
    assert_eq!(
        core.detach_all_except(&["run-2".to_owned(), "never-attached".to_owned()]),
        2
    );
    let left: Vec<String> = core.attachments().into_iter().map(|(_, run)| run).collect();
    assert_eq!(left, ["run-2"]);
    assert_eq!(core.detach_all_except(&["run-2".to_owned()]), 0);
    assert_eq!(core.detach_all_except(&[]), 1);
}

/// Asserts that `pins_live` names the trees attached now once each, and none when there are none.
#[test]
fn pins_live_names_the_trees_attached_now() {
    let rig = attached("tfs-core-live");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = core.attach(&root, "run-2").unwrap();
    core.pins_live();
    let _ = core.detach("run-1");
    let _ = core.detach("run-2");
    core.pins_live();
    assert_eq!(
        rig.upstream.asked("pins-live"),
        [
            format!("pins-live mount-1 {root}"),
            "pins-live mount-1 ".to_owned()
        ]
    );
}

/// Asserts that a tree stays in memory while the kernel holds an inode of it, however the runs
/// went, leaves memory at the last forget, and is read again, with the same inode numbers, when a
/// request names one of its inodes; and that a forget of more than was looked up stops at zero.
#[test]
fn a_tree_leaves_memory_when_unattached_and_forgotten_and_comes_back_on_a_request() {
    let rig = attached("tfs-core-memory");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = found(core, ino::TREES, &root);
    let bin = found(core, root_ino(), "bin");
    assert_eq!(core.trees_in_memory(), 1);
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.trees_in_memory(), 1);
    core.forget(bin.attr.ino, 1);
    assert_eq!(core.trees_in_memory(), 1);
    core.forget(root_ino(), 5);
    assert_eq!(core.trees_in_memory(), 0);
    // The kernel still has inodes of it: any request reads the tree again.
    let again = core.getattr(bin.attr.ino).unwrap();
    assert_eq!(again, bin);
    assert_eq!(core.trees_in_memory(), 1);
    core.forget(bin.attr.ino, 1);
    assert_eq!(core.trees_in_memory(), 0);
    core.forget(ino::of(9, 0), 1);
    core.forget(ino::ROOT, 1);
}

/// Asserts that a tree attached is never dropped by a forget, and that counting the entries of a
/// listing (`READDIRPLUS`) is a lookup each.
#[test]
fn an_attached_tree_is_kept_and_a_counted_listing_entry_is_a_lookup() {
    let rig = attached("tfs-core-count");
    let core = &rig.core;
    core.forget(root_ino(), 100);
    assert_eq!(core.trees_in_memory(), 1);
    core.count(ino::of(1, 2));
    core.count(ino::of(1, 3));
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.trees_in_memory(), 1);
    core.forget(ino::of(1, 2), 1);
    assert_eq!(core.trees_in_memory(), 1);
    core.forget(ino::of(1, 3), 1);
    assert_eq!(core.trees_in_memory(), 0);
    core.count(ino::of(9, 0));
}

/// Asserts that a tree whose manifests cannot be read again is `EIO` and a tree number the table
/// does not know is `ENOENT`, and that a table that cannot be read is `EIO`.
#[test]
fn a_tree_that_cannot_be_read_again_is_an_io_error() {
    let rig = attached("tfs-core-reread");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = found(core, ino::TREES, &root);
    assert!(core.detach(RUN).is_some());
    core.forget(root_ino(), 1);
    assert_eq!(core.trees_in_memory(), 0);
    // The manifests are gone from the cache and from the upstream.
    std::fs::remove_dir_all(rig.cache.join("obj")).ok();
    for entry in std::fs::read_dir(&rig.cache).unwrap() {
        let _ = std::fs::remove_dir_all(entry.unwrap().path());
    }
    rig.upstream.built.lock().unwrap().manifests.clear();
    assert_eq!(core.getattr(root_ino()), Err(Errno::Io));
    assert_eq!(core.getattr(ino::of(5, 0)), Err(Errno::NoEnt));
    // A table that cannot be read.
    let table = std::fs::OpenOptions::new()
        .write(true)
        .open(rig.runtime.join("trees"))
        .unwrap();
    table.set_len(16).unwrap();
    assert_eq!(core.getattr(root_ino()), Err(Errno::Io));
}

/// Asserts that a manifest in the cache that is larger than a manifest may be is refused by size
/// and not read whole into memory past the limit by one byte.
#[test]
fn a_manifest_file_is_read_up_to_one_byte_past_the_limit() {
    let rig = Rig::new("tfs-core-big", Built::default());
    let path = rig.cache.join("big");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(crate::tree_manifest::MAX_MANIFEST_BYTES as u64 + 10)
        .unwrap();
    let bytes = super::read_manifest(&path).unwrap();
    assert_eq!(bytes.len(), crate::tree_manifest::MAX_MANIFEST_BYTES + 1);
    assert!(super::read_manifest(&rig.cache.join("none")).is_err());
}

/// Asserts the path of an entry in its tree: the root is `/`, a deep entry is its names from the
/// root.
#[test]
fn the_path_of_an_entry_is_its_names_from_the_root() {
    let rig = attached("tfs-core-path");
    let loaded = rig.core.tree(1).unwrap();
    assert_eq!(super::path_of(&loaded.tree, 0), "/");
    assert_eq!(super::path_of(&loaded.tree, 3), "/bin/cc");
    assert_eq!(super::path_of(&loaded.tree, 7), "/include/a.h");
}

/// Asserts that a blob pinned for a tree is pinned again when the tree is attached again after its
/// detach, even though the tree stayed in memory because the kernel held an inode of it. Catches a
/// pinned set that outlives the unpin.
#[test]
fn a_tree_attached_again_pins_the_blobs_it_opens_again() {
    let rig = attached("tfs-core-repin");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let cc = blob_of(&rig, 3000);
    let _ = found(core, ino::TREES, &root);
    let first = core.open(ino::of(1, 3), false).unwrap();
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.trees_in_memory(), 1);
    let _ = core.attach(&root, "run-2").unwrap();
    let second = core.open(ino::of(1, 3), false).unwrap();
    assert_eq!(
        rig.upstream.asked("pin"),
        vec![format!("pin mount-1 {root} {cc}"); 2]
    );
    core.release(first);
    core.release(second);
}

/// Asserts that a pin the cache refuses is not recorded as done: the next open of the blob asks
/// again, and once an answer is `ok` it asks no more.
#[test]
fn a_refused_pin_is_asked_again_by_the_next_open() {
    let rig = attached("tfs-core-pin-refused");
    let core = &rig.core;
    *rig.upstream.pin_answer.lock().unwrap() = Some("busy".to_owned());
    let first = core.open(ino::of(1, 3), false).unwrap();
    *rig.upstream.pin_answer.lock().unwrap() = None;
    let second = core.open(ino::of(1, 3), false).unwrap();
    let third = core.open(ino::of(1, 3), false).unwrap();
    assert_eq!(rig.upstream.asked("pin").len(), 2);
    for fh in [first, second, third] {
        core.release(fh);
    }
}

/// Asserts that an attach whose attachments file cannot be written takes back only the attachment
/// it added: the run's earlier trees stay attached and a repeat of an existing attach leaves it.
#[test]
fn a_failed_save_takes_back_only_the_attach_that_failed() {
    let mut built = sample();
    let second = built.add_tree(vec![("only", file(b"x"))]);
    let rig = Rig::new("tfs-core-save-failed", built);
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = core.attach(&root, RUN).unwrap();
    std::fs::remove_dir_all(&rig.runtime).unwrap();
    assert!(core.attach(&second, RUN).is_err());
    assert!(core.attach(&root, RUN).is_err());
    let left: Vec<_> = core
        .attachments()
        .iter()
        .map(|(tree, _)| tree.to_string())
        .collect();
    assert_eq!(left, [root]);
}

/// Asserts that a detach and an attach of one tree do not interleave: the unpin of the detach is
/// sent before the attach takes the tree, so that it cannot remove the pins of the new run.
#[test]
fn an_attach_waits_for_the_detach_of_the_same_tree() {
    let rig = attached("tfs-core-serial");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let guard = core.lifecycle.lock().unwrap();
    std::thread::scope(|scope| {
        let attach = scope.spawn(|| core.attach(&root, "run-2").unwrap());
        std::thread::yield_now();
        assert_eq!(core.attachments().len(), 1);
        drop(guard);
        let _ = attach.join().unwrap();
    });
    assert_eq!(core.attachments().len(), 2);
}

/// Asserts that a restore attaches what it can read, writes the file once, and keeps in the file
/// the pair whose tree cannot be read now: it is not served, a detach of its run drops it, and an
/// attach of it later serves it.
#[test]
fn a_restore_keeps_the_record_of_a_tree_it_cannot_read() {
    let rig = Rig::new("tfs-core-restore", sample());
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let gone = "ab".repeat(32);
    let digest = |hex: &str| crate::tree_manifest::TreeDigest::from_hex(hex).unwrap();
    core.restore(vec![
        (digest(&gone), "run-1".to_owned()),
        (digest(&root), "run-2".to_owned()),
        (digest(&root), "run-3".to_owned()),
    ]);
    assert_eq!(core.attachments().len(), 2);
    let file = || std::fs::read_to_string(rig.runtime.join("attachments")).unwrap();
    assert!(file().contains(&format!("{gone} run-1\n")));
    assert!(file().contains(&format!("{root} run-2\n")));
    assert_eq!(core.detach_all_except(&["run-2".to_owned()]), 2);
    assert_eq!(file(), format!("{root} run-2\n"));
    assert_eq!(core.detach("run-1"), None);
}

/// Asserts that a restore with every tree number taken keeps the pair in the file, and that a
/// restore whose file cannot be written still attaches what it read.
#[test]
fn a_restore_with_no_number_left_or_no_file_still_keeps_what_it_can() {
    let rig = Rig::new("tfs-core-restore-full", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let digest = crate::tree_manifest::TreeDigest::from_hex(&root).unwrap();
    rig.core.table.exhaust();
    rig.core.restore(vec![(digest, RUN.to_owned())]);
    assert!(rig.core.attachments().is_empty());
    assert_eq!(
        std::fs::read_to_string(rig.runtime.join("attachments")).unwrap(),
        format!("{root} {RUN}\n")
    );
    let rig = Rig::new("tfs-core-restore-nofile", sample());
    std::fs::remove_dir_all(&rig.runtime).unwrap();
    rig.core.restore(vec![(digest, RUN.to_owned())]);
    assert_eq!(rig.core.attachments().len(), 1);
}
