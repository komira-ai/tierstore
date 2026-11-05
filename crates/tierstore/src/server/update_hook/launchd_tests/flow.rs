//! An update on launchd, step by step: the calls of each step, the record each leaves, what a
//! re-run does, and what a failing cutover leaves behind.

use std::collections::BTreeSet;

use super::fake::{Fake, LABEL, NEW, NEW_VERSION, OLD, assert_failed, assert_ok, in_flight, path};
use crate::server::update_hook::{LAUNCHD_PLIST, Marker, Outcome, Reason, Record, Step};

use Step::{Cleanup, Cutover, Prepare, Verify};

fn bootout() -> String {
    format!("launchd.bootout {LABEL}")
}

fn bootstrap() -> String {
    format!("launchd.bootstrap {LAUNCHD_PLIST}")
}

fn loaded() -> String {
    format!("launchd.loaded {LABEL}")
}

/// Asserts that an update is a restart with a gap, in this order: `prepare` records the update
/// and calls launchd not at all; `cutover` asks whether the job is loaded, boots it out and boots
/// the plist in; `verify` and `cleanup` ask once each; every launchd call names the label of the
/// params or the linked plist and nothing else; the commit makes the report irreversible and
/// leaves the record at rest with all three markers. Catches a restart that skips the bootout, a
/// call for a label the params do not name, and a cleanup that leaves the update in flight.
#[test]
fn an_update_boots_the_job_out_and_in_and_touches_no_other_label() {
    let fake = Fake::new();
    let prepared = fake.host_run(Prepare);
    assert_ok(&prepared);
    assert!(prepared.report.reversible);
    assert!(fake.launchd_calls().is_empty());
    assert_eq!(fake.snapshot().job, path(OLD));
    assert_eq!(fake.record(), in_flight(false));

    let cut = fake.host_run(Cutover);
    assert_ok(&cut);
    assert!(cut.report.reversible);
    assert_eq!(fake.launchd_calls(), [loaded(), bootout(), bootstrap()]);
    assert_eq!(fake.snapshot().job, path(NEW));
    assert_eq!(
        fake.record(),
        Record {
            markers: BTreeSet::from([Marker::Swapped]),
            ..in_flight(false)
        }
    );

    let verified = fake.host_run(Verify);
    assert_ok(&verified);
    assert!(verified.report.reversible);
    assert!(!verified.report.promotion_pending);
    let cleaned = fake.host_run(Cleanup);
    assert_ok(&cleaned);
    assert!(!cleaned.report.reversible);
    assert_eq!(
        fake.launchd_calls(),
        [loaded(), bootout(), bootstrap(), loaded(), loaded()]
    );
    assert!(fake.strays().is_empty());
    assert_eq!(
        fake.record(),
        Record {
            markers: BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]),
            to: path(NEW),
            version: Some(NEW_VERSION.to_owned()),
            ..Record::idle(None)
        }
    );
    assert_eq!(fake.live_locks(), 0);
}

/// Asserts that `prepare` reads the one configuration file the params name, writes the record
/// and does nothing else: no launchd call, no capability of any release asked, no instance
/// looked at. Catches a `prepare` that starts or stops the job, or reads another path.
#[test]
fn prepare_reads_only_the_configuration_file_and_records_the_update() {
    let fake = Fake::new();
    assert_ok(&fake.run(Prepare));
    assert_eq!(
        fake.log(),
        [
            "state.lock".to_owned(),
            "state.load".to_owned(),
            format!("host.config {}", super::fake::CONFIG),
            "state.save".to_owned(),
        ]
    );
}

/// Asserts that `prepare` succeeds on a node whose configuration has no `server.handoff_socket`:
/// a Mac does not hand over, so the key is not asked for. Catches a launchd `prepare` that holds
/// the hand-over requirement of the systemd arm.
#[test]
fn prepare_succeeds_when_the_configuration_names_no_handoff_socket() {
    let fake = Fake::new();
    fake.set_config(
        super::fake::CONFIG,
        Some("[server]\nlisten = \"127.0.0.1:9300\"\n"),
    );
    let done = fake.run(Prepare);
    assert_ok(&done);
    assert_eq!(fake.record(), in_flight(false));
}

