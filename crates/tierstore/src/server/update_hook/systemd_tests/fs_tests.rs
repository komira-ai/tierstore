//! The update of the second unit, `tierstore-fs`: handed over with the store, left alone, rolled
//! back, retired, restored at boot and uninstalled.

use std::path::PathBuf;

use crate::server::update_hook::{
    HANDOFF_FS_CAPABILITY, Instance, Marker, Markers, Mode, Outcome, Params, Reason, Record,
    SERVICE_BINARY, Step, UnitState,
};

use super::fake::{
    CONFIG, NEW, NEWER, OLD, SystemdFake, assert_deferred, assert_failed, assert_ok, detail,
    fs_marker_path, fs_unit_condition, fs_unit_program, params, params_with,
};

use Step::{Cleanup, Cutover, Prepare, Restore, Rollback, Uninstall, Verify};

/// The position of the first call of the log that starts with `prefix`.
fn at(fake: &SystemdFake, prefix: &str) -> usize {
    fake.log()
        .iter()
        .position(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no call {prefix:?} in {:?}", fake.log()))
}

/// Asserts that the marker the hook writes for the fs unit is the path `tierstore-fs@.service`'s
/// `ConditionPathExists=` names, for both instances, and that the unit runs the binary the hook
/// installs. Catches a marker name that differs from the unit file's condition: the unit would
/// never start.
#[test]
fn the_fs_marker_is_the_one_the_fs_unit_file_names() {
    for instance in Instance::ALL {
        assert_eq!(
            fs_marker_path(instance),
            PathBuf::from(fs_unit_condition().replace("%i", instance.letter()))
        );
    }
    assert_eq!(fs_unit_program(), SERVICE_BINARY);
}

/// Asserts that `prepare` on a node with an fs process records `fs_live` a and `fs_next` b, says
/// so, and starts nothing; that `cutover` starts the store first and the fs instance after it
/// (marker, then start), which takes the mount while the old one deactivates without a stop; and
/// that `verify` and `cleanup` retire the old instance, remove its marker and record b as
/// `fs_live`, with the store's instance b running the staged release beside it. Catches an fs
/// process that stays on a, a stop of the instance that holds the mount, and a marker of a left
/// behind.
#[test]
fn a_hand_over_update_moves_the_store_and_the_fs_process_to_b() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    let prepared = fake.record();
    assert_eq!(prepared.fs_live, Some(Instance::A));
    assert_eq!(prepared.fs_next, Some(Instance::B));
    assert!(
        detail(&fake.run(Prepare, &p))
            .ends_with("; fs serving tierstore-fs@a, next tierstore-fs@b")
    );
    assert!(fake.fs_actions().is_empty(), "prepare starts nothing");

    let done = fake.run(Cutover, &p);
    assert_ok(&done);
    assert!(detail(&done).ends_with("; tierstore-fs@b holds the mount"));
    assert_eq!(
        fake.fs_actions(),
        ["fs_markers.set b", "fs_units.start b", "fs_markers.clear a"]
    );
    assert!(at(&fake, "units.start b") < at(&fake, "fs_markers.set b"));
    assert_eq!(fake.peek_fs(Instance::B), UnitState::Active);
    assert_eq!(fake.fs_release(Instance::B), Some(PathBuf::from(NEW)));
    assert_eq!(fake.peek_fs(Instance::A), UnitState::Deactivating);
    assert!(fake.record().has(Marker::Swapped));
    assert_eq!(fake.record().fs_live, Some(Instance::A));

    fake.drive(&p, &[Verify, Cleanup]);
    let finished = fake.record();
    assert_eq!(finished.fs_live, Some(Instance::B));
    assert_eq!(finished.fs_next, None);
    assert_eq!(
        fake.fs_actions(),
        [
            "fs_markers.set b",
            "fs_units.start b",
            "fs_markers.clear a",
            "fs_markers.clear a"
        ]
    );
    assert_eq!(fake.fs_running(), [Instance::B]);
    assert_eq!(fake.fs_markers(), [Instance::B]);
    assert_eq!(fake.running(), [Instance::B]);
}

