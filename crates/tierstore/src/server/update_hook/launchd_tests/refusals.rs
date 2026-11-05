//! What every step refuses or reports when the record, the params or a port is not as the step
//! needs: the lock, the record, the order of the steps, the params, the commit and each port.

use std::collections::BTreeSet;

use super::fake::{
    CONFIG, Fake, LABEL, NEW, NEW_VERSION, OLD, assert_failed, assert_ok, in_flight, params_for,
    path,
};
use crate::server::update_hook::{Instance, Marker, Mode, Params, Reason, Record, Step};

use Step::{Cleanup, Cutover, Prepare, Restore, Rollback, Uninstall, Verify};

const OTHER: &str = "/var/lib/hostkeeper/packages/tierstore/9.9.9";

/// Asserts that a step that finds the lock held fails with `locked` before it reads or does
/// anything. Catches a step that runs beside another.
#[test]
fn a_held_lock_refuses_every_step_before_it_does_anything() {
    for step in Step::ALL {
        let fake = Fake::new();
        fake.set_busy(true);
        let done = fake.run(step);
        assert_failed(&done, Reason::Locked);
        assert_eq!(fake.log(), ["state.lock"], "{step}");
        assert_eq!(fake.live_locks(), 0);
    }
}

/// Asserts that a record that is not JSON, one that breaks the order of its markers and one of a
/// newer schema each fail every step with `state_invalid` before any launchd call. Catches a
/// step that acts on a record it cannot trust.
#[test]
fn an_unreadable_or_inconsistent_record_refuses_every_step() {
    let broken_order = Record {
        markers: BTreeSet::from([Marker::Verified]),
        ..in_flight(false)
    };
    let newer = br#"{"schema":2,"kind":"single","updating":false,"markers":[]}"#.to_vec();
    for bytes in [b"not json".to_vec(), broken_order.encode(), newer] {
        for step in Step::ALL {
            let fake = Fake::new();
            fake.set_record_bytes(Some(&bytes));
            assert_failed(&fake.host_run(step), Reason::StateInvalid);
            assert!(fake.launchd_calls().is_empty(), "{step}");
        }
    }
}

/// Asserts that a record that names a systemd instance, a next instance or a mode (what the
/// systemd arm writes) fails every step with `state_invalid`, and no launchd call is made.
/// Catches a launchd step that acts on a record of the other arm.
#[test]
fn a_record_of_the_systemd_arm_is_refused() {
    let records = [
        Record::idle(Some(Instance::A)),
        Record {
            next: Some(Instance::B),
            ..in_flight(false)
        },
        Record {
            mode: Some(Mode::Restart),
            ..in_flight(false)
        },
    ];
    for record in records {
        for step in Step::ALL {
            let fake = Fake::new();
            fake.set_record(&record);
            assert_failed(&fake.host_run(step), Reason::StateInvalid);
            assert!(fake.launchd_calls().is_empty(), "{step}");
        }
    }
}

/// Asserts that a `prepare` after `cutover` ran is refused with `out_of_order` and leaves the
/// record as it was. Catches a prepare that overwrites an update past its cutover.
#[test]
fn prepare_past_the_cutover_is_out_of_order() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    let before = fake.snapshot();
    assert_failed(&fake.host_run(Prepare), Reason::OutOfOrder);
    assert_eq!(fake.snapshot(), before);
}

/// Asserts that a `prepare` whose params have no `config_file` fails with `bad_params`, and one
/// whose file does not exist fails with `config_missing`, each leaving the record as it was.
/// Catches a prepare that records an update for a node it cannot run.
#[test]
fn prepare_needs_a_configuration_file_that_exists() {
    let fake = Fake::new();
    let bare = Params {
        config_file: None,
        ..fake.params()
    };
    assert_failed(&fake.run_with(Prepare, &bare, LABEL), Reason::BadParams);
    fake.set_config(CONFIG, None);
    assert_failed(&fake.run(Prepare), Reason::ConfigMissing);
    assert_eq!(fake.record(), Record::idle(None));
}

