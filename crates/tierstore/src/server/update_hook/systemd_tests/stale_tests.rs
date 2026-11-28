//! A record of another update, in flight and not committed, whose updater is gone: the host lost
//! its network or its power during an update, hostkeeper gave that update up and moved `current`
//! back to the release it started from, and the rollback of that update never ran. The next
//! order's `prepare` (past that update's cutover) and `rollback` roll it back with its own target
//! and releases, then do their own step; a committed record is finished, never rolled back; and
//! an order that races an update still in its cutover (`current` names that update's staged
//! release) is refused as before.

use crate::server::update_hook::{Instance, Marker, Reason, Record, Step, UnitState};

use super::fake::{
    NEW, NEWER, OLD, SystemdFake, assert_deferred, assert_failed, assert_ok, detail, params,
    version_of,
};

use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// The host after the update to `NEW` swapped and its updater went away: `tierstore@a` serves
/// `OLD` again (the boot step, or a rollback that never recorded its end, started it with the
/// binary `current` names), `tierstore@b` is inactive, and the record still has that update in
/// flight, past its cutover.
fn left_by_a_gone_updater() -> (SystemdFake, Record) {
    let fake = SystemdFake::installed();
    fake.drive(&params(NEW, Some(OLD)), &[Prepare, Cutover]);
    fake.set_unit(Instance::B, UnitState::Inactive, None, false);
    fake.set_unit(Instance::A, UnitState::Active, Some(OLD), true);
    fake.set_installed(Some(OLD));
    let stale = fake.record();
    assert_eq!(
        (
            stale.updating,
            stale.has(Marker::Swapped),
            stale.committed()
        ),
        (true, true, false)
    );
    (fake, stale)
}

/// The note that leads the detail of a step that rolled the update to `NEW` back.
fn rolled_back_note() -> String {
    format!(
        "{}: rolled back with its own target and releases (rolled back: tierstore@a serves {})",
        whose(),
        version_of(OLD)
    )
}

/// The words that say the record is of the update to `NEW`, and the step of the one to `NEWER`.
fn whose() -> String {
    format!(
        "the record is of the update to {NEW} {}, not {NEWER} {}",
        version_of(NEW),
        version_of(NEWER)
    )
}

/// The refusal of params of `staged` that name `previous` while the update to `NEW`, which started
/// from `OLD`, is on record.
fn kept(staged: &str, previous: &str) -> String {
    format!(
        "the params name {staged} {}, and the update on record is Some({NEW:?}) Some({:?}): the \
         update to {} returns the host to {OLD}, and the params name {previous} as the release \
         the host runs: it is rolled back only by params that name the release it started from",
        version_of(staged),
        version_of(NEW),
        version_of(NEW),
    )
}

/// Asserts that hostkeeper's `rollback` of the next order (to `NEWER`, from `OLD`, the release
/// `current` names), which it runs after that order's `prepare` failed, rolls back the update to
/// `NEW` that is on record with that update's own target and releases: the marker of `b` goes,
/// `a` serves on, the record is at rest, and the detail says whose update it rolled back.
/// Catches the `bad_params` of the record of another update, after which hostkeeper halts the
/// service and the host never takes another version by itself.
#[test]
fn the_rollback_of_the_next_order_rolls_back_a_stale_update_with_its_own_target() {
    let (fake, _) = left_by_a_gone_updater();
    let next = params(NEWER, Some(OLD));
    let before = fake.log().len();
    let done = fake.run(Rollback, &next);
    assert_ok(&done);
    assert_eq!(
        detail(&done),
        format!(
            "{}; nothing to roll back: no update is in flight",
            rolled_back_note()
        )
    );
    assert!(done.report.reversible);
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.markers(), [Instance::A]);
    assert_eq!(
        fake.binary_acts()[..],
        [
            format!("host.install_binary {NEW}"),
            "units.start b".to_owned(),
            format!("host.install_binary {OLD}"),
        ]
    );
    // Nothing of the staged release of the update on record is read.
    assert!(!fake.log()[before..].iter().any(|line| line.contains(NEW)));
    // A run again finds nothing in flight.
    let again = fake.run(Rollback, &next);
    assert_ok(&again);
    assert_eq!(
        detail(&again),
        "nothing to roll back: no update is in flight"
    );
}

