//! The calls of the staged-file store before a commit: create, write, release, the rename of one
//! `.part` name to another, unlink, lookup and list, and the quotas.

use std::fs::File;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use rustix::io::Errno;

use super::commit::PutOutcome;
use super::error::PutError;
use super::journal::{Record, decode};
use super::store::{Attr, Handle, Kind, Rename, stage_id, stage_name};
use super::testing::{PREFIX, Rig, START_MS, commit, dir, sha, staged, warnings, written};
use crate::server::local_cache::testing::Quirk;
use crate::server::treefs::locked;

/// Asserts that a created `.part` file holds what was written, in a file of the stage named by its
/// number and not by its key, of mode 0600 (no set-id bits, this process alone), and that its name,
/// size and time are looked up and listed.
#[test]
fn a_created_part_is_staged_under_an_internal_number() {
    let rig = Rig::new("put-create");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "pack.part", b"hello");
    let attr = Attr {
        size: 5,
        ms: START_MS,
        kind: Kind::Staged,
    };
    assert_eq!(store.lookup(&out, "pack.part"), Some(attr));
    assert_eq!(store.list(&out), vec![("pack.part".to_owned(), attr)]);
    assert_eq!(rig.stage_files(), vec![stage_name(0), "journal".to_owned()]);
    assert_eq!(
        std::fs::read(rig.stage().join(stage_name(0))).unwrap(),
        b"hello"
    );
    let mode = std::fs::metadata(rig.stage().join(stage_name(0)))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o600);
    let (records, _) = decode(rig.journal().as_bytes());
    assert_eq!(
        records,
        vec![Record::Create {
            id: 0,
            dir: "cas/b/1/n".to_owned(),
            name: "pack.part".to_owned(),
            ms: START_MS,
        }]
    );
}

/// Asserts that only the names of 20 digits and `.stage` are the names of staged files.
#[test]
fn a_staged_file_is_named_by_20_digits() {
    assert_eq!(stage_name(7), "00000000000000000007.stage");
    assert_eq!(stage_id("00000000000000000007.stage"), Some(7));
    assert_eq!(stage_id(&stage_name(u64::MAX)), Some(u64::MAX));
    for not in [
        "7.stage",
        "0000000000000000000x.stage",
        "00000000000000000007",
        "99999999999999999999.stage",
        "journal",
    ] {
        assert_eq!(stage_id(not), None, "{not}");
    }
}

/// Asserts that a create is refused for a name that does not end in `.part`, a name staged in the
/// directory, a name that is not one, and a key over 1024 bytes; the same name in another directory
/// is taken.
#[test]
fn a_create_is_refused_for_a_final_taken_or_bad_name() {
    let rig = Rig::new("put-create-refused");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    assert!(matches!(
        store.create(&out, "pack"),
        Err(PutError::NotPermitted(_))
    ));
    let _ = store.create(&out, "pack.part").unwrap();
    assert!(matches!(
        store.create(&out, "pack.part"),
        Err(PutError::Exists)
    ));
    let _ = store.create(&dir("cas/b/1/m"), "pack.part").unwrap();
    assert!(matches!(
        store.create(&out, ".."),
        Err(PutError::InvalidName(_))
    ));
    let deep = dir(&format!("cas/b/{0}/{0}/{0}/{0}", "d".repeat(200)));
    let name = format!("{}.part", "n".repeat(250));
    assert!(matches!(
        store.create(&deep, &name),
        Err(PutError::NameTooLong)
    ));
}

