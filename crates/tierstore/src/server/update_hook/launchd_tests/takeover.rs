//! A first install over a copy of tierstore installed by hand: no previous release on record,
//! and the job's label (and the fs job's) as that copy left it, stopped or running with its mount.
//! The first install takes the copy over as an update would, the fs job out first with its mount
//! checked, and the record of every step says what the host then runs.

use std::collections::BTreeSet;

use super::fake::{
    FS_LABEL, FS_MOUNT, Fake, LABEL, NEW, NEW_VERSION, OLD, assert_failed, assert_ok, in_flight,
    path,
};
use crate::server::update_hook::{
    FS_CONFIG_FILE, FS_LAUNCHD_PLIST, LAUNCHD_PLIST, Marker, Reason, Record, Step,
};

use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// The calls to the launchd domain and the host so far, in order.
fn acts(fake: &Fake) -> Vec<String> {
    fake.log()
        .into_iter()
        .filter(|call| call.starts_with("launchd.") || call.starts_with("host."))
        .collect()
}

fn loaded() -> String {
    format!("launchd.loaded {LABEL}")
}

fn fs_loaded() -> String {
    format!("launchd.loaded {FS_LABEL}")
}

fn bootout() -> String {
    format!("launchd.bootout {LABEL}")
}

fn fs_bootout() -> String {
    format!("launchd.bootout {FS_LABEL}")
}

fn bootstrap() -> String {
    format!("launchd.bootstrap {LAUNCHD_PLIST}")
}

fn fs_bootstrap() -> String {
    format!("launchd.bootstrap {FS_LAUNCHD_PLIST}")
}

fn fs_config() -> String {
    format!("host.config {FS_CONFIG_FILE}")
}

fn mounted() -> String {
    format!("host.mounted {FS_MOUNT}")
}

fn install() -> String {
    format!("host.install_binary {NEW}")
}

/// The record a finished first install leaves: at rest on `NEW`, committed, with no `from`.
fn installed() -> Record {
    Record {
        to: path(NEW),
        version: Some(NEW_VERSION.to_owned()),
        markers: BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]),
        ..Record::idle(None)
    }
}

/// A first install, prepared, on a host where a copy installed by hand runs `OLD` under the
/// job's label and, when `fs` says so, has its fs job loaded and its mount listed.
fn over_a_running_copy(fs: bool) -> Fake {
    let fake = Fake::first_install();
    if fs {
        fake.with_fs(true);
    }
    fake.set_job(Some(OLD));
    fake.advance(&[Prepare]);
    assert_eq!(fake.record(), in_flight(true));
    fake
}

/// Asserts that a first install whose label is loaded by a copy installed by hand that runs
/// nothing (stopped: its program exited and launchd does not start it again) boots that job
/// out and in, so that it runs the staged release, and that `cutover`, `verify` and `cleanup`
/// all succeed and leave the record of a finished install, with `swapped` after `cutover`.
/// Catches a first install that waits on a loaded job it never restarts.
#[test]
fn a_first_install_over_a_stopped_loaded_copy_boots_it_out_and_in() {
    let fake = Fake::first_install();
    fake.set_job_idle();
    fake.advance(&[Prepare]);
    let before = acts(&fake).len();
    assert_ok(&fake.run(Cutover));
    assert_eq!(
        acts(&fake)[before..],
        [loaded(), install(), bootout(), bootstrap()]
    );
    assert_eq!(fake.snapshot().job, path(NEW));
    assert_eq!(fake.record(), in_flight(true).with(Marker::Swapped));
    fake.advance(&[Verify, Cleanup]);
    assert_eq!(fake.record(), installed());
}

/// Asserts that a first install over a stopped copy installed by hand whose fs job was left
/// loaded with its mount (the main job not loaded) boots the fs job out first and reads the
/// mount list, then loads the main job without a boot-out of it, boots the fs job in after the
/// main job serves, and ends with the record of a finished install, both jobs loaded and the
/// mount listed. Catches a first install that starts the main job beside the old copy's file
/// service, or that boots out a main job that is not loaded.
#[test]
fn a_first_install_over_a_stopped_copy_whose_fs_job_is_up_unmounts_first() {
    let fake = Fake::first_install();
    fake.with_fs(true);
    fake.advance(&[Prepare]);
    let before = acts(&fake).len();
    assert_ok(&fake.run(Cutover));
    assert_eq!(
        acts(&fake)[before..],
        [
            loaded(),
            fs_loaded(),
            fs_config(),
            fs_bootout(),
            mounted(),
            install(),
            bootstrap(),
            fs_loaded(),
            fs_bootstrap(),
        ]
    );
    fake.advance(&[Verify, Cleanup]);
    assert_eq!(fake.record(), installed());
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job, end.mounted), (path(NEW), true, true));
}

