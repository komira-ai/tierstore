//! A placement whose tree is an image, end to end through the core: the fake fill socket answers
//! `place` and `image`, the image file is in the tree's directory of the rig's cache directory, and
//! a file's bytes are a span the fake answers.

use super::{Errno, key_of};
use crate::TreeDigest;
use crate::server::local_cache::trees::image_path;
use crate::server::treefs::core::{Entry, Kind, Looked};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Built, Rig, image_bytes, open_cached};

const ID: &str = "0123456789abcdef0123456789abcdef";

/// A rig whose node places the image of [`image_bytes`] at `jobs`, with the image file in the
/// cache directory; the digest of the tree.
fn placed(name: &str) -> (Rig, TreeDigest) {
    let rig = Rig::new(name, Built::of(Vec::new()));
    let bytes = image_bytes();
    let tree = TreeDigest::of(&bytes);
    let file = image_path(&rig.cache, &tree);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, &bytes).unwrap();
    answer(&rig, "place", &format!("tree {ID}"));
    answer(&rig, "image", &format!(r#"image {tree} ["jobs"]"#));
    (rig, tree)
}

/// Answers the requests of `verb` of the fake fill socket of `rig` with `answer`.
fn answer(rig: &Rig, verb: &str, answer: &str) {
    let _ = crate::server::treefs::locked(&rig.upstream.answers)
        .insert(verb.to_owned(), answer.to_owned());
}

fn found(rig: &Rig, parent: u64, name: &str) -> Entry {
    match rig.core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => entry,
        other => panic!("{name}: {other:?}"),
    }
}

/// The names of the listing of `dir` from `offset`, with the offsets after each, stopping once the
/// sink has `most` of them.
fn listed(rig: &Rig, dir: u64, offset: u64, most: usize) -> Vec<(u64, String)> {
    let mut items = Vec::new();
    rig.core
        .readdir(dir, offset, &mut |next, item| {
            items.push((next, item.name));
            items.len() < most
        })
        .unwrap();
    items
}

/// Asserts that the root of an image placement is found by its location with the stored mode of
/// the root and a link count of 2 and its subdirectories, numbered under the key of the placement
/// at index 0, and that the node was asked `image` with the placement id.
#[test]
fn an_image_placement_is_found_at_its_location_with_the_modes_as_stored() {
    let (rig, _) = placed("tfs-img-root");
    let root = found(&rig, ino::ROOT, "jobs");
    assert_eq!(root.attr.kind, Kind::Dir);
    assert_eq!((root.attr.perm, root.attr.nlink), (0o755, 4));
    let (number, index) = ino::split(root.attr.ino);
    assert_eq!(index, 0);
    assert_eq!(
        rig.core.table.find(&key_of(ID, "jobs").unwrap()),
        Some(number)
    );
    assert_eq!(rig.upstream.asked("image"), [format!("image {ID}")]);
    assert!(rig.upstream.asked("flat").is_empty());
}

/// Asserts that the entries of the image are looked up by name with their kind, size, stored mode
/// and link count, their numbers being their index under the placed tree's number, and that a
/// missing name is absent and a name below a file is not a directory.
#[test]
fn entries_of_an_image_placement_are_looked_up_with_their_index_as_inode() {
    let (rig, _) = placed("tfs-img-lookup");
    let root = found(&rig, ino::ROOT, "jobs");
    let (number, _) = ino::split(root.attr.ino);
    let at = |index: u32| ino::of(number, index);
    let bin = found(&rig, root.attr.ino, "bin");
    let d = found(&rig, root.attr.ino, "d");
    let empty = found(&rig, root.attr.ino, "empty");
    let link = found(&rig, root.attr.ino, "link");
    let tool = found(&rig, bin.attr.ino, "tool");
    let other = found(&rig, bin.attr.ino, "other");
    assert_eq!(
        [bin.attr.ino, d.attr.ino, empty.attr.ino, link.attr.ino],
        [at(1), at(2), at(3), at(4)]
    );
    assert_eq!([other.attr.ino, tool.attr.ino], [at(5), at(6)]);
    assert_eq!(
        (bin.attr.kind, bin.attr.perm, bin.attr.nlink),
        (Kind::Dir, 0o755, 2)
    );
    assert_eq!((d.attr.perm, d.attr.nlink), (0o700, 2));
    assert_eq!(
        (tool.attr.kind, tool.attr.size, tool.attr.perm),
        (Kind::File, 5, 0o755)
    );
    assert_eq!(
        (other.attr.size, other.attr.perm, other.attr.nlink),
        (3, 0o644, 1)
    );
    assert_eq!((empty.attr.size, empty.attr.perm), (0, 0o644));
    assert_eq!(
        (link.attr.kind, link.attr.size, link.attr.perm),
        (Kind::Symlink, 8, 0o777)
    );
    assert_eq!(rig.core.getattr(tool.attr.ino).unwrap().attr, tool.attr);
    assert_eq!(rig.core.getattr(at(7)).err(), Some(Errno::NoEnt));
    assert!(matches!(
        rig.core.lookup(bin.attr.ino, "nope"),
        Ok(Looked::Absent(_))
    ));
    assert!(matches!(
        rig.core.lookup(d.attr.ino, "nope"),
        Ok(Looked::Absent(_))
    ));
    assert_eq!(
        rig.core.lookup(tool.attr.ino, "x").err(),
        Some(Errno::NotDir)
    );
}

