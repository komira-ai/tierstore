//! The update of a farm node, on each platform arm, fed the farm's committed files as they are
//! (`live_configs`): the update of a release by hand-over, and the config hook's switch of a node
//! from two processes to one, over the fake systemd and the fake launchd. Each ends with the node
//! serving its mount, or fails and rolls back to the node that served before.
//!
//! The hook reads paths, not files under them, so the files are read unmodified: the tests here
//! put the committed text at the path the hook reads, and nothing is moved onto a scratch
//! directory.

use std::path::{Path, PathBuf};

use super::super::super::launchd_tests::fake as mac;
use super::super::super::launchd_tests::fake::{FS_LABEL, LABEL};
use super::super::super::systemd_tests::fake::{NEW, OLD, SystemdFake, params, version_of};
use super::super::super::{
    CACHE_HANDOFF_CAPABILITY, FS_CONFIG_FILE, FS_LAUNCHD_PLIST, HANDOFF_CAPABILITY,
    HANDOFF_EXEC_CAPABILITY, HANDOFF_SERVICE_CAPABILITY, HANDOFF_STATE_DIR_CAPABILITY, Instance,
    Instances, LAUNCHD_PLIST, Marker, Mode, Reason, Record, Step, UnitState,
};
use super::macos::{Mac, mac_update};
use super::{Linux, lin, staged, update};
use crate::server::build_info::gauge;
use crate::server::config_testing::live_configs::{
    LINUX, MAC, MAC_BEFORE_SWITCH, MAC_FS, MAC_MOUNT, swap,
};
use crate::server::update_hook::launchd::fs::mountpoint_of;
use crate::server::update_hook::shape::fs_interface;
use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// The words a release lists that hands over a node whose file is the Linux farm's: the store's
/// three, and the one process's (the file has an `fs` interface).
const LINUX_WORDS: [&str; 4] = [
    HANDOFF_CAPABILITY,
    CACHE_HANDOFF_CAPABILITY,
    HANDOFF_STATE_DIR_CAPABILITY,
    HANDOFF_SERVICE_CAPABILITY,
];

/// A build the fakes do not run.
const OTHER_BUILD: &str = "9.9.9+0123456789ab";

/// The Linux farm's file with its `fs` interface cut out: the file of a node that serves its
/// mount from a process of its own (`tierstore-fs`), which the switch moves into the one process.
fn linux_without_fs() -> String {
    let start = LINUX
        .find("[[interface]]\ntype = \"fs\"\n")
        .expect("the Linux farm file has an fs interface");
    let end = LINUX
        .find("[server]\n")
        .expect("the Linux farm file has a [server] table");
    let block = &LINUX[start..end];
    assert!(block.contains("[interface.params.put]"), "{block}");
    let text = swap(LINUX, block, "", 1);
    assert!(fs_interface(LINUX).is_some());
    assert!(fs_interface(&text).is_none());
    assert!(text.contains("type   = \"mount-provider\""));
    text
}

/// A Linux node that serves the farm's file from instance `a`, which both of its releases can hand
/// over.
fn linux_node() -> SystemdFake {
    let fake = SystemdFake::installed();
    fake.set_config(Some(LINUX));
    fake.set_capabilities(OLD, &LINUX_WORDS);
    fake.set_capabilities(NEW, &LINUX_WORDS);
    fake
}

/// What the node answers at `path` of its public port.
fn answer(fake: &SystemdFake, path: &str) -> (u16, String) {
    let reply = fake.get(9300, path).expect("the node answers");
    (reply.status, reply.body)
}

/// Asserts that the node serves `release` and says it is ready to write.
fn assert_serving(fake: &SystemdFake, release: &str) {
    assert_eq!(
        answer(fake, "/readyz"),
        (200, "ready read-write".to_owned())
    );
    let version = version_of(release);
    let (crate_version, commit) = version.split_once('+').expect("a version with a commit");
    assert_eq!(
        answer(fake, "/metricz"),
        (200, gauge(crate_version, commit, None))
    );
}

