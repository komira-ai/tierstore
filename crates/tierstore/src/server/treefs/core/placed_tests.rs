//! Placements at fs locations, end to end: a core over the real tree store, which it calls in
//! process (`live_testing.rs`), the lookups that find a placement, the
//! reads of its files as spans of the pack, the hold of the placement while a tree is attached,
//! and the refusals.

use std::time::Duration;

use super::{dir_ino, key_of};
use crate::server::local_cache::trees::testing::Bench;
use crate::server::local_cache::trees::testing::{built, fs_at};
use crate::server::local_cache::trees::{CreateRequest, Created};
use crate::server::local_cache::{Mode, Tick, blob_path};
use crate::server::treefs::backing::{FILE, Saved};
use crate::server::treefs::core::{AttachError, Core, Entry, Errno, Kind, Looked, Opened};
use crate::server::treefs::ino;
use crate::server::treefs::live_testing::{Live, MOUNT};
use crate::server::treefs::testing::{FakeKernel, TREE_IDLE, open_cached};
use crate::{NewKind, Op, TreeDigest};

/// The wait for the write role of the cores that call a read-only cache in process: what a test
/// that waits it out takes.
const SHORT: Duration = Duration::from_millis(100);

fn found(core: &Core, parent: u64, name: &str) -> Entry {
    match core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => entry,
        other => panic!("{name}: {other:?}"),
    }
}

/// The entry at `names` below the mount's root.
fn walk(core: &Core, names: &[&str]) -> Entry {
    let mut at = ino::ROOT;
    let mut entry = None;
    for name in names {
        let next = found(core, at, name);
        at = next.attr.ino;
        entry = Some(next);
    }
    entry.expect("at least one name")
}

/// The runs of the mount that hold the placement `created`.
fn attachments(live: &Live, created: &Created) -> usize {
    live.bench
        .store
        .placement(&created.placement)
        .expect("a placement")
        .attachments
}

/// The `..` of the listing of the directory `dir`.
fn dotdot(core: &Core, dir: u64) -> u64 {
    let mut up = None;
    core.readdir(dir, 0, &mut |_, item| {
        if item.name == ".." {
            up = Some(item.entry.attr.ino);
        }
        true
    })
    .unwrap();
    up.expect("a `..`")
}

/// Asserts that a tree placed at an fs location, with no fill, is read by the mount from the
/// tree store alone: the directories above the location are of mode 0711 and the tree's root of
/// 0755, all with a timeout of zero; the files have the modes they were stored with; each file
/// reads as the bytes of its member of the pack, a read past its end gets only its own bytes and
/// never the next member's, a read in the middle gets the bytes at that offset, and an empty file
/// reads empty; the link reads as its target; and the upstream was asked for no manifest and the
/// cache holds no copy of any blob.
#[tokio::test(flavor = "multi_thread")]
async fn fs_core_reads_created_tree_without_manifest() {
    let live = Live::new("tfs-placed-read").await;
    let created = live.place(vec![fs_at("jobs/1f3a/root")]);
    let contents: Vec<Vec<u8>> = (0..3)
        .map(|i| live.bench.pack.content(i).to_vec())
        .collect();
    live.on_core(move |core| {
        let jobs = found(core, ino::ROOT, "jobs");
        assert!(ino::is_placed_dir(jobs.attr.ino));
        assert_eq!(jobs.attr.kind, Kind::Dir);
        assert_eq!((jobs.attr.perm, jobs.ttl), (0o711, Duration::ZERO));
        let dir = found(core, jobs.attr.ino, "1f3a");
        assert!(ino::is_placed_dir(dir.attr.ino));
        assert_eq!((dir.attr.perm, dir.ttl), (0o711, Duration::ZERO));
        let root = found(core, dir.attr.ino, "root");
        assert_eq!(root.attr.kind, Kind::Dir);
        assert_eq!((root.attr.perm, root.ttl), (0o755, Duration::ZERO));
        let bin = found(core, root.attr.ino, "bin");
        let tool = found(core, bin.attr.ino, "tool");
        assert_eq!((tool.attr.perm, tool.attr.size), (0o4755, 1000));
        let other = found(core, bin.attr.ino, "other");
        let deep = found(core, found(core, root.attr.ino, "lib").attr.ino, "deep");
        let data = found(core, deep.attr.ino, "data");
        let read_all = |entry: &Entry| {
            let fh = open_cached(core, entry.attr.ino, false).unwrap();
            let bytes = core.read(fh, 0, 10_000).unwrap();
            core.release(fh);
            bytes
        };
        assert_eq!(read_all(&tool), contents[0]);
        assert_eq!(read_all(&other), contents[1]);
        assert_eq!(read_all(&data), contents[2]);

        let fh = open_cached(core, tool.attr.ino, false).unwrap();
        assert_eq!(core.read(fh, 900, 500).unwrap(), contents[0][900..]);
        assert_eq!(core.read(fh, 10, 20).unwrap(), contents[0][10..30]);
        assert!(core.read(fh, 1000, 10).unwrap().is_empty());
        assert!(core.read(fh, 5000, 10).unwrap().is_empty());
        core.release(fh);

        let zero = found(core, root.attr.ino, "zero");
        assert_eq!(zero.attr.size, 0);
        assert!(read_all(&zero).is_empty());
        let link = found(core, root.attr.ino, "link");
        assert_eq!(link.attr.kind, Kind::Symlink);
        assert_eq!(&*core.readlink(link.attr.ino).unwrap(), "bin/tool");
    })
    .await;
    let manifest = format!("cas/l/{}", created.tree);
    assert_eq!(live.bench.rig.upstream.calls_on(Op::Get, &manifest), 0);
    for (sha, _, _) in &live.bench.pack.blobs {
        assert!(!blob_path(&live.bench.rig.config.path, sha).exists());
    }
}

