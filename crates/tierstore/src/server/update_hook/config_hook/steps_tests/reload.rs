//! The reload class: a config package whose staged file differs from the one that serves in
//! nothing a running process cannot apply (`fs.tree_list_ram_bytes`), or in nothing at all, is
//! applied in place by the instance that serves, and starts no other instance.

use std::path::{Path, PathBuf};

use super::super::super::{
    Instance, ListCap, Marker, Mode, Reason, Record, Reload, Step, UnitState,
};
use super::super::GUARD_FILE;
use super::{
    Cleanup, Cutover, Linux, MOUNT, PKG_NEW, Prepare, Verify, build, first, in_flight, lin,
    release_record, staged_copy, update,
};
use Step::Rollback;

/// The control socket an `fs` interface has when its params name none.
const DEFAULT_SOCKET: &str = "/run/tierstore-fs/control.sock";
/// 512 MiB, the cap a file that names none has, and 1 GiB.
const DEFAULT_CAP: u64 = 512 << 20;
const GIB: u64 = 1 << 30;

/// A file with an `fs` interface whose params are `params` (lines of the table).
fn fs_file(params: &str) -> String {
    format!(
        "{}\n[[interface]]\ntype = \"fs\"\n\n[interface.params]\n{params}",
        lin::WITH_SOCKET
    )
}

/// The params line of the cap `bytes`.
fn cap(bytes: u64) -> String {
    format!("tree_list_ram_bytes = {bytes}\n")
}

/// A node serving a file with an `fs` interface whose cap is `from`, and a config package that
/// stages the same file with the cap `to` (a `None` names no cap).
fn changing(from: Option<u64>, to: Option<u64>) -> Linux {
    let text = |bytes: Option<u64>| fs_file(&bytes.map(cap).unwrap_or_default());
    Linux::new(Some(&text(from)), &text(to))
}

/// The set request of `bytes` to the default control socket.
fn set(bytes: u64) -> (PathBuf, String) {
    (
        PathBuf::from(DEFAULT_SOCKET),
        format!("set tree_list_ram_bytes {bytes}"),
    )
}

/// The record of the reload from `from` to `to` on the default socket, prepared (no mount yet).
fn changed(from: u64, to: u64) -> Reload {
    Reload {
        guard: false,
        tree_list_ram_bytes: Some(ListCap {
            from,
            to,
            control_socket: PathBuf::from(DEFAULT_SOCKET),
            mount: None,
        }),
    }
}

/// The same, once `cutover` set it.
fn set_on_mount(from: u64, to: u64) -> Reload {
    let mut reload = changed(from, to);
    if let Some(cap) = &mut reload.tree_list_ram_bytes {
        cap.mount = Some(MOUNT.to_owned());
    }
    reload
}

/// The config hook's record of a reload `reload` committed and at rest, `A` serving.
fn at_rest(reload: Reload) -> Record {
    Record {
        updating: false,
        markers: [Marker::Swapped, Marker::Verified, Marker::Stopping].into(),
        reload: Some(reload),
        ..in_flight(Some(Instance::A))
    }
}

/// Asserts that a staged file byte-identical to the one that serves takes no hand-over: no
/// instance is started or stopped, no marker changes, the guard is not reloaded and the control
/// socket not asked, and the instance that served still serves and is written back. Catches a
/// config update that hands over for a file that changes nothing.
#[test]
fn a_staged_file_identical_to_the_serving_one_starts_no_other_instance() {
    let node = Linux::new(Some(lin::WITH_SOCKET), lin::WITH_SOCKET);
    let p = update();
    let prepared = node.step(Prepare, &p);
    lin::assert_ok(&prepared);
    assert_eq!(
        lin::detail(&prepared),
        "prepared in place: tierstore@a applies nothing a running process cannot apply; no \
         other instance starts"
    );
    node.go(&p, &[Cutover, Verify, Cleanup]);
    assert_eq!(node.fake.actions(), Vec::<String>::new());
    assert_eq!(node.fake.running(), [Instance::A]);
    assert_eq!(node.fake.markers(), [Instance::A]);
    assert_eq!(node.reloads(), 0);
    assert_eq!(node.control.asked(), []);
    assert_eq!(node.fake.record(), at_rest(Reload::default()));
    assert_eq!(node.service.get(), Some(release_record(Instance::A)));
}

