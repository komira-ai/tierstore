//! A placement whose tree is an image, end to end through the core over the real tree store: the
//! tree store answers where a path is and what the tree of a placement is, the image is held in
//! the tree store's directory of the cache directory, and a file's bytes are a span of the pack
//! the cache fills. And a version 2 tree list named by its digest under `trees/`, which the
//! cache's fill puts in the cache as the object `cas/l/<digest>`: read as the list, or refused, and
//! never as a manifest.

use super::{Errno, Fetched, Unread, key_of};
use crate::TreeDigest;
use crate::server::local_cache::trees::testing::fs_at;
use crate::server::local_cache::trees::{Created, MountTree, image_path};
#[cfg(target_os = "linux")]
use crate::server::treefs::core::blob::descriptor_tests::descriptors_on;
use crate::server::treefs::core::{Core, Entry, Kind, Looked};
use crate::server::treefs::ino;
use crate::server::treefs::live_testing::{Live, walk};
use crate::server::treefs::testing::{Built, Logged, Rig, image_bytes, logged, open_cached, sha};
use crate::tree_manifest::{TreeError, flatten_in_waves};

/// A live core whose tree store places the image of the bench's small tree (`Bench::image_request`)
/// at the fs location `jobs`.
///
/// The tree: the root (index 0), then in index order `bin` (1), `empty` (2), `lib` (3), `link`
/// (4), `zero` (5), `bin/other` (6), `bin/tool` (7), `lib/deep` (8) and `lib/deep/data` (9).
async fn placed(name: &str) -> (Live, Created) {
    placed_at(name, &["jobs"]).await
}

/// [`placed`], at each of the fs locations `at`.
async fn placed_at(name: &str, at: &[&str]) -> (Live, Created) {
    let live = Live::new(name).await;
    let at = at.iter().map(|path| fs_at(path)).collect();
    let created = live
        .bench
        .store
        .create_image(live.bench.image_request(at))
        .expect("an image placement");
    (live, created)
}

fn found(core: &Core, parent: u64, name: &str) -> Entry {
    match core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => entry,
        other => panic!("{name}: {other:?}"),
    }
}

/// The names of the listing of `dir` from `offset`, with the offsets after each, stopping once the
/// sink has `most` of them.
fn listed(core: &Core, dir: u64, offset: u64, most: usize) -> Vec<(u64, String)> {
    let mut items = Vec::new();
    core.readdir(dir, offset, &mut |next, item| {
        items.push((next, item.name));
        items.len() < most
    })
    .unwrap();
    items
}

/// Asserts that the root of an image placement is found by its location with the stored mode of
/// the root and a link count of 2 and its subdirectories, numbered under the key of the placement
/// at index 0; that the tree store gives the mount the placement as an image, which the core reads
/// as a tree list (mapped once, the location kept) and holds for the mount.
#[tokio::test(flavor = "multi_thread")]
async fn an_image_placement_is_found_at_its_location_with_the_modes_as_stored() {
    let (live, created) = placed("tfs-img-root").await;
    let id = created.placement.as_str().to_owned();
    live.on_core(move |core| {
        let root = found(core, ino::ROOT, "jobs");
        assert_eq!(root.attr.kind, Kind::Dir);
        assert_eq!((root.attr.perm, root.attr.nlink), (0o755, 5));
        let (number, index) = ino::split(root.attr.ino);
        assert_eq!(index, 0);
        assert_eq!(core.table.find(&key_of(&id, "jobs").unwrap()), Some(number));
        let loaded = core.tree(number).unwrap();
        assert_eq!(loaded.tree.len(), 10);
        assert_eq!(loaded.tree.location(), Some("jobs"));
        assert!(loaded.tree.list().is_some());
        assert_eq!(core.lists.mapped.mapped().lists, 1);
    })
    .await;
    assert_eq!(
        live.bench.store.mount_tree(&created.placement),
        Ok(MountTree::Image {
            tree: created.tree,
            paths: vec!["jobs".to_owned()]
        })
    );
    let held = live.bench.store.placement(&created.placement).unwrap();
    assert_eq!(held.attachments, 1);
}

