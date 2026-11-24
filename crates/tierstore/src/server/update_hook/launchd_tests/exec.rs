//! The hand-over of an update on a Mac: the next image of the job's process on `SIGUSR2` when both
//! releases list `handoff-exec-v1`, the refusal of a restart for a node that holds a mount, and
//! the restart that remains for every other update.

use super::fake::{
    CONFIG, Fake, LABEL, NEW, NEW_VERSION, OLD, assert_failed, assert_ok, in_flight, params_with,
    path,
};
use crate::server::update_hook::{
    HANDOFF_EXEC_CAPABILITY, LAUNCHD_PLIST, Marker, Reason, Record, Step,
};

use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// A node file that has an `fs` interface: the node holds a mount.
const WITH_MOUNT: &str = "[server]\n\n[[interface]]\ntype = \"fs\"\n";

fn loaded() -> String {
    format!("launchd.loaded {LABEL}")
}

fn bootout() -> String {
    format!("launchd.bootout {LABEL}")
}

fn bootstrap() -> String {
    format!("launchd.bootstrap {LAUNCHD_PLIST}")
}

fn kill() -> String {
    format!("launchd.kill SIGUSR2 {LABEL}")
}

/// A host where both releases list `handoff-exec-v1`.
fn capable() -> Fake {
    let fake = Fake::new();
    fake.set_capabilities(OLD, &[HANDOFF_EXEC_CAPABILITY]);
    fake.set_capabilities(NEW, &[HANDOFF_EXEC_CAPABILITY]);
    fake
}

/// The params of the update with `mode = "restart"`.
fn restart_params() -> crate::server::update_hook::Params {
    params_with(
        NEW,
        NEW_VERSION,
        Some((OLD, super::fake::OLD_VERSION)),
        "mode = \"restart\"\n",
    )
}

/// Asserts that an update between releases that both list `handoff-exec-v1` has the staged
/// release check the file in `prepare`, and that `cutover` puts the staged binary in place and
/// sends the job `SIGUSR2`: the launchd calls are the look at the job and the kill, with no
/// bootout and no bootstrap, the job then runs the new release, the wait is the build gauge on
/// `/metricz`, and the record has no mode. Catches an update that restarts a job that can hand
/// over, which unmounts, or one that does not wait for the new image.
#[test]
fn a_mac_update_with_handoff_exec_v1_sends_sigusr2_and_waits_for_the_target() {
    let fake = capable();
    fake.advance(&[Prepare]);
    let checks: Vec<String> = fake
        .log()
        .into_iter()
        .filter(|line| line.starts_with("host.check_config"))
        .collect();
    assert_eq!(checks, [format!("host.check_config {NEW} {CONFIG}")]);
    assert_eq!(fake.record(), in_flight(false));
    fake.advance(&[Cutover]);
    assert_eq!(fake.launchd_calls(), [loaded(), kill()]);
    assert_eq!(fake.binary_acts(), [format!("host.install_binary {NEW}")]);
    assert_eq!(fake.snapshot().job, path(NEW));
    assert!(fake.log().iter().any(|line| line == "get 9300 /metricz"));
    assert!(fake.record().has(Marker::Swapped));
    fake.advance(&[Verify, Cleanup]);
    let calls = fake.launchd_calls();
    assert!(!calls.contains(&bootout()));
    assert!(!calls.contains(&bootstrap()));
    assert_eq!(fake.record().mode, None);
    assert!(fake.strays().is_empty());
}

/// Asserts that a `cutover` run again after the hand-over only looks: the job already serves the
/// target, so no second `SIGUSR2` is sent. Catches a re-run that hands the process over twice.
#[test]
fn a_second_cutover_after_a_hand_over_sends_no_signal() {
    let fake = capable();
    fake.advance(&[Prepare, Cutover]);
    fake.advance(&[Cutover]);
    let kills = fake
        .launchd_calls()
        .iter()
        .filter(|c| **c == kill())
        .count();
    assert_eq!(kills, 1);
}