/// Asserts that a first install over a running copy installed by hand, with no fs job, boots
/// the job out and in and then serves the staged release; the whole install ends with the
/// record of a finished install. Catches a first install that leaves the copy's version
/// serving and fails `cutover` on it.
#[test]
fn a_first_install_over_a_running_copy_takes_it_over() {
    let fake = over_a_running_copy(false);
    let before = acts(&fake).len();
    assert_ok(&fake.run(Cutover));
    assert_eq!(
        acts(&fake)[before..],
        [loaded(), install(), bootout(), bootstrap()]
    );
    assert_eq!(fake.snapshot().job, path(NEW));
    fake.advance(&[Verify, Cleanup]);
    assert_eq!(fake.record(), installed());
}

/// Asserts that a first install over a running copy installed by hand whose fs job is loaded
/// with its mount up boots the fs job out first and reads the mount list, and only once the
/// mount is gone puts the binary in place and boots the main job out and in; the fs job is booted
/// in after the main job serves, and the install ends with both jobs loaded, the mount listed and
/// the record of a finished install. Catches a takeover that stops the main job, or the file
/// service, while the mount is listed.
#[test]
fn a_first_install_over_a_running_copy_with_its_mount_up_unmounts_first() {
    let fake = over_a_running_copy(true);
    let before = acts(&fake).len();
    assert_ok(&fake.run(Cutover));
    assert_eq!(
        acts(&fake)[before..],
        [
            loaded(),
            fs_loaded(),
            fs_config(),
            fs_bootout(),
            mounted(),
            install(),
            bootout(),
            bootstrap(),
            fs_loaded(),
            fs_bootstrap(),
        ]
    );
    assert_eq!(fake.record(), in_flight(true).with(Marker::Swapped));
    fake.advance(&[Verify, Cleanup]);
    assert_eq!(fake.record(), installed());
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job, end.mounted), (path(NEW), true, true));
    assert!(fake.strays().is_empty());
}

/// Asserts that a first install over a running copy whose mount stays after its fs job is
/// booted out fails `cutover` with `not_ready`, reversible, naming the mount point; the main job
/// is neither booted out nor given a binary, the fs job is booted in again, so the copy serves
/// on with its mount, and the record is the update in flight with no marker. The `rollback`
/// hostkeeper then runs meets the same mount, fails `not_ready`, and leaves the copy running and
/// the record as it was. Catches a takeover, or its rollback, that stops the main job beside a
/// mount without a server, and one that records a failure as a stop the host did not make.
#[test]
fn a_mount_that_stays_refuses_the_takeover_and_its_rollback() {
    let fake = over_a_running_copy(true);
    fake.keep_mount(true);
    let before = acts(&fake).len();
    let done = fake.run(Cutover);
    assert_failed(&done, Reason::NotReady);
    assert!(done.report.reversible);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "{FS_MOUNT} is still mounted after {FS_LABEL} stopped: {LABEL} is left as it is, \
                 and {FS_LABEL} is loaded again"
            )
            .as_str()
        )
    );
    assert_eq!(
        acts(&fake)[before..],
        [
            loaded(),
            fs_loaded(),
            fs_config(),
            fs_bootout(),
            mounted(),
            fs_bootstrap(),
        ]
    );
    let held = fake.snapshot();
    assert_eq!(
        (
            held.job.clone(),
            held.installed.clone(),
            held.fs_job,
            held.mounted
        ),
        (path(OLD), None, true, true)
    );
    assert_eq!(fake.record(), in_flight(true));
    let rollback = fake.run(Rollback);
    assert_failed(&rollback, Reason::NotReady);
    assert_eq!(fake.snapshot(), held);
}

/// Asserts that a takeover whose staged release never answers fails `cutover` with `not_ready`
/// and no `swapped`, and that the `rollback` hostkeeper then runs boots the fs job out first,
/// with the mount check, then the main job, and fails `no_previous_release` saying the job is
/// stopped, which is what the host then is: neither job loaded, no mount, no binary, and the
/// update in flight with no marker. Catches a first-install rollback whose record says stopped
/// while a job runs, or that stops the main job before the file service.
#[test]
fn a_takeover_that_never_serves_is_rolled_back_to_a_stopped_host() {
    let fake = over_a_running_copy(true);
    fake.mute(NEW);
    let cutover = fake.run(Cutover);
    assert_failed(&cutover, Reason::NotReady);
    assert!(cutover.report.reversible);
    assert_eq!(fake.record(), in_flight(true));
    let before = acts(&fake).len();
    let done = fake.run(Rollback);
    assert_failed(&done, Reason::NoPreviousRelease);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(format!("nothing to go back to: {LABEL} is stopped").as_str())
    );
    assert_eq!(
        acts(&fake)[before..],
        [fs_loaded(), bootout(), "host.remove_binary".to_owned(),]
    );
    let end = fake.snapshot();
    assert_eq!(
        (end.job, end.installed, end.fs_job, end.mounted),
        (None, None, false, false)
    );
    assert_eq!(fake.record(), in_flight(true));
}