/// Asserts that a release update of a Linux node on the farm's file (S3, the mount provider, the
/// `fs` interface and a local cache) is a hand-over: `prepare` has both releases check the file as
/// committed and records the hand-over, starts nothing; `cutover` starts the other instance, which
/// takes the sockets while the old one drains; `verify` and `cleanup` commit it; and the node ends
/// with the new build alone serving, ready to write. Catches an update hook that refuses or
/// mis-reads the farm's file, or one that commits an update that leaves nothing serving the node.
#[test]
fn a_release_update_of_the_farms_linux_node_ends_with_the_new_build_serving() {
    let fake = linux_node();
    let p = params(NEW, Some(OLD));
    assert_serving(&fake, OLD);

    fake.drive(&p, &[Prepare]);
    let prepared = fake.record();
    assert_eq!(prepared.mode, Some(Mode::Handover));
    assert_eq!(prepared.next, Some(Instance::B));
    assert!(!prepared.fs_switch);
    assert!(fake.actions().is_empty());
    assert_eq!(
        fake.config_checks(),
        [
            (NEW.to_owned(), lin::CONFIG.to_owned()),
            (OLD.to_owned(), lin::CONFIG.to_owned())
        ]
    );

    fake.drive(&p, &[Cutover]);
    assert_eq!(fake.actions(), ["markers.set b", "units.start b"]);
    assert_serving(&fake, NEW);
    fake.drive(&p, &[Verify, Cleanup]);

    let end = fake.record();
    assert!(!end.updating && end.committed());
    assert_eq!(end.live, Some(Instance::B));
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.markers(), [Instance::B]);
    assert_serving(&fake, NEW);
}

/// Asserts that a release update of the farm's Linux node whose new build answers as another build
/// fails `verify` with wrong_build and is rolled back to the old release on its own letter, which
/// serves alone with nothing in flight. Catches an update that goes on to the commit with a node
/// that serves another build, and a rollback that leaves the new instance.
#[test]
fn a_release_update_of_the_farms_linux_node_whose_new_build_answers_wrong_is_rolled_back() {
    let fake = linux_node();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.lie(NEW, OTHER_BUILD);
    let verify = fake.run(Verify, &p);
    lin::assert_failed(&verify, Reason::WrongBuild);
    assert!(verify.report.reversible);

    lin::assert_ok(&fake.run_as_hostkeeper(Rollback, &p));
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.markers(), [Instance::A]);
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
    assert_serving(&fake, OLD);
}

/// Asserts that a release update of the farm's Linux node whose new instance does not start fails
/// `cutover` with unit_failed, still reversible, and is rolled back: the old release serves alone
/// and nothing is in flight. Catches a cutover that reports success for a unit that failed, and a
/// rollback that cannot start from what a failed cutover leaves.
#[test]
fn a_release_update_of_the_farms_linux_node_whose_new_unit_does_not_start_is_rolled_back() {
    let fake = linux_node();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    fake.fail_on(Some("units.start b"));
    let cut = fake.run(Cutover, &p);
    lin::assert_failed(&cut, Reason::UnitFailed);
    assert!(cut.report.reversible);
    fake.fail_on(None);
    assert_serving(&fake, OLD);

    lin::assert_ok(&fake.run_as_hostkeeper(Rollback, &p));
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.markers(), [Instance::A]);
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
    assert_serving(&fake, OLD);
}

/// Asserts that a release update of the farm's Linux node, whose file has an `fs` interface, is
/// refused with mode_unsupported by `prepare` when the staged or the previous release lacks
/// `handoff-service-v1`, with nothing recorded or started (a restart would drop the mount under
/// the jobs); and that `mode = "restart"` in the params lets it through as a restart. Catches an
/// update that restarts the process that serves the node's mount.
#[test]
fn a_release_update_of_the_farms_linux_node_needs_handoff_service_v1_in_both_releases() {
    let words_without: Vec<&str> = LINUX_WORDS
        .into_iter()
        .filter(|word| *word != HANDOFF_SERVICE_CAPABILITY)
        .collect();
    for lacking in [NEW, OLD] {
        let fake = linux_node();
        fake.set_capabilities(lacking, &words_without);
        let p = params(NEW, Some(OLD));
        let done = fake.run(Prepare, &p);
        lin::assert_failed(&done, Reason::ModeUnsupported);
        assert!(
            lin::detail(&done).contains(&format!("release {lacking}")),
            "{}",
            lin::detail(&done)
        );
        assert!(lin::detail(&done).contains(HANDOFF_SERVICE_CAPABILITY));
        assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
        assert!(fake.actions().is_empty());

        let declared = lin::params_with(
            NEW,
            Some(OLD),
            &format!("config_file = \"{}\"\nmode = \"restart\"\n", lin::CONFIG),
        );
        lin::assert_ok(&fake.run(Prepare, &declared));
        assert_eq!(fake.record().mode, Some(Mode::Restart));
    }
}

