//! The start of the store over a stage a process before it left: the staged files and committed
//! names it serves again, what it drops and removes, the journal lines it skips, and the stage it
//! refuses.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use super::commit::PutOutcome;
use super::error::{OpenError, PutError};
use super::journal::{NEW_FILE, Record, decode, encode};
use super::replay::{Replayed, same_volume};
use super::store::{Kind, Rename, StagedStore, stage_name};
use super::testing::{Rig, START_MS, commit, dir, sha, staged, written};
use crate::server::treefs::locked;

/// Writes `records` as the journal of the stage of `rig`, which is made.
fn journal_of(rig: &Rig, records: &[Record]) {
    std::fs::create_dir_all(rig.stage()).unwrap();
    let mut bytes = Vec::new();
    for record in records {
        bytes.extend(encode(record).unwrap());
    }
    std::fs::write(rig.stage().join("journal"), bytes).unwrap();
}

fn create(id: u64, dir: &str, name: &str) -> Record {
    Record::Create {
        id,
        dir: dir.to_owned(),
        name: name.to_owned(),
        ms: START_MS,
    }
}

/// Asserts that a restart (a process killed between writes, its handles gone) stages its files
/// again under their last names with the sizes of their files, that a write cannot go on through a
/// handle of the process before, and that a commit after it hashes the file, the inline hash gone.
#[test]
fn a_restart_stages_the_files_of_the_process_before() {
    let rig = Rig::new("put-replay-staged");
    let out = dir("cas/b/1/n");
    let named = format!("b.sha256-{}.part", sha(b"bytes"));
    let old_handle = {
        let store = rig.open();
        let handle = written(&store, &out, "a.part", b"hello");
        staged(&store, &out, "b.part", b"bytes");
        assert!(matches!(
            store.rename(&out, "b.part", &named).unwrap(),
            Rename::Renamed
        ));
        handle
    };
    let (store, replayed) = rig.open_replayed();
    assert_eq!(
        replayed,
        Replayed {
            staged: 2,
            ..Replayed::default()
        }
    );
    assert_eq!(store.lookup(&out, "a.part").unwrap().size, 5);
    assert_eq!(store.lookup(&out, "a.part").unwrap().ms, START_MS);
    assert_eq!(store.lookup(&out, &named).unwrap().kind, Kind::Staged);
    assert!(matches!(
        store.write(old_handle, 5, b"x"),
        Err(PutError::BadHandle)
    ));
    let Rename::Commit(commit) = store.rename(&out, &named, "b").unwrap() else {
        panic!("a commit");
    };
    assert_eq!((commit.sha256(), commit.size()), (sha(b"bytes"), 5));
    commit.finish(PutOutcome::AlreadyStored).unwrap();
    commit_after_restart(&store);
}

fn commit_after_restart(store: &StagedStore) {
    let out = dir("cas/b/1/n");
    let handle = store.create(&out, "c.part").unwrap();
    store.release(handle);
    assert_eq!(locked(&store.table).next_id, 3);
}

/// Asserts that a committed name is shown again after a restart, with its digest, size and time,
/// until its committed TTL, and that the staged file a commit failed to remove is an orphan.
#[test]
fn a_restart_shows_the_committed_names_again() {
    let rig = Rig::new("put-replay-committed");
    let out = dir("cas/b/1/n");
    {
        let store = rig.open();
        staged(&store, &out, "a.part", b"abc");
        commit(&store, &out, "a.part", "a", PutOutcome::Created).unwrap();
    }
    std::fs::write(rig.stage().join(stage_name(0)), b"abc").unwrap();
    let (store, replayed) = rig.open_replayed();
    assert_eq!((replayed.committed, replayed.orphans), (1, 1));
    let attr = store.lookup(&out, "a").unwrap();
    assert_eq!(
        (attr.size, attr.ms, attr.kind),
        (3, START_MS, Kind::Committed(sha(b"abc")))
    );
    drop(store);
    let (store, replayed) = rig.open_replayed();
    assert_eq!(
        replayed,
        Replayed {
            committed: 1,
            orphans: 1,
            ..Replayed::default()
        }
    );
    assert_eq!(store.lookup(&out, "a"), Some(attr));
    rig.clock.advance(600_000);
    assert_eq!(store.sweep().committed, 1);
    assert_eq!(store.lookup(&out, "a"), None);
}

