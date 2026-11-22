//! The job of `tierstore fs` in the launchd arm: it is booted out before the main job and its
//! mount must be gone before the update goes on, and it is booted in after the main job.

use std::time::Duration;

use super::fake::{
    FS_LABEL, FS_MOUNT, Fake, LABEL, NEW, OLD, assert_failed, assert_ok, in_flight, path,
};
use crate::server::update_hook::launchd::fs::{FS_BOOTOUT_TIMEOUT, FS_GONE_ATTEMPTS};
use crate::server::update_hook::{
    FS_CONFIG_FILE, FS_LAUNCHD_PLIST, LAUNCHD_PLIST, Marker, Reason, Record, Step,
};

use Step::{Cleanup, Cutover, Prepare, Restore, Rollback, Uninstall, Verify};

/// The calls to the launchd domain and the host so far, in order.
fn acts(fake: &Fake) -> Vec<String> {
    fake.log()
        .into_iter()
        .filter(|call| call.starts_with("launchd.") || call.starts_with("host."))
        .collect()
}

fn loaded() -> String {
    format!("launchd.loaded {LABEL}")
}

fn fs_loaded() -> String {
    format!("launchd.loaded {FS_LABEL}")
}

fn bootout() -> String {
    format!("launchd.bootout {LABEL}")
}

fn fs_bootout() -> String {
    format!("launchd.bootout {FS_LABEL}")
}

fn bootstrap() -> String {
    format!("launchd.bootstrap {LAUNCHD_PLIST}")
}

fn fs_bootstrap() -> String {
    format!("launchd.bootstrap {FS_LAUNCHD_PLIST}")
}

fn config() -> String {
    format!("host.config {FS_CONFIG_FILE}")
}

fn mounted() -> String {
    format!("host.mounted {FS_MOUNT}")
}

fn install(release: &str) -> String {
    format!("host.install_binary {release}")
}

/// An update of a host whose fs job is loaded, prepared and not yet cut over.
fn prepared() -> Fake {
    let fake = Fake::new();
    fake.with_fs(true);
    fake.advance(&[Prepare]);
    fake
}

/// Asserts that the `cutover` of an update boots the fs job out first, with the fs job's own
/// bounds (the 150 s the plist gives it plus 30 s, and 30 looks), reads the mount list after it,
/// and only then installs the binary and restarts the main job, and that the fs job is booted in
/// after the main job serves; the record gets `swapped`, and no other job or plist is named.
/// Catches a cutover that restarts the main job while the file service runs, one that does not
/// wait for the fs job to go, and one that leaves the fs job out.
#[test]
fn a_cutover_boots_the_fs_job_out_before_the_main_job_and_in_after_it() {
    let fake = prepared();
    let before = acts(&fake).len();
    assert_ok(&fake.run(Cutover));
    assert_eq!(
        acts(&fake)[before..],
        [
            loaded(),
            fs_loaded(),
            config(),
            fs_bootout(),
            mounted(),
            install(NEW),
            bootout(),
            bootstrap(),
            fs_loaded(),
            fs_bootstrap(),
        ]
    );
    assert_eq!(
        fake.bootout_bounds(),
        [(FS_LABEL.to_owned(), FS_BOOTOUT_TIMEOUT, FS_GONE_ATTEMPTS)]
    );
    assert_eq!(
        (FS_BOOTOUT_TIMEOUT, FS_GONE_ATTEMPTS),
        (Duration::from_secs(180), 30)
    );
    let end = fake.snapshot();
    assert_eq!((end.fs_job, end.mounted, end.job), (true, true, path(NEW)));
    assert!(fake.record().has(Marker::Swapped));
    assert!(fake.strays().is_empty());
}

/// Asserts that a `cutover` run again after it finished only looks: the main job serves the
/// target and the fs job is loaded, so nothing is booted out or in. Catches a re-run that takes
/// the mount down once more.
#[test]
fn a_cutover_run_again_leaves_both_jobs_alone() {
    let fake = prepared();
    assert_ok(&fake.run(Cutover));
    let before = acts(&fake).len();
    assert_ok(&fake.run(Cutover));
    assert_eq!(acts(&fake)[before..], [loaded(), fs_loaded()]);
}