/// Asserts that a link of an image reads as its target and that a file and a directory do not.
#[test]
fn a_link_of_an_image_placement_reads_as_its_target() {
    let (rig, _) = placed("tfs-img-link");
    let root = found(&rig, ino::ROOT, "jobs");
    let link = found(&rig, root.attr.ino, "link");
    assert_eq!(&*rig.core.readlink(link.attr.ino).unwrap(), "bin/tool");
    let bin = found(&rig, root.attr.ino, "bin");
    assert_eq!(rig.core.readlink(bin.attr.ino), Err(Errno::Inval));
    let empty = found(&rig, root.attr.ino, "empty");
    assert_eq!(rig.core.readlink(empty.attr.ino), Err(Errno::Inval));
}

/// Asserts that a listing of an image directory has `.` and `..` then the children in name order
/// with the offset after each, resumes from an offset (through the dots and among the children),
/// is empty past the end and at an offset of `u64::MAX`, stops where the sink says, and that the
/// `..` of the root is the mount's root and of a subdirectory its parent.
#[test]
fn a_listing_of_an_image_directory_has_the_dots_then_the_children_and_resumes() {
    let (rig, _) = placed("tfs-img-readdir");
    let root = found(&rig, ino::ROOT, "jobs");
    let (number, _) = ino::split(root.attr.ino);
    let names = |items: Vec<(u64, String)>| items.into_iter().map(|(_, n)| n).collect::<Vec<_>>();
    let all = listed(&rig, root.attr.ino, 0, 99);
    assert_eq!(names(all.clone()), [".", "..", "bin", "d", "empty", "link"]);
    assert_eq!(
        all.iter().map(|(next, _)| *next).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        names(listed(&rig, root.attr.ino, 1, 99)),
        ["..", "bin", "d", "empty", "link"]
    );
    assert_eq!(names(listed(&rig, root.attr.ino, 4, 99)), ["empty", "link"]);
    assert!(listed(&rig, root.attr.ino, 6, 99).is_empty());
    assert!(listed(&rig, root.attr.ino, u64::MAX, 99).is_empty());
    assert_eq!(names(listed(&rig, root.attr.ino, 0, 1)), ["."]);
    assert_eq!(names(listed(&rig, root.attr.ino, 0, 2)), [".", ".."]);
    assert_eq!(
        names(listed(&rig, root.attr.ino, 0, 4)),
        [".", "..", "bin", "d"]
    );
    assert_eq!(names(listed(&rig, root.attr.ino, 3, 1)), ["d"]);
    let bin = ino::of(number, 1);
    let mut dots = Vec::new();
    rig.core
        .readdir(bin, 0, &mut |_, item| {
            dots.push((item.name, item.entry.attr.ino));
            true
        })
        .unwrap();
    assert_eq!(dots[0], (".".to_owned(), bin));
    assert_eq!(dots[1], ("..".to_owned(), root.attr.ino));
    assert_eq!(names(listed(&rig, bin, 2, 99)), ["other", "tool"]);
    assert_eq!(
        rig.core.readdir(ino::of(number, 6), 0, &mut |_, _| true),
        Err(Errno::NotDir)
    );
    let mut up = 0;
    rig.core
        .readdir(root.attr.ino, 1, &mut |_, item| {
            up = item.entry.attr.ino;
            false
        })
        .unwrap();
    assert_eq!(up, ino::ROOT);
    assert_eq!(rig.core.parent(bin).unwrap(), root.attr.ino);
}