/// Asserts that three updates in a row alternate the fs process a, b, a, as the store, with the
/// record's `fs_live` following. Catches an fs process that always starts the same letter.
#[test]
fn two_updates_alternate_the_fs_process_a_b_a() {
    let fake = SystemdFake::fs_installed();
    fake.complete(&params(NEW, Some(OLD)));
    assert_eq!(fake.fs_running(), [Instance::B]);
    fake.complete(&params(NEWER, Some(NEW)));
    assert_eq!(fake.fs_running(), [Instance::A]);
    assert_eq!(fake.fs_markers(), [Instance::A]);
    assert_eq!(fake.record().fs_live, Some(Instance::A));
    assert_eq!(fake.fs_release(Instance::A), Some(PathBuf::from(NEWER)));
}

/// Asserts that a declared `mode = "restart"` is the store's alone: the store's old instance is
/// stopped first, and the fs process is still handed over, with no stop of its instance. Catches
/// a restart of the fs process on a node that asked for the store's.
#[test]
fn a_declared_restart_of_the_store_still_hands_the_fs_process_over() {
    let fake = SystemdFake::fs_installed();
    let p = params_with(
        NEW,
        Some(OLD),
        &format!("config_file = \"{CONFIG}\"\nmode = \"restart\"\n"),
    );
    fake.drive(&p, &[Prepare]);
    assert_eq!(fake.record().mode, Some(Mode::Restart));
    assert_eq!(fake.record().fs_next, Some(Instance::B));
    fake.drive(&p, &[Cutover]);
    assert!(fake.actions().contains(&"units.stop a".to_owned()));
    assert_eq!(
        fake.fs_actions(),
        ["fs_markers.set b", "fs_units.start b", "fs_markers.clear a"]
    );
    assert_eq!(fake.peek_fs(Instance::B), UnitState::Active);
}

/// Asserts that on a node with an fs process, the previous release is asked what it can do even
/// when the params declare a restart (the fs process decides on its own), and that on a node
/// with none it is not. Catches an fs decision made on a list that was never read, and a call
/// added to every node that has no fs process.
#[test]
fn the_previous_release_is_asked_for_the_fs_word_only_on_a_node_that_has_the_fs_process() {
    let restart = params_with(
        NEW,
        Some(OLD),
        &format!("config_file = \"{CONFIG}\"\nmode = \"restart\"\n"),
    );
    let asked = format!("host.capabilities {OLD}");
    let with = SystemdFake::fs_installed();
    with.drive(&restart, &[Prepare]);
    assert!(with.log().contains(&asked));
    let without = SystemdFake::installed();
    without.drive(&restart, &[Prepare]);
    assert!(!without.log().contains(&asked));
}

/// Asserts that a staged release that does not list `handoff-fs-v1`, and a previous release that
/// does not, each leave the fs process on its release: `prepare` records `fs_live` and no
/// `fs_next` and says it is left; `cutover` makes no fs call but the look; the store updates; the
/// instance, its marker and its release are what they were after `cleanup`. Catches a restart of
/// the fs process across releases that cannot hand it over, which would leave the jobs' requests
/// unanswered on a kernel without resend.
#[test]
fn a_pair_of_releases_without_the_fs_word_leaves_the_fs_process_alone() {
    for (release, staged) in [(NEW, true), (OLD, false)] {
        let fake = SystemdFake::fs_installed();
        fake.list_fs_hand_over(release, false);
        let p = params(NEW, Some(OLD));
        let done = fake.run(Prepare, &p);
        assert_ok(&done);
        assert!(
            detail(&done).ends_with(&format!(
                "; fs tierstore-fs@a is left on its release: both releases must list {HANDOFF_FS_CAPABILITY}"
            )),
            "{staged}: {}",
            detail(&done)
        );
        assert_eq!(fake.record().fs_live, Some(Instance::A));
        assert_eq!(fake.record().fs_next, None);
        let done = fake.run(Cutover, &p);
        assert_ok(&done);
        assert!(detail(&done).ends_with("; tierstore-fs@a is left on its release"));
        fake.drive(&p, &[Verify, Cleanup]);
        assert!(fake.fs_actions().is_empty());
        assert_eq!(fake.fs_running(), [Instance::A]);
        assert_eq!(fake.fs_markers(), [Instance::A]);
        assert_eq!(fake.fs_release(Instance::A), Some(PathBuf::from(OLD)));
        assert_eq!(fake.record().fs_live, Some(Instance::A));
        assert_eq!(fake.running(), [Instance::B]);
    }
}