/// Asserts (T23) that `prepare` of a node whose configuration has `[server] handoff_socket` and a
/// `[local_cache]` table succeeds as a restart: no release is asked for its capabilities or to
/// check the configuration (the fake fails any such call), the log is the read of the
/// configuration and the save, and the record has no mode. Catches a launchd `prepare` that picks
/// a hand-over for a cache shape, or asks a release.
#[test]
fn prepare_of_a_cache_shape_with_a_handoff_socket_is_a_restart_and_asks_no_release() {
    let fake = Fake::new();
    fake.set_config(
        super::fake::CONFIG,
        Some(
            "[server]\nhandoff_socket = \"/var/lib/tierstore/handoff.sock\"\n\n\
             [local_cache]\npath = \"/var/lib/tierstore/cache\"\n",
        ),
    );
    let done = fake.run(Prepare);
    assert_ok(&done);
    assert_eq!(fake.record(), in_flight(false));
    assert!(!fake.log().iter().any(|line| {
        line.starts_with("host.capabilities") || line.starts_with("host.check_config")
    }));
}

/// Asserts that `prepare` after a finished update starts a new one: the record at rest, with its
/// three markers, is replaced by an update in flight with none. Catches a `prepare` that treats
/// the commit of the last update as an update past its cutover.
#[test]
fn prepare_after_a_commit_starts_a_fresh_update() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify, Cleanup]);
    let done = fake.run(Prepare);
    assert_ok(&done);
    assert_eq!(fake.record(), in_flight(false));
}

/// Asserts that a `cutover` run again after it succeeded only looks: the job is asked about and
/// seen to serve the target, and it is not booted out. Catches a re-run that restarts a job that
/// serves, which would repeat the gap.
#[test]
fn a_second_cutover_leaves_a_serving_job_alone() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    let before = fake.launchd_calls().len();
    let done = fake.host_run(Cutover);
    assert_ok(&done);
    assert_eq!(fake.launchd_calls()[before..], [loaded()]);
}

/// Asserts that a `cutover` that finds the job already loaded and serving the target (the step
/// died after the bootstrap) records `swapped` and boots nothing. Catches a step that restarts
/// the job again, or that does not record what it saw.
#[test]
fn cutover_finding_the_target_served_records_swapped_without_a_restart() {
    let fake = Fake::new();
    fake.advance(&[Prepare]);
    fake.switch_current(NEW);
    fake.set_job(Some(NEW));
    assert_ok(&fake.run(Cutover));
    assert_eq!(fake.launchd_calls(), [loaded()]);
    assert!(fake.record().has(Marker::Swapped));
}

/// Asserts that a first install loads the job and never boots anything out: `cutover` asks
/// whether the job is loaded and bootstraps the plist, and the whole install makes no `bootout`.
/// Catches a first install that boots out a job that is not there, or that does not load one.
#[test]
fn a_first_install_bootstraps_without_any_bootout() {
    let fake = Fake::first_install();
    fake.advance(&[Prepare]);
    assert_eq!(fake.record(), in_flight(true));
    fake.advance(&[Cutover]);
    assert_eq!(fake.launchd_calls(), [loaded(), bootstrap()]);
    assert_eq!(fake.snapshot().job, path(NEW));
    fake.advance(&[Verify, Cleanup]);
    assert!(fake.launchd_calls().iter().all(|call| *call != bootout()));
    assert_eq!(fake.record().from, None);
    assert!(fake.record().committed());
    assert!(!fake.record().updating);
}

/// Asserts that a first install whose job is already loaded but does not serve the target is
/// neither booted out nor booted in again: the step waits on what is there and reports that no
/// request is answered by the target. Catches a first install that restarts a job it did not
/// load.
#[test]
fn a_first_install_never_boots_out_a_loaded_job() {
    let fake = Fake::first_install();
    fake.advance(&[Prepare]);
    fake.switch_current(NEW);
    fake.set_job(Some(NEW));
    fake.lie(NEW, "9.9.9");
    let done = fake.run(Cutover);
    assert_failed(&done, Reason::ProbeFailed);
    assert_eq!(fake.launchd_calls(), [loaded()]);
}

