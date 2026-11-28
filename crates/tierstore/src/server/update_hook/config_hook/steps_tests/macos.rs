//! The steps of the config hook on a Mac.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::super::super::launchd_tests::fake as mac;
use super::super::super::{
    Done, FS_CONFIG_FILE, LAUNCHD_PLIST, LaunchdPorts, Marker, Params, Reason, Record, Step,
};
use super::super::{Common, Running, launchd_step};
use super::{Files, Mem, PKG_NEW, PKG_OLD, in_flight, lin, staged};
use Step::{Cleanup, Cutover, Prepare, Rollback, Uninstall, Verify};

/// A Mac: one launchd job running the binary of `mac::OLD`, the config hook's record is the
/// fake's, the release's record is `service`.
pub(super) struct Mac {
    pub(super) fake: mac::Fake,
    pub(super) files: Files,
    pub(super) service: Mem,
}

impl Mac {
    pub(super) fn new() -> Mac {
        let files = Files::new(&[]);
        files.put(Path::new(mac::CONFIG), "[server]\n");
        files.put(&staged(mac::CONFIG), "[server]\n");
        Mac {
            fake: mac::Fake::new(),
            files,
            service: Mem::holding(Some(&Record::idle(None))),
        }
    }

    pub(super) fn step(&self, step: Step, params: &Params) -> Done {
        let running = Running {
            host: &self.files,
            release: Path::new(mac::OLD),
        };
        let ports = LaunchdPorts {
            launchd: &self.fake,
            instances: &self.fake,
            host: &running,
            provider: &self.fake.provider,
            clock: &self.fake,
            state: &self.fake,
        };
        let common = Common {
            service: &self.service,
            release: Path::new(mac::OLD),
            build: mac::OLD_VERSION,
        };
        launchd_step(step, params, mac::LABEL, &ports, &common)
    }

    pub(super) fn go(&self, params: &Params, steps: &[Step]) {
        for step in steps {
            lin::assert_ok(&self.step(*step, params));
        }
    }
}

pub(super) fn mac_update() -> Params {
    mac::params_for(PKG_NEW, "1.1.0", Some((PKG_OLD, "1.0.0")))
}

fn mac_first() -> Params {
    mac::params_for(PKG_NEW, "1.1.0", None)
}

fn bootout() -> String {
    format!("launchd.bootout {}", mac::LABEL)
}

fn bootstrap() -> String {
    format!("launchd.bootstrap {}", LAUNCHD_PLIST)
}

fn loaded() -> String {
    format!("launchd.loaded {}", mac::LABEL)
}

/// Asserts that a config update on a Mac restarts the job (the config hook's bootout, then the
/// arm's look, bootout of a job no longer loaded, and bootstrap), and that
/// `cleanup` ends it with the three markers and the version of the serving build.
#[test]
fn a_mac_config_update_restarts_the_job_and_commits() {
    let node = Mac::new();
    let p = mac_update();
    node.go(&p, &[Prepare]);
    assert!(node.fake.launchd_calls().is_empty());
    node.go(&p, &[Cutover]);
    assert_eq!(
        node.fake.launchd_calls(),
        [bootout(), loaded(), bootout(), bootstrap()]
    );
    node.go(&p, &[Verify, Cleanup]);
    let record = node.fake.record();
    assert!(!record.updating);
    assert_eq!(record.version.as_deref(), Some(mac::OLD_VERSION));
    assert_eq!(
        record.markers,
        BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping])
    );
    assert_eq!(node.fake.snapshot().job, Some(PathBuf::from(mac::OLD)));
    assert_eq!(node.service.get(), Some(Record::idle(None)));
}

/// Asserts that the first config package of a Mac also boots the job out and in, as an update.
#[test]
fn the_first_config_package_of_a_mac_also_boots_the_job_out_and_in() {
    let node = Mac::new();
    let p = mac_first();
    node.go(&p, &[Prepare, Cutover]);
    assert_eq!(
        node.fake.launchd_calls(),
        [bootout(), loaded(), bootout(), bootstrap()]
    );
}

