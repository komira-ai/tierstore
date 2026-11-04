//! `uninstall` and `restore` (the boot step), and the fake's own behaviour that the other tests
//! lean on.

use std::path::Path;

use super::fake::{Fake, LABEL, NEW, OLD, assert_failed, assert_ok, path};
use crate::server::update_hook::{
    Host, Instances, LAUNCHD_PLIST, Launchd, PortError, Reason, Record, Step,
};

use Step::{Cleanup, Cutover, Prepare, Restore, Uninstall, Verify};

/// Asserts that `uninstall` boots the job out and replaces the record with one at rest, and does
/// nothing else: the host is asked nothing (the fake's host has no call that changes a file, and
/// none was made), and the lock is released. The data, cache and configuration of the node are
/// the host's and are never named. Catches an uninstall that reads or removes a path of the node.
#[test]
fn uninstall_boots_the_job_out_and_touches_nothing_else() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify, Cleanup]);
    let before = fake.log().len();
    let done = fake.host_run(Uninstall);
    assert_ok(&done);
    assert_eq!(
        fake.log()[before..],
        [
            "state.lock".to_owned(),
            "state.load".to_owned(),
            format!("launchd.bootout {LABEL}"),
            "state.save".to_owned(),
        ]
    );
    assert_eq!(fake.snapshot().job, None);
    assert_eq!(fake.record(), Record::idle(None));
    assert!(fake.strays().is_empty());
    assert_eq!(fake.live_locks(), 0);
}

/// Asserts that `uninstall` of a host that has no record and no job succeeds and leaves a record
/// at rest. Catches an uninstall that needs what a failed first install never made.
#[test]
fn uninstall_of_a_host_with_nothing_recorded_succeeds() {
    let fake = Fake::first_install();
    assert_ok(&fake.host_run(Uninstall));
    assert_eq!(fake.record(), Record::idle(None));
}

/// Asserts that the boot step loads the job when it is not loaded, and with the plist hostkeeper
/// links; a host with a record or none alike. Catches a restore that leaves a rebooted Mac
/// without its job.
#[test]
fn restore_bootstraps_an_unloaded_job() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify, Cleanup]);
    fake.reboot();
    let before = fake.launchd_calls().len();
    assert_ok(&fake.host_run(Restore));
    assert_eq!(
        fake.launchd_calls()[before..],
        [
            format!("launchd.loaded {LABEL}"),
            format!("launchd.bootstrap {LAUNCHD_PLIST}")
        ]
    );
    assert_eq!(fake.snapshot().job, path(NEW));
    let bare = Fake::first_install();
    bare.switch_current(OLD);
    assert_ok(&bare.run(Restore));
    assert_eq!(bare.snapshot().job, path(OLD));
}

/// Asserts that the boot step does nothing to a job that is loaded: one question to launchd and
/// no bootstrap. Catches a restore that fails on a loaded job (the domain refuses a second
/// bootstrap) or restarts it.
#[test]
fn restore_leaves_a_loaded_job_alone() {
    let fake = Fake::new();
    assert_ok(&fake.host_run(Restore));
    assert_eq!(fake.launchd_calls(), [format!("launchd.loaded {LABEL}")]);
    assert_eq!(fake.snapshot().job, path(OLD));
}

/// Asserts that the fake names what it is asked about: a job label or a plist other than the
/// job's is noted as a stray and changes nothing, a second bootstrap of a loaded job fails as
/// launchd does, and a bootout of an unloaded job is not an error. The tests that assert "no
/// other label" rest on this.
#[test]
fn the_fake_notes_strays_and_refuses_a_second_bootstrap() {
    let fake = Fake::new();
    assert_eq!(fake.loaded("other.label"), Ok(false));
    assert_eq!(fake.bootout("other.label"), Ok(()));
    assert_eq!(fake.strays(), ["other.label", "other.label"]);
    assert_eq!(fake.snapshot().job, path(OLD));
    let err = fake.bootstrap(Path::new("/other.plist")).unwrap_err();
    assert!(matches!(err, PortError::Failed { .. }), "{err:?}");
    assert_eq!(fake.strays().len(), 3);
    assert_eq!(fake.bootout(LABEL), Ok(()));
    assert_eq!(fake.bootout(LABEL), Ok(()));
    assert_eq!(fake.snapshot().job, None);
}

/// Asserts that the fake's job answers on the public port only while it is loaded, times out when
/// it is muted, answers 404 for a path it does not serve, and that its host refuses to run a
/// release. Catches a fake that answers for a job that is not there.
#[test]
fn the_fake_answers_only_for_a_loaded_job_on_the_public_port() {
    let fake = Fake::new();
    assert_eq!(fake.get(9300, "/readyz").unwrap().status, 200);
    assert_eq!(fake.get(9300, "/other").unwrap().status, 404);
    assert!(fake.get(9301, "/readyz").is_err());
    fake.mute(OLD);
    assert!(fake.get(9300, "/readyz").is_err());
    fake.reboot();
    assert!(fake.get(9300, "/readyz").is_err());
    assert!(fake.capabilities(Path::new(OLD)).is_err());
    assert_failed(
        &fake.run_with(Cutover, &fake.params(), LABEL),
        Reason::OutOfOrder,
    );
    assert!(fake.pauses() == 0 && fake.calls() > 0);
}
