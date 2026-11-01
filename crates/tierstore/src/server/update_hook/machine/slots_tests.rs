//! The slot links on real temporary directories.
//!
//! The tests that make a directory read-only assume that they do not run as root, which a
//! root user's permissions would ignore.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::super::ports::{PortError, PortKind, Slots};
use super::SlotLinks;
use super::testing::{files, scratch_state};

fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Slots,
            message,
        } => message,
        other => panic!("not a slots failure: {other:?}"),
    }
}

fn state_message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::State,
            message,
        } => message,
        other => panic!("not a state failure: {other:?}"),
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Asserts that a port with no link, and a state directory with no `slots` directory yet, have
/// no target.
#[test]
fn a_slot_with_no_link_has_no_target() {
    let (_guard, root) = scratch_state("slots-none");
    let state = files(&root);
    assert_eq!(SlotLinks::new(&state).target(9301).unwrap(), None);
}

/// Asserts that `point` makes the `slots` directory when it is absent, links `slots/<port>` to
/// the release as given, and that `target` reads it back. It would catch a link at the wrong
/// path or one made absolute.
#[test]
fn point_makes_the_link_and_target_reads_it() {
    let (_guard, root) = scratch_state("slots-point");
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    slots.point(9301, Path::new("/releases/one")).unwrap();
    assert_eq!(
        fs::read_link(root.join("slots/9301")).unwrap(),
        PathBuf::from("/releases/one")
    );
    assert_eq!(
        slots.target(9301).unwrap(),
        Some(PathBuf::from("/releases/one"))
    );
    assert_eq!(slots.target(9302).unwrap(), None);
}

/// Asserts that pointing an existing link elsewhere replaces it and leaves no temporary link
/// beside it, and that pointing it at where it already points is fine (the steps run twice).
#[test]
fn point_replaces_the_link_and_leaves_nothing_behind() {
    let (_guard, root) = scratch_state("slots-replace");
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    slots.point(9301, Path::new("/releases/one")).unwrap();
    slots.point(9301, Path::new("/releases/two")).unwrap();
    slots.point(9301, Path::new("/releases/two")).unwrap();
    assert_eq!(
        slots.target(9301).unwrap(),
        Some(PathBuf::from("/releases/two"))
    );
    assert_eq!(names(&root.join("slots")), vec!["9301".to_owned()]);
}

/// Asserts that a temporary link left by a crash between making it and renaming it does not
/// stop the next `point`.
#[test]
fn point_goes_over_a_temporary_link_a_crash_left() {
    let (_guard, root) = scratch_state("slots-stale");
    fs::create_dir_all(root.join("slots")).unwrap();
    symlink("/stale", root.join("slots/.9301.tmp")).unwrap();
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    slots.point(9301, Path::new("/releases/one")).unwrap();
    assert_eq!(
        slots.target(9301).unwrap(),
        Some(PathBuf::from("/releases/one"))
    );
    assert_eq!(names(&root.join("slots")), vec!["9301".to_owned()]);
}

/// Asserts that a reader never finds the link missing while another thread moves it between two
/// releases: every read gives one of the two. It would catch a replace done as remove then link.
#[test]
fn a_reader_never_sees_the_link_missing_while_it_moves() {
    let (_guard, root) = scratch_state("slots-atomic");
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    slots.point(9301, Path::new("/releases/a")).unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let state = files(&root);
            let reads = SlotLinks::new(&state);
            loop {
                let target = reads.target(9301).unwrap().unwrap();
                assert!(
                    [Path::new("/releases/a"), Path::new("/releases/b")]
                        .contains(&target.as_path())
                );
                if stop.load(Ordering::SeqCst) {
                    break;
                }
            }
        });
        for turn in 0..300 {
            let release = ["/releases/b", "/releases/a"][turn % 2];
            slots.point(9301, Path::new(release)).unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        reader.join().unwrap();
    });
}

/// Asserts that a path that is not a link, a regular file at `slots/<port>`, is an error that
/// says so, for `target` and for `remove`, and that `remove` keeps the file.
#[test]
fn a_path_that_is_not_a_link_is_refused() {
    let (_guard, root) = scratch_state("slots-file");
    fs::create_dir_all(root.join("slots")).unwrap();
    fs::write(root.join("slots/9301"), "x").unwrap();
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    let text = message(slots.target(9301).unwrap_err());
    assert!(
        text.ends_with("slots/9301 exists and is not a symbolic link"),
        "{text}"
    );
    assert!(slots.remove(9301).is_err());
    assert!(root.join("slots/9301").exists());
}