/// Asserts that the next order's `prepare`, which finds the update to `NEW` past its cutover on
/// record, rolls it back first and then prepares its own update from the record at rest, which
/// then goes through to its commit. Catches the `out_of_order` refusal that made hostkeeper run
/// the `rollback` that refused with `bad_params`.
#[test]
fn prepare_of_the_next_order_rolls_back_a_stale_update_and_prepares_its_own() {
    let (fake, _) = left_by_a_gone_updater();
    let next = params(NEWER, Some(OLD));
    let done = fake.run(Prepare, &next);
    assert_ok(&done);
    assert_eq!(
        detail(&done),
        format!(
            "{}; prepared {}: handover, serving tierstore@a, next tierstore@b",
            rolled_back_note(),
            version_of(NEWER)
        )
    );
    let record = fake.record();
    assert_eq!((record.updating, record.markers.len()), (true, 0));
    assert_eq!(record.to.as_deref(), Some(std::path::Path::new(NEWER)));
    assert_eq!(record.from.as_deref(), Some(std::path::Path::new(OLD)));
    assert_eq!(
        (record.live, record.next),
        (Some(Instance::A), Some(Instance::B))
    );
    assert_eq!(fake.markers(), [Instance::A]);
    fake.drive(&next, &[Cutover, Verify, Cleanup]);
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.record().live, Some(Instance::B));
}

/// Asserts that when the instance the update on record started still serves its release (`b`
/// runs `NEW`, `a` drained away), the next order's `prepare` brings `a` back on `OLD`, which
/// takes the sockets from `b`, and defers while `b` drains, with the record kept as it was; the
/// run after `b` has ended rolls the update back and prepares the next one. Catches a step that
/// records over an update whose instance still serves, or that stops it under its connections.
#[test]
fn a_stale_update_whose_instance_still_serves_is_handed_back_before_the_next_prepare() {
    let fake = SystemdFake::installed();
    fake.drive(&params(NEW, Some(OLD)), &[Prepare, Cutover]);
    // `a` handed its sockets over and ended.
    fake.set_unit(Instance::A, UnitState::Inactive, None, false);
    assert_eq!(fake.running(), [Instance::B]);
    let stale = fake.record();
    fake.set_drain_looks(100);
    let next = params(NEWER, Some(OLD));
    let waits = fake.run(Prepare, &next);
    assert_deferred(&waits, Reason::Draining, 10);
    assert_eq!(
        detail(&waits),
        format!(
            "{}, and its rollback with its own target and releases is not done; tierstore@a and \
             tierstore@b both run, and one of them drains: the rollback waits",
            whose()
        )
    );
    assert_eq!(fake.record(), stale);
    assert_eq!(fake.running(), [Instance::A, Instance::B]);
    // `b` never was stopped: it drains on its own.
    assert!(!fake.actions().iter().any(|line| line == "units.stop b"));

    fake.set_unit(Instance::B, UnitState::Inactive, None, false);
    let done = fake.run(Prepare, &next);
    assert_ok(&done);
    assert!(detail(&done).starts_with(&rolled_back_note()), "{done:?}");
    assert_eq!(
        fake.record().to.as_deref(),
        Some(std::path::Path::new(NEWER))
    );
    assert_eq!(fake.running(), [Instance::A]);
}