/// Asserts that on a node with no fs instance running and none recorded, an update looks at the
/// two fs units (`prepare`, and `cutover` for a record that has no plan) and does nothing else to
/// them, says nothing of them, and records none; and that a first install does not look at them
/// at all. Catches an fs call, or an fs record, on every node that has no fs process.
#[test]
fn a_node_with_no_fs_process_is_not_touched_by_the_fs_steps() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    let done = fake.run(Prepare, &p);
    assert!(!detail(&done).contains("fs"), "{}", detail(&done));
    fake.drive(&p, &[Cutover, Verify, Cleanup]);
    assert!(fake.fs_actions().is_empty());
    assert_eq!(
        fake.fs_looks(),
        4,
        "prepare and cutover look at both, nothing else does"
    );
    let finished = fake.record();
    assert_eq!((finished.fs_live, finished.fs_next), (None, None));

    let first = SystemdFake::empty();
    first.complete(&params(NEW, None));
    assert_eq!(first.fs_looks(), 0);
    assert!(first.fs_actions().is_empty());
}

/// Asserts that with no fs instance running, `prepare` takes the record's `fs_live`, so that the
/// update starts the other letter, which has no peer to ask and makes the mount afresh. Catches a
/// node that forgets its fs process because the unit happens to be stopped.
#[test]
fn with_no_fs_instance_running_the_record_names_the_one_that_served() {
    let fake = SystemdFake::fs_installed();
    fake.set_fs_unit(Instance::A, UnitState::Inactive, None);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    assert_eq!(fake.record().fs_live, Some(Instance::A));
    assert_eq!(fake.record().fs_next, Some(Instance::B));
    fake.drive(&p, &[Cutover]);
    assert_eq!(fake.fs_running(), [Instance::B]);
}

/// Asserts that `prepare` defers (75, `host_not_ready`, 30 s) while both fs instances run, the
/// old one of an earlier update still draining, and records nothing. Catches an update started
/// over a mount two processes hold.
#[test]
fn both_fs_instances_running_defers_prepare() {
    let fake = SystemdFake::fs_installed();
    fake.set_fs_unit(Instance::B, UnitState::Active, Some(OLD));
    let before = fake.record();
    let done = fake.run(Prepare, &params(NEW, Some(OLD)));
    assert_deferred(&done, Reason::HostNotReady, 30);
    assert!(detail(&done).contains("both tierstore-fs@a and tierstore-fs@b run"));
    assert_eq!(fake.record(), before);
}

/// Asserts that a `prepare` whose previous release cannot be asked, on a node with an fs process
/// and a declared restart, fails with `host_failed` and records nothing. Catches an fs decision
/// taken without a list.
#[test]
fn an_unreadable_previous_release_fails_prepare_on_a_node_with_an_fs_process() {
    let fake = SystemdFake::fs_installed();
    let before = fake.record();
    let p = params_with(
        NEW,
        Some("/var/lib/hostkeeper/packages/tierstore/0.0.1"),
        &format!("config_file = \"{CONFIG}\"\nmode = \"restart\"\n"),
    );
    assert_failed(&fake.run(Prepare, &p), Reason::HostFailed);
    assert_eq!(fake.record(), before);
}

