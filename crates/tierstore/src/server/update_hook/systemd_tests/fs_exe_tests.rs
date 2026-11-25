//! `verify`'s look at the binary the fs process runs: after a hand-over the new instance's
//! process must run the binary the update installed; an fs process the update left alone is
//! named in the detail with the release it still runs and the one the update installed.

use std::path::Path;

use crate::server::update_hook::{
    Instance, Marker, Reason, Record, SERVICE_BINARY, Step, UnitState,
};

use super::fake::{
    NEW, NEWER, OLD, SystemdFake, assert_failed, assert_ok, detail, file_of, fs_pid, params,
    version_of,
};

use Step::{Cleanup, Cutover, Prepare, Verify};

/// The record the release before this one wrote for an update of a node whose fs process it
/// hands over, in flight after `cutover`: from `NEW` on a to `NEWER` on b, fs a to fs b.
const PREVIOUS_RECORD: &[u8] =
    include_bytes!("../../../../tests/stored_formats/fixtures/hook_record_systemd_fs.json");

/// What `verify` says of a target that serves read-write.
fn serves(release: &str) -> String {
    format!("{} serves on port 9300", version_of(release))
}

/// The calls of `fake` that look at an fs process.
fn process_looks(fake: &SystemdFake) -> Vec<String> {
    fake.log()
        .into_iter()
        .filter(|line| line.starts_with("fs_proc."))
        .collect()
}

/// A node whose fs process the update to `NEW` leaves alone (`OLD` lacks `handoff-fs-v1`), after
/// `prepare` and `cutover`.
fn left_alone() -> SystemdFake {
    let fake = SystemdFake::fs_installed();
    fake.list_fs_hand_over(OLD, false);
    fake.drive(&params(NEW, Some(OLD)), &[Prepare, Cutover]);
    fake
}

/// Asserts that after a hand-over whose new fs instance runs the binary the update installed
/// (its `/proc/<pid>/exe` is the file at the service binary's path), `verify` succeeds, records
/// `verified`, and says nothing more than before; and that it asked for the pid of `tierstore-fs@b`,
/// the file that pid runs and the installed file. Catches a check that refuses the binary it should
/// accept, or one that never looks.
#[test]
fn a_hand_over_whose_fs_process_runs_the_installed_binary_passes_verify() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    let done = fake.run(Verify, &p);
    assert_ok(&done);
    assert_eq!(detail(&done), serves(NEW));
    assert!(fake.record().has(Marker::Verified));
    let pid = fs_pid(Instance::B);
    assert_eq!(
        process_looks(&fake),
        [
            "fs_proc.main_pid b".to_owned(),
            format!("fs_proc.exe {pid}"),
            "fs_proc.installed".to_owned(),
        ]
    );
    fake.drive(&p, &[Cleanup]);
}

/// Asserts that when the process of the new fs instance runs the old binary, which the update's
/// copy replaced (its `/proc/<pid>/exe` reads `<path> (deleted)` and is another file), `verify`
/// fails `not_ready`, reversible, records no `verified`, and the detail names the unit, its pid,
/// both paths and both files, exactly. Then the same when the link reads the installed path and
/// is another file all the same (another inode). Catches an fs process left on an older release
/// that passes `verify` because its unit is active.
#[test]
fn a_hand_over_whose_fs_process_runs_the_replaced_binary_fails_verify() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_fs_unit(Instance::B, UnitState::Active, Some(OLD));
    let (old, new) = (file_of(Path::new(OLD)), file_of(Path::new(NEW)));
    assert_ne!(old, new);
    for shown in [
        format!("{SERVICE_BINARY} (deleted)"),
        SERVICE_BINARY.to_owned(),
    ] {
        fake.set_fs_exe_shown(Some(&shown));
        let done = fake.run(Verify, &p);
        assert_failed(&done, Reason::NotReady);
        assert!(done.report.reversible, "{done:?}");
        assert_eq!(
            detail(&done),
            format!(
                "tierstore-fs@b (pid 4102) runs {shown} (device {}, inode {}), not the binary the \
                 update installed, {SERVICE_BINARY} (device {}, inode {}): it is still on an older \
                 release",
                old.dev, old.ino, new.dev, new.ino
            )
        );
        assert!(!fake.record().has(Marker::Verified));
    }
    // The fake's own reading of a replaced file is the `(deleted)` one.
    fake.set_fs_exe_shown(None);
    let done = fake.run(Verify, &p);
    assert_failed(&done, Reason::NotReady);
    assert!(detail(&done).starts_with(&format!(
        "tierstore-fs@b (pid 4102) runs {SERVICE_BINARY} (deleted) "
    )));
}