/// Asserts that a staged file that differs from the one that serves only in comments, spacing and
/// the order of its keys is applied in place too: the files are compared as tables.
#[test]
fn a_staged_file_alike_as_a_table_starts_no_other_instance() {
    let serving =
        "[server]\nhandoff_socket = \"/var/lib/tierstore/handoff.sock\"\nproxy_id = \"p\"\n";
    let staged = "# the same\n[server]\nproxy_id   = \"p\"\nhandoff_socket = \"/var/lib/tierstore/handoff.sock\"\n";
    let node = Linux::new(Some(serving), staged);
    node.go(&update(), &[Prepare, Cutover, Verify, Cleanup]);
    assert_eq!(node.fake.actions(), Vec::<String>::new());
    assert_eq!(node.fake.record().reload, Some(Reload::default()));
}

/// Asserts the whole of a change of `tree_list_ram_bytes` alone: `prepare` records it in place
/// with no next instance and no mode, `cutover` has the control socket set the staged value and
/// starts nothing, `verify` reads the value back from the gauge of the mount the socket named on
/// `/metricz` (the gauge of an older mount beside it shows another), and `cleanup` commits with
/// `A` still serving. Catches a cap change that hands over, and one verified by a gauge that does
/// not show it.
#[test]
fn a_changed_tree_list_cap_is_set_in_place_and_read_back_from_the_gauge() {
    let node = changing(Some(DEFAULT_CAP), Some(GIB));
    let p = update();
    node.go(&p, &[Prepare]);
    let record = node.fake.record();
    assert!(record.updating);
    assert_eq!(
        (record.live, record.next, record.mode),
        (Some(Instance::A), None, None)
    );
    assert_eq!(record.reload, Some(changed(DEFAULT_CAP, GIB)));

    let cut = node.step(Cutover, &p);
    lin::assert_ok(&cut);
    assert_eq!(
        lin::detail(&cut),
        "applied in place: tierstore@a took fs.tree_list_ram_bytes 536870912 to 1073741824; no \
         other instance started"
    );
    assert_eq!(node.control.asked(), [set(GIB)]);
    assert_eq!(node.fake.actions(), Vec::<String>::new());
    let record = node.fake.record();
    assert!(record.has(Marker::Swapped));
    assert_eq!(record.reload, Some(set_on_mount(DEFAULT_CAP, GIB)));

    let verified = node.step(Verify, &p);
    lin::assert_ok(&verified);
    assert_eq!(
        lin::detail(&verified),
        format!(
            "fs.tree_list_ram_bytes is 1073741824 in place; {} serves on port 9300",
            build()
        )
    );
    assert!(node.fake.log().iter().any(|line| line == "get /metricz"));
    assert!(node.fake.record().has(Marker::Verified));

    let done = node.step(Cleanup, &p);
    lin::assert_ok(&done);
    assert!(!done.report.reversible);
    assert_eq!(
        lin::detail(&done),
        "committed: tierstore@a serves the new file, applied in place"
    );
    assert_eq!(node.fake.actions(), Vec::<String>::new());
    assert_eq!(node.fake.running(), [Instance::A]);
    assert_eq!(node.fake.record(), at_rest(set_on_mount(DEFAULT_CAP, GIB)));
    assert_eq!(node.service.get(), Some(release_record(Instance::A)));
    assert_eq!(node.control.asked(), [set(GIB)]);
}

/// Asserts the boundaries of what counts as a change of the cap: a file that names none has the
/// default, so naming the default changes nothing and asks the socket nothing; dropping the key
/// sets the default back; and a params table left with nothing but the cap is alike to none.
#[test]
fn the_cap_a_file_leaves_out_is_the_default() {
    let node = changing(None, Some(DEFAULT_CAP));
    node.go(&update(), &[Prepare, Cutover, Verify, Cleanup]);
    assert_eq!(node.control.asked(), []);
    assert_eq!(node.fake.actions(), Vec::<String>::new());

    let node = changing(Some(GIB), None);
    node.go(&update(), &[Prepare, Cutover]);
    assert_eq!(node.control.asked(), [set(DEFAULT_CAP)]);

    let bare = format!("{}\n[[interface]]\ntype = \"fs\"\n", lin::WITH_SOCKET);
    let node = Linux::new(Some(&fs_file(&cap(GIB))), &bare);
    node.go(&update(), &[Prepare, Cutover]);
    assert_eq!(node.control.asked(), [set(DEFAULT_CAP)]);
    assert_eq!(node.fake.actions(), Vec::<String>::new());
}