/// Asserts that a committed name and a staged file outlive a sweep that rewrites the journal and a
/// restart after it, as they were.
#[test]
fn a_rewritten_journal_replays_to_the_same_names() {
    let rig = Rig::new("put-replay-compacted");
    let out = dir("cas/b/1/n");
    {
        let store = rig.open();
        staged(&store, &out, "a.part", b"abc");
        commit(&store, &out, "a.part", "a", PutOutcome::Created).unwrap();
        staged(&store, &out, "b.part", b"de");
        for _ in 0..40 {
            staged(&store, &out, "x.part", b"x");
            store.unlink(&out, "x.part").unwrap();
        }
        let _ = store.sweep();
        assert_eq!(locked(&store.table).journal.lines(), 3);
    }
    let (store, replayed) = rig.open_replayed();
    assert_eq!(
        replayed,
        Replayed {
            staged: 1,
            committed: 1,
            ..Replayed::default()
        }
    );
    let names: Vec<(String, u64, Kind)> = store
        .list(&out)
        .into_iter()
        .map(|(name, attr)| (name, attr.size, attr.kind))
        .collect();
    assert_eq!(
        names,
        vec![
            ("a".to_owned(), 3, Kind::Committed(sha(b"abc"))),
            ("b.part".to_owned(), 2, Kind::Staged),
        ]
    );
}

/// Asserts that a staged file the stage holds as a symbolic link is not followed: the start that
/// would open it is refused, naming the file.
#[test]
fn a_staged_file_that_is_a_symlink_is_not_followed() {
    let rig = Rig::new("put-replay-symlink");
    let outside = rig.root.path().join("outside");
    std::fs::write(&outside, b"secret").unwrap();
    journal_of(&rig, &[create(0, "cas/b/1/n", "a.part")]);
    std::os::unix::fs::symlink(&outside, rig.stage().join(stage_name(0))).unwrap();
    let path = rig.stage().join(stage_name(0));
    assert!(matches!(open_err(&rig), OpenError::Io(at, _) if at == path));
    assert_eq!(std::fs::read(&outside).unwrap(), b"secret");
}

/// Asserts that a staged entry whose file is gone is dropped at the start, and that the journal the
/// start writes no longer has it.
#[test]
fn a_staged_entry_whose_file_is_gone_is_dropped() {
    let rig = Rig::new("put-replay-dropped");
    let out = dir("cas/b/1/n");
    {
        let store = rig.open();
        staged(&store, &out, "a.part", b"abc");
    }
    std::fs::remove_file(rig.stage().join(stage_name(0))).unwrap();
    let (store, replayed) = rig.open_replayed();
    assert_eq!(replayed.dropped, 1);
    assert_eq!(store.lookup(&out, "a.part"), None);
    assert_eq!(rig.journal(), "");
}

/// Asserts that a staged file with no entry (its line never written) is kept until ten minutes
/// after the start and removed by the first sweep then, and that numbers go on after its own.
#[test]
fn a_file_with_no_entry_is_removed_ten_minutes_after_the_start() {
    let rig = Rig::new("put-replay-orphan");
    std::fs::create_dir_all(rig.stage()).unwrap();
    std::fs::write(rig.stage().join(stage_name(9)), b"lost").unwrap();
    let (store, replayed) = rig.open_replayed();
    assert_eq!(replayed.orphans, 1);
    rig.clock.advance(599_999);
    assert_eq!(store.sweep().orphans, 0);
    assert!(rig.stage().join(stage_name(9)).exists());
    rig.clock.advance(1);
    assert_eq!(store.sweep().orphans, 1);
    assert!(!rig.stage().join(stage_name(9)).exists());
    let _ = store.create(&dir("cas/b/1/n"), "a.part").unwrap();
    assert!(rig.stage().join(stage_name(10)).exists());
}

