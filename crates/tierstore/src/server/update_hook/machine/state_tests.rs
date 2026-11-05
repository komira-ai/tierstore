use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use super::super::ports::{PortError, PortKind, StateDir};
use super::testing::{files, me, scratch, scratch_state};
use super::{LOCK_FILE, STATE_FILE, StateFiles};

fn mode(path: &Path, bits: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
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

/// Asserts that a directory owned by the expected user with mode 0700 takes the lock, and that the lock file is made.
#[test]
fn a_directory_that_is_as_it_must_be_takes_the_lock() {
    let (_guard, dir) = scratch_state("state-ok");
    assert!(files(&dir).lock().is_ok());
    assert!(dir.join(LOCK_FILE).is_file());
}

/// Asserts that a second attempt on the lock, while the first is held, is `Busy`, and that
/// once the first is dropped the lock can be taken again. Two descriptors of one process
/// conflict on `flock`, so this is the contention of two steps. It would catch a lock that is
/// not exclusive, or one that is not released on drop.
#[test]
fn the_lock_is_exclusive_and_released_on_drop() {
    let (_guard, dir) = scratch_state("state-busy");
    let state = files(&dir);
    let first = state.lock().expect("the first lock");
    assert!(matches!(state.lock().err(), Some(PortError::Busy)));
    drop(first);
    assert!(state.lock().is_ok());
}

/// Asserts that a directory owned by someone other than the expected user is refused, and the
/// message names both users. It would catch a directory another account owns.
#[test]
fn a_directory_of_another_owner_is_refused() {
    let (_guard, dir) = scratch_state("state-owner");
    let state = StateFiles::new(dir.clone(), me() + 1);
    let text = refused(&state);
    assert!(
        text.contains(&format!("is owned by user {}, not {}", me(), me() + 1)),
        "{text}"
    );
}

/// Asserts that only the mode 0700 is accepted: 0750, 0500, 0755, 0770, 0701 and 02700 (setgid)
/// are refused, and the message names the mode. It would catch a check of single bits.
#[test]
fn only_the_mode_0700_is_accepted() {
    let (_guard, dir) = scratch_state("state-mode");
    for bits in [0o750, 0o500, 0o755, 0o770, 0o701, 0o2700] {
        mode(&dir, bits);
        let text = refused(&files(&dir));
        assert!(text.contains("has mode"), "{bits:o}: {text}");
        assert!(text.contains("not 0700"), "{bits:o}: {text}");
    }
    mode(&dir, 0o000);
    let text = refused(&files(&dir));
    mode(&dir, 0o700);
    assert!(text.starts_with("state directory:"), "{text}");
    assert!(files(&dir).lock().is_ok());
}

/// Asserts that a symbolic link to a good directory is refused, as is a file, and a path under a
/// file, which cannot be looked at and is not made.
#[test]
fn a_link_a_file_and_a_missing_path_are_refused() {
    let (guard, dir) = scratch_state("state-kinds");
    let link = guard.path().join("link");
    symlink(&dir, &link).unwrap();
    assert!(refused(&files(&link)).contains("is a symbolic link"));
    let file = guard.path().join("file");
    fs::write(&file, "x").unwrap();
    assert!(refused(&files(&file)).contains("is not a directory"));
    assert!(refused(&files(&file.join("state"))).contains("is not a directory"));
}

/// Asserts that a lock file that is a symbolic link is not followed: the lock is refused.
#[test]
fn a_lock_file_that_is_a_link_is_refused() {
    let (_guard, dir) = scratch_state("state-lock-link");
    symlink(dir.join("elsewhere"), dir.join(LOCK_FILE)).unwrap();
    let text = refused(&files(&dir));
    assert!(text.starts_with("open "), "{text}");
}

/// Asserts that a failure of `flock` that is not contention is an error, not `Busy`: a
/// descriptor that cannot be locked (opened for its path only) is `EBADF`. `O_PATH` is Linux's,
/// so the test is too.
#[cfg(target_os = "linux")]
#[test]
fn a_flock_failure_other_than_contention_is_an_error() {
    use std::os::fd::AsFd;

    use rustix::fs::{Mode, OFlags, open};

    use super::state::try_lock;

    let (_guard, dir) = scratch_state("state-ebadf");
    let fd = open(dir.as_path(), OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).unwrap();
    let text = message(try_lock(fd.as_fd()).unwrap_err());
    assert!(text.starts_with("lock:"), "{text}");
}

/// Asserts that there is no record before the first save, and that a record saved is the
/// record loaded, byte for byte, replaced whole by the next save.
#[test]
fn a_record_saved_is_the_record_loaded() {
    let (_guard, dir) = scratch_state("state-roundtrip");
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
    let (_guard, dir) = scratch_state("state-private");
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
    let (_guard, dir) = scratch_state("state-stale");
    fs::write(dir.join("state.json.tmp"), "garbage garbage garbage").unwrap();
    files(&dir).save(b"new").unwrap();
    assert_eq!(files(&dir).load().unwrap().as_deref(), Some(&b"new"[..]));
}

/// Asserts that a save that cannot write leaves the old record as it was and says so: the
/// directory, already open, is read-only.
#[test]
fn a_save_that_cannot_write_leaves_the_old_record() {
    let (_guard, dir) = scratch_state("state-readonly");
    let state = files(&dir);
    state.save(b"old").unwrap();
    mode(&dir, 0o500);
    let result = state.save(b"new");
    mode(&dir, 0o700);
    assert!(message(result.unwrap_err()).starts_with("write "));
    assert_eq!(state.load().unwrap().as_deref(), Some(&b"old"[..]));
}

/// Asserts that a record that cannot be replaced, because a directory holds its name, is an
/// error that leaves no temporary file and keeps the directory.
#[test]
fn a_save_over_a_directory_is_refused_and_leaves_no_temporary_file() {
    let (_guard, dir) = scratch_state("state-rename");
    fs::create_dir(dir.join(STATE_FILE)).unwrap();
    let state = files(&dir);
    assert!(message(state.save(b"x").unwrap_err()).starts_with("write "));
    assert!(!dir.join("state.json.tmp").exists());
    assert!(dir.join(STATE_FILE).is_dir());
}

/// Asserts that a record that cannot be read (a directory is at its name) is an error and not
/// an absent record: an absent one would let a first install lay a record over it.
#[test]
fn a_record_that_cannot_be_read_is_an_error_not_an_absent_record() {
    let (_guard, dir) = scratch_state("state-unreadable");
    fs::create_dir(dir.join(STATE_FILE)).unwrap();
    assert!(message(files(&dir).load().unwrap_err()).starts_with("read "));
}

/// Asserts that a record that is a symbolic link is not read through: loading it is an error and
/// does not return what the link points at.
#[test]
fn a_symlinked_record_is_not_followed_when_loaded() {
    let (_guard, dir) = scratch_state("state-link-load");
    fs::write(dir.join("elsewhere"), "secret").unwrap();
    symlink(dir.join("elsewhere"), dir.join(STATE_FILE)).unwrap();
    assert!(message(files(&dir).load().unwrap_err()).starts_with("read "));
}

/// Asserts that saving over a record that is a symbolic link is refused and writes nothing
/// through it or over it.
#[test]
fn a_symlinked_record_is_refused_on_save() {
    let (_guard, dir) = scratch_state("state-link-save");
    let victim = dir.join("victim");
    fs::write(&victim, "keep").unwrap();
    symlink(&victim, dir.join(STATE_FILE)).unwrap();
    assert!(message(files(&dir).save(b"new").unwrap_err()).starts_with("write "));
    assert_eq!(fs::read(&victim).unwrap(), b"keep");
    assert!(
        fs::symlink_metadata(dir.join(STATE_FILE))
            .unwrap()
            .is_symlink()
    );
}

/// Asserts that a symbolic link at the temporary file's name is removed and not written
/// through: what it pointed at is as it was, and the record is the new bytes.
#[test]
fn a_symlinked_temporary_file_is_not_written_through() {
    let (_guard, dir) = scratch_state("state-link-temp");
    let victim = dir.join("victim");
    fs::write(&victim, "keep").unwrap();
    symlink(&victim, dir.join("state.json.tmp")).unwrap();
    files(&dir).save(b"new").unwrap();
    assert_eq!(fs::read(&victim).unwrap(), b"keep");
    assert_eq!(files(&dir).load().unwrap().as_deref(), Some(&b"new"[..]));
}

/// Asserts that a record the service could have written is refused on load and on save: group
/// writable (0660) and other writable (0602) files, not a forged record taken as the hook's. A
/// record of 0600 and of 0640 (the group may read) is read. It would catch a record trusted
/// whatever its mode.
#[test]
fn a_record_that_a_group_or_others_can_write_is_refused() {
    let (_guard, dir) = scratch_state("state-record-mode");
    let state = files(&dir);
    state.save(b"mine").unwrap();
    for bits in [0o600, 0o640] {
        mode(&dir.join(STATE_FILE), bits);
        assert_eq!(state.load().unwrap().as_deref(), Some(&b"mine"[..]));
    }
    for bits in [0o660, 0o602, 0o666, 0o620] {
        mode(&dir.join(STATE_FILE), bits);
        let loaded = message(state.load().unwrap_err());
        assert!(loaded.starts_with("read "), "{bits:o}: {loaded}");
        assert!(
            loaded.contains("can be written by a group or others"),
            "{loaded}"
        );
        let saved = message(state.save(b"new").unwrap_err());
        assert!(saved.starts_with("write "), "{bits:o}: {saved}");
        assert_eq!(fs::read(dir.join(STATE_FILE)).unwrap(), b"mine");
    }
}

/// Asserts that every file is reached through the directory descriptor that was checked: after
/// the first use, the path is replaced by another directory of the right mode holding a forged
/// record, a forged lock is taken in it and a save is made; the record loaded and saved is the
/// original directory's, and the forged directory is untouched. It would catch a port that opens
/// each file by the path again.
#[test]
fn the_files_are_reached_through_the_directory_that_was_checked() {
    let (guard, dir) = scratch_state("state-swap");
    let state = files(&dir);
    state.save(b"mine").unwrap();
    let moved = guard.path().join("moved");
    fs::rename(&dir, &moved).unwrap();
    fs::create_dir(&dir).unwrap();
    mode(&dir, 0o700);
    fs::write(dir.join(STATE_FILE), "forged").unwrap();
    mode(&dir.join(STATE_FILE), 0o600);
    assert_eq!(state.load().unwrap().as_deref(), Some(&b"mine"[..]));
    let held = state.lock().unwrap();
    state.save(b"newer").unwrap();
    drop(held);
    assert_eq!(fs::read(dir.join(STATE_FILE)).unwrap(), b"forged");
    assert!(!dir.join(LOCK_FILE).exists());
    assert_eq!(fs::read(moved.join(STATE_FILE)).unwrap(), b"newer");
    assert!(moved.join(LOCK_FILE).exists());
}

/// Asserts that a state directory that is absent is made with mode 0700, its missing parents
/// with mode 0755, and then takes the lock and keeps a record; a second use finds it as it is.
#[test]
fn an_absent_directory_is_made_private_with_its_parents() {
    let (_guard, root) = scratch("state-made");
    let dir = root.join("var/lib/hook");
    let state = StateFiles::new(dir.clone(), me());
    let _lock = state.lock().expect("the lock");
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for parent in [root.join("var"), root.join("var/lib")] {
        assert_eq!(
            fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    state.save(b"{}\n").unwrap();
    assert_eq!(
        StateFiles::new(dir, me()).load().unwrap(),
        Some(b"{}\n".to_vec())
    );
}

/// Asserts that a directory that cannot be made is refused with the state port's words: a
/// parent that cannot be made (the root is read-only) and the directory itself in a read-only
/// parent.
#[test]
fn a_directory_that_cannot_be_made_is_refused() {
    let (_guard, root) = scratch("state-unmakeable");
    mode(&root, 0o500);
    let deep = refused(&StateFiles::new(root.join("a/b"), me()));
    let direct = refused(&StateFiles::new(root.join("state"), me()));
    mode(&root, 0o700);
    assert!(deep.starts_with("state directory:"), "{deep}");
    assert!(direct.starts_with("state directory:"), "{direct}");
    assert!(!root.join("state").exists() && !root.join("a").exists());
}

/// Asserts that a record in a state directory that a group could write to is refused: a record
/// of mode 0620 is not read.
#[test]
fn a_record_that_a_group_could_write_is_not_read() {
    let (_guard, dir) = scratch_state("state-group-record");
    fs::write(dir.join(STATE_FILE), b"{}").unwrap();
    mode(&dir.join(STATE_FILE), 0o620);
    let text = message(files(&dir).load().unwrap_err());
    assert!(
        text.ends_with("can be written by a group or others"),
        "{text}"
    );
}
