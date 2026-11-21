//! The binary the units run, on a host where hostkeeper keeps every package closed to every user
//! but root: the template runs the copy at `SERVICE_BINARY`, which `cutover`, `rollback` and
//! `restore` make the binary of the release `current` names before they start an instance, and
//! which a first install's rollback and `uninstall` remove. The fake's instance runs the
//! template's program as the service account, which reaches only that copy.

use std::path::PathBuf;

use crate::server::update_hook::{Instance, Params, Reason, SERVICE_BINARY, Step, UnitState};

use super::fake::{
    CONFIG, NEW, OLD, SystemdFake, assert_failed, assert_ok, params, params_with, unit_program,
};

use Step::{Cutover, Prepare, Restore, Rollback, Uninstall, Verify};

fn restart_params() -> Params {
    params_with(
        NEW,
        Some(OLD),
        &format!("config_file = \"{CONFIG}\"\nmode = \"restart\"\n"),
    )
}

fn release(path: &str) -> Option<PathBuf> {
    Some(PathBuf::from(path))
}

/// Asserts that the template the package ships runs the copy at `SERVICE_BINARY`, and that an
/// instance whose copy is not there does not start, as on a host whose packages only root may
/// enter. Catches a template that runs the binary inside the package, which the service account
/// cannot reach.
#[test]
fn the_template_runs_the_copy_and_an_instance_without_it_does_not_start() {
    assert_eq!(unit_program(), SERVICE_BINARY);
    let fake = SystemdFake::installed();
    fake.set_installed(None);
    fake.reboot();
    assert_eq!(fake.peek(Instance::A), UnitState::Failed);
    assert!(fake.running().is_empty());
}

/// Asserts that a hand-over puts the staged release's binary in place before it starts the new
/// instance, which then serves the staged release while the old one drains on the old copy.
/// Catches a cutover that starts the new instance on the previous binary, or on none.
#[test]
fn a_hand_over_puts_the_staged_binary_in_place_before_it_starts_the_new_instance() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    assert!(fake.binary_acts().is_empty());
    fake.drive(&p, &[Cutover, Verify]);
    assert_eq!(
        fake.binary_acts(),
        [
            format!("host.install_binary {NEW}"),
            "units.start b".to_owned()
        ]
    );
    let end = fake.snapshot();
    assert_eq!(end.installed, release(NEW));
    assert_eq!(end.units[1], (Instance::B, UnitState::Active, release(NEW)));
}

/// Asserts that a restart puts the staged release's binary in place before it stops the old
/// instance and starts the new one. Catches a restart that stops the service before it knows the
/// new binary is there.
#[test]
fn a_restart_puts_the_staged_binary_in_place_before_it_stops_the_old_instance() {
    let fake = SystemdFake::installed();
    let p = restart_params();
    fake.drive(&p, &[Prepare, Cutover]);
    let log = fake.log();
    let at = |line: &str| log.iter().position(|l| l == line).expect(line);
    assert!(at(&format!("host.install_binary {NEW}")) < at("units.stop a"));
    assert!(at("units.stop a") < at("units.start b"));
    assert_eq!(fake.snapshot().units[1].2, release(NEW));
}

/// Asserts that a first install puts the staged release's binary in place before it starts the
/// first instance. Catches a first install whose instance has no binary to run.
#[test]
fn a_first_install_puts_the_binary_in_place_before_it_starts_the_first_instance() {
    let fake = SystemdFake::empty();
    let p = params(NEW, None);
    fake.drive(&p, &[Prepare, Cutover]);
    assert_eq!(
        fake.binary_acts(),
        [
            format!("host.install_binary {NEW}"),
            "units.start a".to_owned()
        ]
    );
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.snapshot().units[0].2, release(NEW));
}

/// Asserts that a cutover whose binary cannot be put in place fails `host_failed`, reversible,
/// before it stops or starts anything: the old instance serves on. Catches a restart that stops
/// the service and then finds it has nothing to start.
#[test]
fn a_cutover_whose_binary_cannot_be_put_in_place_touches_no_instance() {
    let fake = SystemdFake::installed();
    let p = restart_params();
    fake.drive(&p, &[Prepare]);
    fake.fail_on(Some("host.install_binary"));
    let done = fake.run(Cutover, &p);
    assert_failed(&done, Reason::HostFailed);
    assert!(done.report.reversible);
    assert!(fake.actions().is_empty());
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.snapshot().installed, release(OLD));
}