/// Asserts that the staged release of the update on record is not needed to roll it back: the
/// steps of the next order read nothing of `NEW` (its directory may be gone from the host), so
/// the next update is prepared all the same. Catches a rollback that asks the staged release, or
/// puts its binary in place, and fails on a host that no longer has it.
#[test]
fn a_stale_update_whose_staged_release_is_gone_is_rolled_back_without_it() {
    let (fake, _) = left_by_a_gone_updater();
    let before = fake.log().len();
    let done = fake.run(Prepare, &params(NEWER, Some(OLD)));
    assert_ok(&done);
    assert!(!fake.log()[before..].iter().any(|line| line.contains(NEW)));
    assert_eq!(
        fake.record().to.as_deref(),
        Some(std::path::Path::new(NEWER))
    );
}

/// Asserts that a committed record of another update still in flight is finished by the next
/// `prepare`, as before, and never rolled back: the new instance stays live and the old one's
/// marker goes; and that a `rollback` whose params name the release that update started from
/// finds nothing to roll back and touches nothing. Catches a rule that rolls back what `cleanup`
/// has committed.
#[test]
fn a_committed_stale_update_is_finished_and_never_rolled_back() {
    let committed = || {
        let fake = SystemdFake::installed();
        fake.drive(&params(NEW, Some(OLD)), &[Prepare, Cutover, Verify]);
        fake.set_record(&fake.record().with(Marker::Stopping));
        fake
    };
    let fake = committed();
    let before = fake.actions().len();
    let done = fake.run(Rollback, &params(NEWER, Some(OLD)));
    assert_ok(&done);
    assert!(
        detail(&done).starts_with("nothing to roll back"),
        "{done:?}"
    );
    assert_eq!(fake.actions().len(), before);
    assert_eq!(
        (fake.record().committed(), fake.record().updating),
        (true, true)
    );

    let fake = committed();
    let done = fake.run(Prepare, &params(NEWER, Some(NEW)));
    assert_ok(&done);
    assert!(
        detail(&done).starts_with(&format!(
            "finished the committed update to {}",
            version_of(NEW)
        )),
        "{done:?}"
    );
    assert!(
        !fake
            .binary_acts()
            .contains(&format!("host.install_binary {OLD}"))
    );
    assert_eq!(fake.record().live, Some(Instance::B));
    assert_eq!(fake.markers(), [Instance::B]);
}

/// Asserts that an order that races an update still in its cutover is refused as before:
/// hostkeeper moved `current` to `NEW` before that cutover, so the second order names `NEW` as
/// the release the host runs, and its `prepare` is `out_of_order` and its `rollback` `bad_params`
/// with why the record is kept; and that `cutover`, `verify` and `cleanup` of another update never
/// roll back, even with params whose previous release is the one the update started from (no
/// `prepare` of theirs succeeded, so they are a second order). Each leaves the host and the
/// record as they were. Catches a second order that rolls back an update whose updater is alive.
#[test]
fn an_order_that_races_an_update_in_its_cutover_is_refused() {
    let fake = SystemdFake::installed();
    fake.drive(&params(NEW, Some(OLD)), &[Prepare, Cutover]);
    let before = fake.snapshot();
    let actions = fake.actions().len();
    let racing = params(NEWER, Some(NEW));
    assert_failed(&fake.run(Prepare, &racing), Reason::OutOfOrder);
    let refused = fake.run(Rollback, &racing);
    assert_failed(&refused, Reason::BadParams);
    assert_eq!(detail(&refused), kept(NEWER, NEW));
    for step in [Cutover, Verify, Cleanup] {
        assert_failed(
            &fake.run(step, &params(NEWER, Some(OLD))),
            Reason::BadParams,
        );
    }
    assert_eq!(fake.snapshot(), before);
    assert_eq!(fake.actions().len(), actions);
}

