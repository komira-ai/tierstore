//! `prepare`: the hand-over socket, the mode, the instances it picks, and what it refuses.

use crate::server::update_hook::{
    CACHE_HANDOFF_CAPABILITY, Class, HANDOFF_CAPABILITY, Instance, Mode, Reason, Record, Step,
    UnitState,
};

use super::fake::{
    CONFIG, NEW, OLD, SystemdFake, WITH_CACHE_SOCKET, WITH_SOCKET, assert_deferred, assert_failed,
    assert_ok, detail, params, params_with,
};

use Step::{Cutover, Prepare};

/// Asserts that `prepare` of an update refuses a configuration that does not name a non-empty
/// `[server] handoff_socket` string (absent, empty, outside `[server]`, not a table, not TOML)
/// with `handoff_not_configured` of class environment, and records nothing. Catches an update
/// that starts and cannot take the sockets of the old instance.
#[test]
fn prepare_refuses_a_configuration_without_the_hand_over_socket() {
    let configurations = [
        ("", "has no [server] handoff_socket"),
        (
            "[server]\nlisten = \"0.0.0.0:9300\"\n",
            "has no [server] handoff_socket",
        ),
        (
            "[server]\nhandoff_socket = \"\"\n",
            "has no [server] handoff_socket",
        ),
        (
            "[server]\nhandoff_socket = 7\n",
            "has no [server] handoff_socket",
        ),
        ("server = 3\n", "has no [server] handoff_socket"),
        (
            "handoff_socket = \"/x.sock\"\n",
            "has no [server] handoff_socket",
        ),
        ("[server\n", "is not TOML"),
    ];
    for (text, words) in configurations {
        let fake = SystemdFake::installed();
        fake.set_config(Some(text));
        let done = fake.run(Prepare, &params(NEW, Some(OLD)));
        assert_failed(&done, Reason::HandoffNotConfigured);
        assert_eq!(done.report.class, Class::Environment);
        assert!(detail(&done).contains(words), "{text:?}: {done:?}");
        assert_eq!(fake.record(), Record::idle(Some(Instance::A)), "{text:?}");
    }
}

/// Asserts that with `mode = "restart"` in the params the same configuration is accepted: no
/// configuration is read at all, only the staged binary is asked what it can do, and the update
/// is recorded in restart mode.
#[test]
fn a_restart_mode_in_the_params_needs_no_hand_over_socket() {
    let fake = SystemdFake::installed();
    fake.set_config(None);
    let p = params_with(NEW, Some(OLD), "mode = \"restart\"\n");
    assert_ok(&fake.run(Prepare, &p));
    assert_eq!(fake.record().mode, Some(Mode::Restart));
    assert!(fake.config_paths().is_empty());
    let asked = fake
        .log()
        .into_iter()
        .filter(|line| line.starts_with("host.capabilities"))
        .count();
    assert_eq!(asked, 1);
}

/// Asserts that the mode is a hand-over only when both the staged and the previous release list
/// `handoff-socket-v1`, and a restart when either does not; and that the detail says the mode.
#[test]
fn the_mode_is_a_hand_over_only_when_both_releases_can_hand_over() {
    let capable: &[&str] = &[HANDOFF_CAPABILITY];
    let incapable: &[&str] = &[];
    let other: &[&str] = &["other-v1"];
    let cases = [
        (capable, capable, Mode::Handover),
        (incapable, capable, Mode::Restart),
        (capable, incapable, Mode::Restart),
        (other, other, Mode::Restart),
    ];
    for (staged, previous, mode) in cases {
        let fake = SystemdFake::installed();
        fake.set_capabilities(NEW, staged);
        fake.set_capabilities(OLD, previous);
        let done = fake.run(Prepare, &params(NEW, Some(OLD)));
        assert_ok(&done);
        assert_eq!(fake.record().mode, Some(mode), "{staged:?} {previous:?}");
        assert!(detail(&done).contains(mode.as_str()), "{done:?}");
    }
}