/// Asserts that a takeover whose staged release, once the copy is booted out and the job in
/// again, answers with another build fails `cutover` with `probe_failed`, reversible and with
/// no `swapped`, and that the `rollback` then says `no_previous_release` with nothing loaded.
/// Catches a takeover that takes any answer for the target's.
#[test]
fn a_takeover_that_serves_another_build_fails_probe_failed() {
    let fake = over_a_running_copy(false);
    fake.lie(NEW, "9.9.9");
    let before = acts(&fake).len();
    let done = fake.run(Cutover);
    assert_failed(&done, Reason::ProbeFailed);
    assert!(done.report.reversible);
    assert_eq!(
        acts(&fake)[before..],
        [loaded(), install(), bootout(), bootstrap()]
    );
    assert_eq!(fake.record(), in_flight(true));
    assert_failed(&fake.run(Rollback), Reason::NoPreviousRelease);
    assert_eq!(fake.snapshot().job, None);
}

/// Asserts that a takeover whose fs job is loaded and whose `fs.toml` is missing, or names no
/// absolute mount point, is refused by `cutover` (`config_missing`, `config_refused`) and by the
/// `rollback` after it, each saying that nothing was stopped, before any boot-out or binary: the
/// copy and its fs job run on with the mount, and the record is the update in flight. Catches a
/// takeover that stops the file service without knowing which mount to check.
#[test]
fn a_takeover_with_no_readable_mount_point_stops_nothing() {
    for (text, reason) in [
        (None, Reason::ConfigMissing),
        (Some("[fs]\nmountpoint = \"fs\"\n"), Reason::ConfigRefused),
    ] {
        let fake = over_a_running_copy(true);
        fake.set_config(FS_CONFIG_FILE, text);
        let held = fake.snapshot();
        for step in [Cutover, Rollback] {
            let before = acts(&fake).len();
            let done = fake.run(step);
            assert_failed(&done, reason);
            let detail = done.report.detail.clone().unwrap_or_default();
            assert!(
                detail.ends_with(": nothing was stopped"),
                "{step}: {detail}"
            );
            assert_eq!(fake.snapshot(), held, "{step} {reason:?}");
            let after = acts(&fake)[before..].to_vec();
            assert!(
                after.iter().all(|call| !call.starts_with("launchd.bootout")
                    && !call.starts_with("launchd.bootstrap")
                    && !call.starts_with("host.install_binary")
                    && !call.starts_with("host.remove_binary")),
                "{step}: {after:?}"
            );
        }
        assert_eq!(fake.record(), in_flight(true));
    }
}

/// Asserts that a first install whose loaded job already serves the target, with the fs job
/// loaded and its mount up, only looks: no fs boot-out, no binary, no boot-out or boot-in, and
/// `swapped` is recorded. Catches a takeover that bounces a pair that already serves the target
/// (a run again after a crash after the restart).
#[test]
fn a_first_install_finding_the_target_served_bounces_nothing() {
    let fake = Fake::first_install();
    fake.with_fs(true);
    fake.set_job(Some(NEW));
    fake.advance(&[Prepare]);
    let before = acts(&fake).len();
    assert_ok(&fake.run(Cutover));
    assert_eq!(acts(&fake)[before..], [loaded(), fs_loaded()]);
    assert_eq!(fake.record(), in_flight(true).with(Marker::Swapped));
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job, end.mounted), (path(NEW), true, true));
}

/// Asserts that a takeover whose staged release answers with another build, interrupted at
/// any call of `cutover` (a call failing, or the world stopping there), with or without the fs
/// job, is finished by the `rollback` hostkeeper runs in the state an uninterrupted failed
/// takeover and its rollback reach: `no_previous_release`, nothing loaded, no mount, no binary,
/// the update in flight with no marker, and no other job named. Catches a crash inside a
/// takeover that leaves the copy, the new job or the mount without its server behind.
#[test]
fn an_interrupted_takeover_is_finished_by_its_rollback() {
    for fs in [false, true] {
        let failing = || {
            let fake = over_a_running_copy(fs);
            fake.lie(NEW, "9.9.9");
            fake
        };
        let clean = failing();
        let from = clean.calls();
        assert_failed(&clean.run(Cutover), Reason::ProbeFailed);
        let calls = clean.calls() - from;
        assert_failed(&clean.run(Rollback), Reason::NoPreviousRelease);
        let end = clean.snapshot();
        assert_eq!(
            (end.job.clone(), end.fs_job, end.mounted),
            (None, false, false)
        );
        for n in 1..=calls {
            for halt in [false, true] {
                let fake = failing();
                if halt {
                    fake.halt_at(n);
                } else {
                    fake.fail_at(n);
                }
                let _ = fake.run(Cutover);
                assert_eq!(fake.live_locks(), 0, "at {n}, fs {fs}");
                fake.heal();
                let back = fake.run(Rollback);
                assert_failed(&back, Reason::NoPreviousRelease);
                assert_eq!(fake.snapshot(), end, "at {n}, halt {halt}, fs {fs}");
                assert!(fake.strays().is_empty(), "at {n}, fs {fs}");
            }
        }
    }
}