/// Asserts that `cutover` and `verify` on a host with no update in flight fail with
/// `out_of_order`, as does `cutover` after the commit, and `verify` before `cutover`. Catches a
/// step that acts on an update that was never prepared or one that is over.
#[test]
fn steps_out_of_order_are_refused() {
    let fake = Fake::new();
    assert_failed(&fake.host_run(Cutover), Reason::OutOfOrder);
    assert_failed(&fake.host_run(Verify), Reason::OutOfOrder);
    fake.advance(&[Prepare]);
    assert_failed(&fake.host_run(Verify), Reason::OutOfOrder);
    assert!(fake.launchd_calls().is_empty());
    fake.advance(&[Cutover, Verify, Cleanup]);
    assert_failed(&fake.host_run(Cutover), Reason::OutOfOrder);
}

/// Asserts that every step after `prepare` refuses params that name another release, or another
/// version of the same release, than the update on record, with `bad_params` and no launchd call,
/// while the update is in flight and after it is committed. Catches a step that acts on an
/// update it does not belong to.
#[test]
fn params_of_another_update_are_refused() {
    let other_release = params_for(OTHER, NEW_VERSION, Some((OLD, "0.3.0+000000000003")));
    let other_version = params_for(NEW, "0.4.1", Some((OLD, "0.3.0+000000000003")));
    for params in [&other_release, &other_version] {
        let fake = Fake::new();
        fake.advance(&[Prepare, Cutover, Verify]);
        let before = fake.launchd_calls().len();
        for step in [Cutover, Verify, Cleanup, Rollback] {
            assert_failed(&fake.run_with(step, params, LABEL), Reason::BadParams);
        }
        assert_eq!(fake.launchd_calls().len(), before);
        fake.advance(&[Cleanup]);
        assert_failed(&fake.run_with(Verify, params, LABEL), Reason::BadParams);
        assert_failed(&fake.run_with(Cleanup, params, LABEL), Reason::BadParams);
    }
}

/// Asserts that `verify` fails with `not_ready` when the job is not loaded, and records nothing.
/// Catches a verify that records `verified` for a job launchd does not have.
#[test]
fn verify_of_an_unloaded_job_is_not_ready() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.set_job(None);
    assert_failed(&fake.host_run(Verify), Reason::NotReady);
    assert!(!fake.record().has(Marker::Verified));
}

/// Asserts that `cleanup` before a `verify` fails with `not_verified` and is still reversible.
/// Catches a cleanup that commits on its own authority.
#[test]
fn cleanup_before_verify_is_refused() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    let done = fake.host_run(Cleanup);
    assert_failed(&done, Reason::NotVerified);
    assert!(done.report.reversible);
    assert!(!fake.record().committed());
}

/// Asserts that a `cleanup` that finds the job unloaded, or serving another build, fails with
/// `not_confirmed`, writes no `stopping` and stays reversible. Catches a commit made on what
/// `verify` saw earlier rather than on what is there now.
#[test]
fn cleanup_that_cannot_confirm_the_release_does_not_commit() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify]);
    fake.set_job(None);
    let unloaded = fake.host_run(Cleanup);
    assert_failed(&unloaded, Reason::NotConfirmed);
    assert!(unloaded.report.reversible);
    fake.host_run(Restore);
    fake.lie(NEW, "9.9.9");
    let wrong = fake.host_run(Cleanup);
    assert_failed(&wrong, Reason::NotConfirmed);
    assert!(wrong.report.reversible);
    assert!(!fake.record().committed());
}