/// A Linux node of two processes: the farm's file without its `fs` interface serves, the
/// `tierstore-fs` instance `a` holds the mount, and the config package stages the farm's file.
fn linux_switching() -> Linux {
    let serving = linux_without_fs();
    let fake = SystemdFake::fs_installed();
    fake.set_config(Some(&serving));
    Linux::on(fake, &[], Some(&serving), LINUX)
}

/// Asserts that the config hook's switch of a Linux node from two processes to the farm's file
/// ends with the one process serving: `prepare` records the switch with the fs instance that
/// holds the mount, `cutover` starts the other store instance (which takes the mount from the fs
/// process) and only then clears both fs markers, and once the fs process has ended `verify` and
/// `cleanup` commit with no fs instance on record, no fs marker and no fs unit running. Catches a
/// hook that refuses or mis-reads the farm's file at the switch, one that leaves the old fs unit
/// startable, and one that commits with the fs process still holding the mount.
#[test]
fn the_switch_of_the_farms_linux_node_to_one_process_ends_with_no_fs_instance() {
    let node = linux_switching();
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

    lin::assert_ok(&node.step(Cutover, &p));
    assert_eq!(
        node.fake.fs_actions(),
        ["fs_markers.clear a", "fs_markers.clear b"]
    );
    assert!(node.fake.fs_markers().is_empty());
    // The fs process ends by itself once the new instance has taken the mount.
    node.fake
        .set_fs_unit(Instance::A, UnitState::Inactive, None);
    node.fake
        .set_unit(Instance::A, UnitState::Inactive, None, false);
    node.go(&p, &[Verify, Cleanup]);

    let end = node.fake.record();
    assert!(!end.updating && end.committed());
    assert_eq!((end.fs_live, end.fs_next), (None, None));
    assert!(node.fake.fs_running().is_empty());
    assert!(node.fake.fs_markers().is_empty());
    assert_eq!(node.fake.running(), [Instance::B]);
    assert_eq!(node.fake.markers(), [Instance::B]);
    assert_eq!(
        node.fake.fs_actions(),
        ["fs_markers.clear a", "fs_markers.clear b"]
    );
}

/// Asserts that the switch of the farm's Linux node whose new instance does not start fails
/// `cutover` with unit_failed with the fs markers and the fs process untouched, and that the
/// rollback then leaves the node as it was: the fs process holds the mount and the old store
/// instance serves, with no fs unit started or stopped. Catches a cutover that gives the fs
/// process up before the new instance serves.
#[test]
fn the_switch_of_the_farms_linux_node_whose_new_instance_does_not_start_keeps_the_mount() {
    let node = linux_switching();
    let p = update();
    node.go(&p, &[Prepare]);
    node.fake.fail_on(Some("units.start b"));
    lin::assert_failed(&node.step(Cutover, &p), Reason::UnitFailed);
    node.fake.fail_on(None);
    assert_eq!(node.fake.fs_markers(), [Instance::A]);
    assert_eq!(node.fake.fs_running(), [Instance::A]);

    lin::assert_ok(&node.step(Rollback, &p));
    assert!(node.fake.fs_actions().is_empty());
    assert_eq!(node.fake.fs_markers(), [Instance::A]);
    assert_eq!(node.fake.fs_running(), [Instance::A]);
    assert_eq!(node.fake.running(), [Instance::A]);
    let mut want = Record::idle(Some(Instance::A));
    want.fs_live = Some(Instance::A);
    assert_eq!(node.fake.record(), want);
}

