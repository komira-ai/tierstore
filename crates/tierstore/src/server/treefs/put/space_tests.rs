//! The namespace of the put directory: which names are directories, files or nothing, their
//! attributes, listings, `mkdir`, creates, opens and changes of attributes, and the directories the
//! kernel holds until it forgets them. The commit and the hand-over are in `space_commit_tests.rs`.

use rustix::io::Errno;

use super::super::testing::{OWNER, SpaceRig};
use super::{Dir, PutAttr, PutItem, PutSpace, dir_ino};
use crate::server::treefs::ino::{self, PutIno};

/// The inode number of the directory `path`, looked up from `put/` name by name, as the kernel
/// does.
fn walk(space: &PutSpace, path: &str) -> u64 {
    let mut ino = ino::PUT;
    for name in path.split('/') {
        let attr = space.lookup(ino, name).unwrap().unwrap();
        assert!(attr.dir, "{name} of {path} is a directory");
        ino = attr.ino;
    }
    ino
}

/// The names `readdir` gives of `ino` from `offset`, all of them.
fn listed(space: &PutSpace, ino: u64, offset: u64) -> Vec<(u64, String)> {
    let mut names = Vec::new();
    space
        .readdir(ino, offset, &mut |next, item: PutItem| {
            names.push((next, item.name));
            true
        })
        .unwrap();
    names
}

/// Asserts that `put/` and the directories that lead to the prefix `cas/b/` are directories of
/// mode 0711 owned by root that nobody lists, that the prefix itself is one of mode 0700 owned by
/// `fs.owner`, and that a name that leads nowhere is not there.
#[test]
fn the_directories_that_lead_to_a_prefix_are_shown_and_never_listed() {
    let rig = SpaceRig::new("put-space-leading");
    let space = &rig.space;
    let root = space.getattr(ino::PUT).unwrap();
    assert_eq!(
        (root.dir, root.perm, root.uid, root.gid),
        (true, 0o711, 0, 0)
    );
    let cas = space.lookup(ino::PUT, "cas").unwrap().unwrap();
    assert_eq!((cas.dir, cas.perm, cas.uid, cas.gid), (true, 0o711, 0, 0));
    assert_eq!(cas.ino, dir_ino("cas"));
    let prefix = space.lookup(cas.ino, "b").unwrap().unwrap();
    assert_eq!(
        (prefix.dir, prefix.perm, prefix.uid, prefix.gid),
        (true, 0o700, OWNER.0, OWNER.1)
    );
    assert_eq!(space.lookup(ino::PUT, "other").unwrap(), None);
    assert_eq!(space.lookup(cas.ino, "c").unwrap(), None);
    assert_eq!(space.opendir(ino::PUT), Err(Errno::ACCESS));
    assert_eq!(space.opendir(cas.ino), Err(Errno::ACCESS));
    assert_eq!(
        space.readdir(cas.ino, 0, &mut |_, _| true),
        Err(Errno::ACCESS)
    );
    assert_eq!(space.opendir(prefix.ino), Ok(()));
    assert_eq!(space.getattr(cas.ino).unwrap(), cas);
}

/// Asserts that every directory at or under a prefix exists before anything is in it, so that the
/// job's directory `cas/b/<build>/<nonce>` can be opened as it is given, and that it lists `.` and
/// `..` alone while empty.
#[test]
fn a_directory_under_a_prefix_exists_before_anything_is_in_it() {
    let rig = SpaceRig::new("put-space-implicit");
    let space = &rig.space;
    let nonce = walk(space, "cas/b/build-1/nonce-1");
    let attr = space.getattr(nonce).unwrap();
    assert_eq!((attr.dir, attr.perm, attr.uid), (true, 0o700, OWNER.0));
    assert_eq!(
        listed(space, nonce, 0),
        vec![(1, ".".to_owned()), (2, "..".to_owned())]
    );
    let mut items = Vec::new();
    space
        .readdir(nonce, 0, &mut |_, item| {
            items.push(item.attr.ino);
            true
        })
        .unwrap();
    assert_eq!(items, vec![nonce, dir_ino("cas/b/build-1")]);
}