/// Asserts that a `cutover` whose job never answers `/readyz` fails with `not_ready` and leaves
/// the update on record without `swapped`, the job as it is (the hook does not roll back
/// there), and the lock released. Catches a cutover that records a swap it did not see or that
/// undoes itself.
#[test]
fn a_cutover_whose_job_never_answers_fails_not_ready_and_records_no_swap() {
    let fake = Fake::new();
    fake.advance(&[Prepare]);
    fake.mute(NEW);
    let done = fake.host_run(Cutover);
    assert_failed(&done, Reason::NotReady);
    assert!(done.report.reversible);
    assert_eq!(fake.record(), in_flight(false));
    assert_eq!(fake.launchd_calls(), [loaded(), bootout(), bootstrap()]);
    assert_eq!(fake.snapshot().job, path(NEW));
    assert_eq!(fake.live_locks(), 0);
}

/// Asserts that a `cutover` whose job answers with another build fails with `probe_failed` and
/// records no swap. Catches a cutover that takes any answer for the target's.
#[test]
fn a_cutover_whose_job_runs_another_build_fails_probe_failed() {
    let fake = Fake::new();
    fake.advance(&[Prepare]);
    fake.lie(NEW, "9.9.9");
    let done = fake.host_run(Cutover);
    assert_failed(&done, Reason::ProbeFailed);
    assert!(!fake.record().has(Marker::Swapped));
}

/// Asserts that `verify` on a node that still serves read-only succeeds with `promotion_pending`
/// and records `verified` once; a later `verify` that sees it read-write clears the flag and
/// writes nothing. Catches a verify that fails on a pending promotion, or one that rewrites
/// the record on every look.
#[test]
fn verify_reports_a_pending_promotion_and_records_verified_once() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.set_read_only(true);
    let pending = fake.host_run(Verify);
    assert_ok(&pending);
    assert!(pending.report.promotion_pending);
    assert!(fake.record().has(Marker::Verified));
    let saves = |fake: &Fake| fake.log().iter().filter(|c| *c == "state.save").count();
    let before = saves(&fake);
    fake.set_read_only(false);
    let done = fake.host_run(Verify);
    assert_ok(&done);
    assert!(!done.report.promotion_pending);
    assert_eq!(saves(&fake), before);
}

/// Asserts that a `verify` after the commit only observes: it succeeds on the serving target,
/// reports a pending promotion, makes no launchd call and writes no record; and on another build
/// it fails with `wrong_build`. Catches a post-commit verify that touches the job or the record.
#[test]
fn verify_after_the_commit_only_observes() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify, Cleanup]);
    let before = fake.log().len();
    fake.set_read_only(true);
    let done = fake.host_run(Verify);
    assert_ok(&done);
    assert!(done.report.promotion_pending);
    let calls = fake.log()[before..].to_vec();
    assert!(calls.iter().all(|c| !c.starts_with("launchd.")));
    assert!(calls.iter().all(|c| c != "state.save"));
    fake.lie(NEW, "9.9.9");
    assert_failed(&fake.host_run(Verify), Reason::WrongBuild);
}

/// Asserts that a `cleanup` run again after it finished succeeds and changes nothing. Catches a
/// cleanup that fails, or rewrites the record, once it has nothing left to do.
#[test]
fn cleanup_after_it_finished_succeeds_and_changes_nothing() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify, Cleanup]);
    let before = fake.snapshot();
    let saves = fake.log().iter().filter(|c| *c == "state.save").count();
    let done = fake.host_run(Cleanup);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!done.report.reversible);
    assert_eq!(fake.snapshot(), before);
    assert_eq!(
        fake.log().iter().filter(|c| *c == "state.save").count(),
        saves
    );
}
