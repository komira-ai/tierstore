//! The systemd steps of the config hook: a config update by hand-over and by restart, and the
//! refusals of `prepare`, `cutover`, `verify` and `cleanup`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::super::super::{Instance, Marker, Mode, Params, Reason, Record, Step};
use super::{
    Linux, PKG_NEW, PKG_OLD, assert_restarts, build, first, in_flight, lin, release_record, staged,
    update, with_interface, with_socket,
};
use Step::{Cleanup, Cutover, Prepare, Verify};

/// Asserts that a config update whose two files listen alike and name one hand-over socket starts
/// the other instance with the old one draining, reloads the guard once, and that cleanup retires
/// the old instance and writes the new letter into the release's record, leaving its `to` and
/// `version` as they were.
#[test]
fn a_config_update_hands_over_and_retires_the_old_instance() {
    let node = Linux::alike();
    let p = update();

    let prepared = node.step(Prepare, &p);
    lin::assert_ok(&prepared);
    let record = node.fake.record();
    assert!(record.updating);
    assert_eq!(record.mode, Some(Mode::Handover));
    assert_eq!(
        (record.live, record.next),
        (Some(Instance::A), Some(Instance::B))
    );
    assert_eq!(record.to.as_deref(), Some(Path::new(PKG_NEW)));
    assert_eq!(record.version, Some(build()));
    assert!(node.fake.actions().is_empty());
    assert_eq!(node.reloads(), 0);

    lin::assert_ok(&node.step(Cutover, &p));
    assert_eq!(node.reloads(), 1);
    assert_eq!(node.fake.actions(), ["markers.set b", "units.start b"]);
    assert_eq!(node.fake.running(), [Instance::A, Instance::B]);

    lin::assert_ok(&node.step(Verify, &p));
    assert!(node.fake.record().has(Marker::Verified));
    assert_eq!(node.service.get(), Some(release_record(Instance::A)));

    lin::assert_ok(&node.step(Cleanup, &p));
    assert_eq!(node.fake.running(), [Instance::B]);
    assert_eq!(node.fake.markers(), [Instance::B]);
    let record = node.fake.record();
    assert!(!record.updating);
    assert_eq!(record.live, Some(Instance::B));
    assert_eq!(node.service.get(), Some(release_record(Instance::B)));
}

/// Asserts that the staged file is checked by the serving binary in `prepare` (by the hook and
/// by the arm, for the staged and the previous release alike) and the installed one at `verify`,
/// every time by the release that serves.
#[test]
fn the_serving_binary_checks_the_staged_file_and_then_the_installed_one() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare]);
    node.go(&p, &[Cutover, Verify]);
    let release = PathBuf::from(lin::OLD);
    assert_eq!(
        node.files.checked(),
        [
            (release.clone(), staged(lin::CONFIG)),
            (release.clone(), staged(lin::CONFIG)),
            (release.clone(), staged(lin::CONFIG)),
            (release, PathBuf::from(lin::CONFIG)),
        ]
    );
}

/// Asserts that a staged file that listens on another interface is a restart: the old instance
/// stops, then the new one starts.
#[test]
fn a_config_update_that_changes_the_interface_restarts() {
    assert_restarts(
        Some(&with_interface("0.0.0.0")),
        &with_interface("10.0.0.7"),
        &update(),
    );
}

/// Asserts that a staged file that names another hand-over socket is a restart.
#[test]
fn a_config_update_that_names_another_handoff_socket_restarts() {
    assert_restarts(
        Some(lin::WITH_SOCKET),
        &with_socket("/var/lib/tierstore/other.sock"),
        &update(),
    );
}

/// Asserts that a staged file with a directory hot tier is a restart.
#[test]
fn a_staged_file_with_a_directory_hot_tier_restarts() {
    assert_restarts(Some(lin::WITH_SOCKET), lin::DIR_HOT, &update());
}

/// Asserts that a serving file with a directory hot tier makes the change a restart.
#[test]
fn a_serving_file_with_a_directory_hot_tier_restarts() {
    assert_restarts(Some(lin::DIR_HOT), lin::WITH_SOCKET, &update());
}