/// Asserts that the open of a file of a placed tree is a cached open even where an earlier release
/// registered a backing id for its blob, since the kernel would map the whole pack onto the file;
/// the file reads as its member of the pack.
#[tokio::test(flavor = "multi_thread")]
async fn mount_reads_pack_member_without_passthrough() {
    let live = Live::new("tfs-placed-pass").await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    let saved = Saved {
        blob: TreeDigest::from_hex(&live.bench.pack.blobs[1].0).unwrap(),
        id: 7,
        opens: 1,
        exact: false,
    };
    let kernel = FakeKernel::new();
    live.core
        .adopt_backing(&live.runtime.join(FILE), &[saved], &|id| kernel.wrap(id));
    let want = live.bench.pack.content(1).to_vec();
    live.on_core(move |core| {
        let file = walk(core, &["jobs", "1f3a", "root", "bin", "other"]);
        let opened = core.open(file.attr.ino, false).unwrap();
        let Opened::Cached(fh) = opened else {
            panic!("an adopted id for a placed file: {opened:?}");
        };
        assert_eq!(core.read(fh, 0, 5000).unwrap(), want);
    })
    .await;
}

/// Asserts that a lookup holds the placement for the mount once, however many names are looked
/// up; that the hold is not given back before the tree has been idle for the idle
/// time, and is given back by the detach once it has been.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_holds_the_placement_until_the_tree_is_idle() {
    let live = Live::new("tfs-placed-hold").await;
    let created = live.place(vec![fs_at("jobs/1f3a/root")]);
    assert_eq!(attachments(&live, &created), 0);
    live.on_core(|core| {
        let _ = walk(core, &["jobs", "1f3a", "root", "bin", "tool"]);
        let _ = walk(core, &["jobs", "1f3a", "root"]);
    })
    .await;
    assert_eq!(attachments(&live, &created), 1);
    live.clock.advance(TREE_IDLE - Duration::from_secs(1));
    assert_eq!(live.on_core(Core::detach_idle).await, 0);
    assert_eq!(attachments(&live, &created), 1);
    live.clock.advance(Duration::from_secs(1));
    assert_eq!(live.on_core(Core::detach_idle).await, 1);
    assert_eq!(attachments(&live, &created), 0);
}