/// Asserts that a mount that is still listed after the fs job is gone refuses the `cutover` with
/// `not_ready` naming the mount point, boots nothing else out and installs nothing (the main job
/// keeps serving the old release and the record has no `swapped`), and boots the fs job in again
/// so that the mount has a server. Catches an update that goes on over a mount with no server,
/// which hangs every process that touches it.
#[test]
fn a_mount_that_stays_refuses_the_cutover_and_the_fs_job_is_booted_in_again() {
    let fake = prepared();
    fake.keep_mount(true);
    let before = acts(&fake).len();
    let done = fake.run(Cutover);
    assert_failed(&done, Reason::NotReady);
    let detail = done.report.detail.expect("a detail");
    assert!(detail.contains(FS_MOUNT), "{detail}");
    assert!(detail.contains("is still mounted"), "{detail}");
    assert_eq!(
        acts(&fake)[before..],
        [
            loaded(),
            fs_loaded(),
            config(),
            fs_bootout(),
            mounted(),
            fs_bootstrap(),
        ]
    );
    let end = fake.snapshot();
    assert_eq!(end.job, path(OLD));
    assert_eq!(end.installed, path(OLD));
    assert!(end.fs_job);
    assert_eq!(fake.record(), in_flight(false));
    assert_eq!(fake.live_locks(), 0);
}

/// Asserts that when the fs job cannot be booted in again after a mount stayed, the step reports
/// that failure as `unit_failed` and still has not touched the main job. Catches a refusal that
/// swallows the failure of the fs job's boot-in.
#[test]
fn a_failed_boot_in_of_the_fs_job_after_a_staying_mount_is_reported() {
    let fake = prepared();
    fake.keep_mount(true);
    fake.fail_nth(&fs_bootstrap(), 1);
    assert_failed(&fake.run(Cutover), Reason::UnitFailed);
    assert_eq!(fake.snapshot().job, path(OLD));
    assert!(!fake.snapshot().fs_job);
}

/// Asserts that a mount list that cannot be had after the fs job is gone fails the `cutover` with
/// `host_failed`, boots the fs job in again, and leaves the main job untouched. Catches a check
/// that takes an unreadable listing for a mount that is gone.
#[test]
fn a_mount_list_that_cannot_be_had_fails_the_cutover_and_boots_the_fs_job_in_again() {
    let fake = prepared();
    fake.fail_nth("host.mounted", 1);
    assert_failed(&fake.run(Cutover), Reason::HostFailed);
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job), (path(OLD), true));
    assert!(!acts(&fake).contains(&bootout()));
    fake.heal();
    assert_ok(&fake.run(Cutover));
    assert_eq!(fake.snapshot().job, path(NEW));
}

/// Asserts that an fs job that will not go (its boot-out fails) fails the `cutover` with
/// `unit_failed` before the main job is touched. Catches a cutover that restarts the main job
/// beside a file service it could not stop.
#[test]
fn an_fs_job_that_will_not_go_stops_the_cutover_before_the_main_job() {
    let fake = prepared();
    fake.fail_nth(&fs_bootout(), 1);
    assert_failed(&fake.run(Cutover), Reason::UnitFailed);
    assert!(!acts(&fake).contains(&bootout()));
    assert_eq!(fake.snapshot().job, path(OLD));
}

/// Asserts that the fs configuration is read before anything is stopped: a file that is not there
/// is refused with `config_missing` and one that is not TOML with `config_refused`, each naming
/// the file and saying nothing was stopped, and neither boots a job out. Catches an update that
/// stops the file service without knowing which mount to check.
#[test]
fn an_fs_configuration_that_names_no_mount_point_refuses_before_anything_is_stopped() {
    let fake = prepared();
    let before = acts(&fake).len();
    fake.set_config(FS_CONFIG_FILE, None);
    let done = fake.run(Cutover);
    assert_failed(&done, Reason::ConfigMissing);
    let detail = done.report.detail.expect("a detail");
    assert!(detail.contains(FS_CONFIG_FILE), "{detail}");
    assert!(detail.contains("nothing was stopped"), "{detail}");
    fake.set_config(FS_CONFIG_FILE, Some("= nope\n"));
    let done = fake.run(Cutover);
    assert_failed(&done, Reason::ConfigRefused);
    let detail = done.report.detail.expect("a detail");
    assert!(detail.contains(FS_CONFIG_FILE), "{detail}");
    assert!(detail.contains("nothing was stopped"), "{detail}");
    assert!(
        acts(&fake)[before..]
            .iter()
            .all(|call| !call.starts_with("launchd.boot"))
    );
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job, end.mounted), (path(OLD), true, true));
}