/// Asserts that a node whose serving file is missing restarts.
#[test]
fn a_config_update_over_a_missing_serving_file_restarts() {
    assert_restarts(None, lin::WITH_SOCKET, &update());
}

/// Asserts that a node whose serving file is not TOML restarts.
#[test]
fn a_config_update_over_a_serving_file_that_is_not_toml_restarts() {
    assert_restarts(Some("not = [toml"), lin::WITH_SOCKET, &update());
}

/// Asserts that params that declare a restart (`mode = "restart"`) force one when the files
/// would hand over.
#[test]
fn a_declared_restart_restarts_although_the_files_listen_alike() {
    let p = lin::params_with(
        PKG_NEW,
        Some(PKG_OLD),
        &format!("config_file = \"{}\"\nmode = \"restart\"\n", lin::CONFIG),
    );
    let node = Linux::alike();
    node.go(&p, &[Prepare]);
    assert_eq!(node.fake.record().mode, Some(Mode::Restart));
}

/// Asserts that the first config package of a node (params with no previous package) still
/// updates by hand-over and then cleans up.
#[test]
fn the_first_config_package_updates_like_any_other() {
    let node = Linux::alike();
    let p = first();
    node.go(&p, &[Prepare, Cutover, Verify, Cleanup]);
    assert_eq!(node.fake.running(), [Instance::B]);
    assert_eq!(node.service.get(), Some(release_record(Instance::B)));
}

/// Asserts that every step run again after it succeeded succeeds and changes neither the
/// instances, the markers, the config hook's record nor the release's record.
#[test]
fn a_step_run_again_after_it_succeeded_changes_nothing() {
    let node = Linux::alike();
    let p = update();
    for step in [Prepare, Cutover, Verify, Cleanup] {
        node.go(&p, &[step]);
        let (snapshot, service) = (node.fake.snapshot(), node.service.get());
        lin::assert_ok(&node.step(step, &p));
        assert_eq!(node.fake.snapshot(), snapshot, "{step} again");
        assert_eq!(node.service.get(), service, "{step} again");
    }
}

/// Asserts that `prepare` refuses with out_of_order while the release's record says an update is
/// in flight, and records and starts nothing.
#[test]
fn prepare_waits_while_an_update_of_the_release_is_in_flight() {
    let node = Linux::alike();
    node.service.set(&in_flight(Some(Instance::A)));
    let before = node.fake.snapshot();
    let done = node.step(Prepare, &update());
    lin::assert_failed(&done, Reason::OutOfOrder);
    assert!(lin::detail(&done).contains("an update of the release is in flight"));
    assert_eq!(node.fake.snapshot(), before);
}

/// Asserts that `prepare` goes on when the node has no record of the release at all.
#[test]
fn prepare_goes_on_when_the_release_has_no_record() {
    let node = Linux::alike();
    *node.service.bytes.borrow_mut() = None;
    node.go(&update(), &[Prepare]);
    assert!(node.fake.record().updating);
}

/// Asserts that `prepare` refuses with state_invalid when the release's record cannot be read.
#[test]
fn prepare_refuses_a_release_record_that_cannot_be_decoded() {
    let node = Linux::alike();
    *node.service.bytes.borrow_mut() = Some(b"not json".to_vec());
    lin::assert_failed(&node.step(Prepare, &update()), Reason::StateInvalid);
}

/// Asserts that `prepare` refuses with state_io when the release's record cannot be loaded.
#[test]
fn prepare_refuses_when_the_release_record_cannot_be_loaded() {
    let node = Linux::alike();
    node.service.fail_load.set(true);
    lin::assert_failed(&node.step(Prepare, &update()), Reason::StateIo);
}

/// Asserts that `prepare` refuses with config_missing, naming the path, when the package stages
/// no file at `<staged>/tree/<config file>`, and records nothing.
#[test]
fn prepare_refuses_a_package_that_stages_no_configuration() {
    let node = Linux::alike();
    node.files.texts.borrow_mut().remove(&staged(lin::CONFIG));
    let before = node.fake.snapshot();
    let done = node.step(Prepare, &update());
    lin::assert_failed(&done, Reason::ConfigMissing);
    assert_eq!(
        lin::detail(&done),
        format!(
            "the config package stages no {}",
            staged(lin::CONFIG).display()
        )
    );
    assert_eq!(node.fake.snapshot(), before);
}