/// Asserts that a `.part` name is not there until it is created, and is then a file of mode 0600
/// owned by `fs.owner`, of the number its create gave, with no bytes.
#[test]
fn a_part_name_is_not_there_until_it_is_created() {
    let rig = SpaceRig::new("put-space-part");
    let space = &rig.space;
    let nonce = walk(space, "cas/b/build/nonce");
    assert_eq!(space.lookup(nonce, "pack.part").unwrap(), None);
    let (created, _) = space.create(nonce, "pack.part").unwrap();
    let found = space.lookup(nonce, "pack.part").unwrap().unwrap();
    assert_eq!(found, created);
    assert_eq!(
        (found.dir, found.perm, found.uid, found.gid, found.size),
        (false, 0o600, OWNER.0, OWNER.1, 0)
    );
    assert!(matches!(ino::put_of(found.ino), Some(PutIno::File(_))));
    assert_eq!(found.ms, super::super::testing::START_MS);
    assert_eq!(space.getattr(found.ino).unwrap(), found);
}

/// Asserts that in a directory that holds a file every free name is not there (the name of a
/// commit must not be a directory), while in an empty directory beside it the same name is a
/// directory.
#[test]
fn in_a_directory_that_holds_a_file_a_free_name_is_not_there() {
    let rig = SpaceRig::new("put-space-filedir");
    let space = &rig.space;
    let nonce = walk(space, "cas/b/build/nonce");
    let _ = space.create(nonce, "pack.part").unwrap();
    assert_eq!(space.lookup(nonce, "pack").unwrap(), None);
    assert_eq!(space.lookup(nonce, "sub").unwrap(), None);
    let other = walk(space, "cas/b/build/other");
    assert!(space.lookup(other, "pack").unwrap().unwrap().dir);
}

/// Asserts that a directory that has a name below it is a directory even in a directory that holds
/// a file.
#[test]
fn a_directory_with_a_name_below_it_is_found_where_free_names_are_not() {
    let rig = SpaceRig::new("put-space-known");
    let space = &rig.space;
    let x = walk(space, "cas/b/x");
    let y = walk(space, "cas/b/x/y");
    let _ = space.create(y, "deep.part").unwrap();
    let _ = space.create(x, "top.part").unwrap();
    assert_eq!(space.lookup(x, "y").unwrap().unwrap().ino, y);
    assert_eq!(space.lookup(x, "z").unwrap(), None);
}

/// Asserts that a name that is not one is refused by its errno: white space and control
/// characters (a key is one word of one line), `.`, `..`, and more than 255 bytes.
#[test]
fn a_name_that_is_not_one_is_refused() {
    let rig = SpaceRig::new("put-space-names");
    let space = &rig.space;
    let nonce = walk(space, "cas/b/build/nonce");
    for name in ["a b", "a\nb", "a\tb", "..", ".", "a\u{7f}b", "a\u{a0}b"] {
        assert_eq!(space.lookup(nonce, name), Err(Errno::INVAL), "{name:?}");
        assert_eq!(space.create(nonce, name).map(|_| ()), Err(Errno::INVAL));
        assert_eq!(space.mkdir(nonce, name).map(|_| ()), Err(Errno::INVAL));
    }
    let long = "a".repeat(256);
    assert_eq!(space.lookup(nonce, &long), Err(Errno::NAMETOOLONG));
    let deepest = (0..30).map(|_| "d").collect::<Vec<_>>().join("/");
    let deep = walk(space, &format!("cas/b/{deepest}"));
    assert_eq!(space.lookup(deep, "one-more").unwrap(), None);
}

/// Asserts that a directory this process was not asked for is `ESTALE` (the number of a process
/// before, after a start that was handed nothing), and a file as a parent `ENOTDIR`.
#[test]
fn a_directory_this_process_does_not_know_is_stale() {
    let rig = SpaceRig::new("put-space-stale");
    let space = &rig.space;
    let unknown = dir_ino("cas/b/never");
    assert_eq!(space.lookup(unknown, "x"), Err(Errno::STALE));
    assert_eq!(space.getattr(unknown), Err(Errno::STALE));
    assert_eq!(space.opendir(unknown), Err(Errno::STALE));
    assert_eq!(space.lookup(5 << 24, "x"), Err(Errno::STALE));
    let nonce = walk(space, "cas/b/build/nonce");
    let (file, _) = space.create(nonce, "a.part").unwrap();
    assert_eq!(space.lookup(file.ino, "x"), Err(Errno::NOTDIR));
    assert_eq!(space.getattr(ino::put_file(999)), Err(Errno::NOENT));
}

