//! The refusal of the switch of a node from two processes to one, and the refusals of a config
//! update that would move or drop the mount, on systemd.

use super::super::super::{Instance, Mode, Params, Reason, Record, Step, UnitState};
use super::{Linux, PKG_NEW, PKG_OLD, lin, release_record, update};
use Step::{Cleanup, Cutover, Prepare, Verify};

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

/// Asserts that a config update that would be the switch of a node to one process (the staged
/// file has an `fs` interface, the serving one has none, and an fs instance runs) is refused with
/// `mode_unsupported` and the exact detail that names the unit and the record key, whichever
/// letter runs, with the params' `mode = "restart"` no way round it, and that nothing is recorded,
/// started or stopped. Catches a switch that this release cannot carry out, which would end the fs
/// unit's marker or leave the mount held by a unit nobody hands over.
#[test]
fn a_systemd_config_update_that_would_switch_is_refused() {
    for (letter, instance) in [("a", Instance::A), ("b", Instance::B)] {
        let node = switching();
        node.fake.set_fs_unit(Instance::A, UnitState::Inactive);
        node.fake.set_fs_unit(instance, UnitState::Active);
        let before = node.fake.record();
        for p in [update(), restart_params()] {
            let done = node.step(Prepare, &p);
            lin::assert_failed(&done, Reason::ModeUnsupported);
            assert_eq!(
                lin::detail(&done),
                format!(
                    "this node still runs the two-process layout (tierstore-fs@{letter}.service is \
                     active): switch this node to the fs interface with release M first"
                )
            );
            assert_eq!(node.fake.record(), before);
            assert!(node.fake.actions().is_empty());
            assert!(node.fake.fs_actions().is_empty());
        }
    }
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
/// mode_unsupported both times; that `mode = "restart"` lets the first through, and the second is
/// the refused switch; and that where the node has no mount to drop (no fs interface served, no fs
/// instance) the same change is the plain restart.
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

    // A switch that would be a restart: refused as a restart that drops the mount, and with the
    // params' restart declared, as the switch.
    let node = Linux::on(
        lin::SystemdFake::fs_installed(),
        &[],
        Some(&listening("0.0.0.0:9300")),
        &with("192.0.2.7:9300"),
    );
    let done = node.step(Prepare, &update());
    lin::assert_failed(&done, Reason::ModeUnsupported);
    assert!(lin::detail(&done).contains("drop the mount"));
    assert!(!node.fake.record().updating);
    let done = node.step(Prepare, &restart_params());
    lin::assert_failed(&done, Reason::ModeUnsupported);
    assert!(lin::detail(&done).contains("two-process layout"));
    assert!(!node.fake.record().updating);

    // No mount to drop: a plain restart.
    let node = Linux::new(Some(&listening("0.0.0.0:9300")), &with("192.0.2.7:9300"));
    node.go(&update(), &[Prepare]);
    assert_eq!(node.fake.record().mode, Some(Mode::Restart));
    assert!(!node.fake.record().fs_switch);
}

/// Asserts that a config update whose staged file drops the `fs` interface of the file that serves
/// is refused with mode_unsupported, naming the interface, with nothing recorded or started (the
/// new process would not take the mount, and the old one's stop would let the kernel abort it);
/// and that `mode = "restart"` lets it through. Catches a config update that silently ends the
/// mount under the node's jobs.
#[test]
fn a_config_update_that_drops_the_fs_interface_is_refused() {
    let node = Linux::new(Some(&fs_file("")), lin::WITH_SOCKET);
    let done = node.step(Prepare, &update());
    lin::assert_failed(&done, Reason::ModeUnsupported);
    assert!(
        lin::detail(&done).starts_with("the staged file has no fs interface"),
        "{}",
        lin::detail(&done)
    );
    assert_eq!(node.fake.record(), Record::idle(Some(Instance::A)));
    assert!(node.fake.actions().is_empty());
    lin::assert_ok(&node.step(Prepare, &restart_params()));
}