/// Asserts that `prepare` refuses with config_refused when the serving binary refuses the staged
/// file, with the line it said, and records nothing.
#[test]
fn prepare_refuses_a_staged_file_the_serving_binary_refuses() {
    let node = Linux::alike();
    node.files
        .refuse(&staged(lin::CONFIG), "unknown key `lisen`");
    let before = node.fake.snapshot();
    let done = node.step(Prepare, &update());
    lin::assert_failed(&done, Reason::ConfigRefused);
    assert_eq!(
        lin::detail(&done),
        format!(
            "the staged configuration {} is refused: unknown key `lisen`",
            staged(lin::CONFIG).display()
        )
    );
    assert_eq!(node.fake.snapshot(), before);
}

/// Asserts that `prepare` fails with host_failed when the binary cannot be run to check the
/// staged file.
#[test]
fn prepare_fails_when_the_staged_file_cannot_be_checked() {
    let node = Linux::alike();
    node.files.checks_fail.set(true);
    lin::assert_failed(&node.step(Prepare, &update()), Reason::HostFailed);
}

/// Asserts that `prepare` fails with host_failed when the staged file cannot be read.
#[test]
fn prepare_fails_when_the_staged_file_cannot_be_read() {
    let node = Linux::alike();
    node.files
        .unreadable
        .borrow_mut()
        .insert(staged(lin::CONFIG));
    lin::assert_failed(&node.step(Prepare, &update()), Reason::HostFailed);
}

/// Asserts that `prepare` fails with host_failed when the serving file cannot be read.
#[test]
fn prepare_fails_when_the_serving_file_cannot_be_read() {
    let node = Linux::alike();
    node.files
        .unreadable
        .borrow_mut()
        .insert(PathBuf::from(lin::CONFIG));
    lin::assert_failed(&node.step(Prepare, &update()), Reason::HostFailed);
}

/// Asserts that params that name no configuration file are refused with bad_params.
#[test]
fn a_step_without_a_configuration_file_in_its_params_is_refused() {
    let node = Linux::alike();
    let mut p = update();
    p.config_file = None;
    let done = node.step(Prepare, &p);
    lin::assert_failed(&done, Reason::BadParams);
    assert_eq!(lin::detail(&done), "the params name no configuration file");
}

/// Asserts that a guard that fails to reload fails `cutover` with unit_failed and starts nothing.
#[test]
fn a_guard_that_fails_to_reload_fails_cutover_before_any_instance_starts() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare]);
    node.guard.broken.set(true);
    let done = node.step(Cutover, &p);
    lin::assert_failed(&done, Reason::UnitFailed);
    assert!(lin::detail(&done).contains("nft: syntax error"));
    assert!(node.fake.actions().is_empty());
    assert_eq!(node.fake.running(), [Instance::A]);
    assert!(!node.fake.record().has(Marker::Swapped));
}

/// Asserts that `verify` refuses with config_refused when the serving binary refuses the
/// installed file, and records nothing.
#[test]
fn verify_refuses_an_installed_file_the_binary_refuses() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    node.files
        .refuse(Path::new(lin::CONFIG), "[server] needs a listen address");
    let done = node.step(Verify, &p);
    lin::assert_failed(&done, Reason::ConfigRefused);
    assert_eq!(
        lin::detail(&done),
        format!(
            "the installed configuration {} is refused: [server] needs a listen address",
            lin::CONFIG
        )
    );
    assert!(!node.fake.record().has(Marker::Verified));
}

/// Asserts that a `cleanup` before any `verify` fails with not_verified and leaves the release's
/// record alone.
#[test]
fn cleanup_before_verify_fails_and_writes_nothing_back() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    lin::assert_failed(&node.step(Cleanup, &p), Reason::NotVerified);
    assert_eq!(node.service.get(), Some(release_record(Instance::A)));
}