/// Asserts that a first install of another update on record is kept: its rollback stops the
/// instance it started, which only the params it was written under may do, so the next order's
/// `rollback` refuses it with why, and its `prepare` is `out_of_order`, both touching nothing.
/// Catches a second order that stops the only instance of a first install.
#[test]
fn a_stale_first_install_is_kept() {
    let fake = SystemdFake::empty();
    fake.drive(&params(NEW, None), &[Prepare, Cutover]);
    let before = fake.snapshot();
    let next = params(NEWER, None);
    assert_failed(&fake.run(Prepare, &next), Reason::OutOfOrder);
    let refused = fake.run(Rollback, &next);
    assert_failed(&refused, Reason::BadParams);
    assert_eq!(
        detail(&refused),
        format!(
            "the params name {NEWER} {}, and the update on record is Some({NEW:?}) Some({:?}): \
             the first install of {} is rolled back only with the params it was written under: \
             it stops the instance it started, which no other order may do",
            version_of(NEWER),
            version_of(NEW),
            version_of(NEW)
        )
    );
    assert_eq!(fake.snapshot(), before);
}

/// Asserts that the next order whose params name no previous release (a first install) keeps the
/// update to `NEW` on record, which started from `OLD`, and says so. Catches a rule that takes
/// any release for the one the update started from.
#[test]
fn a_stale_update_is_kept_by_params_with_no_previous_release() {
    let (fake, stale) = left_by_a_gone_updater();
    let refused = fake.run(Rollback, &params(NEWER, None));
    assert_failed(&refused, Reason::BadParams);
    assert!(
        detail(&refused).ends_with(
            "and the params name no release as the release the host runs: it is rolled back \
             only by params that name the release it started from"
        ),
        "{refused:?}"
    );
    assert_eq!(fake.record(), stale);
}

/// Asserts that a `prepare` of the next order that finds the update on record before its cutover
/// (`prepare` ran, `cutover` never did) prepares over it as before, with no rollback of it.
/// Catches a rule that rolls back what never started.
#[test]
fn a_stale_update_before_its_cutover_is_prepared_over() {
    let fake = SystemdFake::installed();
    fake.drive(&params(NEW, Some(OLD)), &[Prepare]);
    let actions = fake.actions().len();
    let done = fake.run(Prepare, &params(NEWER, Some(OLD)));
    assert_ok(&done);
    assert!(detail(&done).starts_with("prepared"), "{done:?}");
    assert_eq!(fake.actions().len(), actions);
    assert_eq!(
        fake.record().to.as_deref(),
        Some(std::path::Path::new(NEWER))
    );
}

/// Asserts that a stale update whose rollback fails is reported with what was asked and why, and
/// leaves the record as it was, so that a run again goes on from there.
#[test]
fn a_stale_rollback_that_fails_keeps_the_record() {
    let (fake, stale) = left_by_a_gone_updater();
    fake.fail_on(Some("host.install_binary"));
    let done = fake.run(Rollback, &params(NEWER, Some(OLD)));
    assert_failed(&done, Reason::HostFailed);
    assert!(
        detail(&done).starts_with(&format!(
            "{}, and its rollback with its own target and releases is not done; ",
            whose()
        )),
        "{done:?}"
    );
    assert_eq!(fake.record(), stale);
    fake.fail_on(None);
    assert_ok(&fake.run(Rollback, &params(NEWER, Some(OLD))));
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that when the next order's own `prepare` fails after the update on record was rolled
/// back (its staged release refuses the node's file), the detail says both, the record stays at
/// rest, and hostkeeper's `rollback` that follows has nothing to roll back. Catches a failure that
/// hides the rollback it follows, or one that leaves the record of either update in flight.
#[test]
fn a_prepare_that_fails_after_the_stale_rollback_leaves_the_record_at_rest() {
    let (fake, _) = left_by_a_gone_updater();
    fake.set_refusal(NEWER, "unknown key");
    let next = params(NEWER, Some(OLD));
    let done = fake.run(Prepare, &next);
    assert_failed(&done, Reason::ConfigRefused);
    assert!(
        detail(&done).starts_with(&format!("{}; the staged release ", rolled_back_note())),
        "{done:?}"
    );
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
    let back = fake.run(Rollback, &next);
    assert_ok(&back);
    assert_eq!(
        detail(&back),
        "nothing to roll back: no update is in flight"
    );
}