/// Asserts that the entries of the image are looked up by name with their kind, size, stored mode
/// and link count, their numbers being their index under the placed tree's number, and that a
/// missing name is absent and a name below a file is not a directory.
#[tokio::test(flavor = "multi_thread")]
async fn entries_of_an_image_placement_are_looked_up_with_their_index_as_inode() {
    let (live, _) = placed("tfs-img-lookup").await;
    live.on_core(|core| {
        let root = found(core, ino::ROOT, "jobs");
        let (number, _) = ino::split(root.attr.ino);
        let at = |index: u32| ino::of(number, index);
        let bin = found(core, root.attr.ino, "bin");
        let empty = found(core, root.attr.ino, "empty");
        let lib = found(core, root.attr.ino, "lib");
        let link = found(core, root.attr.ino, "link");
        let zero = found(core, root.attr.ino, "zero");
        let tool = found(core, bin.attr.ino, "tool");
        let other = found(core, bin.attr.ino, "other");
        let deep = found(core, lib.attr.ino, "deep");
        let data = found(core, deep.attr.ino, "data");
        assert_eq!(
            [bin, empty, lib, link, zero].map(|entry| entry.attr.ino),
            [at(1), at(2), at(3), at(4), at(5)]
        );
        assert_eq!([other.attr.ino, tool.attr.ino], [at(6), at(7)]);
        assert_eq!([deep.attr.ino, data.attr.ino], [at(8), at(9)]);
        assert_eq!(
            (bin.attr.kind, bin.attr.perm, bin.attr.nlink),
            (Kind::Dir, 0o755, 2)
        );
        assert_eq!((lib.attr.perm, lib.attr.nlink), (0o755, 3));
        assert_eq!((empty.attr.perm, empty.attr.nlink), (0o755, 2));
        assert_eq!(
            (tool.attr.kind, tool.attr.size, tool.attr.perm),
            (Kind::File, 1000, 0o4755)
        );
        assert_eq!(
            (other.attr.size, other.attr.perm, other.attr.nlink),
            (2000, 0o644, 1)
        );
        assert_eq!((data.attr.size, data.attr.perm), (3000, 0o644));
        assert_eq!((zero.attr.size, zero.attr.perm), (0, 0o644));
        assert_eq!(
            (link.attr.kind, link.attr.size, link.attr.perm),
            (Kind::Symlink, 8, 0o777)
        );
        assert_eq!(core.getattr(tool.attr.ino).unwrap().attr, tool.attr);
        assert_eq!(core.getattr(at(10)).err(), Some(Errno::NoEnt));
        assert!(matches!(
            core.lookup(bin.attr.ino, "nope"),
            Ok(Looked::Absent(_))
        ));
        assert!(matches!(
            core.lookup(empty.attr.ino, "nope"),
            Ok(Looked::Absent(_))
        ));
        assert_eq!(core.lookup(tool.attr.ino, "x").err(), Some(Errno::NotDir));
    })
    .await;
}

/// Asserts that a link of an image reads as its target and that a file and a directory do not.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_of_an_image_placement_reads_as_its_target() {
    let (live, _) = placed("tfs-img-link").await;
    live.on_core(|core| {
        let root = found(core, ino::ROOT, "jobs");
        let link = found(core, root.attr.ino, "link");
        assert_eq!(&*core.readlink(link.attr.ino).unwrap(), "bin/tool");
        let bin = found(core, root.attr.ino, "bin");
        assert_eq!(core.readlink(bin.attr.ino), Err(Errno::Inval));
        let zero = found(core, root.attr.ino, "zero");
        assert_eq!(core.readlink(zero.attr.ino), Err(Errno::Inval));
    })
    .await;
}