/// Asserts that every Mac step run again after it succeeded changes neither the job nor the
/// record.
#[test]
fn a_mac_step_run_again_after_it_succeeded_changes_nothing() {
    let node = Mac::new();
    let p = mac_update();
    for step in [Prepare, Cutover, Verify, Cleanup] {
        node.go(&p, &[step]);
        let snapshot = node.fake.snapshot();
        lin::assert_ok(&node.step(step, &p));
        assert_eq!(node.fake.snapshot(), snapshot, "{step} again");
    }
}

/// Asserts that a Mac's `verify` refuses an installed file the serving binary refuses.
#[test]
fn a_mac_verify_refuses_an_installed_file_the_binary_refuses() {
    let node = Mac::new();
    let p = mac_update();
    node.go(&p, &[Prepare, Cutover]);
    node.files.refuse(Path::new(mac::CONFIG), "bad key");
    let done = node.step(Verify, &p);
    lin::assert_failed(&done, Reason::ConfigRefused);
    assert!(lin::detail(&done).contains("bad key"));
    assert!(!node.fake.record().has(Marker::Verified));
}

/// Asserts that a Mac's `prepare` refuses with out_of_order while the release updates.
#[test]
fn a_mac_prepare_waits_while_an_update_of_the_release_is_in_flight() {
    let node = Mac::new();
    node.service.set(&in_flight(None));
    let done = node.step(Prepare, &mac_update());
    lin::assert_failed(&done, Reason::OutOfOrder);
    assert_eq!(node.fake.record(), Record::idle(None));
}

/// Asserts that a Mac's `prepare` refuses a staged file the serving binary refuses.
#[test]
fn a_mac_prepare_refuses_a_staged_file_the_binary_refuses() {
    let node = Mac::new();
    node.files.refuse(&staged(mac::CONFIG), "bad key");
    lin::assert_failed(&node.step(Prepare, &mac_update()), Reason::ConfigRefused);
}

/// Asserts that a Mac's step with params that name no configuration file is refused.
#[test]
fn a_mac_step_without_a_configuration_file_in_its_params_is_refused() {
    let node = Mac::new();
    let mut p = mac_update();
    p.config_file = None;
    lin::assert_failed(&node.step(Prepare, &p), Reason::BadParams);
}

/// Asserts that the rollback of a Mac's config package with a previous one boots the job out and
/// in and writes an idle record.
#[test]
fn a_mac_rollback_with_a_previous_package_restarts_the_job() {
    let node = Mac::new();
    let p = mac_update();
    node.go(&p, &[Prepare, Cutover]);
    lin::assert_ok(&node.step(Rollback, &p));
    assert_eq!(node.fake.record(), Record::idle(None));
    assert_eq!(node.fake.snapshot().job, Some(PathBuf::from(mac::OLD)));
}

/// Asserts that the rollback of a Mac's first config package leaves the job as it runs, writes
/// an idle record and fails no_previous_release, without a launchd call.
#[test]
fn a_mac_first_packages_rollback_leaves_the_job_and_fails() {
    let node = Mac::new();
    let p = mac_first();
    node.go(&p, &[Prepare, Cutover]);
    let before = node.fake.launchd_calls().len();
    let done = node.step(Rollback, &p);
    lin::assert_failed(&done, Reason::NoPreviousRelease);
    assert!(lin::detail(&done).contains("the job keeps serving on what it read"));
    assert_eq!(node.fake.launchd_calls().len(), before);
    assert_eq!(node.fake.record(), Record::idle(None));
    assert_eq!(node.fake.snapshot().job, Some(PathBuf::from(mac::OLD)));
}