/// Asserts that `slots` being a file is an error for `target` and `point`, not an absent link.
#[test]
fn a_slots_path_that_is_a_file_is_an_error() {
    let (_guard, root) = scratch_state("slots-blocked");
    fs::write(root.join("slots"), "x").unwrap();
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    assert!(message(slots.target(9301).unwrap_err()).starts_with("read "));
    let text = message(slots.point(9301, Path::new("/r")).unwrap_err());
    assert!(text.starts_with("link "), "{text}");
}

/// Asserts that a link that cannot be replaced, because a directory holds its name, is an error,
/// the directory stays, and the temporary link is cleaned away.
#[test]
fn point_failing_leaves_what_was_there_and_no_temporary_link() {
    let (_guard, root) = scratch_state("slots-rename");
    fs::create_dir_all(root.join("slots/9301/inside")).unwrap();
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    assert!(slots.point(9301, Path::new("/r")).is_err());
    assert!(root.join("slots/9301/inside").is_dir());
    assert_eq!(names(&root.join("slots")), vec!["9301".to_owned()]);
}

/// Asserts that `remove` takes the link away, that removing an absent link is fine, and that it
/// removes only the one port's link.
#[test]
fn remove_is_idempotent() {
    let (_guard, root) = scratch_state("slots-remove");
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    slots.remove(9301).unwrap();
    slots.point(9301, Path::new("/r1")).unwrap();
    slots.point(9302, Path::new("/r2")).unwrap();
    slots.remove(9301).unwrap();
    slots.remove(9301).unwrap();
    assert_eq!(slots.target(9301).unwrap(), None);
    assert_eq!(slots.target(9302).unwrap(), Some(PathBuf::from("/r2")));
}

/// Asserts that a link that cannot be removed (its directory is read-only) is an error.
#[test]
fn remove_that_cannot_unlink_is_an_error() {
    let (_guard, root) = scratch_state("slots-readonly");
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    slots.point(9301, Path::new("/r")).unwrap();
    let dir = root.join("slots");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    let result = slots.remove(9301);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(message(result.unwrap_err()).starts_with("remove "));
    assert_eq!(slots.target(9301).unwrap(), Some(PathBuf::from("/r")));
}

/// Asserts that the links are read and made through the state directory that was checked: after
/// the first use, the path is replaced by another directory holding a forged link; `target`
/// still reads the original's link and `point` writes there. It would catch a port that opens
/// `slots` by the path again.
#[test]
fn the_links_are_reached_through_the_directory_that_was_checked() {
    let (guard, root) = scratch_state("slots-swap");
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    slots.point(9301, Path::new("/releases/mine")).unwrap();
    let moved = guard.path().join("moved");
    fs::rename(&root, &moved).unwrap();
    fs::create_dir_all(root.join("slots")).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
    symlink("/releases/forged", root.join("slots/9301")).unwrap();
    assert_eq!(
        slots.target(9301).unwrap(),
        Some(PathBuf::from("/releases/mine"))
    );
    slots.point(9302, Path::new("/releases/next")).unwrap();
    assert!(fs::symlink_metadata(root.join("slots/9302")).is_err());
    assert!(fs::symlink_metadata(moved.join("slots/9302")).is_ok());
}

/// Asserts that a `slots` that is a symbolic link to a directory is not followed: `target`,
/// `point` and `remove` refuse it and nothing is made in what it points at.
#[test]
fn a_slots_directory_that_is_a_link_is_not_followed() {
    let (guard, root) = scratch_state("slots-link");
    let elsewhere = guard.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    symlink(&elsewhere, root.join("slots")).unwrap();
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    assert!(message(slots.target(9301).unwrap_err()).starts_with("read "));
    assert!(message(slots.point(9301, Path::new("/r")).unwrap_err()).starts_with("link "));
    assert!(message(slots.remove(9301).unwrap_err()).starts_with("remove "));
    assert!(names(&elsewhere).is_empty());
}

/// Asserts that a state directory that is not as it must be (its mode is wrong) is refused by
/// every call on the links, with the state port's words, and nothing is made.
#[test]
fn a_state_directory_that_is_refused_refuses_every_link_call() {
    let (_guard, root) = scratch_state("slots-refused");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    let state = files(&root);
    let slots = SlotLinks::new(&state);
    for text in [
        state_message(slots.target(9301).unwrap_err()),
        state_message(slots.point(9301, Path::new("/r")).unwrap_err()),
        state_message(slots.remove(9301).unwrap_err()),
    ] {
        assert!(text.contains("has mode"), "{text}");
    }
    assert!(!root.join("slots").exists());
}
