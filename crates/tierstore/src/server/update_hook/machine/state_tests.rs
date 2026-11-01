//! The state directory on real temporary directories.
//!
//! The tests that make a directory read-only assume that they do not run as root, which a root
//! user's permissions would ignore.

use std::fs;
use std::os::fd::AsFd;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use rustix::fs::{Mode, OFlags, open};

use super::super::ports::{PortError, PortKind, StateDir};
use super::state::try_lock;
use super::testing::scratch;
use super::{LOCK_FILE, STATE_FILE, StateFiles};

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn files(dir: &Path) -> StateFiles {
    StateFiles::new(dir.to_path_buf(), me())
}

fn mode(dir: &Path, bits: u32) {
    fs::set_permissions(dir, fs::Permissions::from_mode(bits)).unwrap();
}

fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::State,
            message,
        } => message,
        other => panic!("not a state failure: {other:?}"),
    }
}

fn refused(state: &StateFiles) -> String {
    message(state.lock().err().expect("the directory is refused"))
}

/// Asserts that a directory owned by the expected user, with no group or other write bit,
/// takes the lock, for the modes 0755 and 0700; and that the lock file is made.
#[test]
fn a_directory_that_is_as_it_must_be_takes_the_lock() {
    let (_guard, dir) = scratch("state-ok");
    for bits in [0o755, 0o700] {
        mode(&dir, bits);
        assert!(files(&dir).lock().is_ok());
    }
    assert!(dir.join(LOCK_FILE).is_file());
}

/// Asserts that a second attempt on the lock, while the first is held, is `Busy`, and that
/// once the first is dropped the lock can be taken again. Two descriptors of one process
/// conflict on `flock`, so this is the contention of two steps. It would catch a lock that is
/// not exclusive, or one that is not released on drop.
#[test]
fn the_lock_is_exclusive_and_released_on_drop() {
    let (_guard, dir) = scratch("state-busy");
    mode(&dir, 0o755);
    let state = files(&dir);
    let first = state.lock().ok().expect("the first lock");
    assert!(matches!(state.lock().err(), Some(PortError::Busy)));
    drop(first);
    assert!(state.lock().is_ok());
}

/// Asserts that a directory owned by someone other than the expected user is refused, and the
/// message names both users. It would catch a directory the service owns.
#[test]
fn a_directory_of_another_owner_is_refused() {
    let (_guard, dir) = scratch("state-owner");
    let state = StateFiles::new(dir.clone(), me() + 1);
    let text = refused(&state);
    assert!(
        text.contains(&format!("is owned by user {}, not {}", me(), me() + 1)),
        "{text}"
    );
}

/// Asserts that a directory a group can write, and one others can write, are refused: the
/// service could rewrite the record.
#[test]
fn a_directory_that_a_group_or_others_can_write_is_refused() {
    let (_guard, dir) = scratch("state-mode");
    for bits in [0o775, 0o757] {
        mode(&dir, bits);
        let text = refused(&files(&dir));
        assert!(
            text.contains("can be written by a group or others"),
            "{text}"
        );
    }
}

/// Asserts that a symbolic link to a good directory is refused, as is a file and a path that is
/// not there.
#[test]
fn a_link_a_file_and_a_missing_path_are_refused() {
    let (_guard, root) = scratch("state-kinds");
    let real = root.join("real");
    fs::create_dir(&real).unwrap();
    mode(&real, 0o755);
    let link = root.join("link");
    symlink(&real, &link).unwrap();
    assert!(refused(&files(&link)).contains("is a symbolic link"));
    let file = root.join("file");
    fs::write(&file, "x").unwrap();
    assert!(refused(&files(&file)).contains("is not a directory"));
    assert!(refused(&files(&root.join("absent"))).starts_with("state directory:"));
}