/// Asserts the Mac first-package rollback's outcomes when nothing is in flight: success with no
/// record and with an idle one, committed after cleanup began.
#[test]
fn a_mac_first_packages_rollback_at_rest_succeeds_and_after_the_commit_fails() {
    let node = Mac::new();
    let p = mac_first();
    node.fake.set_record_bytes(None);
    lin::assert_ok(&node.step(Rollback, &p));
    node.fake.set_record(&Record::idle(None));
    lin::assert_ok(&node.step(Rollback, &p));
    node.fake.set_record(&Record {
        updating: true,
        markers: BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]),
        to: Some(PathBuf::from(PKG_NEW)),
        version: Some(mac::OLD_VERSION.to_owned()),
        ..Record::idle(None)
    });
    lin::assert_failed(&node.step(Rollback, &p), Reason::Committed);
}

/// Asserts that the Mac first-package rollback refuses a record that is not valid, a lock held
/// by another step, and a record it cannot write.
#[test]
fn a_mac_first_packages_rollback_reports_a_record_that_is_not_valid_or_a_port_that_failed() {
    let node = Mac::new();
    let p = mac_first();
    node.fake.set_record_bytes(Some(b"not json"));
    lin::assert_failed(&node.step(Rollback, &p), Reason::StateInvalid);
    node.fake.set_record(&Record {
        markers: BTreeSet::from([Marker::Swapped]),
        ..Record::idle(None)
    });
    lin::assert_failed(&node.step(Rollback, &p), Reason::StateInvalid);

    node.fake.set_record(&in_flight(None));
    node.fake.set_busy(true);
    lin::assert_failed(&node.step(Rollback, &p), Reason::Locked);
    node.fake.set_busy(false);
    node.fake.fail_nth("state.save", 1);
    lin::assert_failed(&node.step(Rollback, &p), Reason::StateIo);
}

/// Asserts that a Mac's `uninstall` succeeds and changes nothing.
#[test]
fn a_mac_uninstall_changes_nothing_and_succeeds() {
    let node = Mac::new();
    let p = mac_update();
    node.go(&p, &[Prepare, Cutover]);
    let (snapshot, calls) = (node.fake.snapshot(), node.fake.launchd_calls());
    lin::assert_ok(&node.step(Uninstall, &p));
    assert_eq!(node.fake.snapshot(), snapshot);
    assert_eq!(node.fake.launchd_calls(), calls);
}

/// Asserts that `restore` is refused on a Mac too: bad_args.
#[test]
fn a_mac_restore_is_refused_as_not_a_step() {
    let node = Mac::new();
    lin::assert_failed(&node.step(Step::Restore, &mac_update()), Reason::BadArgs);
}

/// Asserts that a Mac `cutover` with no config update on record boots nothing out and is refused
/// out of order, and that a record it cannot lock, read or decode fails the step before any
/// bootout.
#[test]
fn a_mac_cutover_boots_nothing_out_without_an_update_on_record_or_a_record_it_can_read() {
    let node = Mac::new();
    let p = mac_update();
    node.fake.set_record_bytes(None);
    lin::assert_failed(&node.step(Cutover, &p), Reason::OutOfOrder);
    assert!(!node.fake.launchd_calls().contains(&bootout()));
    node.fake.set_record(&Record::idle(None));
    lin::assert_failed(&node.step(Cutover, &p), Reason::OutOfOrder);
    assert!(!node.fake.launchd_calls().contains(&bootout()));
    node.go(&p, &[Prepare]);
    node.fake.set_busy(true);
    lin::assert_failed(&node.step(Cutover, &p), Reason::Locked);
    node.fake.set_busy(false);
    node.fake.fail_nth("state.load", 1);
    lin::assert_failed(&node.step(Cutover, &p), Reason::StateIo);
    node.fake.set_record_bytes(Some(b"not json"));
    lin::assert_failed(&node.step(Cutover, &p), Reason::StateInvalid);
    assert!(!node.fake.launchd_calls().contains(&bootout()));
}

