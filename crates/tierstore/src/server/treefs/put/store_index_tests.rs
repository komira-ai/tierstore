//! The handles of each staged file and the keys of the running commits, which the table keeps with
//! its maps so that a commit's checks and a removal do not scan them.

use super::commit::{HANDLE_OPEN, KEY_RUNNING, PutOutcome};
use super::error::PutError;
use super::store::Rename;
use super::testing::{Rig, dir, staged, written};
use crate::server::treefs::locked;

/// Asserts that a release gives the number of its file and the file's other handles stay, that the
/// last release of a file leaves nothing of it in the index, and that the release of a handle that
/// is not open gives nothing.
#[test]
fn a_release_gives_the_file_of_the_handle_and_the_last_leaves_no_index() {
    let rig = Rig::new("put-index-release");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let first = written(&store, &out, "a.part", b"a");
    let (id, _) = store.find(&out, "a.part").unwrap();
    let second = store.adopt(id).unwrap();
    let other = written(&store, &out, "b.part", b"b");
    assert_eq!(store.handles_of(id), vec![first.0, second.0]);
    assert_eq!(store.release(first), Some(id));
    assert_eq!(store.handles_of(id), vec![second.0]);
    assert_eq!(store.release(first), None);
    assert!(matches!(
        store.rename(&out, "a.part", "a"),
        Err(PutError::Busy(HANDLE_OPEN))
    ));
    assert_eq!(store.release(second), Some(id));
    assert!(store.handles_of(id).is_empty());
    assert_eq!(locked(&store.table).open_by_file.len(), 1);
    assert!(store.release(other).is_some());
    assert!(locked(&store.table).open_by_file.is_empty());
    assert!(locked(&store.table).handles.is_empty());
}

/// Asserts that removing a file ends every handle of it and no other file's, whether by unlink or
/// by the sweep.
#[test]
fn removing_a_file_ends_all_its_handles_and_no_others() {
    let rig = Rig::new("put-index-remove");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let gone = written(&store, &out, "a.part", b"a");
    let (id, _) = store.find(&out, "a.part").unwrap();
    let also = store.adopt(id).unwrap();
    let kept = written(&store, &out, "b.part", b"b");
    store.unlink(&out, "a.part").unwrap();
    assert!(store.handles_of(id).is_empty());
    for handle in [gone, also] {
        assert!(matches!(
            store.write(handle, 1, b"x"),
            Err(PutError::BadHandle)
        ));
    }
    assert_eq!(store.write(kept, 1, b"x").unwrap(), 1);
    let (swept, _) = store.find(&out, "b.part").unwrap();
    rig.clock.advance(43_200_000);
    assert_eq!(store.sweep().parts, 1);
    assert!(store.handles_of(swept).is_empty());
    assert!(locked(&store.table).open_by_file.is_empty());
    assert!(locked(&store.table).handles.is_empty());
}

/// Asserts that the key of a running commit is refused to another commit with its own reason, that
/// a commit that is dropped, or finished, or fails, frees the key, and that the file's own reason
/// is another.
#[test]
fn the_key_of_a_running_commit_is_held_until_it_ends() {
    let rig = Rig::new("put-index-keys");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "a.part", b"a");
    staged(&store, &out, "b.part", b"b");
    let Rename::Commit(running) = store.rename(&out, "a.part", "k").unwrap() else {
        panic!("a commit");
    };
    assert!(locked(&store.table).uploading_keys.contains("cas/b/1/n/k"));
    assert!(matches!(
        store.rename(&out, "b.part", "k"),
        Err(PutError::Busy(KEY_RUNNING))
    ));
    assert!(matches!(
        store.probe_commit(&out, "b.part", "k"),
        Err(PutError::Busy(KEY_RUNNING))
    ));
    assert!(matches!(
        store.probe_commit(&out, "a.part", "other"),
        Err(PutError::Busy(super::commit::FILE_RUNNING))
    ));
    drop(running);
    assert!(locked(&store.table).uploading_keys.is_empty());
    assert!(locked(&store.table).uploading.is_empty());
    store.probe_commit(&out, "b.part", "k").unwrap();
    let Rename::Commit(running) = store.rename(&out, "a.part", "k").unwrap() else {
        panic!("a commit");
    };
    running.finish(PutOutcome::Created).unwrap();
    assert!(locked(&store.table).uploading_keys.is_empty());
    let Rename::Commit(running) = store.rename(&out, "b.part", "j").unwrap() else {
        panic!("a commit");
    };
    assert!(running.finish(PutOutcome::Checksum).is_err());
    assert!(locked(&store.table).uploading_keys.is_empty());
}