/// Asserts that a lock file that is a symbolic link is not followed: the lock is refused.
#[test]
fn a_lock_file_that_is_a_link_is_refused() {
    let (_guard, dir) = scratch("state-lock-link");
    mode(&dir, 0o755);
    symlink(dir.join("elsewhere"), dir.join(LOCK_FILE)).unwrap();
    let text = refused(&files(&dir));
    assert!(text.starts_with("open "), "{text}");
}

/// Asserts that a failure of `flock` that is not contention is an error, not `Busy`: a
/// descriptor that cannot be locked (opened for its path only) is `EBADF`.
#[test]
fn a_flock_failure_other_than_contention_is_an_error() {
    let (_guard, dir) = scratch("state-ebadf");
    let fd = open(dir.as_path(), OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).unwrap();
    let text = message(try_lock(fd.as_fd()).unwrap_err());
    assert!(text.starts_with("lock:"), "{text}");
}

/// Asserts that there is no record before the first save, and that a record saved is the
/// record loaded, byte for byte, replaced whole by the next save.
#[test]
fn a_record_saved_is_the_record_loaded() {
    let (_guard, dir) = scratch("state-roundtrip");
    let state = files(&dir);
    assert_eq!(state.load().unwrap(), None);
    state.save(b"{\"schema\":1}").unwrap();
    assert_eq!(
        state.load().unwrap().as_deref(),
        Some(&b"{\"schema\":1}"[..])
    );
    state.save(b"{}").unwrap();
    assert_eq!(state.load().unwrap().as_deref(), Some(&b"{}"[..]));
}

/// Asserts that the file is mode 0600 and no temporary file is left beside it.
#[test]
fn the_record_is_private_and_leaves_no_temporary_file() {
    let (_guard, dir) = scratch("state-private");
    files(&dir).save(b"x").unwrap();
    let meta = fs::metadata(dir.join(STATE_FILE)).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let mut names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, vec![STATE_FILE]);
}

/// Asserts that a temporary file left by a crash, longer than the new record, does not leak
/// into it: the record is exactly the new bytes.
#[test]
fn a_stale_temporary_file_does_not_leak_into_the_record() {
    let (_guard, dir) = scratch("state-stale");
    fs::write(dir.join("state.json.tmp"), "garbage garbage garbage").unwrap();
    files(&dir).save(b"new").unwrap();
    assert_eq!(files(&dir).load().unwrap().as_deref(), Some(&b"new"[..]));
}

/// Asserts that a save that cannot write leaves the old record as it was and says so: the
/// directory is read-only.
#[test]
fn a_save_that_cannot_write_leaves_the_old_record() {
    let (_guard, dir) = scratch("state-readonly");
    let state = files(&dir);
    state.save(b"old").unwrap();
    mode(&dir, 0o500);
    let result = state.save(b"new");
    mode(&dir, 0o755);
    assert!(message(result.unwrap_err()).starts_with("write "));
    assert_eq!(state.load().unwrap().as_deref(), Some(&b"old"[..]));
}

/// Asserts that a failed rename is an error, leaves no temporary file, and keeps what holds
/// the record's name: a directory there makes the rename fail.
#[test]
fn a_save_whose_rename_fails_removes_its_temporary_file() {
    let (_guard, dir) = scratch("state-rename");
    fs::create_dir(dir.join(STATE_FILE)).unwrap();
    let state = files(&dir);
    assert!(state.save(b"x").is_err());
    assert!(!dir.join("state.json.tmp").exists());
    assert!(dir.join(STATE_FILE).is_dir());
}

/// Asserts that a record that cannot be read (a directory is at its name) is an error and not
/// an absent record: an absent one would let a first install lay a record over it.
#[test]
fn a_record_that_cannot_be_read_is_an_error_not_an_absent_record() {
    let (_guard, dir) = scratch("state-unreadable");
    fs::create_dir(dir.join(STATE_FILE)).unwrap();
    assert!(message(files(&dir).load().unwrap_err()).starts_with("read "));
}