/// Asserts that a directory whose number is held for another path is refused with `EIO` rather
/// than answered as the other, and said with both paths.
#[test]
fn two_directories_of_one_number_are_refused() {
    let (_guard, warnings) = super::super::testing::warnings();
    let rig = SpaceRig::new("put-space-collide");
    let space = &rig.space;
    let b = walk(space, "cas/b");
    let _ = space.state.lock().unwrap().dirs.insert(
        dir_ino("cas/b/one"),
        Dir {
            path: "cas/b/two".to_owned(),
            lookups: Some(1),
            made: false,
        },
    );
    assert_eq!(space.lookup(b, "one"), Err(Errno::IO));
    assert_eq!(
        warnings.text(),
        "tierstore fs: the put directories cas/b/one and cas/b/two have one inode number\n"
    );
    assert!(format!("{space:?}").starts_with("PutSpace { store: StagedStore"));
}

/// Asserts that a directory is known until the kernel forgets as many lookups as it was given,
/// and then let go; that a forget of what is not a known directory does nothing; and that a
/// directory whose lookups are not known is never let go.
#[test]
fn a_directory_is_let_go_when_the_kernel_forgets_it() {
    let rig = SpaceRig::new("put-space-forget");
    let space = &rig.space;
    let b = walk(space, "cas/b");
    let x = space.lookup(b, "x").unwrap().unwrap().ino;
    let _ = space.lookup(b, "x").unwrap();
    space.forget(x, 1);
    assert!(space.getattr(x).is_ok());
    space.forget(x, 1);
    assert_eq!(space.getattr(x), Err(Errno::STALE));
    space.forget(ino::put_file(1), 1);
    space.forget(dir_ino("cas/b/never"), 1);
    let _ = space.state.lock().unwrap().dirs.insert(
        dir_ino("cas/b/kept"),
        Dir {
            path: "cas/b/kept".to_owned(),
            lookups: None,
            made: false,
        },
    );
    space.forget(dir_ino("cas/b/kept"), 100);
    assert!(space.getattr(dir_ino("cas/b/kept")).is_ok());
}

/// Asserts that `mkdir` makes a directory where free names are not there (a directory that holds
/// a file), that it is found until the kernel forgets it, and that it is then not there.
#[test]
fn mkdir_makes_a_directory_until_the_kernel_forgets_it() {
    let rig = SpaceRig::new("put-space-mkdir");
    let space = &rig.space;
    let nonce = walk(space, "cas/b/build/nonce");
    let _ = space.create(nonce, "pack.part").unwrap();
    let made = space.mkdir(nonce, "sub").unwrap();
    assert_eq!((made.dir, made.perm, made.uid), (true, 0o700, OWNER.0));
    assert_eq!(space.lookup(nonce, "sub").unwrap(), Some(made));
    assert_eq!(space.mkdir(nonce, "sub"), Err(Errno::EXIST));
    space.forget(made.ino, 2);
    assert_eq!(space.lookup(nonce, "sub").unwrap(), None);
}

/// Asserts that `mkdir` is refused outside a prefix (`EPERM`), for a `.part` name (`EPERM`), and
/// over a file or a directory with a name below it (`EEXIST`).
#[test]
fn mkdir_is_refused_outside_a_prefix_for_a_part_name_and_over_a_name() {
    let rig = SpaceRig::new("put-space-mkdir-no");
    let space = &rig.space;
    let cas = walk(space, "cas");
    assert_eq!(space.mkdir(ino::PUT, "other"), Err(Errno::PERM));
    assert_eq!(space.mkdir(cas, "c"), Err(Errno::PERM));
    let nonce = walk(space, "cas/b/build/nonce");
    assert_eq!(space.mkdir(nonce, "d.part"), Err(Errno::PERM));
    let _ = space.create(nonce, "pack.part").unwrap();
    let _ = space
        .store
        .rename(&dir("cas/b/build/nonce"), "pack.part", "kept.part")
        .unwrap();
    let build = walk(space, "cas/b/build");
    assert_eq!(space.mkdir(build, "nonce"), Err(Errno::EXIST));
    let _ = space.create(build, "file.part").unwrap();
    assert_eq!(space.mkdir(build, "file.part"), Err(Errno::PERM));
    assert_eq!(space.mkdir(ino::put_file(0), "x"), Err(Errno::NOTDIR));
    let (_, fh) = space.create(build, "done.part").unwrap();
    space.release(fh);
    space
        .rename((build, "done.part"), (build, "done"), false)
        .unwrap();
    assert_eq!(space.mkdir(build, "done"), Err(Errno::EXIST));
}

