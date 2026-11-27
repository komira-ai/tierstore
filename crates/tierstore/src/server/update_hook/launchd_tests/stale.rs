//! A record of another update, in flight and not committed, whose updater is gone, on a Mac: the
//! next order's `prepare` (past that update's cutover) and `rollback` roll it back with its own
//! target and releases and then do their own step; a committed record is never rolled back; and
//! an order that races an update still in its cutover is refused as before.

use super::fake::{
    CONFIG, Fake, LABEL, NEW, NEW_VERSION, OLD, OLD_VERSION, assert_failed, assert_ok, params_for,
    path,
};
use crate::server::update_hook::{
    HANDOFF_EXEC_CAPABILITY, LAUNCHD_PLIST, Marker, Params, Reason, Record, Step,
};

use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// The release of the next order.
const NEWER: &str = "/var/lib/hostkeeper/packages/tierstore/0.5.0+000000000005";
/// Its version.
const NEWER_VERSION: &str = "0.5.0+000000000005";

/// The params of the next order, to `NEWER` from `previous`, the release `current` names.
fn next_order(previous: (&str, &str)) -> Params {
    params_for(NEWER, NEWER_VERSION, Some(previous))
}

/// The host after the update to `NEW` swapped and its updater went away, with the job loaded
/// again on `OLD` (the Mac booted, and the boot step loaded the release `current` names): the
/// record still has that update in flight, past its cutover.
fn left_by_a_gone_updater() -> Fake {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.set_installed(Some(OLD));
    fake.set_job(Some(OLD));
    fake.set_capabilities(NEWER, &[]);
    let stale = fake.record();
    assert_eq!(
        (
            stale.updating,
            stale.has(Marker::Swapped),
            stale.committed()
        ),
        (true, true, false)
    );
    fake
}

/// What the step says first when it rolled the update to `NEW` back, `how` being the rollback's
/// own words.
fn rolled_back(how: &str) -> String {
    format!(
        "the record is of the update to {NEW} {NEW_VERSION}, not {NEWER} {NEWER_VERSION}: rolled \
         back with its own target and releases ({how})"
    )
}

/// The launchd calls since `before`, but the looks at whether a job is loaded.
fn acts(fake: &Fake, before: usize) -> Vec<String> {
    fake.launchd_calls()[before..]
        .iter()
        .filter(|call| !call.starts_with("launchd.loaded"))
        .cloned()
        .collect()
}

/// Asserts that hostkeeper's `rollback` of the next order, after its `prepare` failed, rolls back
/// the update to `NEW` on record when the job already serves `OLD`: the binary of `OLD` is put
/// back, no job is booted out or in, and the record is at rest. Catches the `bad_params` that
/// made hostkeeper halt the service on a host that no update could reach again.
#[test]
fn the_rollback_of_the_next_order_rolls_back_a_stale_update_on_a_mac() {
    let fake = left_by_a_gone_updater();
    let before = fake.launchd_calls().len();
    let done = fake.run_with(Rollback, &next_order((OLD, OLD_VERSION)), LABEL);
    assert_ok(&done);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "{}; no update in flight: nothing to roll back",
                rolled_back(&format!(
                    "{LABEL} already serves {OLD_VERSION}: nothing of that update runs"
                ))
            )
            .as_str()
        )
    );
    assert!(acts(&fake, before).is_empty());
    let end = fake.snapshot();
    assert_eq!(end.record, Some(Record::idle(None)));
    assert_eq!(end.installed, path(OLD));
    assert_eq!(end.job, path(OLD));
}

/// Asserts that the next order's `prepare`, which finds the update to `NEW` past its cutover and
/// the job still serving `NEW` (no boot since), rolls it back first by a restart onto `OLD`, then
/// prepares its own update, which goes through to its commit. Catches the `out_of_order` refusal
/// that made hostkeeper run the `rollback` that refused with `bad_params`.
#[test]
fn prepare_of_the_next_order_rolls_back_a_stale_update_whose_job_still_serves_it() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.set_capabilities(NEWER, &[]);
    assert_eq!(fake.snapshot().job, path(NEW));
    let before = fake.launchd_calls().len();
    let next = next_order((OLD, OLD_VERSION));
    let done = fake.run_with(Prepare, &next, LABEL);
    assert_ok(&done);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "{}; prepared {NEWER} to follow {OLD}: {LABEL} is restarted in cutover",
                rolled_back(&format!("rolled back: {LABEL} serves {OLD_VERSION}"))
            )
            .as_str()
        )
    );
    assert_eq!(
        acts(&fake, before),
        [
            format!("launchd.bootout {LABEL}"),
            format!("launchd.bootstrap {LAUNCHD_PLIST}")
        ]
    );
    let record = fake.record();
    assert_eq!((record.updating, record.markers.len()), (true, 0));
    assert_eq!(record.to.as_deref(), path(NEWER).as_deref());
    assert_eq!(record.from.as_deref(), path(OLD).as_deref());
}

/// Asserts that the staged release of the update on record is not read when the job already
/// serves the release that update started from, even between releases that hand over (whose way
/// back would ask both): the steps of the next order name nothing of `NEW`, whose directory may
/// be gone. Catches a rollback that fails on a host that no longer has the staged release.
#[test]
fn a_stale_update_whose_staged_release_is_gone_is_rolled_back_without_it_on_a_mac() {
    let fake = left_by_a_gone_updater();
    for release in [OLD, NEW, NEWER] {
        fake.set_capabilities(release, &[HANDOFF_EXEC_CAPABILITY]);
    }
    let before = fake.log().len();
    let done = fake.run_with(Prepare, &next_order((OLD, OLD_VERSION)), LABEL);
    assert_ok(&done);
    assert!(!fake.log()[before..].iter().any(|call| call.contains(NEW)));
    assert_eq!(fake.record().to.as_deref(), path(NEWER).as_deref());
}