/// Asserts that the number of the next staged file comes after every number a line of the journal
/// names, even one whose name is gone, and after every staged file of the stage.
#[test]
fn numbers_are_never_used_twice() {
    let rig = Rig::new("put-replay-numbers");
    journal_of(
        &rig,
        &[create(41, "cas/b/1/n", "a.part"), Record::Remove { id: 41 }],
    );
    let store = rig.open();
    assert_eq!(locked(&store.table).next_id, 42);
    drop(store);
    std::fs::write(rig.stage().join(stage_name(50)), b"orphan").unwrap();
    journal_of(
        &rig,
        &[
            create(41, "cas/b/1/n", "a.part"),
            Record::Commit {
                id: 41,
                name: "a".to_owned(),
                sha256: sha(b"").to_string(),
                size: 0,
                ms: START_MS,
            },
        ],
    );
    let store = rig.open();
    assert_eq!(locked(&store.table).next_id, 51);
}

/// Asserts that a start skips, and counts, each line that cannot be read or names what cannot be,
/// and applies the rest: a torn line, a create of a bad directory, of a bad or final name, of a
/// number or a name taken; a rename of a number unknown or committed, to a bad, final or taken
/// name; a commit of a number unknown or committed, with a digest that is not one, to a bad or
/// `.part` name; a remove of a number unknown. A commit onto a committed name replaces it, and the
/// same name in another directory is another name.
#[test]
fn a_start_skips_the_lines_that_cannot_be_and_applies_the_rest() {
    let rig = Rig::new("put-replay-skips");
    let hex = sha(b"x").to_string();
    let commit = |id: u64, name: &str, sha256: &str| Record::Commit {
        id,
        name: name.to_owned(),
        sha256: sha256.to_owned(),
        size: 1,
        ms: START_MS,
    };
    let rename = |id: u64, name: &str| Record::Rename {
        id,
        name: name.to_owned(),
    };
    let records = vec![
        create(0, "cas/b/1/n", "a.part"),
        create(1, "cas/b/1/n", "b.part"),
        create(2, "cas/b/1/n", "c.part"),
        create(8, "cas/b/2/n", "a.part"),
        create(3, "cas/b//n", "x.part"),
        create(4, "cas/b/1/n", "x/y.part"),
        create(5, "cas/b/1/n", "final"),
        create(0, "cas/b/1/n", "z.part"),
        create(6, "cas/b/1/n", "a.part"),
        rename(9, "q.part"),
        rename(0, "."),
        rename(0, "final"),
        rename(0, "b.part"),
        commit(9, "q", &hex),
        commit(1, "bad", "not hex"),
        commit(1, "..", &hex),
        commit(1, "q.part", &hex),
        commit(1, "q", &hex),
        rename(1, "r.part"),
        commit(1, "q", &hex),
        commit(2, "q", &hex),
        Record::Remove { id: 9 },
    ];
    journal_of(&rig, &records);
    let mut bytes = std::fs::read(rig.stage().join("journal")).unwrap();
    bytes.extend(b"{\"op\":\"create\",\"id\":7");
    std::fs::write(rig.stage().join("journal"), bytes).unwrap();
    std::fs::write(rig.stage().join(stage_name(0)), b"a").unwrap();
    let (store, replayed) = rig.open_replayed();
    assert_eq!(replayed.skipped, 17);
    assert_eq!(replayed.staged, 1);
    assert_eq!(replayed.committed, 1);
    assert_eq!(replayed.dropped, 1);
    let out = dir("cas/b/1/n");
    let names: Vec<String> = store.list(&out).into_iter().map(|(name, _)| name).collect();
    assert_eq!(names, vec!["a.part".to_owned(), "q".to_owned()]);
    assert_eq!(locked(&store.table).names.len(), 2);
    let (live, _) = decode(rig.journal().as_bytes());
    assert_eq!(live.len(), 3);
}

/// Asserts that a start removes a `journal.new` a rewrite left, leaves files of other names in the
/// stage alone, and rewrites the journal with the live entries alone.
#[test]
fn a_start_rewrites_the_journal_and_leaves_other_files() {
    let rig = Rig::new("put-replay-rewrite");
    let out = dir("cas/b/1/n");
    {
        let store = rig.open();
        for _ in 0..3 {
            staged(&store, &out, "a.part", b"a");
            store.unlink(&out, "a.part").unwrap();
        }
        staged(&store, &out, "b.part", b"b");
    }
    std::fs::write(rig.stage().join(NEW_FILE), b"half").unwrap();
    std::fs::write(rig.stage().join("notes"), b"kept").unwrap();
    let _store = rig.open();
    assert_eq!(
        rig.stage_files(),
        vec![stage_name(3), "journal".to_owned(), "notes".to_owned()]
    );
    let (live, skipped) = decode(rig.journal().as_bytes());
    assert_eq!((live, skipped), (vec![create(3, "cas/b/1/n", "b.part")], 0));
}