/// Asserts that the cap is set on the control socket the file names, under its runtime directory
/// or by its own param.
#[test]
fn the_cap_is_set_on_the_control_socket_the_file_names() {
    for (params, socket) in [
        ("runtime_dir = \"/run/ts\"\n", "/run/ts/control.sock"),
        ("control_socket = \"/run/c.sock\"\n", "/run/c.sock"),
    ] {
        let node = Linux::new(
            Some(&fs_file(&format!("{params}{}", cap(GIB)))),
            &fs_file(&format!("{params}{}", cap(2 * GIB))),
        );
        node.go(&update(), &[Prepare, Cutover]);
        assert_eq!(
            node.control.asked(),
            [(
                PathBuf::from(socket),
                format!("set tree_list_ram_bytes {}", 2 * GIB)
            )]
        );
    }
}

/// Asserts that a staged file that changes a key a running process cannot apply still hands
/// over, with or without a change of the cap beside it, and asks the control socket nothing;
/// and so does a file whose cap is not a whole number of bytes, which the comparison cannot
/// read. Catches a reload class that swallows a change that needs a new process.
#[test]
fn a_changed_key_that_is_not_applied_in_place_still_hands_over() {
    let other = |bytes: u64| fs_file(&format!("fetch_timeout = 9\n{}", cap(bytes)));
    for (serving, staged) in [
        (fs_file(&cap(GIB)), other(GIB)),
        (fs_file(&cap(GIB)), other(2 * GIB)),
        (
            fs_file("tree_list_ram_bytes = \"big\"\n"),
            fs_file(&cap(GIB)),
        ),
        (fs_file("tree_list_ram_bytes = -1\n"), fs_file(&cap(GIB))),
        (
            lin::WITH_SOCKET.to_owned(),
            format!("{}proxy_id = \"p2\"\n", lin::WITH_SOCKET),
        ),
    ] {
        let node = Linux::new(Some(&serving), &staged);
        let p = update();
        node.go(&p, &[Prepare]);
        let record = node.fake.record();
        assert_eq!(record.mode, Some(Mode::Handover), "{staged}");
        assert_eq!(record.reload, None);
        node.go(&p, &[Cutover]);
        assert_eq!(node.fake.actions(), ["markers.set b", "units.start b"]);
        assert_eq!(node.control.asked(), []);
    }
}

/// Asserts that a package that changes the guard's file beside a file applied in place reloads
/// the guard at `cutover` and again at `rollback`, and that one whose guard file is the installed
/// one reloads it at neither: the guard is reloaded only when its file changed.
#[test]
fn the_guard_is_reloaded_in_place_only_when_its_file_changed() {
    let staged_guard = staged_copy(Path::new(PKG_NEW), Path::new(GUARD_FILE));
    let node = changing(Some(GIB), Some(2 * GIB));
    node.files.put(Path::new(GUARD_FILE), "old rules");
    node.files.put(&staged_guard, "new rules");
    let p = update();
    node.go(&p, &[Prepare]);
    assert!(node.fake.record().reload.is_some_and(|reload| reload.guard));
    let cut = node.step(Cutover, &p);
    lin::assert_ok(&cut);
    assert!(lin::detail(&cut).contains("1073741824 to 2147483648 and the guard's file"));
    assert_eq!(node.reloads(), 1);
    node.go(&p, &[Rollback]);
    assert_eq!(node.reloads(), 2);

    let node = changing(Some(GIB), Some(2 * GIB));
    node.files.put(Path::new(GUARD_FILE), "rules");
    node.files.put(&staged_guard, "rules");
    node.go(&p, &[Prepare, Cutover, Rollback]);
    assert_eq!(node.reloads(), 0);

    let node = Linux::new(Some(lin::WITH_SOCKET), lin::WITH_SOCKET);
    node.files.put(&staged_guard, "first rules");
    node.go(&p, &[Prepare, Cutover]);
    assert_eq!(node.reloads(), 1);
    assert_eq!(node.control.asked(), []);
}

