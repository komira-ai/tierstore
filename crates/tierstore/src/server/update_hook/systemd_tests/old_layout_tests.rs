//! A node that still runs the two-process layout (a `tierstore-fs@` unit running, a `run-fs-`
//! marker, or a record that names `fs_live`) is refused by `prepare` before anything is saved or
//! asked of a release, and a record of an update in flight that names `fs_next` or says
//! `fs_switch` is refused, not rolled back: this release has no step for the second unit.

use crate::server::update_hook::{Instance, Marker, Mode, Reason, Record, Step, UnitState};

use super::fake::{NEW, NEWER, OLD, SystemdFake, assert_failed, assert_ok, detail, params};

use Step::{Prepare, Rollback};

/// The closing words of every refusal.
const SWITCH_FIRST: &str = "switch this node to the fs interface with release M first";

/// The detail of the refusal of a node for which `found` was seen.
fn refusal(found: &str) -> String {
    format!("this node still runs the two-process layout ({found}): {SWITCH_FIRST}")
}

/// Asserts that nothing was done: no store or fs call that changes a unit or a marker, and the
/// record is `before`.
fn nothing_done(fake: &SystemdFake, before: &Record) {
    assert_eq!(&fake.record(), before);
    assert!(fake.actions().is_empty(), "{:?}", fake.actions());
    assert!(fake.fs_actions().is_empty(), "{:?}", fake.fs_actions());
}

/// Asserts that `prepare` of a node with `tierstore-fs@a` active, its marker `run-fs-a` and a
/// record naming `fs_live` a is refused with `mode_unsupported` and a detail that names all three,
/// by exact equality, and that nothing was recorded, started or stopped. Catches an update of a
/// node whose fs process this release cannot hand over, which would leave it on its old release
/// with the new store beside it.
#[test]
fn prepare_refuses_a_node_that_still_runs_tierstore_fs() {
    let fake = SystemdFake::fs_installed();
    let before = fake.record();
    let done = fake.run(Prepare, &params(NEW, Some(OLD)));
    assert_failed(&done, Reason::ModeUnsupported);
    assert_eq!(
        detail(&done),
        refusal(
            "tierstore-fs@a.service is active; the marker run-fs-a is present; the record names \
             fs_live a"
        )
    );
    assert!(done.report.reversible);
    nothing_done(&fake, &before);
}

/// Asserts that a unit of the second template that is activating or deactivating counts as
/// running, and is named with its state: the process may still hold the mount. Catches a look
/// that takes only `active` for running.
#[test]
fn prepare_refuses_an_fs_unit_in_any_state_that_may_run_a_process() {
    for (state, word) in [
        (UnitState::Activating, "activating"),
        (UnitState::Deactivating, "deactivating"),
    ] {
        let fake = SystemdFake::installed();
        fake.set_fs_unit(Instance::B, state);
        let before = fake.record();
        let done = fake.run(Prepare, &params(NEW, Some(OLD)));
        assert_failed(&done, Reason::ModeUnsupported);
        assert_eq!(
            detail(&done),
            refusal(&format!("tierstore-fs@b.service is {word}"))
        );
        nothing_done(&fake, &before);
    }
}

/// Asserts that a record at rest that names `fs_live`, with no fs unit running and no marker, is
/// refused on its own, naming the key, and nothing is recorded. Catches a node that an earlier
/// release left with an fs instance recorded being updated as if it had none.
#[test]
fn prepare_refuses_a_record_that_names_fs_live() {
    let fake = SystemdFake::installed();
    fake.set_fs_live(Some(Instance::B));
    let before = fake.record();
    let done = fake.run(Prepare, &params(NEW, Some(OLD)));
    assert_failed(&done, Reason::ModeUnsupported);
    assert_eq!(detail(&done), refusal("the record names fs_live b"));
    nothing_done(&fake, &before);
}

/// Asserts that a lone `run-fs-b` marker, with no unit running and no record key, is refused on
/// its own, naming the marker. Catches an update that starts the store while a marker would start
/// the second unit at the next boot.
#[test]
fn prepare_refuses_a_lone_run_fs_marker() {
    let fake = SystemdFake::installed();
    fake.set_fs_marker(Instance::B);
    let before = fake.record();
    let done = fake.run(Prepare, &params(NEW, Some(OLD)));
    assert_failed(&done, Reason::ModeUnsupported);
    assert_eq!(detail(&done), refusal("the marker run-fs-b is present"));
    nothing_done(&fake, &before);
}