/// Asserts that a placement with two fs locations is two trees of the mount, with different tree
/// numbers; that the hold is kept while either is attached, so that the idle end of one location
/// leaves the hold, and the idle end of the other gives it back.
#[tokio::test(flavor = "multi_thread")]
async fn a_placement_with_two_locations_is_held_until_both_are_idle() {
    let live = Live::new("tfs-placed-two").await;
    let created = live.place(vec![fs_at("a/x"), fs_at("b/y")]);
    let a = live.on_core(|core| walk(core, &["a", "x"])).await;
    live.clock.advance(Duration::from_secs(40));
    let b = live.on_core(|core| walk(core, &["b", "y"])).await;
    assert_ne!(ino::split(a.attr.ino).0, ino::split(b.attr.ino).0);
    assert_eq!(attachments(&live, &created), 1);
    live.clock.advance(Duration::from_secs(30));
    assert_eq!(live.on_core(Core::detach_idle).await, 1);
    assert_eq!(attachments(&live, &created), 1);
    live.clock.advance(Duration::from_secs(40));
    assert_eq!(live.on_core(Core::detach_idle).await, 1);
    assert_eq!(attachments(&live, &created), 0);
}

/// Asserts that a placement that is released stops resolving at the next lookup, with `ENOENT` at
/// the first name of its location, while a file that was open before still reads its bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_released_placement_stops_resolving_but_an_open_file_reads() {
    let live = Live::new("tfs-placed-released").await;
    let created = live.place(vec![fs_at("jobs/1f3a/root")]);
    let want = live.bench.pack.content(0).to_vec();
    let fh = live
        .on_core(|core| {
            let tool = walk(core, &["jobs", "1f3a", "root", "bin", "tool"]);
            open_cached(core, tool.attr.ino, false).unwrap()
        })
        .await;
    live.bench.store.release(&created.placement).unwrap();
    live.on_core(move |core| {
        assert_eq!(core.lookup(ino::ROOT, "jobs"), Err(Errno::NoEnt));
        assert_eq!(core.read(fh, 0, 5000).unwrap(), want);
        core.release(fh);
    })
    .await;
}

/// Asserts that one tree placed at two locations by two placements is two trees of the mount,
/// whose roots and files have different inode numbers, and that the parent of each root is the
/// directory above its own location.
#[tokio::test(flavor = "multi_thread")]
async fn one_tree_in_two_placements_has_two_roots_with_their_own_parents() {
    let live = Live::new("tfs-placed-twice").await;
    let one = live.place(vec![fs_at("a/x")]);
    let other = live.place(vec![fs_at("b/y")]);
    assert_ne!(one.placement, other.placement);
    live.on_core(|core| {
        let a = walk(core, &["a", "x"]);
        let b = walk(core, &["b", "y"]);
        assert_ne!(a.attr.ino, b.attr.ino);
        assert_eq!(core.parent(a.attr.ino), Ok(dir_ino("a")));
        assert_eq!(core.parent(b.attr.ino), Ok(dir_ino("b")));
        assert_eq!(found(core, ino::ROOT, "a").attr.ino, dir_ino("a"));
        let tool_a = walk(core, &["a", "x", "bin", "tool"]);
        let tool_b = walk(core, &["b", "y", "bin", "tool"]);
        assert_ne!(tool_a.attr.ino, tool_b.attr.ino);
    })
    .await;
}

/// Asserts that the parent of a directory above a location is the directory above it, up to the
/// mount's root; that the parent of the tree's root and `..` in its listing are the directory
/// above the location; and that a location of one name has the mount's root for both.
#[tokio::test(flavor = "multi_thread")]
async fn placed_directories_have_their_parents_up_to_the_root() {
    let live = Live::new("tfs-placed-parent").await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    let _ = live.place(vec![fs_at("solo")]);
    live.on_core(|core| {
        let jobs = found(core, ino::ROOT, "jobs");
        let dir = found(core, jobs.attr.ino, "1f3a");
        let root = found(core, dir.attr.ino, "root");
        assert_eq!(core.parent(dir.attr.ino), Ok(jobs.attr.ino));
        assert_eq!(core.parent(jobs.attr.ino), Ok(ino::ROOT));
        assert_eq!(core.parent(root.attr.ino), Ok(dir.attr.ino));
        assert_eq!(dotdot(core, root.attr.ino), dir.attr.ino);
        let solo = found(core, ino::ROOT, "solo");
        assert_eq!(core.parent(solo.attr.ino), Ok(ino::ROOT));
        assert_eq!(dotdot(core, solo.attr.ino), ino::ROOT);
    })
    .await;
}