/// Asserts that a create is refused with `ENOSPC` while the stage's volume has less free space than
/// `min_free_bytes`, taken at exactly that much, and that a free space that cannot be had is said.
#[test]
fn a_create_needs_min_free_bytes_on_the_stage_volume() {
    let mut rig = Rig::new("put-min-free");
    rig.settings.min_free_bytes = Some(100);
    let store = rig.open();
    let out = dir("cas/b/1/n");
    rig.disk.set_space(1000, 99);
    let refused = store.create(&out, "a.part").unwrap_err();
    assert!(matches!(refused, PutError::NoSpace { free: 99, min: 100 }));
    assert_eq!(refused.errno(), Errno::NOSPC);
    assert_eq!(store.lookup(&out, "a.part"), None);
    rig.disk.set_space(1000, 100);
    let _ = store.create(&out, "a.part").unwrap();
    rig.disk.set(Quirk::SpaceFails);
    assert!(matches!(store.create(&out, "b.part"), Err(PutError::Io(_))));
}

/// Asserts that a directory takes `max_dir_files` names and refuses the next with `EDQUOT`, that
/// another directory is not counted with it, and that an abort gives the name back.
#[test]
fn a_directory_holds_at_most_max_dir_files_names() {
    let mut rig = Rig::new("put-dir-files");
    rig.settings.max_dir_files = Some(2);
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let _ = store.create(&out, "a.part").unwrap();
    let _ = store.create(&out, "b.part").unwrap();
    let refused = store.create(&out, "c.part").unwrap_err();
    assert!(matches!(refused, PutError::Quota("files")));
    assert_eq!(refused.errno(), Errno::DQUOT);
    let _ = store.create(&dir("cas/b/1/m"), "c.part").unwrap();
    store.unlink(&out, "a.part").unwrap();
    let _ = store.create(&out, "c.part").unwrap();
}

/// Asserts that a write that would take a file past `max_object_bytes` is refused with `EFBIG` and
/// changes nothing, that one up to the limit and an overwrite inside it are taken, and that an
/// offset whose end is past `u64` is too large.
#[test]
fn a_write_past_max_object_bytes_is_too_large() {
    let mut rig = Rig::new("put-max-object");
    rig.settings.max_object_bytes = Some(10);
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let handle = written(&store, &out, "a.part", b"0123456789");
    let refused = store.write(handle, 10, b"x").unwrap_err();
    assert!(matches!(refused, PutError::TooLarge));
    assert_eq!(refused.errno(), Errno::FBIG);
    assert_eq!(
        std::fs::read(rig.stage().join(stage_name(0))).unwrap(),
        b"0123456789"
    );
    assert_eq!(store.write(handle, 0, b"ab").unwrap(), 2);
    assert_eq!(store.lookup(&out, "a.part").unwrap().size, 10);
    assert!(matches!(
        store.write(handle, u64::MAX, b"x"),
        Err(PutError::TooLarge)
    ));
    store.release(handle);
    let Rename::Commit(commit) = store.rename(&out, "a.part", "a").unwrap() else {
        panic!("a commit");
    };
    assert_eq!((commit.sha256(), commit.size()), (sha(b"ab23456789"), 10));
}

/// Asserts that the staged bytes of a directory stay at most `max_dir_bytes` across its files (the
/// write over it is refused with `EDQUOT`), that an overwrite does not count again, that another
/// directory has its own, and that an abort gives its bytes back.
#[test]
fn a_directory_holds_at_most_max_dir_bytes_staged() {
    let mut rig = Rig::new("put-dir-bytes");
    rig.settings.max_dir_bytes = Some(10);
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let a = written(&store, &out, "a.part", b"012345");
    let b = written(&store, &out, "b.part", b"0123");
    let refused = store.write(b, 4, b"x").unwrap_err();
    assert!(matches!(refused, PutError::Quota("bytes")));
    assert_eq!(refused.errno(), Errno::DQUOT);
    assert_eq!(
        std::fs::read(rig.stage().join(stage_name(1))).unwrap(),
        b"0123"
    );
    assert_eq!(locked(&store.table).dirs["cas/b/1/n"].bytes, 10);
    assert_eq!(store.write(a, 0, b"abcdef").unwrap(), 6);
    let other = written(&store, &dir("cas/b/1/m"), "c.part", b"0123456789");
    store.release(other);
    store.release(a);
    store.unlink(&out, "a.part").unwrap();
    assert_eq!(store.write(b, 4, b"456789").unwrap(), 6);
    assert_eq!(store.lookup(&out, "b.part").unwrap().size, 10);
}