/// Asserts that a listing of an image directory has `.` and `..` then the children in name order
/// with the offset after each, resumes from an offset (through the dots and among the children),
/// is empty past the end and at an offset of `u64::MAX`, stops where the sink says, and that the
/// `..` of the root is the mount's root and of a subdirectory its parent.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_of_an_image_directory_has_the_dots_then_the_children_and_resumes() {
    let (live, _) = placed("tfs-img-readdir").await;
    live.on_core(|core| {
        let root = found(core, ino::ROOT, "jobs");
        let (number, _) = ino::split(root.attr.ino);
        let dir = root.attr.ino;
        let names =
            |items: Vec<(u64, String)>| items.into_iter().map(|(_, n)| n).collect::<Vec<_>>();
        let all = listed(core, dir, 0, 99);
        assert_eq!(
            names(all.clone()),
            [".", "..", "bin", "empty", "lib", "link", "zero"]
        );
        assert_eq!(
            all.iter().map(|(next, _)| *next).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(
            names(listed(core, dir, 1, 99)),
            ["..", "bin", "empty", "lib", "link", "zero"]
        );
        assert_eq!(names(listed(core, dir, 4, 99)), ["lib", "link", "zero"]);
        assert!(listed(core, dir, 7, 99).is_empty());
        assert!(listed(core, dir, u64::MAX, 99).is_empty());
        assert_eq!(names(listed(core, dir, 0, 1)), ["."]);
        assert_eq!(names(listed(core, dir, 0, 2)), [".", ".."]);
        assert_eq!(names(listed(core, dir, 0, 4)), [".", "..", "bin", "empty"]);
        assert_eq!(names(listed(core, dir, 3, 1)), ["empty"]);
        let bin = ino::of(number, 1);
        let mut dots = Vec::new();
        core.readdir(bin, 0, &mut |_, item| {
            dots.push((item.name, item.entry.attr.ino));
            true
        })
        .unwrap();
        assert_eq!(dots[0], (".".to_owned(), bin));
        assert_eq!(dots[1], ("..".to_owned(), dir));
        assert_eq!(names(listed(core, bin, 2, 99)), ["other", "tool"]);
        assert_eq!(
            core.readdir(ino::of(number, 7), 0, &mut |_, _| true),
            Err(Errno::NotDir)
        );
        let mut up = 0;
        core.readdir(dir, 1, &mut |_, item| {
            up = item.entry.attr.ino;
            false
        })
        .unwrap();
        assert_eq!(up, ino::ROOT);
        assert_eq!(core.parent(bin).unwrap(), dir);
    })
    .await;
}

/// Asserts that a listing after an inode gives the children above it, in order, with no dots;
/// from an inode that is not a child, from the last child, and from nothing; and stops where the
/// sink says.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_after_an_inode_of_an_image_directory_gives_the_children_above_it() {
    let (live, _) = placed("tfs-img-after").await;
    live.on_core(|core| {
        let root = found(core, ino::ROOT, "jobs");
        let (number, _) = ino::split(root.attr.ino);
        let after = |dir: u64, after: u64, most: usize| {
            let mut names = Vec::new();
            core.readdir_after(dir, after, &mut |item| {
                names.push(item.name);
                names.len() < most
            })
            .unwrap();
            names
        };
        let dir = root.attr.ino;
        assert_eq!(after(dir, 0, 99), ["bin", "empty", "lib", "link", "zero"]);
        assert_eq!(after(dir, ino::of(number, 2), 99), ["lib", "link", "zero"]);
        assert_eq!(after(dir, ino::of(number, 4), 99), ["zero"]);
        assert_eq!(after(dir, ino::of(number, 5), 99), Vec::<String>::new());
        assert_eq!(after(dir, u64::MAX, 99), Vec::<String>::new());
        assert_eq!(after(dir, 0, 2), ["bin", "empty"]);
        assert_eq!(after(ino::of(number, 1), 0, 99), ["other", "tool"]);
    })
    .await;
}