/// Asserts that a listing after an inode gives the children above it, in order, with no dots;
/// from an inode that is not a child, from the last child, and from nothing; and stops where the
/// sink says.
#[test]
fn a_listing_after_an_inode_of_an_image_directory_gives_the_children_above_it() {
    let (rig, _) = placed("tfs-img-after");
    let root = found(&rig, ino::ROOT, "jobs");
    let (number, _) = ino::split(root.attr.ino);
    let after = |dir: u64, after: u64, most: usize| {
        let mut names = Vec::new();
        rig.core
            .readdir_after(dir, after, &mut |item| {
                names.push(item.name);
                names.len() < most
            })
            .unwrap();
        names
    };
    let dir = root.attr.ino;
    assert_eq!(after(dir, 0, 99), ["bin", "d", "empty", "link"]);
    assert_eq!(after(dir, ino::of(number, 2), 99), ["empty", "link"]);
    assert_eq!(after(dir, ino::of(number, 4), 99), Vec::<String>::new());
    assert_eq!(after(dir, u64::MAX, 99), Vec::<String>::new());
    assert_eq!(after(dir, 0, 2), ["bin", "d"]);
    assert_eq!(after(ino::of(number, 1), 0, 99), ["other", "tool"]);
}

/// Asserts that a file of an image is read as the span the node answers: the request names the
/// digest, size, pack key and offset of the record, the bytes are the file's own size from the
/// base the node gave, a read in the middle and past the end are clipped, a file with no source
/// reads empty, and the open is a cached one.
#[test]
fn a_file_of_an_image_placement_reads_as_its_span() {
    let (rig, _) = placed("tfs-img-read");
    let pack = rig.dir.path().join("pack");
    std::fs::write(&pack, b"XXhelloabc").unwrap();
    answer(&rig, "span", &format!("ok 2 {}", pack.display()));
    let root = found(&rig, ino::ROOT, "jobs");
    let bin = found(&rig, root.attr.ino, "bin");
    let tool = found(&rig, bin.attr.ino, "tool");
    let fh = open_cached(&rig.core, tool.attr.ino, false).unwrap();
    assert_eq!(rig.core.read(fh, 0, 100).unwrap(), b"hello");
    assert_eq!(rig.core.read(fh, 1, 3).unwrap(), b"ell");
    assert!(rig.core.read(fh, 5, 10).unwrap().is_empty());
    rig.core.release(fh);
    assert_eq!(
        rig.upstream.asked("span"),
        [format!("span {} 5 from p0 @ 0", TreeDigest::of(b"hello"))]
    );
    let empty = found(&rig, root.attr.ino, "empty");
    let fh = open_cached(&rig.core, empty.attr.ino, false).unwrap();
    assert!(rig.core.read(fh, 0, 10).unwrap().is_empty());
    rig.core.release(fh);
}

/// Asserts that the open files of one pack share one descriptor of it, whichever of the pack's
/// files they are, and that it is closed with the last of them; and that a span the node answers
/// with a pack file that is not there is asked again, three times in all, and then fails the open
/// with `EIO`.
#[test]
fn the_files_of_a_pack_share_its_descriptor_and_a_pack_file_that_is_gone_is_an_io_error() {
    let (rig, _) = placed("tfs-img-shared");
    let pack = rig.dir.path().join("pack");
    std::fs::write(&pack, b"XXhelloabc").unwrap();
    answer(&rig, "span", &format!("ok 2 {}", pack.display()));
    let root = found(&rig, ino::ROOT, "jobs");
    let bin = found(&rig, root.attr.ino, "bin");
    let tool = found(&rig, bin.attr.ino, "tool");
    let other = found(&rig, bin.attr.ino, "other");
    let first = open_cached(&rig.core, tool.attr.ino, false).unwrap();
    let second = open_cached(&rig.core, other.attr.ino, false).unwrap();
    let third = open_cached(&rig.core, tool.attr.ino, false).unwrap();
    assert_eq!(rig.core.open_descriptors(), 1);
    rig.core.release(first);
    rig.core.release(second);
    assert_eq!(rig.core.open_descriptors(), 1);
    rig.core.release(third);
    assert_eq!(rig.core.open_descriptors(), 0);
    let spans = rig.upstream.asked("span").len();
    std::fs::remove_file(&pack).unwrap();
    assert_eq!(open_cached(&rig.core, tool.attr.ino, false), Err(Errno::Io));
    assert_eq!(rig.upstream.asked("span").len(), spans + 3);
}

