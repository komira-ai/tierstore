//! The open files in their maps: handles of one map, a release of a handle that is not open, and
//! many opens.

use std::sync::Arc;

use super::{Handle, Handles};
use crate::server::treefs::core::Errno;
use crate::server::treefs::locked;
use crate::server::treefs::testing::{Rig, open_cached, sample};

/// Asserts that two handles of one map (numbers 64 apart) are kept and ended separately: the
/// release of one leaves the other, and a handle put again under a number replaces the file.
#[test]
fn two_handles_in_one_map_are_kept_and_released_separately() {
    let handles = Handles::new();
    handles.insert(1, Arc::new(Handle::Empty));
    handles.insert(65, Arc::new(Handle::Empty));
    assert_eq!(handles.len(), 2);
    assert!(handles.contains(1) && handles.contains(65));
    let held = locked(handles.shard(1));
    assert!(!handles.free(65), "a handle of the same map waits");
    assert!(handles.free(2), "a handle of another map does not");
    drop(held);
    handles.remove(1);
    assert!(!handles.contains(1));
    assert!(handles.get(65).is_some());
    handles.insert(65, Arc::new(Handle::Adopted { ino: 1 }));
    assert_eq!(handles.len(), 1);
    assert!(matches!(handles.get(65).as_deref(), Some(Handle::Adopted { .. })));
}

/// Asserts that a handle put in only if absent leaves the file that is open as the number.
#[test]
fn a_handle_put_in_if_absent_leaves_the_open_one() {
    let handles = Handles::new();
    handles.insert_if_absent(7, Handle::Empty);
    handles.insert_if_absent(7, Handle::Adopted { ino: 1 });
    assert!(matches!(handles.get(7).as_deref(), Some(Handle::Empty)));
    assert_eq!(handles.len(), 1);
}

/// Asserts that a release of a handle that is not open, and a read of one, change nothing: the
/// files open stay, and the read is `EINVAL`.
#[test]
fn the_release_of_an_unknown_handle_changes_nothing() {
    let rig = Rig::new("tfs-handles-unknown", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    let core = &rig.core;
    let fh = open_cached(core, crate::server::treefs::ino::of(1, 3), false).unwrap();
    core.release(fh + 1_000_003);
    assert_eq!(core.open_files(), 1);
    assert_eq!(core.read(fh + 1_000_003, 0, 1), Err(Errno::Inval));
    core.release(fh);
    assert_eq!(core.open_files(), 0);
}

/// Asserts that many files open at once, more than there are maps, each read and each released,
/// leave none open, and that each open is a handle of its own.
#[test]
fn many_opens_are_each_a_handle_and_all_are_released() {
    let rig = Rig::new("tfs-handles-many", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    let core = &rig.core;
    let file = crate::server::treefs::ino::of(1, 3);
    let opened: Vec<u64> = (0..200)
        .map(|_| open_cached(core, file, false).unwrap())
        .collect();
    let mut numbers = opened.clone();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers.len(), 200);
    assert_eq!(core.open_files(), 200);
    for fh in &opened {
        assert!(core.read(*fh, 0, 4).is_ok());
    }
    opened.into_iter().for_each(|fh| core.release(fh));
    assert_eq!(core.open_files(), 0);
}