/// Asserts that `prepare` of a node whose file has an `fs` interface is refused with
/// `mode_unsupported` when the previous release, the staged release or both lack
/// `handoff-exec-v1`: the detail names the releases that lack it, nothing is recorded, no launchd
/// call is made and no release checks the file. Catches a restart that unmounts the node under
/// running jobs.
#[test]
fn a_mac_update_without_it_is_refused() {
    let cases = [
        (&[][..], &[HANDOFF_EXEC_CAPABILITY][..], vec![OLD]),
        (&[HANDOFF_EXEC_CAPABILITY][..], &[][..], vec![NEW]),
        (&[][..], &[][..], vec![NEW, OLD]),
    ];
    for (previous, staged, lacking) in cases {
        let fake = Fake::new();
        fake.set_config(CONFIG, Some(WITH_MOUNT));
        fake.set_capabilities(OLD, previous);
        fake.set_capabilities(NEW, staged);
        let done = fake.run(Prepare);
        assert_failed(&done, Reason::ModeUnsupported);
        let text = done.report.detail.clone().unwrap_or_default();
        for release in [NEW, OLD] {
            let named = text.contains(&format!("release {release}"));
            assert_eq!(named, lacking.contains(&release), "{release}: {text}");
        }
        assert!(text.contains(HANDOFF_EXEC_CAPABILITY), "{text}");
        assert!(text.contains("mode = \"restart\""), "{text}");
        assert_eq!(fake.record(), Record::idle(None));
        assert!(fake.launchd_calls().is_empty());
        assert!(
            !fake
                .log()
                .iter()
                .any(|line| line.starts_with("host.check_config"))
        );
    }
}

/// Asserts that with `mode = "restart"` in the params the same update is accepted and is the
/// restart of before: the releases are not asked, the calls are the look at the job, the bootout
/// and the bootstrap, and no signal is sent. Catches an override that does not reach the operator
/// who drained the node.
#[test]
fn mode_restart_overrides_the_refusal() {
    let fake = capable();
    fake.set_config(CONFIG, Some(WITH_MOUNT));
    let p = restart_params();
    for step in [Prepare, Cutover] {
        assert_ok(&fake.run_with(step, &p, LABEL));
    }
    assert_eq!(fake.launchd_calls(), [loaded(), bootout(), bootstrap()]);
    assert_eq!(fake.snapshot().job, path(NEW));
    assert!(
        !fake
            .log()
            .iter()
            .any(|line| line.starts_with("host.capabilities"))
    );
}

/// Asserts that a node whose file has no `fs` interface, and whose releases lack
/// `handoff-exec-v1`, is updated as before: `prepare` succeeds without a check of the file, and
/// `cutover` boots the job out and in. Catches a refusal of nodes that hold no mount.
#[test]
fn a_node_without_an_fs_interface_restarts_as_before() {
    let fake = Fake::new();
    fake.set_capabilities(NEW, &[HANDOFF_EXEC_CAPABILITY]);
    fake.advance(&[Prepare, Cutover]);
    assert_eq!(fake.launchd_calls(), [loaded(), bootout(), bootstrap()]);
    assert_eq!(fake.snapshot().job, path(NEW));
    assert!(
        !fake
            .log()
            .iter()
            .any(|line| line.starts_with("host.check_config"))
    );
}

/// Asserts that a Mac whose params name the fs job restarts as before even when both releases
/// list `handoff-exec-v1` and the file has an `fs` interface: `prepare` is not refused, asks no
/// release, and `cutover` boots the main job out and in with no signal. Catches a hand-over of a
/// node whose fs job would lose its mount.
#[test]
fn a_mac_with_an_fs_job_restarts_as_before() {
    let fake = capable();
    fake.with_fs(true);
    fake.set_config(CONFIG, Some(WITH_MOUNT));
    fake.advance(&[Prepare, Cutover]);
    let calls = fake.launchd_calls();
    assert!(calls.contains(&bootout()));
    assert!(calls.contains(&bootstrap()));
    assert!(!calls.contains(&kill()));
    assert!(
        !fake
            .log()
            .iter()
            .any(|line| line.starts_with("host.capabilities"))
    );
}

/// Asserts that a first install is never refused for a file with an `fs` interface and asks no
/// release: `prepare` and `cutover` bootstrap the job. Catches a refusal of an install that has
/// no mount to lose.
#[test]
fn a_first_install_of_a_node_with_a_mount_is_not_refused() {
    let fake = Fake::first_install();
    fake.set_config(CONFIG, Some(WITH_MOUNT));
    fake.advance(&[Prepare, Cutover]);
    assert_eq!(fake.launchd_calls(), [loaded(), bootstrap()]);
    assert!(
        !fake
            .log()
            .iter()
            .any(|line| line.starts_with("host.capabilities"))
    );
}

/// Asserts that a hand-over whose new image never serves the target fails `cutover` (`probe_failed`
/// when it answers as another build, `not_ready` when it never answers), still reversible, and
/// that `rollback` puts the previous binary in place and sends `SIGUSR2` again: the job runs the
/// previous release, with no bootout, and the record is at rest. Catches a rollback that restarts
/// the job, or one that leaves the new image.
#[test]
fn a_hand_over_whose_new_image_never_serves_is_handed_back_by_sigusr2() {
    for (reason, break_new) in [(Reason::ProbeFailed, true), (Reason::NotReady, false)] {
        let fake = capable();
        fake.advance(&[Prepare]);
        if break_new {
            fake.lie(NEW, "9.9.9");
        } else {
            fake.mute(NEW);
        }
        let done = fake.host_run(Cutover);
        assert_failed(&done, reason);
        assert!(done.report.reversible);
        let before = fake.launchd_calls().len();
        assert_ok(&fake.host_run(Rollback));
        assert_eq!(fake.launchd_calls()[before..], [loaded(), kill()]);
        assert_eq!(
            fake.binary_acts().last(),
            Some(&format!("host.install_binary {OLD}"))
        );
        assert_eq!(fake.snapshot().job, path(OLD));
        assert_eq!(fake.record(), Record::idle(None));
        assert!(!fake.launchd_calls().contains(&bootout()));
    }
}