/// Asserts that the refusal comes before the asks of the releases and applies to a first install
/// too: no capability of any release is read, and a first install on a host with a leftover
/// marker is refused. Catches a refusal that runs after the previous release was run.
#[test]
fn the_refusal_comes_before_any_release_is_asked() {
    let fake = SystemdFake::fs_installed();
    let done = fake.run(Prepare, &params(NEW, Some(OLD)));
    assert_failed(&done, Reason::ModeUnsupported);
    assert!(
        !fake
            .log()
            .iter()
            .any(|line| line.starts_with("host.capabilities")
                || line.starts_with("host.check_config")),
        "{:?}",
        fake.log()
    );

    let fake = SystemdFake::empty();
    fake.set_fs_marker(Instance::A);
    assert_failed(
        &fake.run(Prepare, &params(NEW, None)),
        Reason::ModeUnsupported,
    );
}

/// Asserts that fs units that are inactive or failed, with no marker, are no refusal, and that a
/// committed record that says `fs_switch` with no `fs_live` is none either: `prepare` records the
/// update and the update runs to its end, the hook never touching an fs unit. Catches a refusal of
/// a node that has left the two-process layout.
#[test]
fn prepare_takes_a_node_whose_fs_units_are_inactive_and_markers_gone() {
    let fake = SystemdFake::installed();
    fake.set_fs_unit(Instance::A, UnitState::Inactive);
    fake.set_fs_unit(Instance::B, UnitState::Failed);
    let mut at_rest = Record::idle(Some(Instance::A));
    at_rest.fs_switch = true;
    at_rest.markers = [Marker::Swapped, Marker::Verified, Marker::Stopping]
        .into_iter()
        .collect();
    at_rest.to = Some(OLD.into());
    at_rest.version = Some("0.4.0".to_owned());
    at_rest.check().unwrap();
    fake.set_record(&at_rest);
    let p = params(NEW, Some(OLD));
    assert_ok(&fake.run(Prepare, &p));
    let prepared = fake.record();
    assert!(prepared.updating);
    assert_eq!(prepared.mode, Some(Mode::Handover));
    assert_eq!((prepared.fs_live, prepared.fs_next), (None, None));
    fake.drive(&p, &[Step::Cutover, Step::Verify, Step::Cleanup]);
    assert_eq!(fake.record().live, Some(Instance::B));
    assert!(fake.fs_actions().is_empty(), "{:?}", fake.fs_actions());
}

/// The host after the update to `NEW` swapped and its updater went away, as in `stale_tests`, with
/// the record the earlier release wrote: `fs_live` a and `fs_next` b.
fn left_with_fs_next() -> SystemdFake {
    let fake = SystemdFake::installed();
    fake.drive(&params(NEW, Some(OLD)), &[Prepare, Step::Cutover]);
    fake.set_unit(Instance::B, UnitState::Inactive, None, false);
    fake.set_unit(Instance::A, UnitState::Active, Some(OLD), true);
    fake.set_installed(Some(OLD));
    let mut record = fake.record();
    record.fs_live = Some(Instance::A);
    record.fs_next = Some(Instance::B);
    fake.set_record(&record);
    fake
}

/// Asserts that the `prepare` and the `rollback` of the next order, over a stale record of an
/// update that names `fs_next`, are refused with `mode_unsupported` and the closing words, naming
/// the key, and the update is not rolled back: no unit is started or stopped, no binary put back,
/// and the record is as it was. Catches a rollback that puts the store back and leaves the fs
/// instance on the release the failed update started.
#[test]
fn a_stale_record_with_fs_next_is_refused_not_rolled_back() {
    for step in [Prepare, Rollback] {
        let fake = left_with_fs_next();
        let before = fake.record();
        let acts = fake.log().len();
        let done = fake.run(step, &params(NEWER, Some(OLD)));
        assert_failed(&done, Reason::ModeUnsupported);
        assert_eq!(
            detail(&done),
            refusal("the update in flight names fs_next b")
        );
        assert_eq!(fake.record(), before);
        assert!(
            !fake.log()[acts..]
                .iter()
                .any(|line| line.starts_with("units.") || line.starts_with("host.install")),
            "{:?}",
            &fake.log()[acts..]
        );
        assert!(fake.fs_actions().is_empty());
    }
}

/// Asserts that the `rollback` of an update in flight, with the params of that update, that says
/// `fs_switch` (an earlier release gave the fs unit up for the one process) is refused the same
/// way, naming the key, and nothing is done. Catches a rollback that cannot put the fs instance
/// back and goes on.
#[test]
fn a_rollback_of_an_update_that_gave_the_fs_unit_up_is_refused() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Step::Cutover]);
    let mut record = fake.record();
    record.fs_live = Some(Instance::A);
    record.fs_switch = true;
    fake.set_record(&record);
    let acts = fake.log().len();
    let done = fake.run(Rollback, &p);
    assert_failed(&done, Reason::ModeUnsupported);
    assert_eq!(
        detail(&done),
        refusal("the update in flight says fs_switch")
    );
    assert_eq!(fake.record(), record);
    assert!(
        !fake.log()[acts..]
            .iter()
            .any(|line| line.starts_with("units.") || line.starts_with("host.install")),
        "{:?}",
        &fake.log()[acts..]
    );
}