/// Asserts that when the write of `stopping` fails before it reaches the disk, the record read
/// back is not committed and the report stays reversible; when it fails after reaching the disk,
/// the record read back is committed and the report is irreversible, and the next `cleanup`
/// finishes the update; and when the record cannot be read back, the commit counts as made.
/// Catches a report that says reversible for a commit that is on the disk.
#[test]
fn a_failed_write_of_stopping_is_read_back_to_say_whether_it_committed() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify]);
    fake.fail_nth("state.save", 1);
    let lost = fake.host_run(Cleanup);
    assert_failed(&lost, Reason::StateIo);
    assert!(lost.report.reversible);
    assert!(!fake.record().committed());

    fake.heal();
    fake.fail_late_nth("state.save", 1);
    let written = fake.host_run(Cleanup);
    assert_failed(&written, Reason::StateIo);
    assert!(!written.report.reversible);
    assert!(fake.record().committed());
    assert!(fake.record().updating);
    fake.heal();
    assert_ok(&fake.host_run(Cleanup));
    assert!(!fake.record().updating);

    let twin = Fake::new();
    twin.advance(&[Prepare, Cutover, Verify]);
    let from = twin.calls();
    twin.advance(&[Cleanup]);
    let calls = twin.calls() - from;
    let unknown = Fake::new();
    unknown.advance(&[Prepare, Cutover, Verify]);
    // The stopping write is the call before the last: the one after it ends the update.
    unknown.halt_at(calls - 1);
    let undecided = unknown.host_run(Cleanup);
    assert_failed(&undecided, Reason::StateIo);
    assert!(!undecided.report.reversible);
    assert!(!unknown.record().committed());
}

/// Asserts that a failure of the write that ends the update, after `stopping` was written, is
/// irreversible; that a `cutover` of the committed update still in flight is out of order and
/// changes nothing; and that the next `cleanup` finishes. Catches a cleanup that cannot finish a
/// commit, and a cutover that restarts the job of an update already committed.
#[test]
fn a_failure_after_the_commit_is_irreversible_and_cleanup_finishes_it() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify]);
    fake.fail_nth("state.save", 2);
    let done = fake.host_run(Cleanup);
    assert_failed(&done, Reason::StateIo);
    assert!(!done.report.reversible);
    assert!(fake.record().committed());
    assert!(fake.record().updating);
    fake.heal();
    let committed = fake.record();
    let launchd = fake.launchd_calls().len();
    assert_failed(&fake.run(Cutover), Reason::OutOfOrder);
    assert_eq!(fake.record(), committed);
    assert_eq!(fake.launchd_calls().len(), launchd);
    assert_ok(&fake.host_run(Cleanup));
    assert!(!fake.record().updating);
}

struct Case {
    name: &'static str,
    first: bool,
    setup: &'static [Step],
    reboot: bool,
    act: Step,
    prefix: &'static str,
    reason: Reason,
}

const fn case(
    name: &'static str,
    setup: &'static [Step],
    act: Step,
    prefix: &'static str,
    reason: Reason,
) -> Case {
    Case {
        name,
        first: false,
        setup,
        reboot: false,
        act,
        prefix,
        reason,
    }
}