/// Asserts that a `rollback` after a `cutover` that succeeded returns both units to a on the
/// previous release, the fs process first (its old instance is started and takes the mount back
/// from b, which exits; b's marker goes) and the store after, with the record idle on a and `fs_live`
/// a. Catches a rollback that leaves the fs process on the staged release, and one that stops b
/// while it holds the mount.
#[test]
fn a_rollback_after_cutover_returns_the_fs_process_before_the_store() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    let before = fake.log().len();
    fake.drive(&p, &[Rollback]);
    let acts: Vec<String> = fake.log()[before..]
        .iter()
        .filter(|line| line.contains("start") || line.contains("clear") || line.contains("set"))
        .cloned()
        .collect();
    assert_eq!(
        acts,
        [
            "fs_markers.set a",
            "fs_units.start a",
            "fs_markers.clear b",
            "markers.set a",
            "units.start a",
            "markers.clear b",
        ]
    );
    assert_eq!(fake.fs_running(), [Instance::A]);
    assert_eq!(fake.fs_markers(), [Instance::A]);
    assert_eq!(fake.fs_release(Instance::A), Some(PathBuf::from(OLD)));
    assert_eq!(fake.running(), [Instance::A]);
    let record = fake.record();
    assert_eq!(record, {
        let mut idle = Record::idle(Some(Instance::A));
        idle.fs_live = Some(Instance::A);
        idle
    });
}

/// Asserts that a `rollback` while the old fs instance still drains (running, after it gave the
/// mount away) defers with `draining` and touches no fs unit, and that the run after it exits
/// finishes the same way. Catches a start of a that races the exit of a.
#[test]
fn a_rollback_waits_for_the_old_fs_instance_to_finish_draining() {
    let fake = SystemdFake::fs_installed();
    fake.set_fs_drain_looks(2);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    let done = fake.run(Rollback, &p);
    assert_deferred(&done, Reason::Draining, 10);
    assert!(detail(&done).contains("tierstore-fs@a and tierstore-fs@b both run"));
    assert_eq!(
        fake.fs_actions(),
        ["fs_markers.set b", "fs_units.start b", "fs_markers.clear a"]
    );
    let mut runs = 0;
    while fake.run(Rollback, &p).outcome != Outcome::Success {
        runs += 1;
        assert!(runs < 10, "the drain did not end");
    }
    assert_eq!(fake.fs_running(), [Instance::A]);
    assert_eq!(fake.fs_markers(), [Instance::A]);
}

/// Asserts that when a, started to take the mount back, leaves b running (it takes more looks to
/// exit), the `rollback` defers with `draining` and a later run finishes with b's marker gone.
/// Catches a rollback that clears the marker of an instance that still holds the mount.
#[test]
fn a_rollback_waits_for_the_new_fs_instance_to_exit_after_the_mount_went_back() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_fs_drain_looks(3);
    let done = fake.run(Rollback, &p);
    assert_deferred(&done, Reason::Draining, 10);
    assert!(fake.fs_markers().contains(&Instance::B));
    let mut runs = 0;
    while fake.run(Rollback, &p).outcome != Outcome::Success {
        runs += 1;
        assert!(runs < 10, "the drain did not end");
    }
    assert_eq!(fake.fs_markers(), [Instance::A]);
    assert_eq!(fake.fs_running(), [Instance::A]);
}