/// Asserts that a write through a handle that was released, never made, or whose file was removed
/// is refused with `EBADF`, and that a release of a handle that is not open does nothing.
#[test]
fn a_write_needs_an_open_handle() {
    let rig = Rig::new("put-handles");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let handle = written(&store, &out, "a.part", b"x");
    store.release(handle);
    let refused = store.write(handle, 1, b"y").unwrap_err();
    assert!(matches!(refused, PutError::BadHandle));
    assert_eq!(refused.errno(), Errno::BADF);
    assert!(matches!(
        store.write(Handle(99), 0, b"y"),
        Err(PutError::BadHandle)
    ));
    store.release(Handle(99));
    let open = written(&store, &out, "b.part", b"x");
    store.unlink(&out, "b.part").unwrap();
    assert!(matches!(
        store.write(open, 1, b"y"),
        Err(PutError::BadHandle)
    ));
}

/// Asserts that a write the file refuses is said, takes back the bytes it was given (the size and
/// the directory's use stay), and leaves the digest to be computed at commit.
#[test]
fn a_failed_write_gives_back_its_bytes_and_unknows_the_digest() {
    let mut rig = Rig::new("put-write-fails");
    rig.settings.max_dir_bytes = Some(8);
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let handle = written(&store, &out, "a.part", b"hello");
    let path = rig.stage().join(stage_name(0));
    {
        let body = Arc::clone(&locked(&store.table).bodies[&0]);
        locked(&body).file = File::open(&path).unwrap();
    }
    let refused = store.write(handle, 5, b"!!!").unwrap_err();
    assert!(matches!(refused, PutError::Io(_)));
    assert_eq!(refused.errno(), Errno::BADF);
    assert_eq!(store.lookup(&out, "a.part").unwrap().size, 5);
    assert_eq!(locked(&store.table).dirs["cas/b/1/n"].bytes, 5);
    std::fs::write(&path, b"HELLO").unwrap();
    store.release(handle);
    let Rename::Commit(commit) = store.rename(&out, "a.part", "a").unwrap() else {
        panic!("a commit");
    };
    assert_eq!((commit.sha256(), commit.size()), (sha(b"HELLO"), 5));
}

/// Asserts that the table refuses to grow a file that is gone or whose commit runs (a write that
/// lost the race with an unlink or a release and a commit), and that giving back the bytes of a
/// file that is gone does nothing.
#[test]
fn a_write_that_lost_a_race_is_refused_by_the_table() {
    let rig = Rig::new("put-reserve");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "a.part", b"x");
    assert!(matches!(store.reserve(99, 5), Err(PutError::BadHandle)));
    store.unreserve(99, 5);
    let _ = locked(&store.table)
        .uploading
        .insert(0, "cas/b/1/n/a".to_owned());
    assert!(matches!(store.reserve(0, 5), Err(PutError::Busy(_))));
}

/// Asserts that a rename of a `.part` name to another changes the name alone (no commit, the same
/// staged file and bytes, a handle open before it still writes, and the file then commits), is
/// journaled, and that a rename to the same name does nothing.
#[test]
fn a_part_renamed_to_a_part_is_a_change_of_name() {
    let rig = Rig::new("put-rename-part");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let handle = written(&store, &out, "pack.part", b"hel");
    let named = format!("pack.sha256-{}.part", sha(b"hello"));
    assert!(matches!(
        store.rename(&out, "pack.part", &named).unwrap(),
        Rename::Renamed
    ));
    assert_eq!(store.write(handle, 3, b"lo").unwrap(), 2);
    store.release(handle);
    assert_eq!(store.lookup(&out, "pack.part"), None);
    assert_eq!(store.lookup(&out, &named).unwrap().size, 5);
    assert_eq!(rig.stage_files(), vec![stage_name(0), "journal".to_owned()]);
    let (records, _) = decode(rig.journal().as_bytes());
    assert_eq!(
        records.last(),
        Some(&Record::Rename {
            id: 0,
            name: named.clone(),
        })
    );
    assert!(matches!(
        store.rename(&out, &named, &named).unwrap(),
        Rename::Renamed
    ));
    assert_eq!(decode(rig.journal().as_bytes()).0.len(), 2);
    commit(&store, &out, &named, "pack", PutOutcome::Created).unwrap();
    assert_eq!(
        store.lookup(&out, "pack").unwrap().kind,
        Kind::Committed(sha(b"hello"))
    );
}