/// Asserts that a file of an image is read as the span of the pack the cache fills: the bytes are
/// the member's own, a read in the middle and past the end are clipped, a file with no source
/// reads empty, the open is a cached one, and the cache holds the span once the file was opened
/// and did not before.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_of_an_image_placement_reads_as_its_span() {
    let (live, _) = placed("tfs-img-read").await;
    let want = live.bench.pack.content(0).to_vec();
    let (_, offset, size) = live.bench.pack.blobs[0].clone();
    let pack = live.bench.pack.key.clone();
    assert!(live.bench.store.held_span(&pack, offset, size).is_none());
    live.on_core(move |core| {
        let root = found(core, ino::ROOT, "jobs");
        let bin = found(core, root.attr.ino, "bin");
        let tool = found(core, bin.attr.ino, "tool");
        let fh = open_cached(core, tool.attr.ino, false).unwrap();
        assert_eq!(core.read(fh, 0, 100_000).unwrap(), want);
        assert_eq!(core.read(fh, 1, 3).unwrap(), want[1..4]);
        assert_eq!(core.read(fh, 900, 500).unwrap(), want[900..]);
        assert!(core.read(fh, 1000, 10).unwrap().is_empty());
        assert!(core.read(fh, 5000, 10).unwrap().is_empty());
        core.release(fh);
        let zero = found(core, root.attr.ino, "zero");
        let fh = open_cached(core, zero.attr.ino, false).unwrap();
        assert!(core.read(fh, 0, 10).unwrap().is_empty());
        core.release(fh);
    })
    .await;
    assert!(live.bench.store.held_span(&pack, offset, size).is_some());
}

/// Asserts that the open files of one pack share one descriptor of it, whichever of the pack's
/// files they are, and that it is closed with the last of them; and that a pack file the cache
/// holds a span in that is not there is tried three times in all, said so, and then fails the open
/// with `EIO`.
#[tokio::test(flavor = "multi_thread")]
async fn the_files_of_a_pack_share_its_descriptor_and_a_pack_file_that_is_gone_is_an_io_error() {
    let (live, _) = placed("tfs-img-shared").await;
    let (tool, other) = live
        .on_core(|core| {
            let root = found(core, ino::ROOT, "jobs");
            let bin = found(core, root.attr.ino, "bin");
            let tool = found(core, bin.attr.ino, "tool").attr.ino;
            (tool, found(core, bin.attr.ino, "other").attr.ino)
        })
        .await;
    live.on_core(move |core| {
        let first = open_cached(core, tool, false).unwrap();
        let second = open_cached(core, other, false).unwrap();
        let third = open_cached(core, tool, false).unwrap();
        assert_eq!(core.open_descriptors(), 1);
        core.release(first);
        core.release(second);
        assert_eq!(core.open_descriptors(), 1);
        core.release(third);
        assert_eq!(core.open_descriptors(), 0);
    })
    .await;
    let (_, offset, size) = live.bench.pack.blobs[0].clone();
    let held = live
        .bench
        .store
        .held_span(&live.bench.pack.key, offset, size)
        .expect("the span is held");
    std::fs::remove_file(&held.path).unwrap();
    let log = Logged::default();
    let said = log.clone();
    let opened = live
        .on_core(move |core| {
            let _logging = logged(&said, tracing::Level::WARN);
            open_cached(core, tool, false)
        })
        .await;
    assert_eq!(opened, Err(Errno::Io));
    let said = log.text();
    assert!(said.contains("(after 3 fills)"), "{said}");
    assert!(said.contains(": io: "), "{said}");
}