/// Asserts that a process whose binary cannot be read (gone, or the hook may not look) fails
/// `verify` with words of its own (`host_failed`, reversible, the unit and the pid named, the
/// port's message after them, exactly), and so do an installed binary that cannot be looked at
/// and a unit systemd names no main process of (`not_ready`); none records `verified`, and once
/// each can be read `verify` passes. Catches an unreadable process taken for a pass.
#[test]
fn an_fs_process_whose_binary_cannot_be_read_fails_verify() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    for (prefix, reason, words) in [
        (
            "fs_proc.exe",
            Reason::HostFailed,
            "the binary that tierstore-fs@b (pid 4102) runs cannot be read: host: injected failure",
        ),
        (
            "fs_proc.installed",
            Reason::HostFailed,
            "the binary the update installed cannot be read: host: injected failure",
        ),
        (
            "fs_proc.main_pid",
            Reason::UnitFailed,
            "units: injected failure",
        ),
    ] {
        fake.fail_on(Some(prefix));
        let done = fake.run(Verify, &p);
        fake.fail_on(None);
        assert_failed(&done, reason);
        assert!(done.report.reversible, "{prefix}: {done:?}");
        assert_eq!(detail(&done), words, "{prefix}");
        assert!(!fake.record().has(Marker::Verified));
    }
    fake.set_installed(None);
    let done = fake.run(Verify, &p);
    assert_failed(&done, Reason::HostFailed);
    assert_eq!(
        detail(&done),
        format!(
            "the binary the update installed cannot be read: host: {SERVICE_BINARY}: not there"
        )
    );
    fake.set_installed(Some(NEW));
    fake.set_fs_no_pid(true);
    let done = fake.run(Verify, &p);
    assert_failed(&done, Reason::NotReady);
    assert!(done.report.reversible);
    assert_eq!(
        detail(&done),
        "tierstore-fs@b is active and systemd names no main process of it"
    );
    assert!(!fake.record().has(Marker::Verified));
    fake.set_fs_no_pid(false);
    assert_ok(&fake.run(Verify, &p));
    assert!(fake.record().has(Marker::Verified));
}

/// Asserts that an update that left the fs process alone passes `verify`, and that its detail
/// says first, exactly, which release the fs process still runs (its binary's `--version`) and
/// which the update installed, then what it said before; `verify` asks the process's version,
/// not its binary's file. Catches the left-alone fs process hidden in a sentence of `prepare`,
/// and a `verify` that fails it (which would force the restart the hook refuses).
#[test]
fn a_left_alone_fs_process_passes_verify_and_the_detail_names_both_releases() {
    let fake = left_alone();
    let p = params(NEW, Some(OLD));
    let done = fake.run(Verify, &p);
    assert_ok(&done);
    assert_eq!(
        detail(&done),
        format!(
            "tierstore-fs@a (pid 4101) still runs release {}, and the update installed {}: it was \
             left on its release; {}",
            version_of(OLD),
            version_of(NEW),
            serves(NEW)
        )
    );
    assert!(fake.record().has(Marker::Verified));
    let pid = fs_pid(Instance::A);
    assert_eq!(
        process_looks(&fake),
        [
            "fs_proc.main_pid a".to_owned(),
            format!("fs_proc.version {pid}")
        ]
    );
    fake.drive(&p, &[Cleanup]);
    assert_eq!(fake.fs_running(), [Instance::A]);
}

