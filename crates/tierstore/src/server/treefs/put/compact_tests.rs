//! The rewrite of the journal outside the table's lock: a request that comes while the new file is
//! written is not held and its record is in the new journal, in order; a rewrite that fails at the
//! rename leaves the journal as it was.

use std::sync::Arc;

use super::commit::PutOutcome;
use super::journal::{NEW_FILE, Record, decode};
use super::store::StagedStore;
use super::sweep::COMPACT_SLACK;
use super::testing::{Rig, commit, dir, staged, warnings};
use crate::server::treefs::locked;

/// Stages and removes files until the journal is due for a rewrite with `keep.part` live.
fn dead_lines(store: &StagedStore) {
    let out = dir("cas/b/1/n");
    staged(store, &out, "keep.part", b"k");
    for _ in 0..COMPACT_SLACK {
        staged(store, &out, "a.part", b"a");
        store.unlink(&out, "a.part").unwrap();
    }
}

/// Sets what the rewrite does while its new file is written, outside the table's lock.
fn while_rewriting(store: &StagedStore, hook: impl Fn() + Send + Sync + 'static) {
    *locked(&store.while_rewriting) = Some(Arc::new(hook));
}

/// Asserts that while the new journal is written the table's lock is free and a create, a rename,
/// a removal and a commit complete; that the journal then holds the live entries of the snapshot
/// and those records after them, which is as many lines as the table counts; and that a start over
/// that journal has the names, the sizes and the kinds the store had.
#[test]
fn a_rewrite_holds_no_lock_while_it_writes_and_keeps_the_records_made_meanwhile() {
    let rig = Rig::new("put-compact-meanwhile");
    let store = Arc::new(rig.open());
    let out = dir("cas/b/1/n");
    dead_lines(&store);
    let inside = Arc::clone(&store);
    while_rewriting(&store, move || {
        let out = dir("cas/b/1/n");
        assert!(inside.table.try_lock().is_ok(), "the table is locked");
        let handle = inside.create(&out, "during.part").unwrap();
        assert_eq!(inside.write(handle, 0, b"during").unwrap(), 6);
        let _ = inside.release(handle);
        let _ = inside.rename(&out, "during.part", "moved.part").unwrap();
        staged(&inside, &out, "gone.part", b"g");
        inside.unlink(&out, "gone.part").unwrap();
        staged(&inside, &out, "late.part", b"late");
        commit(&inside, &out, "late.part", "late", PutOutcome::Created).unwrap();
    });
    let _ = store.sweep();
    *locked(&store.while_rewriting) = None;
    let before = store.list(&out);
    assert_eq!(before.len(), 3);
    let (records, skipped) = decode(rig.journal().as_bytes());
    assert_eq!(skipped, 0);
    // The snapshot's one create, then the six records of the requests made meanwhile.
    assert_eq!(records.len(), 7);
    assert_eq!(locked(&store.table).journal.lines(), records.len() as u64);
    assert!(matches!(&records[0], Record::Create { name, .. } if name == "keep.part"));
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Create { name, .. } if name == "during.part"
    )));
    assert!(locked(&store.table).captured.is_none());
    assert!(!rig.stage().join(NEW_FILE).exists());
    drop(store);
    let again = rig.open();
    assert_eq!(again.list(&out), before);
}

/// Asserts that a rewrite whose new file is gone when it is to be put in place (the rename fails)
/// says so in full, leaves the journal as it was, still appended to, and that the next sweep
/// rewrites it.
#[test]
fn a_rewrite_that_fails_at_the_rename_leaves_the_journal() {
    let rig = Rig::new("put-compact-rename-fails");
    let store = Arc::new(rig.open());
    let (_log, warnings) = warnings();
    let out = dir("cas/b/1/n");
    dead_lines(&store);
    let lines = locked(&store.table).journal.lines();
    let stage = rig.stage();
    while_rewriting(&store, move || {
        std::fs::remove_file(stage.join(NEW_FILE)).unwrap();
    });
    let _ = store.sweep();
    *locked(&store.while_rewriting) = None;
    assert_eq!(
        warnings.text(),
        format!(
            "tierstore fs: the put journal in {} is not rewritten: No such file or directory (os error 2)\n",
            rig.stage().display()
        )
    );
    assert_eq!(locked(&store.table).journal.lines(), lines);
    assert!(locked(&store.table).captured.is_none());
    staged(&store, &out, "b.part", b"b");
    let (records, _) = decode(rig.journal().as_bytes());
    assert_eq!(records.len() as u64, lines + 1);
    let _ = store.sweep();
    assert_eq!(locked(&store.table).journal.lines(), 2);
}