/// Asserts that a configuration that leaves out `[fs] mountpoint` is checked at the default mount
/// point of `tierstore fs`, `/run/tierstore/fs`. Catches a check made on another path.
#[test]
fn a_configuration_without_a_mount_point_is_checked_at_the_default() {
    let fake = prepared();
    fake.set_config(FS_CONFIG_FILE, Some("[fs]\n"));
    assert_ok(&fake.run(Cutover));
    assert!(acts(&fake).contains(&"host.mounted /run/tierstore/fs".to_owned()));
}

/// Asserts that an update of a host whose fs job is not loaded skips the boot-out and the mount
/// check (the configuration is not read), and boots the fs job in after the main job serves.
/// Catches a cutover that reads the mount list of a host that has no file service, and one that
/// leaves the fs job out.
#[test]
fn an_fs_job_that_is_not_loaded_is_booted_in_after_the_main_job() {
    let fake = Fake::new();
    fake.with_fs(false);
    fake.advance(&[Prepare]);
    let before = acts(&fake).len();
    assert_ok(&fake.run(Cutover));
    assert_eq!(
        acts(&fake)[before..],
        [
            loaded(),
            fs_loaded(),
            install(NEW),
            bootout(),
            bootstrap(),
            fs_loaded(),
            fs_bootstrap(),
        ]
    );
    assert!(fake.snapshot().fs_job);
}

/// Asserts that a first install loads the main job and then the fs job, with no boot-out and no
/// look at the mounts. Catches a first install that stops what is not there.
#[test]
fn a_first_install_boots_the_fs_job_in_after_the_main_job() {
    let fake = Fake::first_install();
    fake.with_fs(false);
    fake.advance(&[Prepare]);
    let before = acts(&fake).len();
    assert_ok(&fake.run(Cutover));
    assert_eq!(
        acts(&fake)[before..],
        [
            loaded(),
            install(NEW),
            bootstrap(),
            fs_loaded(),
            fs_bootstrap(),
        ]
    );
}

/// Asserts that a `cutover` whose boot-in of the fs job failed is finished by the next run, which
/// finds the main job serving and boots only the fs job in. Catches a run after a crash that
/// restarts the main job again or never loads the fs job.
#[test]
fn a_cutover_that_could_not_boot_the_fs_job_in_is_finished_by_the_next_run() {
    let fake = prepared();
    fake.fail_nth(&fs_bootstrap(), 1);
    assert_failed(&fake.run(Cutover), Reason::UnitFailed);
    assert!(!fake.record().has(Marker::Swapped));
    fake.heal();
    let before = acts(&fake).len();
    assert_ok(&fake.run(Cutover));
    assert_eq!(
        acts(&fake)[before..],
        [loaded(), fs_loaded(), fs_bootstrap()]
    );
    assert!(fake.record().has(Marker::Swapped));
}

/// Asserts that `rollback` boots the fs job out first, with the mount check, puts the previous
/// binary back, restarts the main job on the previous release and boots the fs job in once it
/// serves. Catches a rollback that restarts the main job beside a running file service, and one
/// that leaves the fs job out.
#[test]
fn a_rollback_brings_both_jobs_back_with_the_fs_job_out_first() {
    let fake = prepared();
    assert_ok(&fake.run(Cutover));
    let before = acts(&fake).len();
    assert_ok(&fake.run(Rollback));
    assert_eq!(
        acts(&fake)[before..],
        [
            fs_loaded(),
            config(),
            fs_bootout(),
            mounted(),
            install(OLD),
            bootout(),
            bootstrap(),
            fs_loaded(),
            fs_bootstrap(),
        ]
    );
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job, end.mounted), (path(OLD), true, true));
    assert_eq!(end.record, Some(Record::idle(None)));
}