/// Asserts that `restore` after a `cutover`, on a host that booted with both markers, makes the
/// fs process b (the record's `fs_next`) run alone and holds a's marker off, after it has
/// started the store; and that `restore` at rest starts the recorded `fs_live`. Catches a boot
/// that leaves an fs instance the record does not name running, and one that starts the fs
/// process before its store.
#[test]
fn restore_starts_the_recorded_fs_instance_after_the_store() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.reboot();
    let before = fake.log().len();
    fake.drive(&p, &[Restore]);
    let acts: Vec<String> = fake.log()[before..]
        .iter()
        .filter(|line| line.contains("start") || line.contains("clear") || line.contains("set"))
        .cloned()
        .collect();
    assert_eq!(
        acts,
        [
            "markers.clear a",
            "markers.set b",
            "units.start b",
            "fs_markers.clear a",
            "fs_markers.set b",
            "fs_units.start b",
        ]
    );
    assert_eq!(fake.fs_running(), [Instance::B]);
    assert_eq!(fake.fs_markers(), [Instance::B]);

    fake.drive(&p, &[Verify, Cleanup]);
    fake.reboot();
    fake.drive(&p, &[Restore]);
    assert_eq!(fake.fs_running(), [Instance::B]);
}

/// Asserts that `restore` stops an fs instance the record does not name when it runs and the one
/// the record names is not active (it would race for the mount), and does not when it does not
/// run. Catches a stop of a unit that is not loaded.
#[test]
fn restore_stops_the_other_fs_instance_only_when_it_runs() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, None);
    fake.set_fs_unit(Instance::A, UnitState::Inactive, None);
    fake.set_fs_unit(Instance::B, UnitState::Active, Some(OLD));
    fake.drive(&p, &[Restore]);
    assert!(fake.fs_actions().contains(&"fs_units.stop b".to_owned()));
    fake.drive(&p, &[Restore]);
    let stops = fake
        .fs_actions()
        .iter()
        .filter(|a| a.starts_with("fs_units.stop"))
        .count();
    assert_eq!(stops, 1);
}

/// Asserts that `restore` on a node whose record names no fs instance makes no fs call, and that
/// a record of an update that left the fs process alone restores `fs_live`. Catches a boot step
/// that starts an fs unit on a node that has none.
#[test]
fn restore_acts_on_the_fs_unit_only_when_the_record_names_one() {
    let fake = SystemdFake::installed();
    fake.drive(&params(NEW, None), &[Restore]);
    assert!(fake.fs_actions().is_empty());
    assert_eq!(fake.fs_looks(), 0);

    let fake = SystemdFake::fs_installed();
    fake.list_fs_hand_over(OLD, false);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.reboot();
    fake.drive(&p, &[Restore]);
    assert_eq!(fake.fs_running(), [Instance::A]);
}

/// Asserts that `uninstall` stops each fs instance that runs (and only those), removes both fs
/// markers, and leaves a record with no instance. Catches a stop of a unit that is not loaded,
/// which fails, and a marker left that starts the unit at the next boot.
#[test]
fn uninstall_stops_the_fs_instances_that_run_and_clears_both_markers() {
    let fake = SystemdFake::fs_installed();
    fake.drive(&params(NEW, None), &[Uninstall]);
    assert_eq!(
        fake.fs_actions(),
        [
            "fs_units.stop a",
            "fs_markers.clear a",
            "fs_markers.clear b"
        ]
    );
    assert!(fake.fs_running().is_empty());
    assert!(fake.fs_markers().is_empty());
    assert_eq!(fake.record(), Record::idle(None));

    let both = SystemdFake::fs_installed();
    both.set_fs_unit(Instance::B, UnitState::Active, Some(OLD));
    both.drive(&params(NEW, None), &[Uninstall]);
    assert_eq!(
        both.fs_actions()
            .iter()
            .filter(|a| a.starts_with("fs_units.stop"))
            .count(),
        2
    );
}

/// What `prepare` of a hook that does not know the fs unit leaves: the record without fs keys.
fn prepared_by_the_previous_hook(fake: &SystemdFake, p: &Params) {
    fake.drive(p, &[Prepare]);
    let mut record = fake.record();
    record.fs_live = None;
    record.fs_next = None;
    fake.set_record(&record);
}

