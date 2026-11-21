//! The binary the plist runs, on a Mac where hostkeeper keeps every package closed to every user
//! but root: the plist runs the copy at `SERVICE_BINARY`, which `cutover` and `rollback` make the
//! binary of the release `current` names before they load the job, and which a first install's
//! rollback and `uninstall` remove. The fake's job runs the plist's program as `_tierstore`,
//! which reaches only that copy.

use std::path::Path;

use super::fake::{Fake, LABEL, NEW, OLD, assert_failed, assert_ok, path, plist_program};
use crate::server::update_hook::{Instances, LAUNCHD_PLIST, Launchd, Reason, SERVICE_BINARY, Step};

use Step::{Cutover, Prepare, Rollback, Verify};

fn install(release: &str) -> String {
    format!("host.install_binary {release}")
}

fn bootstrap() -> String {
    format!("launchd.bootstrap {LAUNCHD_PLIST}")
}

/// Asserts that the plist the package ships runs the copy at `SERVICE_BINARY`, and that a job
/// loaded with no copy there serves nothing, as on a Mac whose packages only root may enter.
/// Catches a plist that runs the binary inside the package, which `_tierstore` cannot reach.
#[test]
fn the_plist_runs_the_copy_and_a_job_without_it_serves_nothing() {
    assert_eq!(plist_program(), SERVICE_BINARY);
    let fake = Fake::new();
    fake.set_installed(None);
    fake.bootout(LABEL).unwrap();
    fake.bootstrap(Path::new(LAUNCHD_PLIST)).unwrap();
    assert_eq!(fake.loaded(LABEL), Ok(true));
    assert_eq!(fake.snapshot().job, None);
    assert!(fake.get(9300, "/readyz").is_err());
    assert!(fake.bootstrap(Path::new(LAUNCHD_PLIST)).is_err());
}

/// Asserts that an update puts the staged release's binary in place before it boots the job out
/// and in, and that the job then serves the staged release. Catches a cutover that restarts the
/// job on the previous binary, or on none.
#[test]
fn an_update_puts_the_staged_binary_in_place_before_it_restarts_the_job() {
    let fake = Fake::new();
    fake.advance(&[Prepare]);
    assert!(fake.binary_acts().is_empty());
    fake.advance(&[Cutover, Verify]);
    assert_eq!(fake.binary_acts(), [install(NEW), bootstrap()]);
    let log = fake.log();
    let at = |line: &str| log.iter().position(|l| l == line).expect(line);
    assert!(at(&install(NEW)) < at(&format!("launchd.bootout {LABEL}")));
    assert_eq!(fake.snapshot().job, path(NEW));
    assert_eq!(fake.snapshot().installed, path(NEW));
}

/// Asserts that a first install puts the binary in place before it loads the job. Catches a
/// first install whose job has no binary to run.
#[test]
fn a_first_install_puts_the_binary_in_place_before_it_loads_the_job() {
    let fake = Fake::first_install();
    fake.advance(&[Prepare, Cutover]);
    assert_eq!(fake.binary_acts(), [install(NEW), bootstrap()]);
    assert_eq!(fake.snapshot().job, path(NEW));
}

/// Asserts that a cutover whose binary cannot be put in place fails `host_failed`, reversible,
/// before it boots the job out: the previous release serves on. Catches a cutover that stops the
/// job and then finds it has nothing to load.
#[test]
fn a_cutover_whose_binary_cannot_be_put_in_place_leaves_the_job_alone() {
    let fake = Fake::new();
    fake.advance(&[Prepare]);
    fake.fail_nth("host.install_binary", 1);
    let done = fake.host_run(Cutover);
    assert_failed(&done, Reason::HostFailed);
    assert!(done.report.reversible);
    assert_eq!(fake.launchd_calls(), [format!("launchd.loaded {LABEL}")]);
    assert_eq!(fake.snapshot().job, path(OLD));
}

/// Asserts that a rollback puts the previous release's binary back before it loads the job
/// again, which then serves the previous release. Catches a rollback that reloads the job on the
/// binary that failed.
#[test]
fn a_rollback_puts_the_previous_binary_back_before_it_reloads_the_job() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    let before = fake.binary_acts().len();
    assert_ok(&fake.host_run(Rollback));
    assert_eq!(fake.binary_acts()[before..], [install(OLD), bootstrap()]);
    assert_eq!(fake.snapshot().job, path(OLD));
    assert_eq!(fake.snapshot().installed, path(OLD));
}

/// Asserts that the rollback of a first install removes the binary after it boots the job out.
/// Catches a failed first install that leaves a binary nothing runs any more.
#[test]
fn a_first_install_rollback_removes_the_binary() {
    let fake = Fake::first_install();
    fake.advance(&[Prepare, Cutover]);
    let done = fake.host_run(Rollback);
    assert_failed(&done, Reason::NoPreviousRelease);
    assert_eq!(
        fake.binary_acts().last().map(String::as_str),
        Some("host.remove_binary")
    );
    assert_eq!(fake.snapshot().installed, None);
    assert_eq!(fake.snapshot().job, None);
}