/// Asserts that three thousand handles of the two files of one pack, all open at once, hold one
/// real descriptor of the pack file (counted in `/proc/self/fd`), read their own file's bytes, and
/// leave none once the last is released.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn three_thousand_handles_of_the_files_of_a_pack_hold_one_real_descriptor() {
    let (live, _) = placed("tfs-img-fds").await;
    live.bench.fill_all().await;
    let (_, offset, size) = live.bench.pack.blobs[0].clone();
    let path = live
        .bench
        .store
        .held_span(&live.bench.pack.key, offset, size)
        .expect("the span is held")
        .path;
    let (want_tool, want_other) = (
        live.bench.pack.content(0).to_vec(),
        live.bench.pack.content(1).to_vec(),
    );
    live.on_core(move |core| {
        let root = found(core, ino::ROOT, "jobs");
        let bin = found(core, root.attr.ino, "bin");
        let tool = found(core, bin.attr.ino, "tool").attr.ino;
        let other = found(core, bin.attr.ino, "other").attr.ino;
        let before = descriptors_on(&path);
        let handles: Vec<u64> = (0..3000)
            .map(|i| open_cached(core, [tool, other][i % 2], false).unwrap())
            .collect();
        assert_eq!(descriptors_on(&path), before + 1);
        assert_eq!(core.read(handles[0], 0, 100_000).unwrap(), want_tool);
        assert_eq!(core.read(handles[2999], 0, 100_000).unwrap(), want_other);
        for fh in handles {
            core.release(fh);
        }
        assert_eq!(descriptors_on(&path), before);
    })
    .await;
}

/// Asserts that an image file that is not in the tree's directory is a tree that is gone (its
/// placement ended between the tree store's answer and the read: `ENOENT`, not `EIO`), that one
/// that is not a tree list, one that is cut short and one a byte over the largest tree list are
/// each refused when the core reads the image from the file (`Core::image_of`, the read of a list
/// that no holder has in memory), as a failed read of the tree, and that the read succeeds once
/// the file is the image again.
#[test]
fn a_missing_image_file_is_gone_and_a_malformed_one_fails_the_read_of_the_tree() {
    let rig = Rig::new("tfs-img-bad", Built::of(Vec::new()));
    let bytes = image_bytes();
    let tree = TreeDigest::of(&bytes);
    let file = image_path(&rig.cache, &tree);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let read = || rig.core.image_of(&tree, "jobs".to_owned());
    let Err(Unread::Gone(why)) = read() else {
        panic!("a missing image file is a tree that is gone");
    };
    assert!(
        why.starts_with(&format!("the image of {tree} at jobs: ")),
        "{why}"
    );
    std::fs::write(&file, b"not a tree list at all").unwrap();
    assert!(matches!(read(), Err(Unread::Failed(_))));
    std::fs::write(&file, &bytes[..bytes.len() / 2]).unwrap();
    assert!(matches!(read(), Err(Unread::Failed(_))));
    // One byte over the largest list: refused by its size (a sparse file, read no further).
    let over = std::fs::File::create(&file).unwrap();
    over.set_len(crate::MAX_TREE_LIST_BYTES as u64 + 1).unwrap();
    drop(over);
    assert!(matches!(read(), Err(Unread::Failed(_))));
    std::fs::write(&file, &bytes).unwrap();
    assert!(read().is_ok());
}

/// Asserts that a placement with two fs locations is found at each, as two trees; that the tree
/// of a key at a path that is not one of the placement's locations is gone, and so is that of a
/// placement the tree store no longer has.
#[tokio::test(flavor = "multi_thread")]
async fn an_image_placement_is_found_at_each_of_its_locations_and_nowhere_else() {
    let (live, created) = placed_at("tfs-img-paths", &["jobs", "jobs2"]).await;
    let other = live
        .bench
        .store
        .create_image(live.bench.image_request(vec![fs_at("elsewhere")]))
        .expect("an image placement");
    live.bench.store.release(&other.placement).unwrap();
    let (id, ended) = (
        created.placement.as_str().to_owned(),
        other.placement.as_str().to_owned(),
    );
    live.on_core(move |core| {
        let first = found(core, ino::ROOT, "jobs");
        let second = found(core, ino::ROOT, "jobs2");
        assert_ne!(ino::split(first.attr.ino).0, ino::split(second.attr.ino).0);
        let read =
            |id: &str, path: &str| core.read_tree(&key_of(id, path).unwrap(), &Fetched::default());
        assert!(read(&id, "jobs").is_ok());
        assert!(read(&id, "jobs2").is_ok());
        assert!(matches!(read(&id, "jobs3"), Err(Unread::Gone(_))));
        assert!(matches!(read(&ended, "elsewhere"), Err(Unread::Gone(_))));
        assert_eq!(
            core.lookup(ino::ROOT, "elsewhere").err(),
            Some(Errno::NoEnt)
        );
    })
    .await;
}

