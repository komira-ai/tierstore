//! The update of a node that holds a mount (its file has an `fs` interface): a hand-over when
//! both releases list `handoff-service-v1`, and otherwise a refusal in `prepare`, which
//! `mode = "restart"` overrides.

use crate::server::update_hook::{
    CACHE_HANDOFF_CAPABILITY, HANDOFF_CAPABILITY, HANDOFF_SERVICE_CAPABILITY,
    HANDOFF_STATE_DIR_CAPABILITY, Instance, Mode, Reason, Record, Step,
};

use super::fake::{
    CONFIG, DIR_HOT, NEW, OLD, SystemdFake, WITH_SOCKET, assert_failed, assert_ok, detail, params,
    params_with,
};

use Step::{Cleanup, Cutover, Prepare, Verify};

/// A configuration that names the hand-over socket and has an `fs` interface.
const FS_CONFIG: &str = "[server]\nhandoff_socket = \"/var/lib/tierstore/handoff.sock\"\n\n[[interface]]\ntype = \"fs\"\n";

/// The words of a release that can hand a mount over.
const WITH_SERVICE: [&str; 4] = [
    HANDOFF_CAPABILITY,
    CACHE_HANDOFF_CAPABILITY,
    HANDOFF_STATE_DIR_CAPABILITY,
    HANDOFF_SERVICE_CAPABILITY,
];

/// The words of a release before the one service.
const WITHOUT_SERVICE: [&str; 3] = [
    HANDOFF_CAPABILITY,
    CACHE_HANDOFF_CAPABILITY,
    HANDOFF_STATE_DIR_CAPABILITY,
];

/// A host with a mount on its node file, where `staged` and `previous` list the given words.
fn node(staged: &[&str], previous: &[&str]) -> SystemdFake {
    let fake = SystemdFake::installed();
    fake.set_config(Some(FS_CONFIG));
    fake.set_capabilities(NEW, staged);
    fake.set_capabilities(OLD, previous);
    fake
}

/// Asserts that `prepare` of an update of a node with a mount is refused with `mode_unsupported`
/// when the staged release, the previous release or both lack `handoff-service-v1`: the record is
/// as it was, no unit is started or stopped, and the detail names the release that lacks the word
/// and says the mount would be dropped. Catches an update that restarts a node under running jobs
/// and drops its mount.
#[test]
fn an_update_without_handoff_service_v1_in_both_is_refused_for_a_node_with_a_mount() {
    let cases = [
        (&WITHOUT_SERVICE[..], &WITH_SERVICE[..], vec![NEW]),
        (&WITH_SERVICE[..], &WITHOUT_SERVICE[..], vec![OLD]),
        (&WITHOUT_SERVICE[..], &WITHOUT_SERVICE[..], vec![NEW, OLD]),
    ];
    for (staged, previous, lacking) in cases {
        let fake = node(staged, previous);
        let done = fake.run(Prepare, &params(NEW, Some(OLD)));
        assert_failed(&done, Reason::ModeUnsupported);
        let text = detail(&done);
        for release in [NEW, OLD] {
            let named = text.contains(&format!(
                "release {release} lacks {HANDOFF_SERVICE_CAPABILITY}"
            ));
            assert_eq!(named, lacking.contains(&release), "{release}: {text}");
        }
        assert!(
            text.contains("would drop the mount under running jobs"),
            "{text}"
        );
        assert!(text.contains("mode = \"restart\""), "{text}");
        assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
        assert!(fake.actions().is_empty(), "{:?}", fake.actions());
    }
}

/// Asserts that with `mode = "restart"` in the params the same update is accepted: `Mode::Restart`
/// is recorded, and `cutover` stops the live instance, then starts the next. Catches an override
/// that does not reach the operator who has drained the node.
#[test]
fn mode_restart_overrides_the_refusal() {
    let fake = node(&WITHOUT_SERVICE, &WITHOUT_SERVICE);
    let p = params_with(
        NEW,
        Some(OLD),
        &format!("config_file = \"{CONFIG}\"\nmode = \"restart\"\n"),
    );
    fake.drive(&p, &[Prepare]);
    assert_eq!(fake.record().mode, Some(Mode::Restart));
    fake.drive(&p, &[Cutover]);
    assert_eq!(
        fake.actions(),
        ["units.stop a", "markers.set b", "units.start b"]
    );
}

/// Asserts that when both releases list `handoff-service-v1` the update of a node with a mount is
/// a hand-over: `Mode::Handover` is recorded and the update completes. Catches a mount that is
/// refused although both releases can hand it over.
#[test]
fn both_releases_with_handoff_service_v1_hand_the_mount_over() {
    let fake = node(&WITH_SERVICE, &WITH_SERVICE);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    assert_eq!(fake.record().mode, Some(Mode::Handover));
    fake.drive(&p, &[Cutover, Verify, Cleanup]);
    assert_eq!(fake.running(), [Instance::B]);
}