/// Asserts that a committed record still in flight is never rolled back by the next order, even
/// with params that name the release the update started from: `prepare` is `out_of_order` and
/// `rollback` is `committed`, and nothing is touched. Catches a rule that rolls back what
/// `cleanup` has committed.
#[test]
fn a_committed_stale_update_is_never_rolled_back_on_a_mac() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify]);
    fake.set_record(&fake.record().with(Marker::Stopping));
    fake.set_capabilities(NEWER, &[]);
    let before = fake.snapshot();
    let calls = fake.launchd_calls().len();
    let next = next_order((OLD, OLD_VERSION));
    assert_failed(&fake.run_with(Prepare, &next, LABEL), Reason::OutOfOrder);
    assert_failed(&fake.run_with(Rollback, &next, LABEL), Reason::Committed);
    assert_eq!(fake.snapshot(), before);
    assert_eq!(fake.launchd_calls().len(), calls);
}

/// Asserts that an order that races an update still in its cutover (its params name `NEW`, which
/// hostkeeper made `current` before that cutover, as the release the host runs) is refused as
/// before, its `rollback` saying why the record is kept, and that `cutover`, `verify` and
/// `cleanup` of another update never roll back, whatever release their params name. Nothing is
/// touched. Catches a second order that rolls back an update whose updater is alive.
#[test]
fn an_order_that_races_an_update_in_its_cutover_is_refused_on_a_mac() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify]);
    let before = fake.snapshot();
    let calls = fake.launchd_calls().len();
    let racing = next_order((NEW, NEW_VERSION));
    assert_failed(&fake.run_with(Prepare, &racing, LABEL), Reason::OutOfOrder);
    let refused = fake.run_with(Rollback, &racing, LABEL);
    assert_failed(&refused, Reason::BadParams);
    assert_eq!(
        refused.report.detail.as_deref(),
        Some(
            format!(
                "the params name {NEWER} {NEWER_VERSION}, and the update on record is \
                 Some({NEW:?}) Some({NEW_VERSION:?}): the update to {NEW_VERSION} returns the \
                 host to {OLD}, and the params name {NEW} as the release the host runs: it is \
                 rolled back only by params that name the release it started from"
            )
            .as_str()
        )
    );
    for step in [Cutover, Verify, Cleanup] {
        let done = fake.run_with(step, &next_order((OLD, OLD_VERSION)), LABEL);
        assert_failed(&done, Reason::BadParams);
    }
    assert_eq!(fake.snapshot(), before);
    assert_eq!(fake.launchd_calls().len(), calls);
}

/// Asserts that a stale rollback that fails is reported with what was asked, and leaves the
/// record as it was, so that a run again goes on from there.
#[test]
fn a_stale_rollback_that_fails_keeps_the_record_on_a_mac() {
    let fake = left_by_a_gone_updater();
    let stale = fake.record();
    fake.fail_nth("host.install_binary", 1);
    let done = fake.run_with(Rollback, &next_order((OLD, OLD_VERSION)), LABEL);
    assert_failed(&done, Reason::HostFailed);
    assert!(
        done.report
            .detail
            .as_deref()
            .is_some_and(|said| said.starts_with(&format!(
                "the record is of the update to {NEW} {NEW_VERSION}, not {NEWER} {NEWER_VERSION}, \
             and its rollback with its own target and releases is not done; "
            ))),
        "{done:?}"
    );
    assert_eq!(fake.record(), stale);
}

/// Asserts that when the job is not loaded (it was booted out and never in again), the update on
/// record is rolled back as its own `rollback` does: the job is booted in on `OLD`, which serves,
/// and the record is at rest. Catches a way back that takes a job that is not loaded for one that
/// serves the previous release.
#[test]
fn a_stale_update_whose_job_is_not_loaded_is_rolled_back_by_loading_it() {
    let fake = left_by_a_gone_updater();
    fake.set_job(None);
    let before = fake.launchd_calls().len();
    let done = fake.run_with(Rollback, &next_order((OLD, OLD_VERSION)), LABEL);
    assert_ok(&done);
    assert!(
        done.report
            .detail
            .as_deref()
            .is_some_and(|said| said.starts_with(&rolled_back(&format!(
                "rolled back: {LABEL} serves {OLD_VERSION}"
            )))),
        "{done:?}"
    );
    assert_eq!(
        acts(&fake, before),
        [
            format!("launchd.bootout {LABEL}"),
            format!("launchd.bootstrap {LAUNCHD_PLIST}")
        ]
    );
    let end = fake.snapshot();
    assert_eq!(end.job, path(OLD));
    assert_eq!(end.record, Some(Record::idle(None)));
}

/// Asserts that when the next order's own `prepare` fails after the update on record was rolled
/// back (the node's file is gone), the detail says both and the record stays at rest. Catches a
/// failure that hides the rollback it follows.
#[test]
fn a_prepare_that_fails_after_the_stale_rollback_leaves_the_record_at_rest_on_a_mac() {
    let fake = left_by_a_gone_updater();
    fake.set_config(CONFIG, None);
    let done = fake.run_with(Prepare, &next_order((OLD, OLD_VERSION)), LABEL);
    assert_failed(&done, Reason::ConfigMissing);
    assert!(
        done.report
            .detail
            .as_deref()
            .is_some_and(|said| said.starts_with(&rolled_back(&format!(
                "{LABEL} already serves {OLD_VERSION}: nothing of that update runs"
            )))),
        "{done:?}"
    );
    assert_eq!(fake.record(), Record::idle(None));
}