/// Asserts the refusals on a directory above a location: it cannot be opened as a directory
/// (`EACCES`), is no link (`EINVAL`), cannot be opened or read as a file (`EISDIR`), and its
/// attributes are those of a namespace directory, as a lookup answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_above_a_location_is_a_namespace_directory() {
    let live = Live::new("tfs-placed-refuse").await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    live.on_core(|core| {
        let jobs = found(core, ino::ROOT, "jobs");
        let at = jobs.attr.ino;
        assert_eq!(core.opendir(at), Err(Errno::Acces));
        assert_eq!(core.readlink(at), Err(Errno::Inval));
        assert_eq!(core.open(at, false).unwrap_err(), Errno::IsDir);
        assert_eq!(
            core.blob_file(at, &mut core.open_begins()).unwrap_err(),
            Errno::IsDir
        );
        assert_eq!(core.getattr(at), Ok(jobs));
        assert_eq!(jobs.attr.perm, 0o711);
    })
    .await;
}

/// Asserts that a successor core with nothing in memory answers by inode number alone: the
/// attributes of a file come out the same, read again from the tree store, and the parent of a
/// directory above a location and a lookup in it are found by asking the paths of the locations.
#[tokio::test(flavor = "multi_thread")]
async fn a_successor_core_finds_placed_inodes_it_never_looked_up() {
    let live = Live::new("tfs-placed-successor").await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    let (tool, dir, root) = live
        .on_core(|core| {
            let dir = walk(core, &["jobs", "1f3a"]);
            let root = found(core, dir.attr.ino, "root");
            let bin = found(core, root.attr.ino, "bin");
            (found(core, bin.attr.ino, "tool"), dir, root)
        })
        .await;
    let next = live.successor();
    tokio::task::spawn_blocking(move || {
        assert_eq!(next.getattr(tool.attr.ino).unwrap().attr, tool.attr);
        assert_eq!(next.parent(dir.attr.ino), Ok(dir_ino("jobs")));
        assert_eq!(found(&next, dir.attr.ino, "root").attr.ino, root.attr.ino);
        assert_eq!(next.lookup(dir.attr.ino, "other"), Err(Errno::NoEnt));
    })
    .await
    .unwrap();
}

/// Asserts that the key of a placement's tree is no name of `trees/` and no digest to attach: a
/// lookup of it there is `ENOENT` and an attach of it is refused as not a digest.
#[tokio::test(flavor = "multi_thread")]
async fn a_placement_key_is_refused_by_trees_and_by_attach() {
    let live = Live::new("tfs-placed-key").await;
    let created = live.place(vec![fs_at("jobs/1f3a/root")]);
    let key = key_of(created.placement.as_str(), "jobs/1f3a/root")
        .expect("a key")
        .to_string();
    assert_eq!(live.core.lookup(ino::TREES, &key), Err(Errno::NoEnt));
    assert!(matches!(
        live.core.attach(&key, "run-1"),
        Err(AttachError::Digest)
    ));
}

/// Asserts the failures of a lookup: a name that is no location, below the root and below a
/// directory above a location, is `ENOENT`; so is a directory number no location is below.
#[tokio::test(flavor = "multi_thread")]
async fn a_name_that_is_no_location_is_not_found() {
    let live = Live::new("tfs-placed-absent").await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    live.on_core(|core| {
        assert_eq!(core.lookup(ino::ROOT, "nothing"), Err(Errno::NoEnt));
        let jobs = found(core, ino::ROOT, "jobs");
        assert_eq!(core.lookup(jobs.attr.ino, "other"), Err(Errno::NoEnt));
        let unknown = ino::placed_dir(12345);
        assert_eq!(core.lookup(unknown, "x"), Err(Errno::NoEnt));
        assert_eq!(core.parent(unknown), Err(Errno::NoEnt));
    })
    .await;
}

/// Asserts that a file whose bytes do not hash to its stated digest cannot be opened (`EIO`),
/// while the tree around it is found.
#[tokio::test(flavor = "multi_thread")]
async fn a_placed_file_that_cannot_be_filled_fails_its_open() {
    let live = Live::new("tfs-placed-badspan").await;
    let mut entry = live.bench.member("f", 0);
    if let NewKind::File { sha256, .. } = &mut entry.kind {
        *sha256 = TreeDigest::of(b"not the member");
    }
    let _ = live
        .bench
        .store
        .create(CreateRequest {
            built: built(vec![entry]),
            at: vec![fs_at("bad/tree")],
            ttl: None,
            ready: false,
        })
        .expect("a placement");
    live.on_core(|core| {
        let file = walk(core, &["bad", "tree", "f"]);
        assert_eq!(open_cached(core, file.attr.ino, false), Err(Errno::Io));
    })
    .await;
}