/// Asserts that the mount tells the cache `started <path>` for each placed tree it first looks
/// into, which the cache counts as a sample of `tierstore_request_to_start_seconds` for the run of
/// the path: not for a lookup of a tree's root, its attributes or an open of it for listing (what
/// the agent and the overlay's mount do), but for the first lookup in a directory of the tree, and
/// a first listing counts as a first lookup does. Each of the two trees is of a run of its own, so
/// that each report is a sample; the reports reach the cache at once, in the caller's thread.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_look_into_each_placed_tree_is_reported_and_nothing_else_is() {
    const FIRST: &str = "mount-provider/61/aa";
    const SECOND: &str = "mount-provider/61/bb";
    let live = Live::new("tfs-img-started").await;
    let sampled = || {
        let metrics = live.cache.metrics();
        let count = metrics.lines().find_map(|line| {
            line.strip_prefix("tierstore_request_to_start_seconds_count ")
                .map(str::to_owned)
        });
        count.expect("the count of samples").parse::<u64>().unwrap()
    };
    live.cache.run_requested(FIRST);
    live.cache.run_requested(SECOND);
    let at = vec![
        fs_at("mount-provider/61/aa/0"),
        fs_at("mount-provider/61/bb/0"),
    ];
    let _ = live
        .bench
        .store
        .create_image(live.bench.image_request(at))
        .expect("an image placement");
    let (first, second) = live
        .on_core(|core| {
            let first = walk(core, &["mount-provider", "61", "aa", "0"]);
            let second = walk(core, &["mount-provider", "61", "bb", "0"]);
            for root in [first, second] {
                core.getattr(root).unwrap();
                core.opendir(root).unwrap();
            }
            (first, second)
        })
        .await;
    assert_eq!(sampled(), 0);
    live.on_core(move |core| {
        let bin = found(core, first, "bin").attr.ino;
        let _ = found(core, bin, "tool");
        let _ = listed(core, first, 0, usize::MAX);
    })
    .await;
    assert_eq!(sampled(), 1);
    live.on_core(move |core| {
        let _ = listed(core, second, 0, usize::MAX);
        let _ = found(core, second, "lib");
    })
    .await;
    assert_eq!(sampled(), 2);
}

/// Puts `bytes` in the rig's upstream as the object `cas/l/<their digest>`, where a manifest would
/// be; their digest.
fn object(rig: &Rig, bytes: &[u8]) -> String {
    let digest = sha(bytes);
    let mut built = rig.upstream.built.lock().unwrap();
    let _ = built.manifests.insert(digest.clone(), bytes.to_vec());
    digest
}

/// The fills of whole objects (`fill <sha256>`) the rig's upstream was asked, in order.
fn whole_fills(rig: &Rig) -> Vec<String> {
    let fills = rig.upstream.asked("fill");
    fills
        .into_iter()
        .filter(|line| !line.contains(" from "))
        .collect()
}

/// Asserts that a tree list is attached from the one whole fill of it: the cache's fill of the tree
/// stops at the list, the upstream is asked `fill <digest>` once and nothing else, and the tree is
/// the list, with no location, mapped once.
#[test]
fn a_tree_list_is_attached_from_the_one_fill_of_it() {
    let rig = Rig::new("tfs-list-one-fill", Built::of(Vec::new()));
    let list = object(&rig, &image_bytes());
    let Ok(Looked::Found(root)) = rig.core.lookup(ino::TREES, &list) else {
        panic!("the list's root");
    };
    assert_eq!(whole_fills(&rig), [format!("fill {list}")]);
    let (number, index) = ino::split(root.attr.ino);
    assert_eq!(index, 0);
    let loaded = rig.core.tree(number).unwrap();
    assert_eq!((loaded.tree.len(), loaded.tree.location()), (7, None));
    assert!(loaded.tree.list().is_some());
    assert_eq!(rig.core.lists.mapped.mapped().lists, 1);
}