/// Asserts that a rename is refused from a name that is not `.part`, from one not staged, onto a
/// `.part` name staged, between names that are not names, and when the journal cannot be written
/// (the name is then unchanged).
#[test]
fn a_rename_is_refused_from_a_final_or_missing_name_or_onto_a_taken_one() {
    let rig = Rig::new("put-rename-refused");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "a.part", b"a");
    staged(&store, &out, "b.part", b"b");
    assert!(matches!(
        store.rename(&out, "a", "b"),
        Err(PutError::NotPermitted(_))
    ));
    assert!(matches!(
        store.rename(&out, "c.part", "d.part"),
        Err(PutError::NotFound)
    ));
    assert!(matches!(
        store.rename(&out, "c.part", "d"),
        Err(PutError::NotFound)
    ));
    assert!(matches!(
        store.rename(&out, "a.part", "b.part"),
        Err(PutError::Exists)
    ));
    assert!(matches!(
        store.rename(&out, "a/x.part", "b"),
        Err(PutError::InvalidName(_))
    ));
    assert!(matches!(
        store.rename(&out, "a.part", "."),
        Err(PutError::InvalidName(_))
    ));
    locked(&store.table).journal.break_appends();
    assert!(matches!(
        store.rename(&out, "a.part", "c.part"),
        Err(PutError::Io(_))
    ));
    assert!(store.lookup(&out, "a.part").is_some());
}

/// Asserts that an unlink of a `.part` name aborts it: the name, its staged file and its bytes are
/// gone and the removal is journaled; and that an unlink of a final, missing or bad name is refused.
#[test]
fn an_unlink_of_a_part_aborts_it() {
    let rig = Rig::new("put-unlink");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "a.part", b"abc");
    store.unlink(&out, "a.part").unwrap();
    assert_eq!(store.lookup(&out, "a.part"), None);
    assert_eq!(rig.stage_files(), vec!["journal".to_owned()]);
    assert!(locked(&store.table).dirs.is_empty());
    assert_eq!(
        decode(rig.journal().as_bytes()).0.last(),
        Some(&Record::Remove { id: 0 })
    );
    assert!(matches!(
        store.unlink(&out, "a.part"),
        Err(PutError::NotFound)
    ));
    assert!(matches!(
        store.unlink(&out, "a"),
        Err(PutError::NotPermitted(_))
    ));
    assert!(matches!(
        store.unlink(&out, ""),
        Err(PutError::InvalidName(_))
    ));
}

/// Asserts that an unlink whose staged file is already gone from the disk still removes the name
/// (the failure to remove the file is said, not answered).
#[test]
fn an_unlink_whose_file_is_gone_still_removes_the_name() {
    let rig = Rig::new("put-unlink-gone");
    let store = rig.open();
    let (_log, warnings) = warnings();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "a.part", b"abc");
    std::fs::remove_file(rig.stage().join(stage_name(0))).unwrap();
    store.unlink(&out, "a.part").unwrap();
    assert_eq!(store.lookup(&out, "a.part"), None);
    assert_eq!(
        warnings.text(),
        format!(
            "tierstore fs: cannot remove the staged file {}: No such file or directory (os error 2)\n",
            stage_name(0)
        )
    );
}