/// Asserts that an image file that is not in the tree's directory, one that is not a tree list, one
/// that is cut short and one a byte over the largest tree list each make the lookup an I/O error,
/// and that the lookup succeeds once the file is the image again.
#[test]
fn a_missing_or_malformed_image_file_makes_the_lookup_an_io_error() {
    let (rig, tree) = placed("tfs-img-bad");
    let file = image_path(&rig.cache, &tree);
    let good = std::fs::read(&file).unwrap();
    std::fs::remove_file(&file).unwrap();
    assert_eq!(rig.core.lookup(ino::ROOT, "jobs").err(), Some(Errno::Io));
    std::fs::write(&file, b"not a tree list at all").unwrap();
    assert_eq!(rig.core.lookup(ino::ROOT, "jobs").err(), Some(Errno::Io));
    std::fs::write(&file, &good[..good.len() / 2]).unwrap();
    assert_eq!(rig.core.lookup(ino::ROOT, "jobs").err(), Some(Errno::Io));
    // One byte over the largest list: refused by its size (a sparse file, read no further).
    let over = std::fs::File::create(&file).unwrap();
    over.set_len(crate::MAX_TREE_LIST_BYTES as u64 + 1).unwrap();
    drop(over);
    assert_eq!(rig.core.lookup(ino::ROOT, "jobs").err(), Some(Errno::Io));
    std::fs::write(&file, &good).unwrap();
    assert!(rig.core.lookup(ino::ROOT, "jobs").is_ok());
}

/// Asserts that an image answer whose paths do not hold the location is not found, that `none` is
/// not found, that a refusal of the node (`damaged`) and an image answer that does not parse (a
/// digest that is not one) are I/O errors, and that the location is found among several paths.
#[test]
fn an_image_answer_without_the_location_is_not_found() {
    let (rig, tree) = placed("tfs-img-paths");
    answer(
        &rig,
        "image",
        &format!(r#"image {tree} ["other", "jobs2"]"#),
    );
    assert_eq!(rig.core.lookup(ino::ROOT, "jobs").err(), Some(Errno::NoEnt));
    answer(&rig, "image", &format!(r#"image {tree} []"#));
    assert_eq!(rig.core.lookup(ino::ROOT, "jobs").err(), Some(Errno::NoEnt));
    answer(&rig, "image", "none");
    assert_eq!(rig.core.lookup(ino::ROOT, "jobs").err(), Some(Errno::NoEnt));
    answer(&rig, "image", "damaged");
    assert_eq!(rig.core.lookup(ino::ROOT, "jobs").err(), Some(Errno::Io));
    answer(&rig, "image", r#"image not-a-digest ["jobs"]"#);
    assert_eq!(rig.core.lookup(ino::ROOT, "jobs").err(), Some(Errno::Io));
    answer(&rig, "image", &format!(r#"image {tree} ["jobs", "jobs2"]"#));
    assert!(rig.core.lookup(ino::ROOT, "jobs").is_ok());
}

/// Asserts that the mount tells the node `started <path>` once for each placed tree it first looks
/// into (`tierstore_request_to_start_seconds`), and for nothing else: not for a lookup of a tree's
/// root, its attributes or an open of it for listing (what the agent and the overlay's mount do),
/// not for a second lookup or listing in the same tree, not for a tree of manifests; a first
/// listing counts as a first lookup does. The reports leave on one thread in order, so once the
/// report of the second placed tree is in, any report made before it would be in too.
#[test]
fn the_first_look_into_each_placed_tree_is_reported_once_and_nothing_else_is() {
    let rig = Rig::new("tfs-img-started", Built::of(Vec::new()));
    let bytes = image_bytes();
    let tree = TreeDigest::of(&bytes);
    let file = image_path(&rig.cache, &tree);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, &bytes).unwrap();
    answer(&rig, "place", &format!("tree {ID}"));
    answer(&rig, "image", &format!(r#"image {tree} ["jobs","jobs2"]"#));
    rig.upstream.gate_verb("started");
    rig.upstream.permit_verb("started", 8);
    let root = found(&rig, ino::ROOT, "jobs").attr.ino;
    rig.core.getattr(root).unwrap();
    rig.core.opendir(root).unwrap();
    let bin = found(&rig, root, "bin").attr.ino;
    let _ = found(&rig, bin, "tool");
    assert!(matches!(
        rig.core.lookup(root, "nope"),
        Ok(Looked::Absent(_))
    ));
    let _ = listed(&rig, root, 0, usize::MAX);
    let manifests = rig.upstream.built.lock().unwrap().root.clone();
    let empty = found(&rig, ino::TREES, &manifests).attr.ino;
    assert!(matches!(rig.core.lookup(empty, "x"), Ok(Looked::Absent(_))));
    let second = found(&rig, ino::ROOT, "jobs2").attr.ino;
    let _ = listed(&rig, second, 0, usize::MAX);
    let _ = found(&rig, second, "d");
    rig.upstream.wait_for_verb("started", 2);
    assert_eq!(
        rig.upstream.asked("started"),
        ["started jobs", "started jobs2"]
    );
}