/// Asserts that a node whose file has no `fs` interface is decided as before: a release that
/// lacks `handoff-service-v1` does not stop a hand-over, and one that lacks `handoff-socket-v1`
/// makes a restart that is recorded, not refused. Catches a refusal that reaches nodes with no
/// mount.
#[test]
fn a_node_without_an_fs_interface_is_decided_as_before() {
    let fake = SystemdFake::installed();
    fake.set_config(Some(WITH_SOCKET));
    fake.set_capabilities(NEW, &WITHOUT_SERVICE);
    fake.set_capabilities(OLD, &WITHOUT_SERVICE);
    assert_ok(&fake.run(Prepare, &params(NEW, Some(OLD))));
    assert_eq!(fake.record().mode, Some(Mode::Handover));

    let fake = SystemdFake::installed();
    fake.set_config(Some(
        "[server]\nhandoff_socket = \"/var/lib/tierstore/handoff.sock\"\n\n[[interface]]\ntype = \"s3\"\n",
    ));
    fake.set_capabilities(OLD, &[]);
    assert_ok(&fake.run(Prepare, &params(NEW, Some(OLD))));
    assert_eq!(fake.record().mode, Some(Mode::Restart));
}

/// Asserts that a first install of a node with an `fs` interface proceeds with releases that lack
/// `handoff-service-v1`, and records no mode. Catches a refusal of an install that has nothing to
/// hand over.
#[test]
fn a_first_install_of_a_node_with_a_mount_is_not_refused() {
    let fake = SystemdFake::empty();
    fake.set_config(Some(FS_CONFIG));
    fake.set_capabilities(NEW, &WITHOUT_SERVICE);
    assert_ok(&fake.run(Prepare, &params(NEW, None)));
    assert_eq!(fake.record().mode, None);
}

/// Asserts that a directory hot tier on a node with an `fs` interface is refused too, naming the
/// hot tier, and that nothing is recorded. Catches a restart of a node with a mount that two
/// instances cannot share.
#[test]
fn a_dir_hot_tier_with_an_fs_interface_is_refused() {
    let fake = SystemdFake::installed();
    fake.set_config(Some(&format!("{DIR_HOT}\n[[interface]]\ntype = \"fs\"\n")));
    let done = fake.run(Prepare, &params(NEW, Some(OLD)));
    assert_failed(&done, Reason::ModeUnsupported);
    assert!(
        detail(&done).contains("hot tier is a directory"),
        "{done:?}"
    );
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
    assert!(fake.actions().is_empty());
}

/// Asserts that the boot step, over a record of a switch to one process that has swapped (the one
/// process holds the mount, and the record still names the old fs instance as the one that served
/// before), starts no fs instance and lays no fs marker, while it does start the store's instance;
/// and that before the swap it starts the old fs instance, which still holds the mount. Catches a
/// boot step that starts the old fs instance after the switch, which takes the mount back from the
/// one process.
#[test]
fn the_boot_step_starts_no_fs_instance_once_a_switch_has_swapped() {
    use crate::server::update_hook::Marker;
    let fake = SystemdFake::fs_installed();
    let mut record = Record::idle(Some(Instance::A));
    record.updating = true;
    record.next = Some(Instance::B);
    record.mode = Some(Mode::Handover);
    record.fs_live = Some(Instance::A);
    record.fs_switch = true;
    record.from = Some(OLD.into());
    record.to = Some(NEW.into());
    record.version = Some("0.4.0".to_owned());
    fake.set_record(&record.with(Marker::Swapped));
    fake.reboot();
    let before = fake.log().len();
    assert_ok(&fake.run(Step::Restore, &params(NEW, Some(OLD))));
    let fs_acts = |fake: &SystemdFake, from: usize| -> Vec<String> {
        fake.log()[from..]
            .iter()
            .filter(|line| line.starts_with("fs_units.start") || line.starts_with("fs_markers.set"))
            .cloned()
            .collect()
    };
    assert!(fs_acts(&fake, before).is_empty(), "{:?}", fake.log());
    assert_eq!(fake.running(), [Instance::B]);

    let fake = SystemdFake::fs_installed();
    fake.set_record(&record);
    fake.reboot();
    let before = fake.log().len();
    assert_ok(&fake.run(Step::Restore, &params(NEW, Some(OLD))));
    assert_eq!(
        fs_acts(&fake, before),
        ["fs_markers.set a", "fs_units.start a"]
    );
}
