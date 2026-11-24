//! The switch of a node from two processes to one, and the refusals of a config update that would
//! move or drop the mount, on systemd.

use super::super::super::{Instance, Mode, Params, Reason, Record, Step, UnitState};
use super::{Linux, PKG_NEW, PKG_OLD, lin, release_record, update};
use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// A file with an `fs` interface whose params are `params` (lines of the table).
fn fs_file(params: &str) -> String {
    format!(
        "{}\n[[interface]]\ntype = \"fs\"\n\n[interface.params]\n{params}",
        lin::WITH_SOCKET
    )
}

/// A node whose serving file has no `fs` interface and whose `tierstore-fs` instance `a` runs,
/// and a config package that stages a file with one.
fn switching() -> Linux {
    Linux::on(
        lin::SystemdFake::fs_installed(),
        &[],
        Some(lin::WITH_SOCKET),
        &fs_file(""),
    )
}

/// Params that declare a restart.
fn restart_params() -> Params {
    lin::params_with(
        PKG_NEW,
        Some(PKG_OLD),
        &format!("config_file = \"{}\"\nmode = \"restart\"\n", lin::CONFIG),
    )
}

/// The position of the first log line that starts with `prefix`.
fn at(node: &Linux, prefix: &str) -> usize {
    node.fake
        .log()
        .iter()
        .position(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no call {prefix}: {:?}", node.fake.log()))
}

/// Asserts that a config update that switches a node (the staged file has an `fs` interface, the
/// serving one has none, and an fs instance runs) records the switch with the fs instance that
/// runs, hands the store over, removes both fs markers only after the new instance serves, starts
/// and stops no fs unit, and that `cleanup` records no fs instance in the config hook's record
/// and in the release's. Catches a switch that leaves the old fs unit startable, one that stops
/// the unit that holds the mount, and a release record that goes on naming the old fs instance.
#[test]
fn the_switch_clears_the_fs_markers_and_records_no_fs_instance() {
    let node = switching();
    let p = update();
    let prepared = node.step(Prepare, &p);
    lin::assert_ok(&prepared);
    assert!(
        lin::detail(&prepared)
            .ends_with("; the mount moves from tierstore-fs@a to the one process of the new file")
    );
    let record = node.fake.record();
    assert!(record.fs_switch);
    assert_eq!(
        (record.fs_live, record.fs_next, record.mode),
        (Some(Instance::A), None, Some(Mode::Handover))
    );
    assert!(node.fake.fs_actions().is_empty());

    let cut = node.step(Cutover, &p);
    lin::assert_ok(&cut);
    assert!(lin::detail(&cut).contains("; the mount moved from tierstore-fs@a to the one process"));
    assert!(at(&node, "units.start b") < at(&node, "fs_markers.clear a"));
    assert_eq!(
        node.fake.fs_actions(),
        ["fs_markers.clear a", "fs_markers.clear b"]
    );
    assert!(node.fake.fs_markers().is_empty());
    assert_eq!(node.fake.fs_running(), [Instance::A]);

    // Run again, nothing is done to the fs unit.
    node.go(&p, &[Cutover, Verify, Cleanup]);
    assert_eq!(
        node.fake.fs_actions(),
        ["fs_markers.clear a", "fs_markers.clear b"]
    );
    let record = node.fake.record();
    assert_eq!((record.fs_live, record.fs_next), (None, None));
    assert_eq!(node.service.get(), Some(release_record(Instance::B)));
}

/// Asserts that the switch names the fs instance that runs, whichever letter it has.
#[test]
fn the_switch_names_the_fs_instance_that_runs() {
    let node = switching();
    node.fake
        .set_fs_unit(Instance::A, UnitState::Inactive, None);
    node.fake
        .set_fs_unit(Instance::B, UnitState::Active, Some(lin::OLD));
    node.go(&update(), &[Prepare]);
    assert_eq!(node.fake.record().fs_live, Some(Instance::B));
}

/// Asserts that a switch whose new instance does not start fails `cutover` with the fs markers
/// and units untouched, and that the rollback then restores the store alone: the fs instance
/// still runs, so no fs unit is started or stopped. Catches a cutover that gives the fs unit up
/// before the new instance serves, and a rollback that restarts a unit that holds the mount.
#[test]
fn a_switch_whose_new_instance_does_not_serve_keeps_the_fs_markers() {
    let node = switching();
    let p = update();
    node.go(&p, &[Prepare]);
    node.fake.fail_on(Some("units.start b"));
    lin::assert_failed(&node.step(Cutover, &p), Reason::UnitFailed);
    node.fake.fail_on(None);
    assert!(node.fake.fs_actions().is_empty());
    assert_eq!(node.fake.fs_markers(), [Instance::A]);
    assert_eq!(node.fake.fs_running(), [Instance::A]);

    lin::assert_ok(&node.step(Rollback, &p));
    assert!(node.fake.fs_actions().is_empty());
    assert_eq!(node.fake.fs_markers(), [Instance::A]);
    assert_eq!(node.fake.running(), [Instance::A]);
    let mut want = Record::idle(Some(Instance::A));
    want.fs_live = Some(Instance::A);
    assert_eq!(node.fake.record(), want);
}

/// Asserts that the rollback of a switch whose cutover took the mount (the fs unit ended) lays the
/// marker of the fs instance and starts it before it lays the marker of the store's old instance
/// and starts that, and that the record after names the fs instance and no switch. Catches a
/// rollback that hands the sockets back first, to a process that still holds the mount and
/// refuses a taker that does not ask for it.
#[test]
fn the_switch_rolls_back_mount_first_then_sockets() {
    let node = switching();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    // The old fs process ended after it gave the mount away, and so did the old store.
    node.fake
        .set_fs_unit(Instance::A, UnitState::Inactive, None);
    node.fake
        .set_unit(Instance::A, UnitState::Inactive, None, false);
    lin::assert_ok(&node.step(Rollback, &p));

    let log = node.fake.log();
    let mount = [at(&node, "fs_markers.set a"), at(&node, "fs_units.start a")];
    let sockets = [at(&node, "markers.set a"), at(&node, "units.start a")];
    assert!(mount[0] < mount[1], "{log:?}");
    assert!(mount[1] < sockets[0], "{log:?}");
    assert!(sockets[0] < sockets[1], "{log:?}");
    assert_eq!(node.fake.fs_running(), [Instance::A]);
    assert_eq!(node.fake.fs_markers(), [Instance::A]);
    let mut want = Record::idle(Some(Instance::A));
    want.fs_live = Some(Instance::A);
    assert_eq!(node.fake.record(), want);
}

/// Asserts that the rollback of a switch waits (deferred, draining) while the fs instance is still
/// ending after it gave the mount away, starts nothing of the fs unit, and goes on when it has
/// ended.
#[test]
fn the_switch_rollback_waits_for_an_fs_instance_that_is_still_ending() {
    let node = switching();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    node.fake.set_fs_deactivating(Instance::A, lin::OLD, 100);
    lin::assert_deferred(&node.step(Rollback, &p), Reason::Draining, 10);
    assert_eq!(node.fake.fs_actions().len(), 2);
    assert_eq!(node.fake.record().fs_live, Some(Instance::A));
    node.fake
        .set_fs_unit(Instance::A, UnitState::Inactive, None);
    node.fake
        .set_unit(Instance::A, UnitState::Inactive, None, false);
    lin::assert_ok(&node.step(Rollback, &p));
    assert_eq!(node.fake.fs_running(), [Instance::A]);
}

/// Asserts that an fs instance that cannot be started in the rollback of the switch fails the step
/// and takes its marker away again, so that it is not started by a boot, and leaves the record of
/// the switch as it was.
#[test]
fn a_switch_rollback_whose_fs_instance_does_not_start_removes_its_marker() {
    let node = switching();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    node.fake
        .set_fs_unit(Instance::A, UnitState::Inactive, None);
    node.fake.fail_on(Some("fs_units.start"));
    lin::assert_failed(&node.step(Rollback, &p), Reason::UnitFailed);
    node.fake.fail_on(None);
    assert!(node.fake.fs_markers().is_empty());
    assert!(node.fake.record().fs_switch);
}

/// Asserts that a record of a switch that names no fs instance (which `prepare` never writes) is
/// refused by the rollback as invalid, before the store is touched.
#[test]
fn a_switch_record_that_names_no_fs_instance_fails_the_rollback() {
    let node = switching();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    let mut record = node.fake.record();
    record.fs_live = None;
    node.fake.set_record(&record);
    let before = node.fake.actions().len();
    lin::assert_failed(&node.step(Rollback, &p), Reason::StateInvalid);
    assert_eq!(node.fake.actions().len(), before);
}

/// Asserts that a config package that stages an `fs` interface on a node where no fs instance
/// runs is a plain config update: no switch on record, no fs unit or marker touched, and the
/// store is handed over. Catches a switch noted on a node with no mount to move.
#[test]
fn a_file_with_an_fs_interface_on_a_node_with_no_fs_instance_is_a_plain_update() {
    let node = Linux::new(Some(lin::WITH_SOCKET), &fs_file(""));
    let p = update();
    node.go(&p, &[Prepare]);
    let record = node.fake.record();
    assert!(!record.fs_switch);
    assert_eq!(record.mode, Some(Mode::Handover));
    node.go(&p, &[Cutover, Verify, Cleanup]);
    assert!(node.fake.fs_actions().is_empty());
    assert_eq!(node.service.get(), Some(release_record(Instance::B)));
    assert_eq!(node.fake.record().fs_live, None);
}

/// Asserts that a config update whose staged file names another mount point, control socket or
/// runtime directory than the serving file (both have an `fs` interface) is refused by `prepare`
/// with mode_unsupported naming the path, with nothing recorded and nothing started; and that the
/// params' `mode = "restart"` lets it through as a restart. Catches an update that makes the new
/// process nak the take of the mount and leaves it to the rollback.
#[test]
fn a_config_update_that_moves_the_mount_point_is_refused() {
    for (key, was, now) in [
        ("mountpoint", "/run/tierstore/a", "/run/tierstore/b"),
        ("control_socket", "/run/ts/a.sock", "/run/ts/b.sock"),
        ("runtime_dir", "/run/tsfs-a", "/run/tsfs-b"),
    ] {
        let line = |path: &str| format!("{key} = \"{path}\"\n");
        let node = Linux::new(Some(&fs_file(&line(was))), &fs_file(&line(now)));
        let done = node.step(Prepare, &update());
        lin::assert_failed(&done, Reason::ModeUnsupported);
        assert!(lin::detail(&done).contains(key), "{}", lin::detail(&done));
        assert!(lin::detail(&done).contains("mode = \"restart\""));
        assert_eq!(node.fake.record(), Record::idle(Some(Instance::A)));
        assert!(node.fake.actions().is_empty());

        let declared = node.step(Prepare, &restart_params());
        lin::assert_ok(&declared);
        assert_eq!(node.fake.record().mode, Some(Mode::Restart));
    }
}

/// Asserts that a change of the `fs` interface's other params (and of its paths to their
/// defaults' spelling, where equal) is a hand-over that keeps the mount: the staged file moves
/// none of the three paths, so the mount stays with the process that takes it. Catches an
/// interface comparison that counts the params of the `fs` entry as listening addresses and turns
/// the update into a restart.
#[test]
fn a_change_of_the_fs_params_other_than_the_paths_hands_over() {
    let node = Linux::new(
        Some(&fs_file(
            "mountpoint = \"/run/tierstore/fs\"\nfetch_timeout = 5\n",
        )),
        &fs_file("mountpoint = \"/run/tierstore/fs\"\nfetch_timeout = 9\n"),
    );
    let p = update();
    node.go(&p, &[Prepare]);
    assert_eq!(node.fake.record().mode, Some(Mode::Handover));
    node.go(&p, &[Cutover]);
    assert_eq!(node.fake.actions(), ["markers.set b", "units.start b"]);
}

/// Asserts that a staged file with an `fs` interface whose change must be a restart (it listens on
/// another interface) is refused when the serving file has one too (a restart drops the mount),
/// and when it has none and an fs instance runs (the switch would be a restart), with
/// mode_unsupported both times; that `mode = "restart"` lets either through; and that where the
/// node has no mount to drop (no fs interface served, no fs instance) the same change is the
/// plain restart.
#[test]
fn a_restart_that_would_drop_the_mount_is_refused() {
    let listening = |address: &str| {
        format!(
            "[server]\nhandoff_socket = \"/var/lib/tierstore/handoff.sock\"\nlisten = \"{address}\"\n"
        )
    };
    let with = |address: &str| {
        format!(
            "{}{}",
            listening(address),
            fs_file("").replace(lin::WITH_SOCKET, "")
        )
    };
    // The serving file serves the mount.
    let node = Linux::new(Some(&with("0.0.0.0:9300")), &with("192.0.2.7:9300"));
    let done = node.step(Prepare, &update());
    lin::assert_failed(&done, Reason::ModeUnsupported);
    assert!(lin::detail(&done).contains("drop the mount"));
    lin::assert_ok(&node.step(Prepare, &restart_params()));
    assert_eq!(node.fake.record().mode, Some(Mode::Restart));

    // A switch that would be a restart.
    let node = Linux::on(
        lin::SystemdFake::fs_installed(),
        &[],
        Some(&listening("0.0.0.0:9300")),
        &with("192.0.2.7:9300"),
    );
    lin::assert_failed(&node.step(Prepare, &update()), Reason::ModeUnsupported);
    assert!(!node.fake.record().updating);
    lin::assert_ok(&node.step(Prepare, &restart_params()));
    assert!(node.fake.record().fs_switch);
    assert_eq!(node.fake.record().mode, Some(Mode::Restart));

    // No mount to drop: a plain restart.
    let node = Linux::new(Some(&listening("0.0.0.0:9300")), &with("192.0.2.7:9300"));
    node.go(&update(), &[Prepare]);
    assert_eq!(node.fake.record().mode, Some(Mode::Restart));
    assert!(!node.fake.record().fs_switch);
}