/// Asserts that a `cutover` over a record with no fs keys (the `prepare` of the previous
/// release's hook) on a node with `tierstore-fs@a` running and both releases listing
/// `handoff-fs-v1` saves `fs_live` a and `fs_next` b before the first call of the fs unit, hands
/// the mount over, and that `verify` and `cleanup` then finish with `fs_live` b and the old
/// instance's marker removed. Catches a package node whose fs process stays on its release at
/// every rollout.
#[test]
fn a_cutover_plans_the_hand_over_when_the_previous_hook_prepared_the_record() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    prepared_by_the_previous_hook(&fake, &p);
    fake.fail_on(Some("fs_markers.set"));
    assert_failed(&fake.run(Cutover, &p), Reason::StateIo);
    assert_eq!(fake.record().fs_live, Some(Instance::A));
    assert_eq!(fake.record().fs_next, Some(Instance::B));
    assert!(!fake.record().has(Marker::Swapped));
    fake.fail_on(None);

    let done = fake.run(Cutover, &p);
    assert_ok(&done);
    assert!(detail(&done).ends_with("; tierstore-fs@b holds the mount"));
    assert_eq!(fake.peek_fs(Instance::B), UnitState::Active);
    assert_eq!(fake.fs_release(Instance::B), Some(PathBuf::from(NEW)));
    fake.drive(&p, &[Verify, Cleanup]);
    let record = fake.record();
    assert_eq!(record.fs_live, Some(Instance::B));
    assert_eq!(record.fs_next, None);
    assert_eq!(fake.fs_markers(), [Instance::B]);
}

/// Asserts that the same cutover with a previous release that does not list `handoff-fs-v1`
/// records `fs_live` a with no `fs_next`, starts nothing of the fs unit, and says the fs process
/// is left on its release; that with both fs instances running it records no fs instance and says
/// so, and that a swapped record is not planned again; and that with no fs instance running, or
/// no previous release, it plans nothing. Catches a hand-over to a release that cannot take the
/// mount, and a plan over a node that has no fs process.
#[test]
fn a_late_plan_leaves_the_fs_process_alone_when_it_cannot_hand_over() {
    let fake = SystemdFake::fs_installed();
    fake.list_fs_hand_over(OLD, false);
    let p = params(NEW, Some(OLD));
    prepared_by_the_previous_hook(&fake, &p);
    let done = fake.run(Cutover, &p);
    assert_ok(&done);
    assert!(detail(&done).ends_with("; tierstore-fs@a is left on its release"));
    assert_eq!(fake.record().fs_live, Some(Instance::A));
    assert_eq!(fake.record().fs_next, None);
    assert!(fake.fs_actions().is_empty());

    let fake = SystemdFake::fs_installed();
    prepared_by_the_previous_hook(&fake, &p);
    fake.set_fs_unit(Instance::B, UnitState::Active, Some(OLD));
    let before = fake.record();
    let done = fake.run(Cutover, &p);
    assert_ok(&done);
    assert!(
        detail(&done).ends_with(
            "; both tierstore-fs@a and tierstore-fs@b run: the fs process is left alone"
        )
    );
    assert_eq!(fake.record().fs_live, None);
    assert_eq!(fake.record().fs_next, None);
    assert!(fake.record().has(Marker::Swapped));
    assert!(!before.has(Marker::Swapped));
    assert!(fake.fs_actions().is_empty());
    // A run again after `swapped` that finds the store down starts it and plans nothing more.
    fake.set_unit(Instance::B, UnitState::Inactive, None, false);
    assert_ok(&fake.run(Cutover, &p));
    assert_eq!(fake.record().fs_live, None);
    assert!(fake.fs_actions().is_empty());

    let fake = SystemdFake::fs_installed();
    fake.set_fs_unit(Instance::A, UnitState::Inactive, None);
    prepared_by_the_previous_hook(&fake, &p);
    assert_ok(&fake.run(Cutover, &p));
    assert_eq!(fake.record().fs_live, None);
    assert!(fake.fs_actions().is_empty());

    let fake = SystemdFake::fs_installed();
    prepared_by_the_previous_hook(&fake, &p);
    let no_previous = params(NEW, None);
    let _ = fake.run(Cutover, &no_previous);
    assert_eq!(fake.record().fs_live, None);
    assert!(fake.fs_actions().is_empty());
}

