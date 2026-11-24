//! The switch of a Mac from two jobs to one by a config update.

use std::path::{Path, PathBuf};

use super::super::super::launchd_tests::fake as mac;
use super::super::super::{FS_CONFIG_FILE, FS_LAUNCHD_PLIST, LAUNCHD_PLIST, Reason, Record, Step};
use super::macos::{Mac, mac_update};
use super::{lin, staged};
use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// The mount point of the new file.
const NEW_MOUNT: &str = "/private/var/tierstore/mnt";

/// A file with an `fs` interface that mounts at `mountpoint`.
fn fs_file(mountpoint: &str) -> String {
    format!(
        "[server]\n\n[[interface]]\ntype = \"fs\"\n\n[interface.params]\nmountpoint = \"{mountpoint}\"\n"
    )
}

/// A Mac whose fs job is loaded, with the mount point its `fs.toml` names, and a config package
/// that stages a file with an `fs` interface.
fn switching() -> Mac {
    let node = Mac::new();
    node.fake.with_fs(true);
    node.files.put(
        Path::new(FS_CONFIG_FILE),
        &format!("[fs]\nmountpoint = \"{}\"\n", mac::FS_MOUNT),
    );
    node.files.put(&staged(mac::CONFIG), &fs_file(NEW_MOUNT));
    node
}

/// The calls that act on a job, without the looks.
fn acts(node: &Mac) -> Vec<String> {
    node.fake
        .launchd_calls()
        .into_iter()
        .filter(|call| !call.starts_with("launchd.loaded"))
        .collect()
}

fn call(what: &str, label: &str) -> String {
    format!("launchd.{what} {label}")
}

fn position(calls: &[String], what: &str) -> usize {
    calls
        .iter()
        .position(|call| call == what)
        .unwrap_or_else(|| panic!("no call {what}: {calls:?}"))
}

/// Asserts that a Mac config update whose staged file has an `fs` interface and whose fs job is
/// loaded notes the switch, boots the fs job out (with the bounds of an update's boot-out of it),
/// then disables it, and only then boots the main job out and in; that the fs job is never booted
/// in again; and that `verify` (with the new mount listed) and `cleanup` end it with the fs job
/// out and disabled. Catches a switch that leaves the old plist able to start the fs job at
/// the next boot, and one that restarts the main job while the old mount is still served.
#[test]
fn a_mac_switch_boots_out_and_disables_the_fs_job_before_the_main_job_starts() {
    let node = switching();
    let p = mac_update();
    node.go(&p, &[Prepare]);
    assert!(node.fake.record().fs_switch);
    assert!(acts(&node).is_empty());

    node.go(&p, &[Cutover]);
    let calls = acts(&node);
    let fs_out = position(&calls, &call("bootout", mac::FS_LABEL));
    let disabled = position(&calls, &call("disable", mac::FS_LABEL));
    let main_out = position(&calls, &call("bootout", mac::LABEL));
    let main_in = position(&calls, &format!("launchd.bootstrap {LAUNCHD_PLIST}"));
    assert!(
        fs_out < disabled && disabled < main_out && main_out < main_in,
        "{calls:?}"
    );
    assert!(
        !calls.contains(&format!("launchd.bootstrap {FS_LAUNCHD_PLIST}")),
        "{calls:?}"
    );
    assert_eq!(node.fake.disabled(), [mac::FS_LABEL]);
    let end = node.fake.snapshot();
    assert_eq!(
        (end.job, end.fs_job),
        (Some(PathBuf::from(mac::OLD)), false)
    );
    let bounds = node.fake.bootout_bounds();
    assert_eq!(bounds.len(), 1);
    assert_eq!(bounds[0].0, mac::FS_LABEL);

    // hostkeeper has put the new file in place, and the new process has mounted.
    node.files.put(Path::new(mac::CONFIG), &fs_file(NEW_MOUNT));
    node.files
        .mounts
        .borrow_mut()
        .insert(PathBuf::from(NEW_MOUNT));
    node.go(&p, &[Verify, Cleanup]);
    assert!(node.fake.record().committed());
    assert_eq!(node.fake.disabled(), [mac::FS_LABEL]);
}

/// Asserts that a `cutover` run again after the switch booted the fs job out leaves it alone: no
/// second boot-out or disable of the fs job.
#[test]
fn a_mac_switch_run_again_does_not_touch_the_fs_job_again() {
    let node = switching();
    let p = mac_update();
    node.go(&p, &[Prepare, Cutover]);
    let before = acts(&node);
    node.go(&p, &[Cutover]);
    let after = acts(&node);
    assert_eq!(
        after.iter().filter(|c| c.contains(mac::FS_LABEL)).count(),
        before.iter().filter(|c| c.contains(mac::FS_LABEL)).count()
    );
}

/// Asserts that the rollback of the switch enables the fs job first, then boots the main job out
/// and in on the old file and boots the fs job in (which a disabled job would refuse), and leaves
/// an idle record with no switch on it. Catches a rollback that tries the fs job's plist while the
/// job is still disabled.
#[test]
fn a_mac_switch_rolls_back_enabling_the_fs_job_before_it_boots_it_in() {
    let node = switching();
    let p = mac_update();
    node.go(&p, &[Prepare, Cutover]);
    let before = acts(&node).len();
    lin::assert_ok(&node.step(Rollback, &p));
    let calls: Vec<String> = acts(&node).split_off(before);
    let enabled = position(&calls, &call("enable", mac::FS_LABEL));
    let main_out = position(&calls, &call("bootout", mac::LABEL));
    let main_in = position(&calls, &format!("launchd.bootstrap {LAUNCHD_PLIST}"));
    let fs_in = position(&calls, &format!("launchd.bootstrap {FS_LAUNCHD_PLIST}"));
    assert!(
        enabled < main_out && main_out < main_in && main_in < fs_in,
        "{calls:?}"
    );
    assert!(node.fake.disabled().is_empty());
    let end = node.fake.snapshot();
    assert_eq!((end.job, end.fs_job), (Some(PathBuf::from(mac::OLD)), true));
    assert_eq!(node.fake.record(), Record::idle(None));
}