/// Asserts that, for a core that calls the cache in process, while the cache stays read-only
/// (another process holds the lock of its directory) for the whole of `fs.fetch_timeout`, a lookup
/// of a name that could be a location is `EIO`, while the names of the mount are `ENOENT`.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_while_the_cache_stays_read_only_is_an_io_error_except_for_names_of_the_mount() {
    let read_only = |bench: &Bench| {
        let _holder = bench.rig.locked();
        bench.rig.locked()
    };
    let live = Live::in_process_with("tfs-placed-readonly", read_only, SHORT).await;
    assert_eq!(live.cache.mode(), Mode::ReadOnly);
    live.on_core(|core| {
        assert_eq!(core.lookup(ino::ROOT, "jobs"), Err(Errno::Io));
        assert_eq!(core.lookup(ino::ROOT, "views"), Err(Errno::NoEnt));
        assert_eq!(core.lookup(ino::ROOT, "put"), Err(Errno::NoEnt));
    })
    .await;
}

/// Asserts that an open of a file of a placed tree, by a core that calls the cache in process,
/// fails with `EIO` when the cache lost the write role after the lookup and has not taken it back
/// within `fs.fetch_timeout`: the span of the pack is not filled by a read-only cache, and the
/// open is not left waiting. Once the cache takes the lock again, the same file opens.
#[tokio::test(flavor = "multi_thread")]
async fn a_fill_still_read_only_after_fetch_timeout_fails_with_eio() {
    let live = Live::in_process_with("tfs-placed-readonly-fill", |b| b.rig.locked(), SHORT).await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    let tool = live
        .on_core(|core| {
            walk(core, &["jobs", "1f3a", "root", "bin", "tool"])
                .attr
                .ino
        })
        .await;
    std::fs::remove_file(live.bench.rig.config.path.join(".lock")).unwrap();
    assert_eq!(live.cache.tick().unwrap(), Tick::Lost);
    let opened = live
        .on_core(move |core| open_cached(core, tool, false))
        .await;
    assert_eq!(opened, Err(Errno::Io));
    assert_eq!(live.cache.tick().unwrap(), Tick::Took);
    let opened = live
        .on_core(move |core| open_cached(core, tool, false))
        .await;
    assert!(opened.is_ok(), "{opened:?}");
}

/// Asserts that the first open of a file of a placed tree whose span the node holds (filled
/// before, as the whole pack of a job's output is on the node that wrote it) is answered at once,
/// on the reader that took it, by a core in the node's process: not parked in the queue of the
/// fetches, which every job of the node shares; it reads as its member of the pack, and the next
/// open of it is answered from the span kept with the tree. An open of a file whose span the
/// node does not hold is parked, since it waits for a fill.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_of_a_span_the_node_holds_is_answered_at_once_in_process() {
    let live = Live::new("tfs-placed-now").await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    let (tool, other) = live
        .on_core(|core| {
            let bin = walk(core, &["jobs", "1f3a", "root", "bin"]).attr.ino;
            let tool = found(core, bin, "tool").attr.ino;
            (tool, found(core, bin, "other").attr.ino)
        })
        .await;
    let parked = live.on_core(move |core| core.open_now(tool, false)).await;
    assert!(parked.is_none(), "{parked:?}");
    live.bench.fill(0).await;
    let read_now = move |core: &Core| {
        let Some(Ok(Opened::Cached(fh))) = core.open_now(tool, false) else {
            return None;
        };
        let bytes = core.read(fh, 0, 10_000).unwrap();
        core.release(fh);
        Some(bytes)
    };
    let (first, again) = live
        .on_core(move |core| (read_now(core), read_now(core)))
        .await;
    let want = Some(live.bench.pack.content(0).to_vec());
    assert_eq!(first, want);
    assert_eq!(again, want);
    let parked = live.on_core(move |core| core.open_now(other, false)).await;
    assert!(parked.is_none(), "{parked:?}");
}