/// The directory `path`, under the prefix of the rig.
fn dir(path: &str) -> super::super::names::PutDir {
    super::super::testing::dir(path)
}

/// Asserts that a create is refused in `put/` and in a directory that leads to a prefix (`EPERM`),
/// for a final name (`EPERM`), and for a name taken (`EEXIST`).
#[test]
fn a_create_is_refused_outside_a_prefix_for_a_final_name_and_a_taken_one() {
    let rig = SpaceRig::new("put-space-create-no");
    let space = &rig.space;
    let cas = walk(space, "cas");
    assert_eq!(
        space.create(ino::PUT, "a.part").map(|_| ()),
        Err(Errno::PERM)
    );
    assert_eq!(space.create(cas, "a.part").map(|_| ()), Err(Errno::PERM));
    let nonce = walk(space, "cas/b/build/nonce");
    assert_eq!(space.create(nonce, "pack").map(|_| ()), Err(Errno::PERM));
    let _ = space.create(nonce, "pack.part").unwrap();
    assert_eq!(
        space.create(nonce, "pack.part").map(|_| ()),
        Err(Errno::EXIST)
    );
}

/// Asserts that no file is opened but by its create: a staged one is `EPERM`, a committed one
/// `EACCES` (never read), a directory `EISDIR`, and a file that is gone `ENOENT`.
#[test]
fn a_file_is_never_opened_again() {
    let rig = SpaceRig::new("put-space-open");
    let space = &rig.space;
    let nonce = walk(space, "cas/b/build/nonce");
    let (staged, fh) = space.create(nonce, "pack.part").unwrap();
    assert_eq!(space.open(staged.ino), Err(Errno::PERM));
    assert_eq!(space.write((staged.ino, fh), 0, b"bytes"), Ok(5));
    space.release(fh);
    space
        .rename((nonce, "pack.part"), (nonce, "pack"), false)
        .unwrap();
    assert_eq!(space.open(staged.ino), Err(Errno::ACCESS));
    assert_eq!(space.open(nonce), Err(Errno::ISDIR));
    assert_eq!(space.open(ino::PUT), Err(Errno::ISDIR));
    assert_eq!(space.open(ino::put_file(999)), Err(Errno::NOENT));
}

/// Asserts that a change of attributes is taken only when it changes nothing: the size the file
/// has and the permission bits it has (what the kernel asks at a create with `O_TRUNC`, or to
/// clear set-id bits); a truncate to another size, any other mode (a set-id bit among them), a
/// change of owner or of a time, and a size of a directory are `EPERM`.
#[test]
fn a_change_of_attributes_is_taken_only_when_it_changes_nothing() {
    let rig = SpaceRig::new("put-space-setattr");
    let space = &rig.space;
    let nonce = walk(space, "cas/b/build/nonce");
    let (file, fh) = space.create(nonce, "a.part").unwrap();
    let _ = space.write((file.ino, fh), 0, b"four").unwrap();
    let now = space.getattr(file.ino).unwrap();
    assert_eq!(space.setattr(file.ino, (None, Some(4)), false), Ok(now));
    assert_eq!(
        space.setattr(file.ino, (Some(0o100_600), None), false),
        Ok(now)
    );
    assert_eq!(space.setattr(file.ino, (None, None), false), Ok(now));
    for change in [
        (None, Some(0)),
        (None, Some(5)),
        (Some(0o4600), None),
        (Some(0o644), None),
    ] {
        assert_eq!(
            space.setattr(file.ino, change, false),
            Err(Errno::PERM),
            "{change:?}"
        );
    }
    assert_eq!(
        space.setattr(file.ino, (None, None), true),
        Err(Errno::PERM)
    );
    assert_eq!(
        space.setattr(nonce, (None, Some(0)), false),
        Err(Errno::PERM)
    );
    assert_eq!(
        space
            .setattr(nonce, (Some(0o700), None), false)
            .map(|a| a.dir),
        Ok(true)
    );
    assert_eq!(
        space.setattr(dir_ino("cas/b/never"), (None, None), false),
        Err(Errno::STALE)
    );
}

