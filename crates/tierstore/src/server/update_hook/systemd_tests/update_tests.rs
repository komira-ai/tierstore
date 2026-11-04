//! The update from one instance to the other as hostkeeper runs it, and a first install.

use std::path::PathBuf;

use crate::server::update_hook::{Instance, Marker, Mode, Reason, Record, Step, UnitState};

use super::fake::{
    NEW, NEWER, OLD, SystemdFake, assert_failed, assert_ok, detail, params, version_of,
};

use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// Asserts that an update with a hand-over runs in this order: `prepare` records the update and
/// starts nothing; `cutover` lays down the marker of the idle instance b and starts it, which
/// takes the sockets of a, and a drains without being stopped; `verify` and `cleanup` commit,
/// remove the marker of a once it has exited, and flip `live` to b. Catches a `live` that moves
/// before the commit, a stop of the old instance in a hand-over, and a marker of a left behind.
#[test]
fn a_hand_over_update_moves_the_service_from_a_to_b_and_the_old_instance_exits() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    let prepared = fake.record();
    assert!(prepared.updating);
    assert_eq!(prepared.live, Some(Instance::A));
    assert_eq!(prepared.next, Some(Instance::B));
    assert_eq!(prepared.mode, Some(Mode::Handover));
    assert_eq!(prepared.from, Some(PathBuf::from(OLD)));
    assert!(prepared.markers.is_empty());
    assert!(fake.actions().is_empty(), "prepare starts nothing");

    fake.drive(&p, &[Cutover]);
    assert_eq!(fake.actions(), ["markers.set b", "units.start b"]);
    assert_eq!(fake.peek(Instance::A), UnitState::Deactivating);
    assert_eq!(fake.peek(Instance::B), UnitState::Active);
    assert_eq!(fake.record().live, Some(Instance::A));
    assert!(fake.record().has(Marker::Swapped));

    fake.drive(&p, &[Verify, Cleanup]);
    let finished = fake.record();
    assert!(!finished.updating);
    assert_eq!(finished.live, Some(Instance::B));
    assert_eq!(finished.next, None);
    assert_eq!(finished.mode, None);
    assert_eq!(finished.from, None);
    assert!(finished.committed());
    assert_eq!(
        fake.actions(),
        ["markers.set b", "units.start b", "markers.clear a"]
    );
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.markers(), [Instance::B]);
}

/// Asserts that a second update runs the other way round: a first update moves the service a to
/// b, a second one b to a, and the instance that served last is the one that has no marker in
/// between. Catches a hook that always starts the same letter.
#[test]
fn two_consecutive_updates_alternate_a_b_a() {
    let fake = SystemdFake::installed();
    fake.complete(&params(NEW, Some(OLD)));
    assert_eq!(fake.record().live, Some(Instance::B));
    assert_eq!(fake.markers(), [Instance::B]);

    let second = params(NEWER, Some(NEW));
    fake.drive(&second, &[Prepare]);
    let prepared = fake.record();
    assert_eq!(prepared.live, Some(Instance::B));
    assert_eq!(prepared.next, Some(Instance::A));
    fake.drive(&second, &[Cutover, Verify, Cleanup]);

    assert_eq!(fake.record().live, Some(Instance::A));
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.markers(), [Instance::A]);
    assert_eq!(
        fake.snapshot().units[0],
        (Instance::A, UnitState::Active, Some(PathBuf::from(NEWER)))
    );
}