/// Asserts the other things `verify` may say of an fs process it left alone, each exactly and
/// none a failure: one that runs the installed release already, one with no process, and one
/// whose pid or version cannot be had, with the port's words. Catches a look at a process left
/// alone that fails the update.
#[test]
fn a_left_alone_fs_process_that_cannot_be_asked_does_not_fail_verify() {
    let fake = left_alone();
    let p = params(NEW, Some(OLD));
    let (old, new) = (version_of(OLD), version_of(NEW));
    let said = |fake: &SystemdFake| {
        let done = fake.run(Verify, &p);
        assert_ok(&done);
        detail(&done).to_owned()
    };
    fake.set_fs_unit(Instance::A, UnitState::Active, Some(NEW));
    assert_eq!(
        said(&fake),
        format!(
            "tierstore-fs@a (pid 4101) runs {new}, the release the update installed; {}",
            serves(NEW)
        )
    );
    fake.set_fs_unit(Instance::A, UnitState::Active, Some(OLD));
    fake.set_fs_no_pid(true);
    assert_eq!(
        said(&fake),
        format!(
            "tierstore-fs@a was left on its release and has no process now, and the update \
             installed {new}; {}",
            serves(NEW)
        )
    );
    fake.set_fs_no_pid(false);
    fake.fail_on(Some("fs_proc.version"));
    assert_eq!(
        said(&fake),
        format!(
            "tierstore-fs@a (pid 4101) was left on its release, which cannot be read (host: \
             injected failure), and the update installed {new}; {}",
            serves(NEW)
        )
    );
    fake.fail_on(Some("fs_proc.main_pid"));
    assert_eq!(
        said(&fake),
        format!(
            "tierstore-fs@a was left on its release, which cannot be read (units: injected \
             failure), and the update installed {new}; {}",
            serves(NEW)
        )
    );
    fake.fail_on(None);
    assert!(said(&fake).contains(&format!("still runs release {old}")));
}

/// Asserts that the record the release before this one wrote (the stored fixture of an update in
/// flight that hands the fs process over) verifies with this hook: on a node where the new store
/// and the new fs instance run the staged release, `verify` succeeds and records `verified`.
/// Catches a check that cannot read the record of the previous release.
#[test]
fn a_record_of_the_previous_release_verifies() {
    let record = Record::decode(PREVIOUS_RECORD).expect("the fixture is a record");
    let fake = SystemdFake::fs_installed();
    fake.set_installed(Some(NEWER));
    fake.set_unit(Instance::A, UnitState::Inactive, None, false);
    fake.set_unit(Instance::B, UnitState::Active, Some(NEWER), true);
    fake.set_fs_unit(Instance::A, UnitState::Inactive, None);
    fake.set_fs_unit(Instance::B, UnitState::Active, Some(NEWER));
    fake.set_record(&record);
    let p = params(NEWER, Some(NEW));
    let done = fake.run(Verify, &p);
    assert_ok(&done);
    assert_eq!(detail(&done), serves(NEWER));
    assert_eq!(fake.record(), record.with(Marker::Verified));
}

/// Asserts that a record with no fs keys, as a hook that does not know the fs unit writes it
/// after its `cutover`, verifies as before: `verify` succeeds with the words it said before and
/// looks at no fs process. Catches a check that asks for a process the record names none of.
#[test]
fn a_record_without_fs_keys_verifies_as_before() {
    let fake = left_alone();
    let mut record = fake.record();
    record.fs_live = None;
    record.fs_next = None;
    fake.set_record(&record);
    let p = params(NEW, Some(OLD));
    let done = fake.run(Verify, &p);
    assert_ok(&done);
    assert_eq!(detail(&done), serves(NEW));
    assert!(process_looks(&fake).is_empty());
}