/// Asserts that writes through the open of a create reach the store at their offsets, and that a
/// write through an open that is not one is `EBADF`: one this process never gave out, and one it
/// released.
#[test]
fn writes_reach_the_store_through_an_open_of_this_process_alone() {
    let rig = SpaceRig::new("put-space-write");
    let space = &rig.space;
    let nonce = walk(space, "cas/b/build/nonce");
    let (file, fh) = space.create(nonce, "a.part").unwrap();
    assert_eq!(space.write((file.ino, fh), 0, b"abc"), Ok(3));
    assert_eq!(space.write((file.ino, fh), 3, b"de"), Ok(2));
    assert_eq!(space.getattr(file.ino).unwrap().size, 5);
    let foreign = crate::server::treefs::handle::make(1, 77, false);
    assert_eq!(space.write((file.ino, foreign), 0, b"x"), Err(Errno::BADF));
    assert_eq!(space.write((nonce, foreign), 0, b"x"), Err(Errno::BADF));
    space.release(fh);
    assert_eq!(space.write((file.ino, fh), 5, b"f"), Err(Errno::BADF));
    space.release(fh);
    let (gone, open) = space.create(nonce, "gone.part").unwrap();
    space.unlink(nonce, "gone.part").unwrap();
    assert_eq!(space.write((gone.ino, open), 0, b"x"), Err(Errno::BADF));
}

/// Asserts that a listing gives `.`, `..` and the files of the directory alone, staged and
/// committed, in the order of their names, from the position asked, and stops when the sink is
/// full; a directory with names below it is never listed.
#[test]
fn a_listing_gives_the_files_of_one_directory_and_never_its_directories() {
    let rig = SpaceRig::new("put-space-list");
    let space = &rig.space;
    let x = walk(space, "cas/b/x");
    let y = walk(space, "cas/b/x/y");
    let _ = space.create(y, "deep.part").unwrap();
    let (b, fh) = space.create(x, "b.part").unwrap();
    let _ = space.write((b.ino, fh), 0, b"bytes").unwrap();
    space.release(fh);
    space.rename((x, "b.part"), (x, "b"), false).unwrap();
    let _ = space.create(x, "a.part").unwrap();
    let names: Vec<String> = listed(space, x, 0)
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    assert_eq!(names, [".", "..", "a.part", "b"]);
    assert_eq!(
        listed(space, x, 2),
        vec![(3, "a.part".to_owned()), (4, "b".to_owned())]
    );
    assert_eq!(listed(space, x, 9), vec![]);
    let mut first = Vec::new();
    space
        .readdir(x, 0, &mut |_, item| {
            first.push(item.name);
            false
        })
        .unwrap();
    assert_eq!(first, ["."]);
    let mut kinds = Vec::new();
    space
        .readdir(x, 2, &mut |_, item: PutItem| {
            kinds.push((item.attr.perm, item.attr.size));
            true
        })
        .unwrap();
    assert_eq!(kinds, [(0o600, 0), (0o400, 5)]);
}

/// Asserts that the space of `statfs` is the stage's volume.
#[test]
fn the_space_is_that_of_the_stage_volume() {
    let rig = SpaceRig::new("put-space-statfs");
    rig.rig.disk.set_space(1 << 30, 1 << 20);
    let volume = rig.space.space().unwrap();
    assert_eq!((volume.total, volume.free), (1 << 30, 1 << 20));
}

/// Asserts that a directory reports no bytes and the fixed modification time of the namespace.
#[test]
fn a_directory_reports_no_bytes_and_the_fixed_time() {
    let rig = SpaceRig::new("put-space-attr");
    let attr: PutAttr = rig.space.getattr(ino::PUT).unwrap();
    assert_eq!(
        (attr.size, attr.ms),
        (0, crate::server::treefs::core::FIXED_MTIME * 1000)
    );
}