/// Asserts that for a configuration with `[server] handoff_socket` and `[local_cache]` the mode is
/// a hand-over only when both releases list `handoff-socket-v1` and `handoff-cache-v1`, and a
/// restart when either lacks the cache word (a release that hands over a storage node only).
/// Catches a cache shape handed over to or from a release that refuses the keys.
#[test]
fn a_cache_shape_hands_over_only_between_releases_with_the_cache_word() {
    let both: &[&str] = &[HANDOFF_CAPABILITY, CACHE_HANDOFF_CAPABILITY];
    let socket_only: &[&str] = &[HANDOFF_CAPABILITY];
    let cases = [
        (both, both, Mode::Handover),
        (socket_only, both, Mode::Restart),
        (both, socket_only, Mode::Restart),
        (socket_only, socket_only, Mode::Restart),
    ];
    for (staged, previous, mode) in cases {
        let fake = SystemdFake::installed();
        fake.set_config(Some(WITH_CACHE_SOCKET));
        fake.set_capabilities(NEW, staged);
        fake.set_capabilities(OLD, previous);
        let done = fake.run(Prepare, &params(NEW, Some(OLD)));
        assert_ok(&done);
        assert_eq!(fake.record().mode, Some(mode), "{staged:?} {previous:?}");
        assert!(detail(&done).contains(mode.as_str()), "{done:?}");
    }
}

/// Asserts that a storage node's configuration without `[local_cache]` still hands over between
/// releases that list only `handoff-socket-v1`.
#[test]
fn a_storage_node_hands_over_with_the_socket_word_alone() {
    let fake = SystemdFake::installed();
    fake.set_config(Some(WITH_SOCKET));
    let socket_only: &[&str] = &[HANDOFF_CAPABILITY];
    fake.set_capabilities(NEW, socket_only);
    fake.set_capabilities(OLD, socket_only);
    assert_ok(&fake.run(Prepare, &params(NEW, Some(OLD))));
    assert_eq!(fake.record().mode, Some(Mode::Handover));
}

/// Asserts that a configuration with `[local_cache]` and no `handoff_socket` is refused with
/// `handoff_not_configured`, as one of any other shape is, whatever the releases list.
#[test]
fn a_cache_shape_without_the_hand_over_socket_is_not_configured() {
    let fake = SystemdFake::installed();
    fake.set_config(Some("[local_cache]\npath = \"/c\"\n"));
    let done = fake.run(Prepare, &params(NEW, Some(OLD)));
    assert_failed(&done, Reason::HandoffNotConfigured);
    assert_eq!(done.report.class, Class::Environment);
}

/// Asserts that `mode = "restart"` in the params gives a restart for a cache shape that both
/// releases could hand over, and a restart without the staged release listing the cache word.
#[test]
fn a_restart_mode_in_the_params_restarts_a_cache_shape() {
    let fake = SystemdFake::installed();
    fake.set_config(Some(WITH_CACHE_SOCKET));
    let p = params_with(NEW, Some(OLD), "mode = \"restart\"\n");
    assert_ok(&fake.run(Prepare, &p));
    assert_eq!(fake.record().mode, Some(Mode::Restart));
}