/// Asserts that the switch of the farm's Linux node whose new process took the mount but answers as
/// another build fails `verify` with wrong_build, and that the rollback brings the mount back first
/// (the fs marker, then the fs unit) and the sockets after (the store's marker, then its unit),
/// ending with both serving and the fs instance on record. Catches a rollback that hands the
/// sockets back before the mount, to a process that still holds the mount.
#[test]
fn the_switch_of_the_farms_linux_node_that_serves_the_wrong_build_gives_the_mount_back_first() {
    let node = linux_switching();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    node.fake.lie(lin::OLD, OTHER_BUILD);
    lin::assert_failed(&node.step(Verify, &p), Reason::WrongBuild);
    node.fake.lie(lin::OLD, &lin::version_of(lin::OLD));
    // The old fs process and the old store ended after they gave the mount and the sockets away.
    node.fake
        .set_fs_unit(Instance::A, UnitState::Inactive, None);
    node.fake
        .set_unit(Instance::A, UnitState::Inactive, None, false);

    lin::assert_ok(&node.step(Rollback, &p));
    let log = node.fake.log();
    let at = |prefix: &str| {
        log.iter()
            .position(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no call {prefix}: {log:?}"))
    };
    assert!(at("fs_markers.set a") < at("fs_units.start a"), "{log:?}");
    assert!(at("fs_units.start a") < at("markers.set a"), "{log:?}");
    assert!(at("markers.set a") < at("units.start a"), "{log:?}");
    assert_eq!(node.fake.fs_running(), [Instance::A]);
    assert_eq!(node.fake.fs_markers(), [Instance::A]);
    assert_eq!(node.fake.running(), [Instance::A]);
    let mut want = Record::idle(Some(Instance::A));
    want.fs_live = Some(Instance::A);
    assert_eq!(node.fake.record(), want);
}

/// A Mac whose both releases list `handoff-exec-v1` and whose file is the farm's.
fn mac_fake() -> mac::Fake {
    let fake = mac::Fake::new();
    fake.set_config(mac::CONFIG, Some(MAC));
    fake.set_capabilities(mac::OLD, &[HANDOFF_EXEC_CAPABILITY]);
    fake.set_capabilities(mac::NEW, &[HANDOFF_EXEC_CAPABILITY]);
    fake
}

/// The calls that act on the job, without the looks.
fn acts(calls: &[String]) -> Vec<&str> {
    calls
        .iter()
        .map(String::as_str)
        .filter(|call| !call.starts_with("launchd.loaded"))
        .collect()
}

/// Asserts that a release update of a Mac on the farm's file (S3, the mount provider, an `fs`
/// interface served by the one process, the put ingest) is a hand-over: the staged release checks
/// the file in `prepare`; `cutover` sends the job `SIGUSR2` with no boot-out, so the mount stays
/// under the jobs; and `verify` and `cleanup` commit with the job running the new release. Catches
/// an update hook that refuses or mis-reads the farm's file, one that restarts the job that holds
/// the mount, and one that commits with nothing serving.
#[test]
fn a_release_update_of_the_farms_mac_node_hands_over_and_ends_with_the_new_build_serving() {
    let fake = mac_fake();
    fake.advance(&[Prepare]);
    let checks: Vec<String> = fake
        .log()
        .into_iter()
        .filter(|line| line.starts_with("host.check_config"))
        .collect();
    assert_eq!(
        checks,
        [format!("host.check_config {} {}", mac::NEW, mac::CONFIG)]
    );
    assert!(fake.launchd_calls().is_empty());

    fake.advance(&[Cutover]);
    assert_eq!(
        fake.launchd_calls(),
        [
            format!("launchd.loaded {LABEL}"),
            format!("launchd.kill SIGUSR2 {LABEL}")
        ]
    );
    assert_eq!(fake.snapshot().job, Some(PathBuf::from(mac::NEW)));
    fake.advance(&[Verify, Cleanup]);
    let end = fake.snapshot();
    assert_eq!(end.job, Some(PathBuf::from(mac::NEW)));
    assert!(!fake.record().updating);
    assert!(fake.record().has(Marker::Verified));
    assert!(
        !fake
            .launchd_calls()
            .iter()
            .any(|call| call.starts_with("launchd.bootout"))
    );
    assert!(fake.strays().is_empty());
}

/// Asserts that a release update of a Mac on the farm's file whose new image answers as another
/// build fails `cutover` with probe_failed, still reversible, and is rolled back by handing the
/// job's process back to the old image (`SIGUSR2`, no boot-out): the old release serves and
/// nothing is in flight. Catches a rollback that boots out the job that holds the mount, and one
/// that leaves the new image.
#[test]
fn a_release_update_of_the_farms_mac_node_whose_new_image_answers_wrong_is_handed_back() {
    let fake = mac_fake();
    fake.advance(&[Prepare]);
    fake.lie(mac::NEW, "9.9.9");
    let cut = fake.host_run(Cutover);
    mac::assert_failed(&cut, Reason::ProbeFailed);
    assert!(cut.report.reversible);

    let before = fake.launchd_calls().len();
    mac::assert_ok(&fake.host_run(Rollback));
    assert_eq!(
        fake.launchd_calls()[before..],
        [
            format!("launchd.loaded {LABEL}"),
            format!("launchd.kill SIGUSR2 {LABEL}")
        ]
    );
    let end = fake.snapshot();
    assert_eq!(end.job, Some(PathBuf::from(mac::OLD)));
    assert_eq!(end.record, Some(Record::idle(None)));
}

/// Asserts that a release update of a Mac on the farm's file between releases of which one lacks
/// `handoff-exec-v1` is refused by `prepare` with mode_unsupported before anything is stopped (a
/// restart would drop the mount under the jobs): nothing is recorded, no job is booted out or in,
/// and no release checks the file. Catches an update that unmounts the node under running jobs.
#[test]
fn a_release_update_of_the_farms_mac_node_that_would_restart_is_refused_before_anything_stops() {
    for (previous, staged) in [
        (&[][..], &[HANDOFF_EXEC_CAPABILITY][..]),
        (&[HANDOFF_EXEC_CAPABILITY][..], &[][..]),
        (&[][..], &[][..]),
    ] {
        let fake = mac_fake();
        fake.set_capabilities(mac::OLD, previous);
        fake.set_capabilities(mac::NEW, staged);
        let done = fake.run(Prepare);
        mac::assert_failed(&done, Reason::ModeUnsupported);
        let text = done.report.detail.clone().unwrap_or_default();
        assert!(text.contains(HANDOFF_EXEC_CAPABILITY), "{text}");
        assert!(text.contains("mode = \"restart\""), "{text}");
        assert_eq!(fake.record(), Record::idle(None));
        assert!(fake.launchd_calls().is_empty());
        assert!(
            !fake
                .log()
                .iter()
                .any(|line| line.starts_with("host.check_config"))
        );
        assert_eq!(fake.snapshot().job, Some(PathBuf::from(mac::OLD)));
    }
}

/// Asserts that the mount point the Mac's `fs.toml` names is the mount point of the `fs` interface
/// of its `tierstore.toml`, and the one the fs job's mount is made at in the fake: the switch looks
/// for the old mount at the first and waits for the new mount at the second, and the farm keeps
/// them equal. Catches a farm file that drifts from its `fs.toml`, which would make the switch wait
/// for a mount that is never listed, or look for the old mount in the wrong place.
#[test]
fn the_farms_mac_fs_toml_and_its_fs_interface_name_the_same_mount() {
    let from_fs_toml = mountpoint_of(MAC_FS).expect("fs.toml names a mount point");
    let from_interface = fs_interface(MAC).expect("the Mac file has an fs interface");
    assert_eq!(from_fs_toml, Path::new(MAC_MOUNT));
    assert_eq!(from_interface.mountpoint, from_fs_toml);
    assert_eq!(Path::new(mac::FS_MOUNT), from_fs_toml);
    assert!(fs_interface(MAC_BEFORE_SWITCH).is_none());
}

/// A Mac of two jobs: the farm's file before the switch serves, the fs job is loaded and holds the
/// mount its `fs.toml` (the farm's) names, and the config package stages the farm's file.
fn mac_switching() -> Mac {
    let node = Mac::new();
    node.fake.with_fs(true);
    node.files.put(Path::new(mac::CONFIG), MAC_BEFORE_SWITCH);
    node.files.put(Path::new(FS_CONFIG_FILE), MAC_FS);
    node.files.put(&staged(mac::CONFIG), MAC);
    node
}

/// Asserts that the config hook's switch of a Mac from the farm's file before it (a mount provider,
/// the fs job loaded) to the farm's file ends with the one process serving the mount: `prepare`
/// records the switch; `cutover` boots the fs job out until its mount is gone, disables it, and
/// restarts the main job, never booting the fs job in again; and once hostkeeper has put the file
/// in place and the mount is listed at the mount point it names, `verify` and `cleanup` commit
/// with the fs job out and disabled. Catches a hook that refuses or mis-reads the farm's files at
/// the switch, one that leaves the old plist able to start a second mounter, and one that commits
/// before the new mount is there.
#[test]
fn the_switch_of_the_farms_mac_node_to_one_process_ends_with_the_new_mount_listed() {
    let node = mac_switching();
    let p = mac_update();
    node.go(&p, &[Prepare]);
    assert!(node.fake.record().fs_switch);
    assert!(acts(&node.fake.launchd_calls()).is_empty());
    assert!(node.fake.snapshot().mounted);

    node.go(&p, &[Cutover]);
    let calls = node.fake.launchd_calls();
    let calls = acts(&calls);
    let position = |what: &str| {
        calls
            .iter()
            .position(|call| *call == what)
            .unwrap_or_else(|| panic!("no call {what}: {calls:?}"))
    };
    let fs_out = position(&format!("launchd.bootout {FS_LABEL}"));
    let disabled = position(&format!("launchd.disable {FS_LABEL}"));
    let main_out = position(&format!("launchd.bootout {LABEL}"));
    let main_in = position(&format!("launchd.bootstrap {LAUNCHD_PLIST}"));
    assert!(fs_out < disabled && disabled < main_out && main_out < main_in);
    assert!(!calls.contains(&format!("launchd.bootstrap {FS_LAUNCHD_PLIST}").as_str()));
    assert_eq!(node.fake.disabled(), [FS_LABEL]);
    let cut = node.fake.snapshot();
    assert_eq!((cut.fs_job, cut.mounted), (false, false));
    assert_eq!(cut.job, Some(PathBuf::from(mac::OLD)));

    // hostkeeper puts the farm's file in place, and the new process mounts.
    node.files.put(Path::new(mac::CONFIG), MAC);
    node.files
        .mounts
        .borrow_mut()
        .insert(PathBuf::from(MAC_MOUNT));
    node.go(&p, &[Verify, Cleanup]);
    assert!(node.fake.record().committed());
    assert_eq!(node.fake.disabled(), [FS_LABEL]);
    let end = node.fake.snapshot();
    assert_eq!(
        (end.fs_job, end.job),
        (false, Some(PathBuf::from(mac::OLD)))
    );
}

/// Asserts that the switch of a Mac to the farm's file whose new process does not list its mount
/// fails `verify` with not_ready naming the mount point of the file, records nothing as verified,
/// and that the rollback (with the file before the switch back in place) enables the fs job again
/// and boots it in, ending with the mount listed, the main job running and nothing in flight.
/// Catches a switch that commits a node with no mount under it, and a rollback that leaves the fs
/// job disabled and out.
#[test]
fn the_switch_of_the_farms_mac_node_whose_mount_is_not_listed_is_rolled_back_to_the_fs_job() {
    let node = mac_switching();
    let p = mac_update();
    node.go(&p, &[Prepare, Cutover]);
    node.files.put(Path::new(mac::CONFIG), MAC);
    let done = node.step(Verify, &p);
    lin::assert_failed(&done, Reason::NotReady);
    assert!(
        lin::detail(&done).contains(MAC_MOUNT),
        "{}",
        lin::detail(&done)
    );
    assert!(!node.fake.record().has(Marker::Verified));

    node.files.put(Path::new(mac::CONFIG), MAC_BEFORE_SWITCH);
    lin::assert_ok(&node.step(Rollback, &p));
    let calls = node.fake.launchd_calls();
    let calls = acts(&calls);
    assert!(calls.contains(&format!("launchd.enable {FS_LABEL}").as_str()));
    assert!(calls.contains(&format!("launchd.bootstrap {FS_LAUNCHD_PLIST}").as_str()));
    assert!(node.fake.disabled().is_empty());
    let end = node.fake.snapshot();
    assert_eq!(
        (end.fs_job, end.mounted, end.job),
        (true, true, Some(PathBuf::from(mac::OLD)))
    );
    assert_eq!(node.fake.record(), Record::idle(None));
}