/// Asserts that a mount that stays after the fs job was booted out ends `cutover` with not_ready,
/// with the fs job loaded again and not disabled, and the main job neither booted out nor in; and
/// that a mount list that cannot be read does the same with host_failed.
#[test]
fn a_mac_switch_whose_mount_stays_leaves_the_main_job_untouched() {
    for unreadable in [false, true] {
        let node = switching();
        let p = mac_update();
        node.go(&p, &[Prepare]);
        if unreadable {
            node.files.mount_fails.set(true);
        } else {
            node.files
                .mounts
                .borrow_mut()
                .insert(PathBuf::from(mac::FS_MOUNT));
        }
        let done = node.step(Cutover, &p);
        let reason = if unreadable {
            Reason::HostFailed
        } else {
            Reason::NotReady
        };
        lin::assert_failed(&done, reason);
        let calls = acts(&node);
        assert_eq!(
            calls,
            [
                call("bootout", mac::FS_LABEL),
                format!("launchd.bootstrap {FS_LAUNCHD_PLIST}")
            ]
        );
        assert!(node.fake.disabled().is_empty());
        let end = node.fake.snapshot();
        assert_eq!((end.job, end.fs_job), (Some(PathBuf::from(mac::OLD)), true));
    }
}

/// Asserts that a switch whose `fs.toml` is gone, or names no usable mount point, is refused by
/// `cutover` before anything is stopped.
#[test]
fn a_mac_switch_that_cannot_tell_the_old_mount_point_stops_nothing() {
    let missing = switching();
    missing
        .files
        .texts
        .borrow_mut()
        .remove(Path::new(FS_CONFIG_FILE));
    let bad = switching();
    bad.files
        .put(Path::new(FS_CONFIG_FILE), "[fs]\nmountpoint = \"rel\"\n");
    for (node, reason) in [
        (missing, Reason::ConfigMissing),
        (bad, Reason::ConfigRefused),
    ] {
        let p = mac_update();
        node.go(&p, &[Prepare]);
        lin::assert_failed(&node.step(Cutover, &p), reason);
        assert!(acts(&node).is_empty());
        assert!(node.fake.snapshot().fs_job);
    }
}

/// Asserts that a Mac config update whose staged file has an `fs` interface but whose fs job is
/// not loaded is no switch: no switch on record, and the main job is restarted alone, with no
/// fs job boot-out, disable or enable.
#[test]
fn a_mac_config_update_with_no_fs_job_loaded_is_not_a_switch() {
    let node = Mac::new();
    node.files.put(&staged(mac::CONFIG), &fs_file(NEW_MOUNT));
    let p = mac_update();
    node.go(&p, &[Prepare, Cutover]);
    assert!(!node.fake.record().fs_switch);
    let calls = acts(&node);
    assert_eq!(
        calls,
        [
            call("bootout", mac::LABEL),
            call("bootout", mac::LABEL),
            format!("launchd.bootstrap {LAUNCHD_PLIST}")
        ]
    );
    node.fake.set_disabled(mac::FS_LABEL);
    lin::assert_ok(&node.step(Rollback, &p));
    assert!(!acts(&node).contains(&call("enable", mac::FS_LABEL)));
    assert_eq!(node.fake.disabled(), [mac::FS_LABEL]);
}

/// Asserts that `verify` of the switch fails with not_ready when the mount is not listed at the
/// mount point of the new file, with host_failed when the list cannot be read, and with
/// config_missing when the installed file has no `fs` interface, and records `verified` in none
/// of them.
#[test]
fn a_mac_switch_is_verified_only_with_the_new_mount_listed() {
    let node = switching();
    let p = mac_update();
    node.go(&p, &[Prepare, Cutover]);
    node.files.put(Path::new(mac::CONFIG), &fs_file(NEW_MOUNT));
    let done = node.step(Verify, &p);
    lin::assert_failed(&done, Reason::NotReady);
    assert!(lin::detail(&done).contains(NEW_MOUNT));
    node.files.mount_fails.set(true);
    lin::assert_failed(&node.step(Verify, &p), Reason::HostFailed);
    node.files.mount_fails.set(false);
    node.files.put(Path::new(mac::CONFIG), "[server]\n");
    lin::assert_failed(&node.step(Verify, &p), Reason::ConfigMissing);
    assert!(
        !node
            .fake
            .record()
            .has(super::super::super::Marker::Verified)
    );
    node.files.put(Path::new(mac::CONFIG), &fs_file(NEW_MOUNT));
    node.files
        .mounts
        .borrow_mut()
        .insert(PathBuf::from(NEW_MOUNT));
    lin::assert_ok(&node.step(Verify, &p));
}

/// Asserts that a Mac config update whose staged file moves the mount point of a file that already
/// has an `fs` interface is refused with mode_unsupported.
#[test]
fn a_mac_config_update_that_moves_the_mount_point_is_refused() {
    let node = Mac::new();
    node.files.put(
        Path::new(mac::CONFIG),
        &fs_file("/private/var/tierstore/old"),
    );
    node.files.put(&staged(mac::CONFIG), &fs_file(NEW_MOUNT));
    lin::assert_failed(&node.step(Prepare, &mac_update()), Reason::ModeUnsupported);
    assert_eq!(node.fake.record(), Record::idle(None));
}