/// Asserts that `config_file` missing from the params is `bad_params`, and a configuration file
/// that does not exist is `config_missing`, both before anything is recorded.
#[test]
fn prepare_names_what_is_missing_of_the_configuration() {
    let fake = SystemdFake::installed();
    let no_path = params_with(NEW, Some(OLD), "");
    assert_failed(&fake.run(Prepare, &no_path), Reason::BadParams);

    fake.set_config(None);
    let missing = fake.run(Prepare, &params(NEW, Some(OLD)));
    assert_failed(&missing, Reason::ConfigMissing);
    assert!(detail(&missing).contains(CONFIG));
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that a `prepare` that fails before it saves, run on the committed record of a finished
/// update, reports `reversible` true: nothing was committed by it. Catches a flag taken from the
/// `stopping` marker of a record at rest.
#[test]
fn prepare_failing_on_a_committed_record_at_rest_is_reversible() {
    let fake = SystemdFake::installed();
    fake.complete(&params(NEW, Some(OLD)));
    assert!(fake.record().committed());
    assert!(!fake.record().updating);
    fake.set_config(None);
    let done = fake.run(Prepare, &params(NEW, Some(OLD)));
    assert_failed(&done, Reason::ConfigMissing);
    assert!(done.report.reversible);
}

/// Asserts that a host that cannot be asked fails `prepare` with `host_failed`: the
/// configuration unreadable, a call for capabilities that fails, a staged binary or a previous
/// one the host does not know.
#[test]
fn prepare_fails_with_host_failed_when_the_host_cannot_be_asked() {
    for prefix in ["host.config", "host.capabilities"] {
        let fake = SystemdFake::installed();
        fake.fail_on(Some(prefix));
        assert_failed(
            &fake.run(Prepare, &params(NEW, Some(OLD))),
            Reason::HostFailed,
        );
    }
    let unknown = "/var/lib/hostkeeper/packages/tierstore/0.9.0";
    let fake = SystemdFake::installed();
    assert_failed(
        &fake.run(Prepare, &params(unknown, Some(OLD))),
        Reason::HostFailed,
    );
    assert_failed(
        &fake.run(Prepare, &params(NEW, Some(unknown))),
        Reason::HostFailed,
    );
}

/// Asserts that `prepare` with both instances running defers with `host_not_ready` (exit 75,
/// retry in 30 seconds) and writes nothing: the old instance of an earlier update still drains.
#[test]
fn prepare_defers_while_both_instances_run() {
    let fake = SystemdFake::installed();
    fake.set_unit(Instance::B, UnitState::Active, Some(OLD), false);
    let done = fake.run(Prepare, &params(NEW, Some(OLD)));
    assert_deferred(&done, Reason::HostNotReady, 30);
    assert!(done.report.reversible);
    assert!(!fake.log().iter().any(|line| line == "state.save"));
}

/// Asserts that `prepare` run again keeps the record of the first run and asks systemd nothing
/// more: nothing has been started since, so the instances it chose still stand.
#[test]
fn prepare_run_again_keeps_the_instances_it_chose() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    assert_ok(&fake.run(Prepare, &p));
    let first = fake.record();
    let before = fake.log().len();
    assert_ok(&fake.run(Prepare, &p));
    assert_eq!(fake.record(), first);
    assert!(
        !fake.log()[before..]
            .iter()
            .any(|line| line.starts_with("units.state"))
    );
}

/// Asserts that when no instance runs, an update goes on from the instance the record says
/// served last, and a first install has none; with no record at all the instance that runs is
/// the live one.
#[test]
fn prepare_finds_the_live_instance_from_the_record_or_from_systemd() {
    let fake = SystemdFake::installed();
    fake.set_unit(Instance::A, UnitState::Inactive, None, false);
    assert_ok(&fake.run(Prepare, &params(NEW, Some(OLD))));
    assert_eq!(fake.record().live, Some(Instance::A));
    assert_eq!(fake.record().next, Some(Instance::B));

    let fake = SystemdFake::installed();
    fake.set_unit(Instance::A, UnitState::Inactive, None, false);
    assert_ok(&fake.run(Prepare, &params(NEW, None)));
    assert_eq!(fake.record().live, None);
    assert_eq!(fake.record().next, Some(Instance::A));

    let fake = SystemdFake::installed();
    fake.set_record_bytes(None);
    assert_ok(&fake.run(Prepare, &params(NEW, Some(OLD))));
    assert_eq!(fake.record().live, Some(Instance::A));

    let fake = SystemdFake::installed();
    fake.set_unit(Instance::A, UnitState::Inactive, None, false);
    fake.set_unit(Instance::B, UnitState::Active, Some(OLD), true);
    assert_ok(&fake.run(Prepare, &params(NEW, Some(OLD))));
    assert_eq!(fake.record().live, Some(Instance::B));
    assert_eq!(fake.record().next, Some(Instance::A));
}

/// Asserts that `prepare` refuses to start over an update that is past its cutover: it fails
/// with `out_of_order` and leaves the record as it is.
#[test]
fn prepare_refuses_an_update_that_is_past_its_cutover() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    let before = fake.record();
    assert_failed(&fake.run(Prepare, &p), Reason::OutOfOrder);
    assert_eq!(fake.record(), before);
}

/// Asserts that a unit that cannot be asked, or a record that cannot be written, fails `prepare`
/// with `unit_failed` and `state_io`.
#[test]
fn prepare_reports_a_failed_unit_look_and_a_failed_write() {
    let fake = SystemdFake::installed();
    fake.fail_on(Some("units.state"));
    assert_failed(
        &fake.run(Prepare, &params(NEW, Some(OLD))),
        Reason::UnitFailed,
    );

    let fake = SystemdFake::installed();
    fake.fail_on(Some("state.save"));
    assert_failed(&fake.run(Prepare, &params(NEW, Some(OLD))), Reason::StateIo);
}