/// Asserts that a `rollback` after a cutover that planned the fs process itself returns the fs
/// process to a, as after a cutover over a record the hook wrote whole. Catches a late plan that
/// the rollback cannot see.
#[test]
fn a_rollback_after_a_late_plan_returns_the_fs_process_to_a() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    prepared_by_the_previous_hook(&fake, &p);
    fake.drive(&p, &[Cutover]);
    assert_eq!(fake.peek_fs(Instance::B), UnitState::Active);
    fake.drive(&p, &[Rollback]);
    assert_eq!(fake.fs_running(), [Instance::A]);
    assert_eq!(fake.fs_markers(), [Instance::A]);
    assert_eq!(fake.fs_release(Instance::A), Some(PathBuf::from(OLD)));
    let mut idle = Record::idle(Some(Instance::A));
    idle.fs_live = Some(Instance::A);
    assert_eq!(fake.record(), idle);
}

/// Asserts that a cutover that plans the fs process itself on a node where `tierstore-fs@b` is
/// the instance that runs records `fs_live` b and `fs_next` a, and hands the mount over to a.
/// Catches a late plan that assumes the letter a serves.
#[test]
fn a_late_plan_names_the_fs_instance_that_runs() {
    let fake = SystemdFake::fs_installed();
    fake.set_fs_unit(Instance::A, UnitState::Inactive, None);
    fake.set_fs_unit(Instance::B, UnitState::Active, Some(OLD));
    let p = params(NEW, Some(OLD));
    prepared_by_the_previous_hook(&fake, &p);
    let done = fake.run(Cutover, &p);
    assert_ok(&done);
    assert!(detail(&done).ends_with("; tierstore-fs@a holds the mount"));
    assert_eq!(fake.record().fs_live, Some(Instance::B));
    assert_eq!(fake.record().fs_next, Some(Instance::A));
    assert_eq!(fake.peek_fs(Instance::A), UnitState::Active);
}

/// Asserts that `cutover` over a record with no fs keys, whose capabilities cannot be read, fails
/// with `host_failed` before its first act: no binary is installed, no unit or marker is touched,
/// and the record is as `prepare` left it. Catches a plan that is made after the store has been
/// stopped or started.
#[test]
fn a_late_plan_that_fails_changes_nothing() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    prepared_by_the_previous_hook(&fake, &p);
    let before = fake.record();
    let actions = fake.actions().len();
    fake.fail_on(Some("host.capabilities"));
    assert_failed(&fake.run(Cutover, &p), Reason::HostFailed);
    fake.fail_on(None);
    assert_eq!(fake.record(), before);
    assert!(fake.binary_acts().is_empty());
    assert_eq!(fake.actions().len(), actions);
    assert!(fake.fs_actions().is_empty());
}

/// Asserts that `restore` does not stop the other fs instance when it is only activating (it may
/// be taking the mount): its marker is cleared and it is left, while the recorded instance is
/// started; and that an active other is stopped as before. Catches a stop of an instance that is
/// taking the mount, which aborts it.
#[test]
fn restore_leaves_an_activating_other_fs_instance_and_clears_its_marker() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, None);
    fake.set_fs_unit(Instance::B, UnitState::Activating, Some(NEW));
    fake.fs_ports().set(Instance::B).unwrap();
    fake.drive(&p, &[Restore]);
    let actions = fake.fs_actions();
    assert!(!actions.iter().any(|a| a.starts_with("fs_units.stop")));
    assert!(actions.contains(&"fs_markers.clear b".to_owned()));
    assert_eq!(fake.fs_markers(), [Instance::A]);
    assert_eq!(fake.peek_fs(Instance::B), UnitState::Activating);
}