/// Asserts that an open of a file whose span the node holds is parked, not failed, when the pack's
/// file the node names cannot be opened now (gone from the cache's directory): the open asks the
/// span on a thread of the fetches, which fills it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_span_whose_file_does_not_open_is_parked() {
    let live = Live::new("tfs-placed-now-gone").await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    let tool = live
        .on_core(|core| {
            walk(core, &["jobs", "1f3a", "root", "bin", "tool"])
                .attr
                .ino
        })
        .await;
    live.bench.fill(0).await;
    let (_, offset, size) = &live.bench.pack.blobs[0];
    let held = live
        .bench
        .store
        .held_span(&live.bench.pack.key, *offset, *size);
    std::fs::remove_file(held.expect("the span is held").path).unwrap();
    let parked = live.on_core(move |core| core.open_now(tool, false)).await;
    assert!(parked.is_none(), "{parked:?}");
}

/// Asserts that a core in process answers no open at once from a cache that does not hold the
/// write role, though the span is in the index: the index of a read-only cache is the holder's as
/// of its last read, so the open is parked and asks the span, which waits for the write role. Once
/// the cache takes the lock again, the same open is answered at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_cache_answers_no_open_at_once() {
    let live = Live::in_process_with("tfs-placed-now-ro", |b| b.rig.locked(), SHORT).await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    let tool = live
        .on_core(|core| {
            walk(core, &["jobs", "1f3a", "root", "bin", "tool"])
                .attr
                .ino
        })
        .await;
    live.bench.fill(0).await;
    std::fs::remove_file(live.bench.rig.config.path.join(".lock")).unwrap();
    assert_eq!(live.cache.tick().unwrap(), Tick::Lost);
    let parked = live.on_core(move |core| core.open_now(tool, false)).await;
    assert!(parked.is_none(), "{parked:?}");
    assert_eq!(live.cache.tick().unwrap(), Tick::Took);
    let opened = live.on_core(move |core| core.open_now(tool, false)).await;
    assert!(matches!(opened, Some(Ok(Opened::Cached(_)))), "{opened:?}");
}

/// Asserts that a lookup of a location whose tree is attached holds the placement again when its
/// hold was given back in between (an idle detach of another process's view, or an `unplace`
/// that came between the lookup's `place` and its attach), so that a tree the mount serves is
/// always held.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_of_an_attached_tree_holds_its_placement_again() {
    let live = Live::new("tfs-placed-rehold").await;
    let created = live.place(vec![fs_at("jobs/1f3a/root")]);
    let _ = live
        .on_core(|core| walk(core, &["jobs", "1f3a", "root"]))
        .await;
    assert_eq!(attachments(&live, &created), 1);
    assert!(live.bench.store.detach(&created.placement, MOUNT));
    assert_eq!(attachments(&live, &created), 0);
    let _ = live
        .on_core(|core| walk(core, &["jobs", "1f3a", "root"]))
        .await;
    assert_eq!(attachments(&live, &created), 1);
}

/// Asserts that a lookup of a location whose tree cannot be read gives back the hold its `place`
/// took: the tree's records are damaged, the lookup is an I/O error, and the placement is held by
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_that_fails_gives_the_hold_back() {
    let live = Live::new("tfs-placed-fail-hold").await;
    let created = live.place(vec![fs_at("jobs/1f3a/root")]);
    let records =
        crate::server::local_cache::trees::testing::tree_path(&live.bench.rig, &created.tree)
            .join("records");
    std::fs::write(&records, b"damaged").unwrap();
    let looked = live
        .on_core(|core| {
            let _ = walk(core, &["jobs", "1f3a"]);
            core.lookup(dir_ino("jobs/1f3a"), "root").err()
        })
        .await;
    assert_eq!(looked, Some(Errno::Io));
    assert_eq!(attachments(&live, &created), 0);
}

/// Asserts that a start's `tell_live` keeps the holds of the placements whose trees are attached
/// and ends the others of the mount: a process that takes over with nothing attached ends the hold
/// a process before it left, and one that has the tree attached keeps it.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_ends_the_holds_of_placements_it_does_not_have() {
    let live = Live::new("tfs-placed-live").await;
    let created = live.place(vec![fs_at("jobs/1f3a/root")]);
    let _ = live
        .on_core(|core| walk(core, &["jobs", "1f3a", "root"]))
        .await;
    live.on_core(|core| core.tell_live().unwrap()).await;
    assert_eq!(attachments(&live, &created), 1);
    let successor = live.successor();
    tokio::task::spawn_blocking(move || successor.tell_live().unwrap())
        .await
        .unwrap();
    assert_eq!(attachments(&live, &created), 0);
}