/// Asserts that a create whose journal line cannot be written is refused, leaves no staged file and
/// no name, and does not use up the number.
#[test]
fn a_create_whose_journal_line_fails_leaves_nothing() {
    let rig = Rig::new("put-create-journal");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    locked(&store.table).journal.break_appends();
    assert!(matches!(store.create(&out, "a.part"), Err(PutError::Io(_))));
    assert_eq!(store.lookup(&out, "a.part"), None);
    assert_eq!(rig.stage_files(), vec!["journal".to_owned()]);
    assert_eq!(locked(&store.table).next_id, 0);
}

/// Asserts that a list gives the names of the one directory asked for, staged and committed, in
/// the order of their bytes, and none of a directory whose name only starts the same or is below.
#[test]
fn a_list_gives_the_names_of_one_directory_in_order() {
    let rig = Rig::new("put-list");
    let store = rig.open();
    let out = dir("cas/b/1");
    staged(&store, &out, "b.part", b"b");
    staged(&store, &out, "a.part", b"a");
    staged(&store, &out, "c.part", b"cc");
    commit(&store, &out, "c.part", "c", PutOutcome::Created).unwrap();
    staged(&store, &dir("cas/b/1x"), "x.part", b"x");
    staged(&store, &dir("cas/b/1/n"), "y.part", b"y");
    staged(&store, &dir("cas/b/0"), "z.part", b"z");
    let names: Vec<(String, Kind)> = store
        .list(&out)
        .into_iter()
        .map(|(name, attr)| (name, attr.kind))
        .collect();
    assert_eq!(
        names,
        vec![
            ("a.part".to_owned(), Kind::Staged),
            ("b.part".to_owned(), Kind::Staged),
            ("c".to_owned(), Kind::Committed(sha(b"cc"))),
        ]
    );
    assert!(store.list(&dir("cas/b/2")).is_empty());
    assert_eq!(store.lookup(&dir("cas/b/2"), "a.part"), None);
    assert_eq!(PREFIX, "cas/b/");
}

/// Asserts that files written at once by many threads, and one file whose chunks two threads write
/// in an order no test fixes, each commit with the SHA-256 of the bytes they hold: the inline hash
/// is right whenever it is used, and is otherwise computed.
#[test]
fn concurrent_writers_commit_the_digest_of_their_bytes() {
    let rig = Rig::new("put-concurrent");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let chunk = |file: usize, index: usize| vec![(file * 31 + index) as u8; 4096];
    std::thread::scope(|scope| {
        for file in 0..8 {
            let (store, out) = (&store, &out);
            let _ = scope.spawn(move || {
                let handle = store.create(out, &format!("f{file}.part")).unwrap();
                for index in 0..16 {
                    let offset = (index * 4096) as u64;
                    let _ = store.write(handle, offset, &chunk(file, index)).unwrap();
                }
                store.release(handle);
            });
        }
    });
    let shared = store.create(&out, "shared.part").unwrap();
    std::thread::scope(|scope| {
        for half in 0..2 {
            let store = &store;
            let _ = scope.spawn(move || {
                for index in (half..32).step_by(2) {
                    let offset = (index * 4096) as u64;
                    let _ = store.write(shared, offset, &chunk(9, index)).unwrap();
                }
            });
        }
    });
    store.release(shared);
    for file in 0..8 {
        let bytes: Vec<u8> = (0..16).flat_map(|index| chunk(file, index)).collect();
        let Rename::Commit(commit) = store
            .rename(&out, &format!("f{file}.part"), &format!("f{file}"))
            .unwrap()
        else {
            panic!("a commit");
        };
        assert_eq!(
            (commit.sha256(), commit.size()),
            (sha(&bytes), bytes.len() as u64)
        );
    }
    let bytes: Vec<u8> = (0..32).flat_map(|index| chunk(9, index)).collect();
    let Rename::Commit(commit) = store.rename(&out, "shared.part", "shared").unwrap() else {
        panic!("a commit");
    };
    assert_eq!(
        (commit.sha256(), commit.size()),
        (sha(&bytes), bytes.len() as u64)
    );
}