/// Asserts that a `cleanup` that finds the old instance still draining is deferred and leaves
/// the release's record alone, and that a run again retires it and writes the letter back.
#[test]
fn cleanup_waits_for_the_drain_before_it_writes_the_instance_back() {
    let node = Linux::alike();
    let p = update();
    node.fake.set_drain_looks(1);
    node.go(&p, &[Prepare, Cutover, Verify]);
    lin::assert_deferred(&node.step(Cleanup, &p), Reason::Draining, 10);
    assert_eq!(node.service.get(), Some(release_record(Instance::A)));
    node.go(&p, &[Cleanup]);
    assert_eq!(node.service.get(), Some(release_record(Instance::B)));
}

/// Asserts that `cleanup` leaves a release record that is absent as it is.
#[test]
fn cleanup_writes_nothing_when_the_release_has_no_record() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover, Verify]);
    *node.service.bytes.borrow_mut() = None;
    node.go(&p, &[Cleanup]);
    assert_eq!(node.service.get(), None);
}

/// Asserts that `cleanup` leaves a release record whose update is in flight as it is.
#[test]
fn cleanup_writes_nothing_into_a_release_record_that_is_updating() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover, Verify]);
    let updating = in_flight(Some(Instance::A));
    node.service.set(&updating);
    node.go(&p, &[Cleanup]);
    assert_eq!(node.service.get(), Some(updating));
}

/// Asserts that `cleanup` leaves a release record that already names the new instance as it is,
/// without a write.
#[test]
fn cleanup_does_not_write_a_release_record_that_already_names_the_instance() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover, Verify]);
    node.service.set(&release_record(Instance::B));
    node.service.fail_save.set(true);
    node.go(&p, &[Cleanup]);
    assert_eq!(node.service.get(), Some(release_record(Instance::B)));
}

/// Asserts that a `cleanup` over a release record that cannot be decoded fails with
/// state_invalid.
#[test]
fn cleanup_fails_over_a_release_record_that_cannot_be_decoded() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover, Verify]);
    *node.service.bytes.borrow_mut() = Some(b"not json".to_vec());
    lin::assert_failed(&node.step(Cleanup, &p), Reason::StateInvalid);
}

/// Asserts that a `cleanup` over a release record that would be invalid once written fails with
/// state_invalid and leaves it as it was.
#[test]
fn cleanup_does_not_write_a_release_record_that_breaks_the_record_rules() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover, Verify]);
    let orphan = Record {
        markers: BTreeSet::from([Marker::Swapped]),
        ..release_record(Instance::A)
    };
    node.service.set(&orphan);
    lin::assert_failed(&node.step(Cleanup, &p), Reason::StateInvalid);
    assert_eq!(node.service.get(), Some(orphan));
}

/// Asserts that a `cleanup` whose write of the release's record fails reports state_io.
#[test]
fn cleanup_fails_when_the_release_record_cannot_be_written() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover, Verify]);
    node.service.fail_save.set(true);
    lin::assert_failed(&node.step(Cleanup, &p), Reason::StateIo);
}

/// The number of port calls a `cleanup` makes on a node ready for it.
fn cleanup_calls(p: &Params) -> usize {
    let node = Linux::alike();
    node.go(p, &[Prepare, Cutover, Verify]);
    let before = node.fake.calls();
    node.go(p, &[Cleanup]);
    node.fake.calls() - before
}

/// Asserts that a `cleanup` that cannot take the lock of its own record to write back (the last
/// two calls are the lock and the load) fails with state_io.
#[test]
fn cleanup_fails_when_its_own_record_cannot_be_locked_or_read_for_the_write_back() {
    let p = update();
    let calls = cleanup_calls(&p);
    for (from_the_end, reason) in [(1, Reason::StateIo), (0, Reason::StateIo)] {
        let node = Linux::alike();
        node.go(&p, &[Prepare, Cutover, Verify]);
        node.fake.halt_at(Some(calls - from_the_end));
        lin::assert_failed(&node.step(Cleanup, &p), reason);
        assert_eq!(node.service.get(), Some(release_record(Instance::A)));
    }
}

/// Asserts that `verify` fails with host_failed when the binary cannot be run to check the
/// installed file, and records nothing.
#[test]
fn verify_fails_when_the_installed_file_cannot_be_checked() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    node.files.checks_fail.set(true);
    lin::assert_failed(&node.step(Verify, &p), Reason::HostFailed);
    assert!(!node.fake.record().has(Marker::Verified));
}