/// Asserts that the quotas count what a start found: a directory at `max_dir_files` before the
/// restart is at it after.
#[test]
fn the_quotas_count_what_a_start_found() {
    let mut rig = Rig::new("put-replay-quota");
    rig.settings.max_dir_files = Some(1);
    rig.settings.max_dir_bytes = Some(3);
    let out = dir("cas/b/1/n");
    {
        let store = rig.open();
        staged(&store, &out, "a.part", b"abc");
    }
    let store = rig.open();
    assert!(matches!(
        store.create(&out, "b.part"),
        Err(PutError::Quota("files"))
    ));
    store.unlink(&out, "a.part").unwrap();
    let handle = written(&store, &out, "b.part", b"abc");
    assert!(matches!(
        store.write(handle, 3, b"d"),
        Err(PutError::Quota("bytes"))
    ));
}

/// Asserts that the stage directory is made, mode 0700, when it is not there.
#[test]
fn the_stage_directory_is_made_for_this_process_alone() {
    let rig = Rig::new("put-replay-made");
    let _store = rig.open();
    let mode = std::fs::metadata(rig.stage()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
}

/// Asserts that a stage and a cache on volumes of different device numbers are refused, naming
/// both, and that the same device number is taken.
#[test]
fn the_stage_must_be_on_the_volume_of_the_cache() {
    let (stage, cache) = (Path::new("/s"), Path::new("/c"));
    same_volume((stage, 7), (cache, 7)).unwrap();
    let refused = same_volume((stage, 7), (cache, 8)).unwrap_err();
    assert!(matches!(
        refused,
        OpenError::OtherVolume { stage, cache } if stage == Path::new("/s") && cache == Path::new("/c")
    ));
}

fn open_err(rig: &Rig) -> OpenError {
    StagedStore::open(
        rig.settings.clone(),
        &rig.stage(),
        &rig.cache(),
        rig.clock.clone(),
        rig.disk.clone(),
    )
    .unwrap_err()
}

/// Asserts that an open is refused, naming what, for a prefix that is not a key prefix and a `/`,
/// a stage that is a file, a cache that is not there, a `journal.new` that cannot be removed, and a
/// journal that cannot be read.
#[test]
fn an_open_is_refused_for_a_bad_prefix_or_a_stage_it_cannot_use() {
    let mut rig = Rig::new("put-open-refused");
    rig.settings.prefixes = vec!["cas/b".to_owned()];
    assert!(matches!(open_err(&rig), OpenError::Prefix(prefix) if prefix == "cas/b"));
    rig.settings.prefixes = vec!["cas/b/".to_owned()];

    std::fs::write(rig.stage(), b"file").unwrap();
    assert!(matches!(open_err(&rig), OpenError::Io(path, _) if path == rig.stage()));
    std::fs::remove_file(rig.stage()).unwrap();

    std::fs::remove_dir(rig.cache()).unwrap();
    assert!(matches!(open_err(&rig), OpenError::Io(path, _) if path == rig.cache()));
    std::fs::create_dir(rig.cache()).unwrap();

    std::fs::create_dir_all(rig.stage().join(NEW_FILE).join("x")).unwrap();
    assert!(matches!(open_err(&rig), OpenError::Io(path, _) if path == rig.stage().join(NEW_FILE)));
    std::fs::remove_dir_all(rig.stage().join(NEW_FILE)).unwrap();

    std::fs::create_dir(rig.stage().join("journal")).unwrap();
    assert!(matches!(open_err(&rig), OpenError::Io(path, _) if path == rig.stage()));
    std::fs::remove_dir(rig.stage().join("journal")).unwrap();

    let _store = rig.open();
    assert_eq!(rig.settings.committed_ttl, Duration::from_secs(600));
}