/// The config package after `PKG_NEW`.
const PKG_NEWER: &str = "/var/lib/hostkeeper/packages/tierstore-config/1.2.0";

/// What a rollback of the update to `PKG_NEWER` says when its prepare recorded nothing and the
/// record is at rest on the update to `PKG_NEW`.
fn nothing_after_failed_prepare() -> String {
    format!(
        "nothing to roll back: the update to {PKG_NEWER} {v} was never recorded (its prepare \
         failed or never ran); the record is at rest on the last update ({PKG_NEW} {v})",
        v = mac::OLD_VERSION
    )
}

/// Asserts the sequence a Mac ran in production: a config update finished (the record at rest,
/// committed), the next package's `prepare` fails before it records anything (here its staged
/// file is not there), and the `rollback` hostkeeper runs next succeeds, says there was nothing to
/// roll back, is reversible (before the commit), calls launchd not at all and leaves the record as
/// it was; for a next package with a previous one and for one without. Catches the rollback that
/// failed `committed` ("cleanup has begun: nothing can be undone") and irreversibly, which
/// hostkeeper reported as a config update failed after its commit.
#[test]
fn a_mac_rollback_after_the_next_packages_prepare_failed_has_nothing_to_roll_back() {
    for previous in [Some((PKG_NEW, "1.1.0")), None] {
        let node = Mac::new();
        node.go(&mac_update(), &[Prepare, Cutover, Verify, Cleanup]);
        let at_rest = node.fake.record();
        assert_eq!(
            (at_rest.committed(), at_rest.updating),
            (true, false),
            "{at_rest:?}"
        );
        let next = mac::params_for(PKG_NEWER, "1.2.0", previous);
        let prepare = node.step(Prepare, &next);
        lin::assert_failed(&prepare, Reason::ConfigMissing);
        assert!(prepare.report.reversible);
        let before = node.fake.launchd_calls().len();
        let rollback = node.step(Rollback, &next);
        lin::assert_ok(&rollback);
        assert!(rollback.report.reversible, "{previous:?}");
        assert_eq!(lin::detail(&rollback), nothing_after_failed_prepare());
        assert_eq!(node.fake.launchd_calls().len(), before);
        assert_eq!(node.fake.record(), at_rest);
    }
}

/// Asserts that a Mac config package whose params name the fs job drives it through the update
/// hook's arm, with the mount list read through the host the config hook gives the arm: with no
/// mount listed the update ends with both jobs loaded, and with a mount still listed after the fs
/// job went the cutover fails with `not_ready` and the fs job is loaded again. Catches a config
/// hook host that does not forward the mount list.
#[test]
fn a_mac_config_update_that_names_the_fs_job_checks_its_mount_through_the_host() {
    let p = mac::params_with(
        PKG_NEW,
        "1.1.0",
        Some((PKG_OLD, "1.0.0")),
        "fs_launchd_label = \"tierstore-fs\"\n",
    );
    let fs_config = format!("[fs]\nmountpoint = \"{}\"\n", mac::FS_MOUNT);

    let node = Mac::new();
    node.fake.with_fs(true);
    node.files.put(Path::new(FS_CONFIG_FILE), &fs_config);
    node.go(&p, &[Prepare, Cutover]);
    let end = node.fake.snapshot();
    assert_eq!((end.job, end.fs_job), (Some(PathBuf::from(mac::OLD)), true));

    let stuck = Mac::new();
    stuck.fake.with_fs(true);
    stuck.files.put(Path::new(FS_CONFIG_FILE), &fs_config);
    stuck
        .files
        .mounts
        .borrow_mut()
        .insert(PathBuf::from(mac::FS_MOUNT));
    stuck.go(&p, &[Prepare]);
    let done = stuck.step(Cutover, &p);
    assert_eq!(done.report.reason, Some(Reason::NotReady), "{done:?}");
    assert!(stuck.fake.snapshot().fs_job);
}