/// Asserts that a `verify` that reads another build than the target fails with `wrong_build`
/// (exit 1, still reversible), and that the `rollback` that follows, once hostkeeper has moved
/// `current` back, starts the previous release on the old letter, which takes the sockets back,
/// and leaves nothing in flight. Catches a rollback that starts the new release again.
#[test]
fn a_wrong_build_fails_verify_and_rollback_restores_the_previous_release_on_the_old_letter() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.lie(NEW, "9.9.9+0123456789ab");

    let verify = fake.run(Verify, &p);
    assert_failed(&verify, Reason::WrongBuild);
    assert_eq!(verify.outcome.code(), 1);
    assert!(verify.report.reversible);

    let back = fake.run_as_hostkeeper(Rollback, &p);
    assert_ok(&back);
    assert_eq!(back.outcome.code(), 0);
    assert_eq!(
        detail(&back),
        format!("rolled back: tierstore@a serves {}", version_of(OLD))
    );
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.markers(), [Instance::A]);
    assert_eq!(
        fake.snapshot().units[0],
        (Instance::A, UnitState::Active, Some(PathBuf::from(OLD)))
    );
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that a first install, with no previous keys and nothing on the host, records no mode
/// and no live instance, reads no configuration, starts tierstore@a with a normal bind (its
/// marker, then its unit, no hand-over and no stop) and ends with a serving as the live
/// instance. Catches a first install that insists on a hand-over socket or on a previous
/// release.
#[test]
fn a_first_install_starts_instance_a_with_a_normal_bind() {
    let fake = SystemdFake::empty();
    let p = params(NEW, None);
    fake.drive(&p, &[Prepare]);
    let prepared = fake.record();
    assert_eq!(prepared.live, None);
    assert_eq!(prepared.next, Some(Instance::A));
    assert_eq!(prepared.mode, None);
    assert_eq!(prepared.from, None);
    assert!(
        fake.config_paths().is_empty(),
        "a first install reads no configuration"
    );

    fake.drive(&p, &[Cutover]);
    assert_eq!(fake.actions(), ["markers.set a", "units.start a"]);
    assert!(!fake.log().iter().any(|line| line.starts_with("skipped")));
    fake.drive(&p, &[Verify, Cleanup]);
    assert_eq!(fake.record().live, Some(Instance::A));
    assert!(!fake.record().updating);
    assert_eq!(fake.running(), [Instance::A]);
}

/// Asserts that the rollback of a first install whose `verify` failed stops the instance
/// `cutover` started, removes its marker, keeps the update on record with no markers and fails
/// with `no_previous_release` (exit 1) naming what it stopped; that no instance is left; that
/// the same step run again says the same and writes nothing; and that a later `prepare` may
/// start afresh. Catches a rollback that reports success with nothing to go back to.
#[test]
fn the_rollback_of_a_failed_first_install_stops_the_instance_and_says_so_twice() {
    let fake = SystemdFake::empty();
    let p = params(NEW, None);
    fake.drive(&p, &[Prepare, Cutover]);
    fake.lie(NEW, "9.9.9+0123456789ab");
    assert_failed(&fake.run(Verify, &p), Reason::WrongBuild);

    let first = fake.run_as_hostkeeper(Rollback, &p);
    assert_failed(&first, Reason::NoPreviousRelease);
    assert_eq!(first.outcome.code(), 1);
    assert_eq!(detail(&first), "nothing to go back to: stopped tierstore@a");
    assert!(fake.running().is_empty());
    assert!(fake.markers().is_empty());
    let record = fake.record();
    assert!(record.updating);
    assert!(record.markers.is_empty());

    let before = fake.log().len();
    let again = fake.run_as_hostkeeper(Rollback, &p);
    assert_failed(&again, Reason::NoPreviousRelease);
    assert_eq!(detail(&again), detail(&first));
    assert!(!fake.log()[before..].iter().any(|line| line == "state.save"));

    fake.drive(&p, &[Prepare]);
    assert_eq!(fake.record().next, Some(Instance::A));
}

/// Asserts that no step asks the host for anything but the configuration file the params name,
/// and only in `prepare`: there is no declaration file in the world and none is read.
#[test]
fn only_the_configuration_file_of_the_params_is_ever_read() {
    let fake = SystemdFake::installed();
    fake.complete(&params(NEW, Some(OLD)));
    assert_eq!(fake.config_paths(), ["/etc/tierstore/config.toml"]);
    assert!(!fake.log().iter().any(|line| line.contains("updater.d")));
}