/// Asserts that a `rollback` after `cutover` sets the value of the file that served again, starts
/// and stops nothing, and leaves the record at rest with `A` serving; and that a `rollback` run
/// again finds nothing in flight.
#[test]
fn a_rollback_sets_the_cap_of_the_file_that_served_again() {
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    let back = node.step(Rollback, &p);
    lin::assert_ok(&back);
    assert_eq!(
        lin::detail(&back),
        "rolled back in place: tierstore@a serves on fs.tree_list_ram_bytes 1073741824 as before"
    );
    assert_eq!(node.control.asked(), [set(2 * GIB), set(GIB)]);
    assert_eq!(node.fake.actions(), Vec::<String>::new());
    assert_eq!(node.fake.record(), Record::idle(Some(Instance::A)));
    let again = node.step(Rollback, &p);
    lin::assert_ok(&again);
    assert_eq!(node.control.asked().len(), 2);
}

/// Asserts that a `rollback` of a reload with no cap to set, and one whose serving instance no
/// longer runs (its start reads the file put back), ask the control socket nothing.
#[test]
fn a_rollback_with_nothing_to_set_asks_the_socket_nothing() {
    let node = Linux::new(Some(lin::WITH_SOCKET), lin::WITH_SOCKET);
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    let back = node.step(Rollback, &p);
    lin::assert_ok(&back);
    assert_eq!(
        lin::detail(&back),
        "rolled back in place: tierstore@a serves as before"
    );

    let node = changing(Some(GIB), Some(2 * GIB));
    node.go(&p, &[Prepare, Cutover]);
    node.fake
        .set_unit(Instance::A, UnitState::Inactive, Some(lin::OLD), false);
    node.go(&p, &[Rollback]);
    assert_eq!(node.control.asked(), [set(2 * GIB)]);
    assert_eq!(node.fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that the `rollback` of a node's first config package applied in place sets the cap the
/// process read at its start again, reloads no guard, and fails `no_previous_release` as every
/// first package's rollback does, with the record at rest.
#[test]
fn a_rollback_of_the_first_package_sets_the_cap_back_and_says_there_is_nothing_to_go_back_to() {
    let node = changing(Some(GIB), Some(2 * GIB));
    node.files.put(
        &staged_copy(Path::new(PKG_NEW), Path::new(GUARD_FILE)),
        "rules",
    );
    let p = first();
    node.go(&p, &[Prepare, Cutover]);
    assert_eq!(node.reloads(), 1);
    let back = node.step(Rollback, &p);
    lin::assert_failed(&back, Reason::NoPreviousRelease);
    assert_eq!(node.reloads(), 1);
    assert_eq!(node.control.asked(), [set(2 * GIB), set(GIB)]);
    assert_eq!(node.fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that a `verify` whose gauge does not show the value set yet (the node has not been
/// told) is deferred, `not_ready` after `verify_retry_secs`, saying what the gauge shows, and
/// records nothing; once the gauge shows it, `verify` succeeds.
#[test]
fn a_verify_whose_gauge_shows_another_cap_is_deferred() {
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.control.lags.set(true);
    node.go(&p, &[Prepare, Cutover]);
    let early = node.step(Verify, &p);
    lin::assert_deferred(&early, Reason::NotReady, 5);
    assert_eq!(
        lin::detail(&early),
        format!(
            "tierstore_fs_tree_list_ram_bytes{{mount=\"{MOUNT}\"}} on port 9300 shows nothing, \
             not 2147483648"
        )
    );
    node.control.gauge.set(Some(GIB));
    let stale = node.step(Verify, &p);
    lin::assert_deferred(&stale, Reason::NotReady, 5);
    assert!(lin::detail(&stale).ends_with("shows 1073741824, not 2147483648"));
    assert!(!node.fake.record().has(Marker::Verified));
    node.control.gauge.set(Some(2 * GIB));
    node.go(&p, &[Verify]);
    assert!(node.fake.record().has(Marker::Verified));
}

/// Asserts that a `verify` of a reload whose serving instance is not active fails `not_ready`,
/// and one that `/readyz` does not answer is deferred.
#[test]
fn a_verify_needs_the_serving_instance_active_and_ready() {
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    node.fake
        .set_unit(Instance::A, UnitState::Inactive, Some(lin::OLD), false);
    let done = node.step(Verify, &p);
    lin::assert_failed(&done, Reason::NotReady);
    assert_eq!(lin::detail(&done), "tierstore@a is Inactive, not active");
    node.fake
        .set_unit(Instance::A, UnitState::Active, Some(lin::OLD), false);
    let done = node.step(Verify, &p);
    lin::assert_deferred(&done, Reason::NotReady, 5);
    assert_eq!(
        lin::detail(&done),
        "tierstore@a does not answer /readyz yet"
    );
}

/// Asserts that a `cutover` whose control socket cannot be reached, or does not take the value
/// (it answers a refusal, or what is not JSON), fails `host_failed` with the answer, records no
/// `swapped`, and is reversible; and that a `cutover` run again after one that succeeded sets the
/// value again and changes nothing else.
#[test]
fn a_cutover_the_control_socket_does_not_take_fails_and_one_run_again_converges() {
    for answer in [
        Some("{\"ok\":false,\"error\":\"unknown request\"}"),
        Some("nope"),
        None,
    ] {
        let node = changing(Some(GIB), Some(2 * GIB));
        let p = update();
        node.go(&p, &[Prepare]);
        match answer {
            Some(answer) => *node.control.answer.borrow_mut() = Some(answer.to_owned()),
            None => node.control.broken.set(true),
        }
        let done = node.step(Cutover, &p);
        lin::assert_failed(&done, Reason::HostFailed);
        assert!(done.report.reversible);
        if let Some(answer) = answer {
            assert_eq!(
                lin::detail(&done),
                format!(
                    "the control socket {DEFAULT_SOCKET} did not take \"set tree_list_ram_bytes \
                     2147483648\": it answered {answer}"
                )
            );
        }
        assert!(!node.fake.record().has(Marker::Swapped));
    }
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    let before = node.fake.snapshot();
    node.go(&p, &[Cutover]);
    assert_eq!(node.fake.snapshot(), before);
    assert_eq!(node.control.asked(), [set(2 * GIB), set(2 * GIB)]);
}

/// Asserts that the steps of a reload keep their order: `verify` before `cutover` and `cutover`
/// after the commit are `out_of_order`, `cleanup` before `verify` is `not_verified`, `rollback`
/// after the commit is `committed`; and that after the commit `cleanup` again has nothing left to
/// clean and `verify` again watches the build and the gauge, records nothing, and is not
/// reversible.
#[test]
fn the_steps_of_a_reload_keep_their_order() {
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.go(&p, &[Prepare]);
    lin::assert_failed(&node.step(Verify, &p), Reason::OutOfOrder);
    node.go(&p, &[Cutover]);
    lin::assert_failed(&node.step(Cleanup, &p), Reason::NotVerified);
    node.go(&p, &[Verify]);
    let verified = node.fake.snapshot();
    node.go(&p, &[Verify]);
    assert_eq!(node.fake.snapshot(), verified);
    node.go(&p, &[Cleanup]);
    lin::assert_failed(&node.step(Cutover, &p), Reason::OutOfOrder);
    lin::assert_failed(&node.step(Rollback, &p), Reason::Committed);
    let before = node.fake.snapshot();
    let again = node.step(Cleanup, &p);
    lin::assert_ok(&again);
    assert_eq!(
        lin::detail(&again),
        "nothing left to clean: the update is committed and finished"
    );
    let watched = node.step(Verify, &p);
    lin::assert_ok(&watched);
    assert!(!watched.report.reversible);
    assert!(lin::detail(&watched).starts_with("fs.tree_list_ram_bytes is 2147483648 in place; "));
    assert_eq!(node.fake.snapshot(), before);
}

/// Asserts that the steps of a reload refuse params of another package (`bad_params`), and that a
/// `rollback` of a package that was never recorded, over a reload at rest, has nothing to roll
/// back and sets nothing.
#[test]
fn a_reload_on_record_refuses_another_package() {
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.go(&p, &[Prepare]);
    let other = lin::params("/var/lib/hostkeeper/packages/tierstore-config/1.2.0", None);
    lin::assert_failed(&node.step(Cutover, &other), Reason::BadParams);
    node.go(&p, &[Cutover, Verify, Cleanup]);
    let back = node.step(Rollback, &other);
    lin::assert_ok(&back);
    assert!(lin::detail(&back).starts_with("nothing to roll back: the update to "));
    assert_eq!(node.control.asked(), [set(2 * GIB)]);
    assert!(node.step(Rollback, &other).report.reversible);
}

/// Asserts that a record of a reload that names no serving instance, or no mount once the cap was
/// set, is `state_invalid` for the steps that need them.
#[test]
fn a_reload_on_record_that_names_no_instance_or_no_mount_is_invalid() {
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    let mut record = node.fake.record();
    record.live = None;
    node.fake.set_record(&record);
    for step in [Cutover, Verify, Rollback, Cleanup] {
        let done = node.step(step, &p);
        if step == Cleanup {
            lin::assert_failed(&done, Reason::NotVerified);
        } else {
            lin::assert_failed(&done, Reason::StateInvalid);
        }
    }
    let node = changing(Some(GIB), Some(2 * GIB));
    node.go(&p, &[Prepare, Cutover]);
    let mut record = node.fake.record();
    record.reload = Some(changed(GIB, 2 * GIB));
    node.fake.set_record(&record);
    let done = node.step(Verify, &p);
    lin::assert_failed(&done, Reason::StateInvalid);
    assert_eq!(
        lin::detail(&done),
        "the record of the update in place names no mount whose cap was set"
    );
}

/// Asserts that `prepare` leaves a change it could apply in place to the arm when the node is not
/// one instance serving: with both instances running it defers as the arm does (`host_not_ready`),
/// and with an update of the arm in flight on the config hook's record it prepares a hand-over
/// over it, as the arm prepares again over an update with no marker.
#[test]
fn a_prepare_in_place_needs_one_instance_and_no_update_of_the_arm_in_flight() {
    let node = changing(Some(GIB), Some(2 * GIB));
    node.fake
        .set_unit(Instance::B, UnitState::Active, Some(lin::OLD), false);
    let done = node.step(Prepare, &update());
    lin::assert_deferred(&done, Reason::HostNotReady, 30);

    let node = changing(Some(GIB), Some(2 * GIB));
    node.fake.set_record(&Record {
        next: Some(Instance::B),
        mode: Some(Mode::Handover),
        ..in_flight(Some(Instance::A))
    });
    node.go(&update(), &[Prepare]);
    assert_eq!(node.fake.record().mode, Some(Mode::Handover));
    assert_eq!(node.fake.record().reload, None);
}

/// Asserts that `prepare` run again over a reload it recorded records it afresh, and that one over
/// a reload past its cutover is `out_of_order` and changes nothing.
#[test]
fn a_prepare_again_over_a_reload_on_record() {
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.go(&p, &[Prepare, Prepare]);
    assert_eq!(node.fake.record().reload, Some(changed(GIB, 2 * GIB)));
    node.go(&p, &[Cutover]);
    let before = node.fake.snapshot();
    let done = node.step(Prepare, &p);
    lin::assert_failed(&done, Reason::OutOfOrder);
    assert_eq!(node.fake.snapshot(), before);
}

/// Asserts that a config update after a reload at rest is the arm's, as after any other: a
/// staged file that needs another instance hands over from the record the reload left.
#[test]
fn a_hand_over_after_a_reload_at_rest_is_the_arms() {
    let node = changing(Some(GIB), Some(2 * GIB));
    node.go(&update(), &[Prepare, Cutover, Verify, Cleanup]);
    node.files.put(
        &staged_copy(
            Path::new("/var/lib/hostkeeper/packages/tierstore-config/1.2.0"),
            Path::new(lin::CONFIG),
        ),
        &fs_file("fetch_timeout = 9\n"),
    );
    let next = lin::params(
        "/var/lib/hostkeeper/packages/tierstore-config/1.2.0",
        Some(PKG_NEW),
    );
    node.go(&next, &[Prepare, Cutover]);
    assert_eq!(node.fake.actions(), ["markers.set b", "units.start b"]);
    assert_eq!(node.fake.record().reload, None);
}

/// Asserts that a reload records the fs instance of the old layout that runs, not the one an
/// older config record names (a release update hands that process over without touching the
/// config record), and writes it into the release's record; and that with both fs instances
/// running (one of them still drains) the change is left to the arm. Catches a stale `fs_live`
/// that the boot step would start.
#[test]
fn a_reload_names_the_fs_instance_that_runs() {
    let node = Linux::on(
        lin::SystemdFake::fs_installed(),
        &[],
        Some(lin::WITH_SOCKET),
        lin::WITH_SOCKET,
    );
    node.fake
        .set_fs_unit(Instance::A, UnitState::Inactive, Some(lin::OLD));
    node.fake
        .set_fs_unit(Instance::B, UnitState::Active, Some(lin::OLD));
    node.fake.set_record(&Record {
        fs_live: Some(Instance::A),
        ..Record::idle(Some(Instance::A))
    });
    let p = update();
    node.go(&p, &[Prepare]);
    assert_eq!(node.fake.record().fs_live, Some(Instance::B));
    node.go(&p, &[Cutover, Verify, Cleanup]);
    assert_eq!(
        node.service.get().and_then(|record| record.fs_live),
        Some(Instance::B)
    );

    let node = Linux::on(
        lin::SystemdFake::fs_installed(),
        &[],
        Some(lin::WITH_SOCKET),
        lin::WITH_SOCKET,
    );
    node.fake
        .set_fs_unit(Instance::B, UnitState::Active, Some(lin::OLD));
    let _ = node.step(Prepare, &p);
    assert_eq!(node.fake.record().reload, None);
}

/// Asserts that a change applied in place is not refused for what would need a new process: a
/// node with an `fs` interface and a hot tier of kind `dir`, which the arm can only restart (and
/// so refuses, the mount held), takes a change of the cap alone in place. Catches the refusals of
/// a restart applied to a change that moves nothing.
#[test]
fn a_node_that_could_only_restart_takes_a_cap_change_in_place() {
    let file = |bytes: u64| {
        format!(
            "{}\n[[interface]]\ntype = \"fs\"\n\n[interface.params]\n{}",
            lin::DIR_HOT,
            cap(bytes)
        )
    };
    let node = Linux::new(Some(&file(GIB)), &file(2 * GIB));
    node.go(&update(), &[Prepare, Cutover]);
    assert_eq!(node.control.asked(), [set(2 * GIB)]);
    assert_eq!(node.fake.actions(), Vec::<String>::new());

    let node = Linux::new(
        Some(&file(GIB)),
        &format!("{}proxy_id = \"p2\"\n", file(GIB)),
    );
    lin::assert_failed(&node.step(Prepare, &update()), Reason::ModeUnsupported);
}

/// Asserts that params that declare a restart (`mode = "restart"`) restart the node even for a
/// byte-identical file: an operator who asks for a restart gets one.
#[test]
fn a_declared_restart_is_a_restart_even_of_an_identical_file() {
    let node = Linux::new(Some(lin::WITH_SOCKET), lin::WITH_SOCKET);
    let p = lin::params_with(
        PKG_NEW,
        Some(super::PKG_OLD),
        &format!("config_file = \"{}\"\nmode = \"restart\"\n", lin::CONFIG),
    );
    node.go(&p, &[Prepare]);
    assert_eq!(node.fake.record().mode, Some(Mode::Restart));
    assert_eq!(node.fake.record().reload, None);
}

/// Asserts that a `prepare` run again after hostkeeper installed the staged file (the file that
/// serves now reads as the staged one) keeps the reload it recorded, the change of the cap with
/// it, so that `cutover` still sets it. Catches a change lost to a `prepare` run again.
#[test]
fn a_prepare_again_after_the_file_was_installed_keeps_the_change() {
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.go(&p, &[Prepare]);
    node.files
        .put(Path::new(lin::CONFIG), &fs_file(&cap(2 * GIB)));
    let again = node.step(Prepare, &p);
    lin::assert_ok(&again);
    assert_eq!(
        lin::detail(&again),
        "prepared in place already: tierstore@a applies fs.tree_list_ram_bytes 1073741824 to \
         2147483648; no other instance starts"
    );
    assert_eq!(node.fake.record().reload, Some(changed(GIB, 2 * GIB)));
    node.go(&p, &[Cutover]);
    assert_eq!(node.control.asked(), [set(2 * GIB)]);
}

/// Asserts that `cleanup` confirms afresh before the commit: an instance that is no longer
/// active, or that does not answer with the build, fails `not_confirmed` and commits nothing.
#[test]
fn a_cleanup_confirms_the_instance_still_serves() {
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.go(&p, &[Prepare, Cutover, Verify]);
    node.fake
        .set_unit(Instance::A, UnitState::Inactive, Some(lin::OLD), false);
    let done = node.step(Cleanup, &p);
    lin::assert_failed(&done, Reason::NotConfirmed);
    assert_eq!(lin::detail(&done), "tierstore@a is Inactive, not active");
    node.fake
        .set_unit(Instance::A, UnitState::Active, Some(lin::OLD), false);
    let done = node.step(Cleanup, &p);
    lin::assert_failed(&done, Reason::NotConfirmed);
    assert!(
        lin::detail(&done).contains("was not seen serving"),
        "{}",
        lin::detail(&done)
    );
    assert!(!node.fake.record().committed());
    assert!(done.report.reversible);
}

/// Asserts that only the first `fs` interface's cap is applied in place, the one whose control
/// socket the hook asks: a change of the cap of a second `fs` entry is a change like any other,
/// and hands over.
#[test]
fn a_cap_of_a_second_fs_entry_is_not_applied_in_place() {
    let two = |bytes: u64| {
        format!(
            "{}\n[[interface]]\ntype = \"fs\"\n\n[[interface]]\ntype = \"fs\"\n\n[interface.params]\n{}",
            lin::WITH_SOCKET,
            cap(bytes)
        )
    };
    let node = Linux::new(Some(&two(GIB)), &two(2 * GIB));
    node.go(&update(), &[Prepare]);
    assert_eq!(node.fake.record().reload, None);
}

/// Asserts that a package that changes only the guard's file reloads the guard at `cutover` and
/// again at the `rollback` of a package that has a previous one, and asks the control socket
/// nothing.
#[test]
fn a_change_of_the_guard_alone_reloads_it_both_ways() {
    let node = Linux::new(Some(lin::WITH_SOCKET), lin::WITH_SOCKET);
    node.files.put(Path::new(GUARD_FILE), "old rules");
    node.files.put(
        &staged_copy(Path::new(PKG_NEW), Path::new(GUARD_FILE)),
        "new rules",
    );
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    assert_eq!(node.reloads(), 1);
    node.go(&p, &[Rollback]);
    assert_eq!(node.reloads(), 2);
    assert_eq!(node.control.asked(), []);
}

/// Asserts that a `verify` whose instance answers with another build fails `wrong_build`, as the
/// arm's does, and is not deferred.
#[test]
fn a_verify_of_another_build_fails() {
    let node = changing(Some(GIB), Some(2 * GIB));
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    node.fake.lie(lin::OLD, "9.9.9+0123456789ab");
    lin::assert_failed(&node.step(Verify, &p), Reason::WrongBuild);
    assert!(!node.fake.record().has(Marker::Verified));
}

/// Asserts that a `prepare` over a reload in flight that is not this package's (another staged
/// release, another build, or one that names no serving instance) records this one afresh.
#[test]
fn a_prepare_over_another_reload_in_flight_records_its_own() {
    for record in [
        Record {
            to: Some(PathBuf::from(
                "/var/lib/hostkeeper/packages/tierstore-config/1.2.0",
            )),
            ..in_flight(Some(Instance::A))
        },
        Record {
            version: Some("9.9.9".to_owned()),
            ..in_flight(Some(Instance::A))
        },
        in_flight(None),
    ] {
        let node = changing(Some(GIB), Some(2 * GIB));
        node.fake.set_record(&Record {
            reload: Some(Reload::default()),
            ..record
        });
        let done = node.step(Prepare, &update());
        lin::assert_ok(&done);
        assert!(lin::detail(&done).starts_with("prepared in place: "));
        let record = node.fake.record();
        assert_eq!(record.to, Some(PathBuf::from(PKG_NEW)));
        assert_eq!(record.version, Some(build()));
        assert_eq!(record.live, Some(Instance::A));
        assert_eq!(record.reload, Some(changed(GIB, 2 * GIB)));
    }
}