/// Asserts that a tree list that fails its shape walk is `EIO` and takes no tree number: an object
/// that begins with the list's magic and hashes to its digest but is cut short, asked twice, is
/// `EIO` both times, refused as a tree list (not as a manifest) in the words said, with no number
/// in the table and nothing mapped; and that an object of the same digest form that is not a list
/// at all is refused as a malformed manifest, `EIO` too, as before.
#[test]
fn a_tree_list_that_fails_its_shape_walk_is_eio_and_takes_no_number() {
    let rig = Rig::new("tfs-list-bad-shape", Built::of(Vec::new()));
    let bytes = image_bytes();
    let short = &bytes[..bytes.len() / 2];
    assert!(crate::is_tree_list_v2(short));
    let cut = object(&rig, short);
    let log = Logged::default();
    {
        let _logging = logged(&log, tracing::Level::WARN);
        assert_eq!(rig.core.lookup(ino::TREES, &cut).err(), Some(Errno::Io));
    }
    let said = log.text();
    let walked = format!("tierstore fs: cannot attach {cut} on a lookup: the tree list {cut}: ");
    assert!(said.contains(&walked), "{said}");
    assert_eq!(rig.core.lookup(ino::TREES, &cut).err(), Some(Errno::Io));
    let digest = TreeDigest::from_hex(&cut).unwrap();
    assert_eq!(rig.core.table.find(&digest), None);
    assert_eq!(rig.core.lists.mapped.mapped().lists, 0);
    // Its own rig: the core says a refusal of the same kind at most so often, and this one must
    // be said.
    let rig = Rig::new("tfs-list-bad-shape-neither", Built::of(Vec::new()));
    let neither = object(&rig, b"neither a manifest nor a tree list");
    let log = Logged::default();
    {
        let _logging = logged(&log, tracing::Level::WARN);
        assert_eq!(rig.core.lookup(ino::TREES, &neither).err(), Some(Errno::Io));
    }
    let said = log.text();
    assert!(said.contains("malformed manifest"), "{said}");
    assert!(!said.contains("the tree list"), "{said}");
    let digest = TreeDigest::from_hex(&neither).unwrap();
    assert_eq!(rig.core.table.find(&digest), None);
}

/// Asserts that a tree list given to the manifest reader is refused, not misread (B8, the reader of
/// a release before this one on the new name form): the reader every release before this one runs
/// for a root, `flatten_in_waves` over `parse_manifest`, refuses the list's bytes as a malformed
/// manifest, so an older mount answers the lookup `EIO`; and this release, which still runs that
/// reader below a root, refuses a manifest tree whose directory names a list: `EIO`, with no
/// tree number and nothing mapped.
#[test]
fn a_tree_list_given_to_the_manifest_reader_is_refused_not_misread() {
    let bytes = image_bytes();
    let list = TreeDigest::of(&bytes);
    let fetch = |_: &TreeDigest, _: usize| -> Result<Vec<u8>, String> { Ok(bytes.clone()) };
    let read = flatten_in_waves(&list, 16, fetch);
    assert!(matches!(read, Err(TreeError::Malformed(_))), "{read:?}");

    let rig = Rig::new("tfs-list-below", Built::of(Vec::new()));
    let below = object(&rig, &bytes);
    let manifest = serde_json::json!({"v": 1, "children": [
        {"name": "sub", "kind": "dir", "sha256": below, "size": bytes.len()},
    ]});
    let root = object(&rig, &serde_json::to_vec(&manifest).unwrap());
    assert_eq!(rig.core.lookup(ino::TREES, &root).err(), Some(Errno::Io));
    let digest = TreeDigest::from_hex(&root).unwrap();
    assert_eq!(rig.core.table.find(&digest), None);
    assert_eq!(rig.core.lists.mapped.mapped().lists, 0);
    assert_eq!(
        whole_fills(&rig),
        [format!("fill {root}"), format!("fill {below}")]
    );
}