fn cases() -> Vec<Case> {
    let installing = |c: Case| Case { first: true, ..c };
    let rebooted = |c: Case| Case { reboot: true, ..c };
    let up: &'static [Step] = &[Prepare];
    let cut: &'static [Step] = &[Prepare, Cutover];
    let ver: &'static [Step] = &[Prepare, Cutover, Verify];
    vec![
        case("prepare lock", &[], Prepare, "state.lock", Reason::StateIo),
        case("prepare load", &[], Prepare, "state.load", Reason::StateIo),
        case(
            "prepare config",
            &[],
            Prepare,
            "host.config",
            Reason::HostFailed,
        ),
        case("prepare save", &[], Prepare, "state.save", Reason::StateIo),
        case(
            "cutover loaded",
            up,
            Cutover,
            "launchd.loaded",
            Reason::UnitFailed,
        ),
        case(
            "cutover bootout",
            up,
            Cutover,
            "launchd.bootout",
            Reason::UnitFailed,
        ),
        case(
            "cutover bootstrap",
            up,
            Cutover,
            "launchd.bootstrap",
            Reason::UnitFailed,
        ),
        case("cutover save", up, Cutover, "state.save", Reason::StateIo),
        installing(case(
            "first cutover loaded",
            up,
            Cutover,
            "launchd.loaded",
            Reason::UnitFailed,
        )),
        installing(case(
            "first cutover bootstrap",
            up,
            Cutover,
            "launchd.bootstrap",
            Reason::UnitFailed,
        )),
        case(
            "verify loaded",
            cut,
            Verify,
            "launchd.loaded",
            Reason::UnitFailed,
        ),
        case("verify save", cut, Verify, "state.save", Reason::StateIo),
        case(
            "cleanup loaded",
            ver,
            Cleanup,
            "launchd.loaded",
            Reason::UnitFailed,
        ),
        case("cleanup save", ver, Cleanup, "state.save", Reason::StateIo),
        case(
            "rollback bootout",
            cut,
            Rollback,
            "launchd.bootout",
            Reason::UnitFailed,
        ),
        case(
            "rollback bootstrap",
            cut,
            Rollback,
            "launchd.bootstrap",
            Reason::UnitFailed,
        ),
        case(
            "rollback save",
            cut,
            Rollback,
            "state.save",
            Reason::StateIo,
        ),
        installing(case(
            "first rollback bootout",
            cut,
            Rollback,
            "launchd.bootout",
            Reason::UnitFailed,
        )),
        installing(case(
            "first rollback save",
            cut,
            Rollback,
            "state.save",
            Reason::StateIo,
        )),
        case(
            "uninstall bootout",
            cut,
            Uninstall,
            "launchd.bootout",
            Reason::UnitFailed,
        ),
        case(
            "uninstall save",
            cut,
            Uninstall,
            "state.save",
            Reason::StateIo,
        ),
        rebooted(case(
            "restore loaded",
            &[],
            Restore,
            "launchd.loaded",
            Reason::UnitFailed,
        )),
        rebooted(case(
            "restore bootstrap",
            &[],
            Restore,
            "launchd.bootstrap",
            Reason::UnitFailed,
        )),
    ]
}

/// Asserts that a failing port call is reported with the reason of its port (launchd as
/// `unit_failed`, the record as `state_io`, the configuration as `host_failed`), by every step
/// that makes the call, and that the lock is released. Catches a step that swallows a port error
/// or blames the wrong party.
#[test]
fn a_failing_port_call_is_reported_with_its_reason() {
    for case in cases() {
        let fake = if case.first {
            Fake::first_install()
        } else {
            Fake::new()
        };
        fake.advance(case.setup);
        if case.reboot {
            fake.reboot();
        }
        fake.fail_nth(case.prefix, 1);
        let done = fake.host_run(case.act);
        assert_failed(&done, case.reason);
        assert!(
            fake.log().iter().any(|c| c.starts_with(case.prefix)),
            "{}",
            case.name
        );
        assert_eq!(fake.live_locks(), 0, "{}", case.name);
    }
}

/// Asserts that a `cutover` whose write of `swapped` fails is finished by the next run, which
/// finds the job serving and records the swap without a restart. Catches a cutover that restarts
/// a job it has already restarted.
#[test]
fn a_cutover_whose_swap_could_not_be_recorded_is_finished_without_a_restart() {
    let fake = Fake::new();
    fake.advance(&[Prepare]);
    fake.fail_nth("state.save", 1);
    assert_failed(&fake.host_run(Cutover), Reason::StateIo);
    fake.heal();
    let before = fake.launchd_calls().len();
    assert_ok(&fake.host_run(Cutover));
    assert_eq!(
        fake.launchd_calls()[before..],
        [format!("launchd.loaded {LABEL}")]
    );
    assert_eq!(fake.snapshot().job, path(NEW));
}