/// Asserts that a job that is not loaded when `cutover` or `rollback` of a hand-over runs is
/// started by bootstrap, as before, and no signal is sent to a process that is not there. Catches
/// a hand-over that signals a job that is not loaded.
#[test]
fn a_job_that_is_not_loaded_is_bootstrapped_by_a_hand_over() {
    let fake = capable();
    fake.advance(&[Prepare]);
    fake.set_job(None);
    fake.advance(&[Cutover]);
    assert_eq!(fake.launchd_calls(), [loaded(), bootout(), bootstrap()]);
    assert_eq!(fake.snapshot().job, path(NEW));
    fake.set_job(None);
    let before = fake.launchd_calls().len();
    assert_ok(&fake.host_run(Rollback));
    assert_eq!(
        fake.launchd_calls()[before..],
        [loaded(), bootout(), bootstrap()]
    );
    assert_eq!(fake.snapshot().job, path(OLD));
}

/// Asserts that a `launchctl kill` that launchctl refuses fails `cutover` with `unit_failed`
/// before `swapped` is recorded; and that in `rollback` (a new image that failed can leave the job
/// with no process to signal) it is followed by the restart of the job on the previous release,
/// which then serves it, and the record is idle. Catches a hand-over that goes on after the signal
/// was not delivered, and a rollback that can never complete while the new image fails.
#[test]
fn a_kill_that_launchctl_refuses_fails_the_step() {
    let fake = capable();
    fake.advance(&[Prepare]);
    fake.fail_nth("launchd.kill", 1);
    assert_failed(&fake.host_run(Cutover), Reason::UnitFailed);
    assert!(!fake.record().has(Marker::Swapped));
    fake.heal();
    fake.advance(&[Cutover]);
    fake.fail_nth("launchd.kill", 1);
    let before = fake.launchd_calls().len();
    assert_ok(&fake.host_run(Rollback));
    assert_eq!(
        fake.launchd_calls()[before..],
        [loaded(), kill(), bootout(), bootstrap()]
    );
    assert_eq!(fake.snapshot().job, path(OLD));
    assert_eq!(fake.record(), Record::idle(None));
}

/// Asserts that a staged release that refuses the file fails `prepare` with `config_refused`,
/// naming the release and its line, records nothing and makes no launchd call; and that a check
/// that cannot be run fails it with `host_failed`. Catches a hand-over to an image that exits at
/// its first read of the file.
#[test]
fn a_staged_release_that_refuses_the_file_fails_prepare() {
    let fake = capable();
    fake.set_refusal(NEW, "unknown key server.nope");
    let done = fake.run(Prepare);
    assert_failed(&done, Reason::ConfigRefused);
    let text = done.report.detail.clone().unwrap_or_default();
    assert!(text.contains(NEW), "{text}");
    assert!(text.contains("unknown key server.nope"), "{text}");
    assert_eq!(fake.record(), Record::idle(None));
    assert!(fake.launchd_calls().is_empty());

    let fake = capable();
    fake.fail_nth("host.check_config", 1);
    assert_failed(&fake.run(Prepare), Reason::HostFailed);
    assert_eq!(fake.record(), Record::idle(None));
}

/// Asserts that a release that cannot be asked for its capabilities fails `prepare`, `cutover`
/// and `rollback` with `host_failed` and changes nothing. Catches a way that is guessed when the
/// releases cannot be asked.
#[test]
fn a_release_that_cannot_be_asked_fails_the_steps_that_decide_the_way() {
    let fake = capable();
    fake.fail_nth("host.capabilities", 1);
    assert_failed(&fake.run(Prepare), Reason::HostFailed);
    assert_eq!(fake.record(), Record::idle(None));
    fake.heal();
    fake.advance(&[Prepare]);
    fake.fail_nth("host.capabilities", 1);
    assert_failed(&fake.host_run(Cutover), Reason::HostFailed);
    assert_eq!(fake.snapshot().job, path(OLD));
    fake.heal();
    fake.advance(&[Cutover]);
    fake.fail_nth("host.capabilities", 1);
    assert_failed(&fake.host_run(Rollback), Reason::HostFailed);
    assert!(fake.record().updating);
}