/// Asserts that the rollback of a hand-over puts the previous release's binary back before it
/// starts the old instance, which then serves the previous release. Catches a rollback that
/// starts the old instance on the new binary.
#[test]
fn a_hand_over_rollback_puts_the_previous_binary_back_before_the_old_instance_starts() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    let before = fake.binary_acts().len();
    assert_ok(&fake.run_as_hostkeeper(Rollback, &p));
    assert_eq!(
        fake.binary_acts()[before..],
        [
            format!("host.install_binary {OLD}"),
            "units.start a".to_owned()
        ]
    );
    let end = fake.snapshot();
    assert_eq!(end.installed, release(OLD));
    assert_eq!(end.units[0], (Instance::A, UnitState::Active, release(OLD)));
    assert_eq!(fake.running(), [Instance::A]);
}

/// Asserts that the rollback of a restart puts the previous release's binary back before it
/// starts the old instance. Catches a restart rolled back onto the binary that failed.
#[test]
fn a_restart_rollback_puts_the_previous_binary_back_before_the_old_instance_starts() {
    let fake = SystemdFake::installed();
    let p = restart_params();
    fake.drive(&p, &[Prepare, Cutover]);
    let before = fake.binary_acts().len();
    assert_ok(&fake.run_as_hostkeeper(Rollback, &p));
    assert_eq!(
        fake.binary_acts()[before..],
        [
            format!("host.install_binary {OLD}"),
            "units.start a".to_owned()
        ]
    );
    assert_eq!(fake.snapshot().units[0].2, release(OLD));
}

/// Asserts that the rollback of a first install removes the binary it put in place, and fails
/// `no_previous_release` with no instance running. Catches a failed first install that leaves a
/// binary nothing runs any more.
#[test]
fn a_first_install_rollback_removes_the_binary() {
    let fake = SystemdFake::empty();
    let p = params(NEW, None);
    fake.drive(&p, &[Prepare, Cutover]);
    let done = fake.run_as_hostkeeper(Rollback, &p);
    assert_failed(&done, Reason::NoPreviousRelease);
    assert_eq!(fake.binary_acts().last().unwrap(), "host.remove_binary");
    assert_eq!(fake.snapshot().installed, None);
    assert!(fake.running().is_empty());
}

/// Asserts that the boot step puts the binary of the release its params name (the current one)
/// in place before it starts the recorded instance, whatever copy was there, and that a host with
/// nothing recorded gets no binary. Catches a reboot that starts the instance on a stale copy.
#[test]
fn restore_puts_the_current_binary_in_place_before_it_starts_the_instance() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.complete(&p);
    fake.set_installed(Some(OLD));
    fake.reboot();
    let before = fake.binary_acts().len();
    assert_ok(&fake.run(Restore, &p));
    assert_eq!(
        fake.binary_acts()[before..],
        [
            format!("host.install_binary {NEW}"),
            "units.start b".to_owned()
        ]
    );
    assert_eq!(fake.snapshot().installed, release(NEW));

    let bare = SystemdFake::empty();
    assert_ok(&bare.run(Restore, &params(NEW, None)));
    assert!(bare.binary_acts().is_empty());
    assert_eq!(bare.snapshot().installed, None);
}

/// Asserts that `uninstall` removes the binary after it has stopped both instances. Catches an
/// uninstall that leaves the binary behind, or removes it under a running instance.
#[test]
fn uninstall_removes_the_binary_after_it_stops_both_instances() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.complete(&p);
    assert_ok(&fake.run(Uninstall, &p));
    let log = fake.log();
    let at = |line: &str| log.iter().position(|l| l == line).expect(line);
    assert!(at("units.stop b") < at("host.remove_binary"));
    assert_eq!(fake.snapshot().installed, None);
}