/// Asserts that a mount that stays refuses the `rollback` with `not_ready`, leaves the main job
/// on the release it runs and the update on record, and boots the fs job in again. Catches a
/// rollback that restarts the main job over a mount with no server.
#[test]
fn a_mount_that_stays_refuses_the_rollback() {
    let fake = prepared();
    assert_ok(&fake.run(Cutover));
    fake.keep_mount(true);
    let before = acts(&fake).len();
    assert_failed(&fake.run(Rollback), Reason::NotReady);
    assert!(!acts(&fake)[before..].contains(&bootout()));
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job), (path(NEW), true));
    assert!(fake.record().updating);
}

/// Asserts that the rollback of a first install boots the fs job out first and then the main job,
/// removes the binary, and fails with `no_previous_release`, leaving neither job loaded. Catches
/// a first-install rollback that leaves the file service running.
#[test]
fn the_rollback_of_a_first_install_boots_the_fs_job_out_first() {
    let fake = Fake::first_install();
    fake.with_fs(false);
    fake.advance(&[Prepare, Cutover]);
    let before = acts(&fake).len();
    assert_failed(&fake.run(Rollback), Reason::NoPreviousRelease);
    assert_eq!(
        acts(&fake)[before..],
        [
            fs_loaded(),
            config(),
            fs_bootout(),
            mounted(),
            bootout(),
            "host.remove_binary".to_owned(),
        ]
    );
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job, end.mounted), (None, false, false));
}

/// Asserts that `uninstall` boots the fs job out first, with the mount check, then the main job,
/// removes the binary and leaves neither job loaded, and that a mount that stays refuses it with
/// `not_ready` before the main job is booted out. Catches an uninstall that stops the main job
/// beside a file service, and one that goes on over a mount with no server.
#[test]
fn uninstall_boots_the_fs_job_out_first_and_refuses_a_mount_that_stays() {
    let fake = Fake::new();
    fake.with_fs(true);
    let before = acts(&fake).len();
    assert_ok(&fake.run(Uninstall));
    assert_eq!(
        acts(&fake)[before..],
        [
            fs_loaded(),
            config(),
            fs_bootout(),
            mounted(),
            bootout(),
            "host.remove_binary".to_owned(),
        ]
    );
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job, end.mounted), (None, false, false));

    let stuck = Fake::new();
    stuck.with_fs(true);
    stuck.keep_mount(true);
    assert_failed(&stuck.run(Uninstall), Reason::NotReady);
    assert!(!acts(&stuck).contains(&bootout()));
    let end = stuck.snapshot();
    assert_eq!((end.job, end.fs_job), (path(OLD), true));
}

/// Asserts that the boot step loads the main job and then the fs job on a host where neither is
/// loaded, only the fs job where the main job is loaded and the fs job is not, and nothing where
/// both are loaded. Catches a restore that leaves a rebooted Mac without its file service.
#[test]
fn restore_brings_back_what_is_not_loaded_main_job_first() {
    let fake = Fake::new();
    fake.with_fs(true);
    fake.reboot();
    assert_ok(&fake.run(Restore));
    assert_eq!(
        acts(&fake),
        [
            loaded(),
            install(NEW),
            bootstrap(),
            fs_loaded(),
            fs_bootstrap(),
        ]
    );
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job), (path(NEW), true));

    let main_only = Fake::new();
    main_only.with_fs(false);
    assert_ok(&main_only.run(Restore));
    assert_eq!(acts(&main_only), [loaded(), fs_loaded(), fs_bootstrap()]);

    let both = Fake::new();
    both.with_fs(true);
    assert_ok(&both.run(Restore));
    assert_eq!(acts(&both), [loaded(), fs_loaded()]);
}

/// Asserts that a full update with the fs job ends with both jobs loaded, the mount listed, the
/// record committed and at rest. Catches a commit that leaves the file service down.
#[test]
fn a_whole_update_leaves_both_jobs_loaded() {
    let fake = Fake::new();
    fake.with_fs(true);
    fake.advance(&[Prepare, Cutover, Verify, Cleanup]);
    let end = fake.snapshot();
    assert_eq!((end.job, end.fs_job, end.mounted), (path(NEW), true, true));
    assert!(fake.record().committed());
    assert!(fake.strays().is_empty());
}
