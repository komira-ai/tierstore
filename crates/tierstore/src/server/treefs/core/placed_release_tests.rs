//! The mount after the mount provider's release of a source (`TreeStore::release_now`), end to end
//! through the core over the real tree store. The release ends the mount's hold on the placement
//! and takes the tree's directory, its image file included, away at once. A tree the mount has
//! in memory keeps serving its open files, its mapped list and its inodes. A new lookup of the
//! source's path is `ENOENT`, and so is a tree the mount has to read again once it has left memory.

use super::{Errno, Fetched, Unread, key_of};
use crate::server::local_cache::trees::testing::fs_at;
use crate::server::local_cache::trees::{Created, image_path};
use crate::server::treefs::core::{Core, Entry, Kind, Looked};
use crate::server::treefs::ino;
use crate::server::treefs::live_testing::Live;
use crate::server::treefs::testing::{TREE_IDLE, open_cached};

/// A live core whose tree store places the image of the bench's small tree at the fs location
/// `jobs`, as the mount provider places a source's tree.
async fn placed(name: &str) -> (Live, Created) {
    let live = Live::new(name).await;
    let created = live
        .bench
        .store
        .create_image(live.bench.image_request(vec![fs_at("jobs")]))
        .expect("an image placement");
    (live, created)
}

fn found(core: &Core, parent: u64, name: &str) -> Entry {
    match core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => entry,
        other => panic!("{name}: {other:?}"),
    }
}

/// The names of the listing of the directory `dir`.
fn names(core: &Core, dir: u64) -> Vec<String> {
    let mut names = Vec::new();
    core.readdir(dir, 0, &mut |_, item| {
        names.push(item.name);
        true
    })
    .unwrap();
    names
}

/// Releases the placement as the mount provider does, and asserts that it went at once though the
/// mount held it, and its tree's directory with it.
fn released_now(live: &Live, created: &Created) {
    let store = &live.bench.store;
    let held = store.placement(&created.placement).expect("a placement");
    assert_eq!(
        held.attachments, 1,
        "the mount's lookup holds the placement"
    );
    store.release_now(&created.placement).unwrap();
    assert!(store.placement(&created.placement).is_none());
    assert_eq!(store.sizes(), (0, 0));
    assert!(!image_path(&live.bench.rig.config.path, &created.tree).exists());
}

/// Asserts that after `release_now` of a placement the mount has looked up, mapped the list of
/// and opened a file of, the open file still reads to its end, the root and a directory still
/// answer `getattr` and `readdir` by inode and a name below the root is still looked up, while
/// the tree is in the mount's memory with its list mapped; and that a new lookup of the source's
/// path is `ENOENT`.
#[tokio::test(flavor = "multi_thread")]
async fn after_release_now_the_tree_in_memory_serves_and_a_new_lookup_is_enoent() {
    let (live, created) = placed("tfs-release-now-serve").await;
    let want = live.bench.pack.content(0).to_vec();
    let (root, bin, fh) = live
        .on_core(|core| {
            let root = found(core, ino::ROOT, "jobs");
            let bin = found(core, root.attr.ino, "bin");
            let tool = found(core, bin.attr.ino, "tool");
            let fh = open_cached(core, tool.attr.ino, false).unwrap();
            assert_eq!(core.lists.mapped.mapped().lists, 1);
            (root.attr.ino, bin.attr.ino, fh)
        })
        .await;
    released_now(&live, &created);
    live.on_core(move |core| {
        assert_eq!(core.read(fh, 0, 100_000).unwrap(), want);
        assert_eq!(core.read(fh, 900, 500).unwrap(), want[900..]);
        assert!(core.read(fh, 5000, 10).unwrap().is_empty());
        core.release(fh);
        assert_eq!(core.getattr(root).unwrap().attr.kind, Kind::Dir);
        assert_eq!(core.getattr(bin).unwrap().attr.kind, Kind::Dir);
        assert_eq!(names(core, bin), [".", "..", "other", "tool"]);
        assert_eq!(found(core, bin, "tool").attr.kind, Kind::File);
        assert_eq!(core.lists.mapped.mapped().lists, 1);
        assert_eq!(core.lookup(ino::ROOT, "jobs"), Err(Errno::NoEnt));
    })
    .await;
}

/// Asserts that a tree whose placement was released with `release_now` and that then leaves the
/// mount's memory at the sweep for idle trees, its list unmapped, is `ENOENT` when an inode of it
/// is named again: the read of the tree finds no placement, a tree that is gone, not one that
/// cannot be read now (`EIO`).
#[tokio::test(flavor = "multi_thread")]
async fn a_tree_that_leaves_memory_after_release_now_is_enoent_when_named_again() {
    let (live, created) = placed("tfs-release-now-idle").await;
    let (root, bin) = live
        .on_core(|core| {
            let root = found(core, ino::ROOT, "jobs");
            let bin = found(core, root.attr.ino, "bin");
            (root.attr.ino, bin.attr.ino)
        })
        .await;
    released_now(&live, &created);
    live.clock.advance(TREE_IDLE);
    let id = created.placement.as_str().to_owned();
    live.on_core(move |core| {
        assert_eq!(core.detach_idle(), 1);
        assert_eq!(core.lists.mapped.mapped().lists, 0);
        assert!(matches!(core.getattr(root), Err(Errno::NoEnt)));
        assert!(matches!(core.getattr(bin), Err(Errno::NoEnt)));
        let key = key_of(&id, "jobs").expect("a key");
        let read = core.read_tree(&key, &Fetched::default());
        assert!(matches!(read, Err(Unread::Gone(_))));
    })
    .await;
}
