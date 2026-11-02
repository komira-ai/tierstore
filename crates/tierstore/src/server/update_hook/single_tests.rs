//! The single-instance steps of a launchd host, over the fake of `single_testing`: the order of
//! the calls of an update, the rollback of a failed probe, what is skipped for want of a
//! hand-over, the gate of the commit, and the refusals of the session.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::record::{Marker, Record};
use super::single_testing::{
    DECLARATION, NEW, NEWER, OLD, PORT, Promotion, SingleFake, Snapshot, in_flight, params_for,
};
use super::steps::{PROBE_ATTEMPTS, PROBE_HITS, READY_ATTEMPTS};
use super::testing::DECLARATION as SLOT_DECLARATION;
use super::{
    Clock, Host, Link, Outcome, Reason, SKIPPED_CLEANUP, SKIPPED_CUTOVER, SKIPPED_PREPARE,
    SingleRecord, Step,
};

use Step::{Cleanup, Cutover, Prepare, Restore, Rollback, Verify};

fn path(release: &str) -> Option<PathBuf> {
    Some(PathBuf::from(release))
}

fn markers(list: &[Marker]) -> BTreeSet<Marker> {
    list.iter().copied().collect()
}

/// The log of the calls `step` makes, alone.
fn calls_of(fake: &SingleFake, step: Step) -> (super::Done, Vec<String>) {
    let before = fake.log().len();
    let done = fake.run(step);
    (done, fake.log()[before..].to_vec())
}

/// Asserts that an update runs as a plain restart, in this order: `prepare` records the update
/// and points `previous` at the running release and starts nothing; `cutover` stops the
/// service, points `current` at the staged release, starts the service, waits for `/readyz` and
/// probes the build on the one port, and records `swapped` last; `verify` and `cleanup` then
/// commit, remove `previous` and leave the new release running with nothing in flight. The
/// calls of `cutover` are compared whole. It would catch a start before the link is swapped (the
/// old release again), a swap while the old process runs, a `swapped` written before the probe,
/// and a cleanup that leaves `previous` behind.
#[test]
fn an_update_stops_the_service_swaps_current_starts_it_and_probes_it() {
    let fake = SingleFake::new();
    let (prepared, prepare_calls) = calls_of(&fake, Prepare);
    assert_eq!(prepared.outcome, Outcome::Success, "{prepared:?}");
    assert_eq!(
        prepare_calls,
        [
            "state.lock",
            "host.declaration",
            "state.load",
            &format!("links.check {NEW}"),
            "links.target current",
            &format!("links.check {OLD}"),
            "state.save",
            &format!("links.point previous {OLD}"),
        ]
    );
    assert_eq!(fake.snapshot().running, path(OLD));
    let (cut, cutover_calls) = calls_of(&fake, Cutover);
    assert_eq!(cut.outcome, Outcome::Success, "{cut:?}");
    let probe = vec!["get 9300 /metricz"; PROBE_HITS as usize];
    let mut expected = vec![
        "state.lock".to_owned(),
        "host.declaration".to_owned(),
        "state.load".to_owned(),
        "service.stop".to_owned(),
        format!("links.point current {NEW}"),
        "service.start".to_owned(),
        "get 9300 /readyz".to_owned(),
    ];
    expected.extend(probe.iter().map(|s| (*s).to_owned()));
    expected.push("state.save".to_owned());
    assert_eq!(cutover_calls, expected);
    assert_eq!(
        fake.snapshot(),
        Snapshot {
            record: Some(SingleRecord {
                markers: markers(&[Marker::Swapped]),
                ..in_flight()
            }),
            current: path(NEW),
            previous: path(OLD),
            running: path(NEW),
        }
    );
    fake.advance(&[Verify, Cleanup]);
    assert_eq!(
        fake.snapshot(),
        Snapshot {
            record: Some(SingleRecord {
                updating: false,
                from: None,
                markers: markers(&[Marker::Swapped, Marker::Verified, Marker::Stopping]),
                ..in_flight()
            }),
            current: path(NEW),
            previous: None,
            running: path(NEW),
        }
    );
    assert_eq!(fake.live_locks(), 0);
}

/// Asserts that when the new release answers with another version, `cutover` fails with
/// `probe_failed`, puts `previous` back as `current`, restarts the service on it (stop, link,
/// start) and says so in its detail, word for word; the update stays on record for the updater's
/// `rollback`, which then removes `previous` and clears the record without another restart. It
/// would catch a failed probe that leaves the bad release running, one that reports success, and
/// one that forgets the release to go back to.
#[test]
fn a_failed_probe_puts_previous_back_restarts_and_reports_rolled_back() {
    let fake = SingleFake::new();
    fake.lie(NEW, "9.9.9");
    fake.advance(&[Prepare]);
    let (done, calls) = calls_of(&fake, Cutover);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed));
    assert!(done.report.reversible);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "no request to {PORT} was answered by 0.4.0; rolled back: the service runs {OLD} again"
            )
            .as_str()
        )
    );
    let tail = &calls[calls.len() - 5..];
    assert_eq!(
        tail,
        [
            "links.target current",
            "service.stop",
            &format!("links.point current {OLD}"),
            "service.start",
            "get 9300 /readyz",
        ]
    );
    assert_eq!(
        fake.snapshot(),
        Snapshot {
            record: Some(in_flight()),
            current: path(OLD),
            previous: path(OLD),
            running: path(OLD),
        }
    );
    let starts = fake.starts();
    let back = fake.run(Rollback);
    assert_eq!(back.outcome, Outcome::Success, "{back:?}");
    assert_eq!(
        back.report.detail.as_deref(),
        Some(format!("rolled back: the service runs {OLD} again").as_str())
    );
    assert_eq!(fake.starts(), starts, "the service was restarted again");
    assert_eq!(
        fake.snapshot(),
        Snapshot {
            record: Some(SingleRecord::idle()),
            current: path(OLD),
            previous: None,
            running: path(OLD),
        }
    );
}

/// Asserts that a new release that never answers `/readyz` fails `cutover` with `not_ready`
/// after the steps' full wait, and is rolled back the same way. It would catch a wait that
/// gives up early, and one that does not roll back an instance that never came up.
#[test]
fn an_instance_that_never_answers_is_rolled_back_as_not_ready() {
    let fake = SingleFake::new();
    fake.mute(NEW);
    fake.advance(&[Prepare]);
    let pauses = fake.pauses();
    let done = fake.run(Cutover);
    assert_eq!(done.report.reason, Some(Reason::NotReady));
    assert_eq!(fake.pauses() - pauses, READY_ATTEMPTS);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "the instance on {PORT} does not answer /readyz; rolled back: the service runs {OLD} again"
            )
            .as_str()
        )
    );
    assert_eq!(fake.snapshot().running, path(OLD));
}

/// Asserts that when the rollback of a failed `cutover` fails too, the detail says that it did
/// and why, and the failure is still the probe's. It would catch a report that claims a rollback
/// that did not happen.
#[test]
fn a_rollback_that_fails_inside_cutover_is_reported_as_failed() {
    let fake = SingleFake::new();
    fake.lie(NEW, "9.9.9");
    fake.advance(&[Prepare]);
    // The cutover's seven calls up to its probe (the lock, the declaration, the record, the stop,
    // the link current, the start and /readyz), every look of the probe, and the rollback's look
    // at the link current: its stop is the call that fails.
    let to_stop = 7 + PROBE_ATTEMPTS as usize + 2;
    fake.fail_at(Some(to_stop));
    let done = fake.run(Cutover);
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed));
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "no request to {PORT} was answered by 0.4.0; the rollback to {OLD} failed: units: injected failure"
            )
            .as_str()
        )
    );
}

/// Asserts that every step of the single-instance path says in its detail what it skips of the
/// two-slot steps (the guard table, the conntrack check and the second instance in `prepare`,
/// the redirect in `cutover`, stopping a previous instance in `cleanup`), and that across a whole
/// update, a rollback and a restore no step asks for anything but the service, the two links,
/// the one port, the declaration, the staged release's check and the state directory: no boot
/// and no conntrack. The words of the details are spelt out here, not taken from the constants,
/// so that a detail that stops naming what it skips fails. It would catch a hand-over step
/// brought onto this path.
#[test]
fn the_steps_skip_what_needs_a_hand_over_and_say_so() {
    let fake = SingleFake::new();
    let prepared = fake.run(Prepare);
    let detail = prepared.report.detail.unwrap();
    assert_eq!(
        detail,
        format!(
            "prepared {NEW} to follow {OLD}, mode restart (single_instance); skipped: the guard table, the conntrack check and a second instance (one instance)"
        )
    );
    assert!(detail.ends_with(SKIPPED_PREPARE));
    let cut = fake.run(Cutover).report.detail.unwrap();
    assert_eq!(
        cut,
        format!(
            "the service runs {NEW} on {PORT}; skipped: the redirect (one instance on its own port)"
        )
    );
    assert!(cut.ends_with(SKIPPED_CUTOVER));
    fake.advance(&[Verify]);
    let clean = fake.run(Cleanup).report.detail.unwrap();
    assert_eq!(
        clean,
        format!("{NEW} serves alone; skipped: stopping a previous instance (there is none)")
    );
    assert!(clean.ends_with(SKIPPED_CLEANUP));
    let again = SingleFake::new();
    again.advance(&[Prepare, Cutover, Rollback]);
    again.reboot();
    again.advance(&[Restore]);
    for log in [fake.log(), again.log()] {
        for call in log {
            let allowed = [
                "state.",
                "links.",
                "service.",
                "host.declaration",
                "host.capabilities ",
                "get 9300 ",
            ]
            .iter()
            .any(|prefix| call.starts_with(prefix));
            assert!(allowed, "{call}");
        }
    }
}

/// Asserts that the fake host refuses the two questions a single-instance step never asks, so
/// that the test above would see a step that asked fail (the boot and the conntrack each fail
/// with a host error and are logged), and refuses the capabilities of a release the updater did
/// not place, as the host refuses a binary that is not root's. It would catch a fake that let a
/// step's stray question pass unseen.
#[test]
fn the_fake_host_refuses_what_a_single_step_never_asks() {
    let fake = SingleFake::new();
    assert!(fake.boot_id().is_err());
    assert!(fake.conntrack().is_err());
    assert!(fake.capabilities(&PathBuf::from("/elsewhere")).is_err());
    assert_eq!(
        fake.log(),
        [
            "host.boot_id",
            "host.conntrack",
            "host.capabilities /elsewhere"
        ]
    );
    assert_eq!(fake.now_ms(), 0);
}

/// Asserts that `prepare` refuses, with `host_failed` and before anything is written or linked, a
/// staged release the host does not vouch for (`Releases::check`: a path through a link, such as
/// one through `current` or `previous`, a relative or dangling one, or one another account could
/// change) and a host whose `current` names such a release. It would catch an update to a binary
/// another account could have written, and a rollback target that is not a release.
#[test]
fn prepare_refuses_a_staged_release_it_cannot_vouch_for() {
    for staged in [
        "/usr/local/tierstore/releases/unplaced",
        "/usr/local/tierstore/current",
        "/usr/local/tierstore/previous/x",
    ] {
        let fake = SingleFake::new();
        let before = fake.snapshot();
        let done = fake.run_with(Prepare, Some(&params_for(staged, "0.4.0")));
        assert_eq!(done.report.reason, Some(Reason::HostFailed), "{staged}");
        assert_eq!(fake.snapshot(), before, "{staged}");
        assert!(!fake.log().iter().any(|c| c.starts_with("service.")));
    }
    let fake = SingleFake::new();
    fake.distrust(OLD);
    let before = fake.snapshot();
    let done = fake.run(Prepare);
    assert_eq!(done.report.reason, Some(Reason::HostFailed), "{done:?}");
    assert_eq!(fake.snapshot(), before);
}

/// Asserts that `verify` waits for the new instance to say `read-write` for at most
/// `promotion_bound_secs` (30 looks a second apart) and succeeds with `promotion_pending` when it
/// does not, recording `verified` either way, and that a second `verify` changes nothing. It would
/// catch a verify that fails on a slow promotion (the updater's ceiling is its own), and one that
/// records `verified` twice.
#[test]
fn verify_waits_for_the_promotion_and_records_verified() {
    let fake = SingleFake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.set_promotion(Promotion::Stuck);
    let pauses = fake.pauses();
    let done = fake.run(Verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(done.report.promotion_pending);
    // The probe's pauses between its hits, then one a second for the 30 seconds of the bound.
    assert_eq!(fake.pauses() - pauses, PROBE_HITS - 1 + 30);
    assert!(fake.record().has(Marker::Verified));
    fake.set_promotion(Promotion::Instant);
    let before = fake.snapshot();
    let saves = fake.log().iter().filter(|c| *c == "state.save").count();
    let done = fake.run(Verify);
    assert!(!done.report.promotion_pending);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(format!("{PORT} is read-write").as_str())
    );
    assert_eq!(fake.snapshot(), before);
    assert_eq!(
        fake.log().iter().filter(|c| *c == "state.save").count(),
        saves
    );
}

/// Asserts that `verify` refuses before a `cutover` (`out_of_order`), when `current` names
/// another release than the staged one, when the port does not answer, and when it answers with
/// another version (`wrong_build`), and that a refusal records nothing. It would catch a verify
/// that passes a host the update never reached.
#[test]
fn verify_refuses_before_cutover_and_when_the_service_is_not_the_staged_release() {
    let fake = SingleFake::new();
    fake.advance(&[Prepare]);
    assert_eq!(fake.run(Verify).report.reason, Some(Reason::OutOfOrder));
    fake.advance(&[Cutover]);
    fake.set_link(Link::Current, Some(NEWER));
    let done = fake.run(Verify);
    assert_eq!(done.report.reason, Some(Reason::WrongBuild));
    assert_eq!(
        done.report.detail.as_deref(),
        Some(format!("the link current does not name {NEW}").as_str())
    );
    fake.set_link(Link::Current, Some(NEW));
    fake.lie(NEW, "0.3.9");
    assert_eq!(fake.run(Verify).report.reason, Some(Reason::WrongBuild));
    fake.mute(NEW);
    assert_eq!(fake.run(Verify).report.reason, Some(Reason::NotReady));
    assert!(!fake.record().has(Marker::Verified));
}

/// Asserts that `cleanup` refuses without a `verify` (`not_verified`) and, after one, refuses
/// (`not_confirmed`, nothing written, still reversible) when it cannot see afresh that `current`
/// names the staged release and the port answers with its version. It would catch a commit made
/// on the strength of an old observation.
#[test]
fn cleanup_commits_only_after_a_fresh_confirmation() {
    let fake = SingleFake::new();
    fake.advance(&[Prepare, Cutover]);
    let done = fake.run(Cleanup);
    assert_eq!(done.report.reason, Some(Reason::NotVerified));
    fake.advance(&[Verify]);
    let verified = fake.snapshot();
    fake.set_link(Link::Current, Some(NEWER));
    let done = fake.run(Cleanup);
    assert_eq!(done.report.reason, Some(Reason::NotConfirmed));
    assert!(done.report.reversible);
    fake.set_link(Link::Current, Some(NEW));
    assert_eq!(fake.snapshot(), verified);
    fake.lie(NEW, "0.3.0");
    let done = fake.run(Cleanup);
    assert_eq!(done.report.reason, Some(Reason::NotConfirmed));
    assert_eq!(
        done.report.detail.as_deref(),
        Some(format!("{NEW} is not seen to serve on {PORT}").as_str())
    );
    assert_eq!(fake.snapshot(), verified);
}

/// Asserts that once `cleanup` has written `stopping`, every report says `"reversible":false`,
/// `rollback` is refused with `committed` and changes nothing, `cutover` is out of order, and
/// `verify` still watches the port. It would catch a rollback after the commit, which would run
/// a release the updater already counts as gone.
#[test]
fn after_the_commit_nothing_is_reversible_and_rollback_is_refused() {
    let fake = SingleFake::new();
    fake.advance(&[Prepare, Cutover, Verify]);
    let done = fake.run(Cleanup);
    assert!(!done.report.reversible);
    let committed = fake.snapshot();
    let back = fake.run(Rollback);
    assert_eq!(back.report.reason, Some(Reason::Committed));
    assert!(!back.report.reversible);
    assert_eq!(fake.snapshot(), committed);
    assert_eq!(fake.run(Cutover).report.reason, Some(Reason::OutOfOrder));
    let watch = fake.run(Verify);
    assert_eq!(watch.outcome, Outcome::Success, "{watch:?}");
    assert!(!watch.report.reversible);
    fake.lie(NEW, "0.3.0");
    assert_eq!(fake.run(Verify).report.reason, Some(Reason::WrongBuild));
}

/// Asserts that a `rollback` after `prepare` alone restarts nothing (the service still runs the
/// release `current` names, and `start` of a running service changes nothing), removes
/// `previous` and clears the update; and that a `rollback` with nothing in flight does nothing.
/// It would catch a rollback that bounces a service it never changed.
#[test]
fn rollback_after_prepare_leaves_the_service_running_and_clears_the_update() {
    let fake = SingleFake::new();
    fake.advance(&[Prepare]);
    let (done, calls) = calls_of(&fake, Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!calls.contains(&"service.stop".to_owned()), "{calls:?}");
    assert_eq!(fake.starts(), 0);
    assert_eq!(
        fake.snapshot(),
        Snapshot {
            record: Some(SingleRecord::idle()),
            current: path(OLD),
            previous: None,
            running: path(OLD),
        }
    );
    let (idle, calls) = calls_of(&fake, Rollback);
    assert_eq!(
        idle.report.detail.as_deref(),
        Some("no update in flight: nothing to roll back")
    );
    assert_eq!(calls, ["state.lock", "host.declaration", "state.load"]);
}

/// Asserts that a `rollback` after `cutover` (and after `verify`) puts the record's `from` back as
/// `current`, restarts the service on it, removes `previous` and clears the update; and that one
/// whose `previous` link is missing (a crash of `prepare` before it made the link) goes by the
/// record all the same. It would catch a rollback that leaves the new release running, or one
/// that trusts a link over the record.
#[test]
fn rollback_after_cutover_puts_previous_back_and_restarts() {
    for setup in [&[Prepare, Cutover][..], &[Prepare, Cutover, Verify][..]] {
        let fake = SingleFake::new();
        fake.advance(setup);
        let done = fake.run(Rollback);
        assert_eq!(done.outcome, Outcome::Success, "{done:?}");
        assert_eq!(
            fake.snapshot(),
            Snapshot {
                record: Some(SingleRecord::idle()),
                current: path(OLD),
                previous: None,
                running: path(OLD),
            }
        );
    }
    let fake = SingleFake::new();
    fake.set_record(&in_flight());
    fake.set_link(Link::Current, Some(NEW));
    let done = fake.run(Rollback);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(format!("rolled back: the service runs {OLD} again").as_str())
    );
    assert_eq!(fake.snapshot().record, Some(SingleRecord::idle()));
    assert_eq!(fake.snapshot().current, path(OLD));
}

/// Asserts that `prepare` refuses a host whose `current` names no release (`no_slot`) and an
/// update past its cutover (`out_of_order`), and that a re-run of `prepare` keeps the `previous`
/// a first run made even when `current` has moved since (a crashed `cutover`). It would catch a
/// re-run that forgets the release to go back to.
#[test]
fn prepare_refuses_without_a_current_link_and_keeps_previous_on_a_rerun() {
    let fake = SingleFake::new();
    fake.set_link(Link::Current, None);
    assert_eq!(fake.run(Prepare).report.reason, Some(Reason::NoSlot));
    let fake = SingleFake::new();
    fake.advance(&[Prepare, Cutover]);
    let done = fake.run(Prepare);
    assert_eq!(done.report.reason, Some(Reason::OutOfOrder));
    let fake = SingleFake::new();
    fake.advance(&[Prepare]);
    fake.set_link(Link::Current, Some(NEW));
    fake.advance(&[Prepare]);
    assert_eq!(fake.snapshot().previous, path(OLD));
    let fake = SingleFake::new();
    fake.set_link(Link::Previous, Some(NEWER));
    fake.advance(&[Prepare]);
    assert_eq!(fake.snapshot().previous, path(OLD));
}

/// Asserts that a second update after a committed one runs from the start: `prepare` clears the
/// markers the first left and points `previous` at the release the first installed. It would
/// catch markers of the first update read as progress of the second.
#[test]
fn a_second_update_starts_afresh_after_the_first_commits() {
    let fake = SingleFake::new();
    fake.advance(&[Prepare, Cutover, Verify, Cleanup]);
    let next = params_for(NEWER, "0.5.0");
    for step in [Prepare, Cutover, Verify, Cleanup] {
        let done = fake.run_with(step, Some(&next));
        assert_eq!(done.outcome, Outcome::Success, "{step}: {done:?}");
        if step == Prepare {
            assert_eq!(fake.snapshot().previous, path(NEW));
            assert_eq!(
                fake.record(),
                SingleRecord::updating(
                    PathBuf::from(NEW),
                    PathBuf::from(NEWER),
                    "0.5.0".to_owned()
                )
            );
        }
    }
    assert_eq!(fake.snapshot().running, path(NEWER));
}

/// Asserts that `restore` starts the service on the release `current` names and changes neither
/// the links nor the record; does nothing without a record; and refuses with `no_slot` when
/// `current` names no release. It would catch a boot entry that moves the links, which the
/// updater's step table does not expect.
#[test]
fn restore_starts_the_service_and_changes_nothing_else() {
    let fake = SingleFake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.reboot();
    let before = fake.snapshot();
    let done = fake.run_with(Restore, None);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail.as_deref(),
        Some(format!("the service runs {NEW}").as_str())
    );
    assert_eq!(
        fake.snapshot(),
        Snapshot {
            running: path(NEW),
            ..before
        }
    );
    let fake = SingleFake::new();
    fake.set_record_bytes(None);
    let done = fake.run_with(Restore, None);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("no record: nothing to restore")
    );
    let fake = SingleFake::new();
    fake.set_link(Link::Current, None);
    assert_eq!(
        fake.run_with(Restore, None).report.reason,
        Some(Reason::NoSlot)
    );
}

/// A case of the session's refusals: its name, what is done to the host, the step and the
/// reason.
type Refusal = (&'static str, Box<dyn Fn(&SingleFake)>, Step, Reason);

/// Asserts what the session refuses before any service or link call: a held lock (`locked`), a
/// declaration of the two-slot hook (`state_invalid`), a declaration read under the lock that
/// names another state directory than the one locked (it changed between the two reads), a staged
/// release inside the state directory, no record, a record of the two-slot hook, a record whose markers are out of order,
/// and a step without its params (`bad_params`). It would catch a step that acts on what it
/// cannot trust.
#[test]
fn the_session_refuses_before_acting() {
    let cases: Vec<Refusal> = vec![
        (
            "busy",
            Box::new(|f| f.set_busy(true)),
            Cutover,
            Reason::Locked,
        ),
        (
            "slot declaration",
            Box::new(|f| f.set_declaration(SLOT_DECLARATION)),
            Prepare,
            Reason::StateInvalid,
        ),
        (
            "no record",
            Box::new(|f| f.set_record_bytes(None)),
            Prepare,
            Reason::StateInvalid,
        ),
        (
            "slot record",
            Box::new(|f| f.set_record_bytes(Some(&Record::idle(9301).encode()))),
            Rollback,
            Reason::StateInvalid,
        ),
        (
            "a declaration that names another state directory",
            Box::new(|f| f.set_declaration(&DECLARATION.replace("hooks/tierstore", "hooks/other"))),
            Cutover,
            Reason::StateInvalid,
        ),
        (
            "markers out of order",
            Box::new(|f| {
                f.set_record(&SingleRecord {
                    markers: markers(&[Marker::Verified]),
                    ..in_flight()
                });
            }),
            Rollback,
            Reason::StateInvalid,
        ),
    ];
    for (name, arrange, step, reason) in cases {
        let fake = SingleFake::new();
        arrange(&fake);
        let done = fake.run(step);
        assert_eq!(done.report.reason, Some(reason), "{name}: {done:?}");
        assert!(
            !fake
                .log()
                .iter()
                .any(|c| c.starts_with("service.") || c.starts_with("links.")),
            "{name}: {:?}",
            fake.log()
        );
        assert_eq!(fake.live_locks(), 0);
    }
    let fake = SingleFake::new();
    let inside = params_for("/var/db/updater/hooks/tierstore/new", "0.4.0");
    let done = fake.run_with(Prepare, Some(&inside));
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the staged release lies in the state directory")
    );
    let done = fake.run_with(Cutover, None);
    assert_eq!(done.report.reason, Some(Reason::BadParams));
}

/// Asserts that `cutover` and `verify` refuse the records they do not follow from: either with
/// no update in flight, and a `cutover` on a record a crashed `cleanup` left committed but still
/// in flight (`out_of_order`, irreversible, nothing changed). It would catch a restart after the
/// commit.
#[test]
fn cutover_and_verify_refuse_what_they_do_not_follow_from() {
    let fake = SingleFake::new();
    assert_eq!(fake.run(Cutover).report.reason, Some(Reason::OutOfOrder));
    assert_eq!(fake.run(Verify).report.reason, Some(Reason::OutOfOrder));
    let half_cleaned = SingleRecord {
        markers: markers(&[Marker::Swapped, Marker::Verified, Marker::Stopping]),
        ..in_flight()
    };
    fake.set_record(&half_cleaned);
    let done = fake.run(Cutover);
    assert_eq!(done.report.reason, Some(Reason::OutOfOrder));
    assert!(!done.report.reversible);
    assert_eq!(fake.record(), half_cleaned);
}

/// Asserts that a `prepare` interrupted after it recorded the update and before it replaced a
/// stale `previous` (one a crash of an older update left, naming another release), run again,
/// recorded and linked the release that ran, so that a `cutover` and a `rollback` end on it,
/// whether the interruption is a failed call or a stop of the world. It would catch a re-run that
/// trusts the stale link over the release `current` named.
#[test]
fn an_interrupted_prepare_over_a_stale_previous_rolls_back_to_the_release_that_ran() {
    for halt in [false, true] {
        let fake = SingleFake::new();
        fake.set_link(Link::Previous, Some(NEWER));
        // The lock, the declaration, the record, two checks and the link current, the save: the
        // eighth call is the link previous.
        if halt {
            fake.halt_at(Some(8));
        } else {
            fake.fail_at(Some(8));
        }
        let first = fake.run(Prepare);
        assert_eq!(first.report.reason, Some(Reason::LinkFailed), "{first:?}");
        assert_eq!(fake.record(), in_flight());
        assert_eq!(fake.snapshot().previous, path(NEWER));
        fake.halt_at(None);
        fake.fail_at(None);
        fake.advance(&[Prepare]);
        assert_eq!(fake.snapshot().previous, path(OLD));
        fake.advance(&[Cutover, Rollback]);
        let end = fake.snapshot();
        assert_eq!(
            (end.current, end.running),
            (path(OLD), path(OLD)),
            "halt {halt}"
        );
    }
}

/// Asserts that a write of `stopping` that reaches the disk and then reports a failure (a rename
/// made, its sync failed) is reported as the commit it is: `"reversible":false`, whether the
/// record then reads back or its read-back fails too; that a write of `stopping` that fails and
/// cannot be read back is undecided and so reported `"reversible":false` as well; and that in each
/// case the next `cleanup` goes forward and finishes. It would catch a report that tells the
/// updater it may still roll back a commit that may be on disk.
#[test]
fn a_commit_whose_write_reported_a_failure_is_irreversible() {
    // The lock, the declaration, the record, the link current, /readyz and the three probes:
    // the ninth call is the write of `stopping`, and the tenth the read-back.
    let cases: [(&str, Option<usize>, Option<usize>, bool); 3] = [
        ("written, read back", Some(9), None, true),
        ("written, read-back failed", Some(9), Some(10), true),
        ("not written, read-back failed", None, Some(9), false),
    ];
    for (name, late, fail, on_disk) in cases {
        let fake = SingleFake::new();
        fake.advance(&[Prepare, Cutover, Verify]);
        fake.fail_late_at(late);
        if let Some(n) = fail {
            // A failure from `fail` on: the write (when it is the ninth) and the read-back.
            fake.halt_at(Some(n));
        }
        let done = fake.run(Cleanup);
        assert_eq!(
            done.report.reason,
            Some(Reason::StateIo),
            "{name}: {done:?}"
        );
        assert!(!done.report.reversible, "{name}");
        assert_eq!(fake.record().committed(), on_disk, "{name}");
        fake.fail_late_at(None);
        fake.halt_at(None);
        fake.advance(&[Cleanup]);
        assert!(
            fake.record().committed() && !fake.record().updating,
            "{name}"
        );
        assert_eq!(fake.snapshot().previous, None, "{name}");
    }
}

/// Asserts that every step after `prepare` refuses params that are not the update on record,
/// with `bad_params` and before any service or link call: another release, another version of
/// the same release, and another release of the same version, for `cutover`, `verify` and
/// `cleanup`; and that a second `prepare` before the cutover records the new release, keeping
/// the release to go back to. It would catch a cutover to a release `prepare` never checked.
#[test]
fn later_steps_refuse_params_that_are_not_the_update_on_record() {
    let others = [
        params_for(NEWER, "0.5.0"),
        params_for(NEW, "0.4.1"),
        params_for(NEWER, "0.4.0"),
    ];
    for setup in [
        &[Prepare][..],
        &[Prepare, Cutover][..],
        &[Prepare, Cutover, Verify][..],
    ] {
        let step = [Cutover, Verify, Cleanup][setup.len() - 1];
        for other in &others {
            let fake = SingleFake::new();
            fake.advance(setup);
            let before = fake.snapshot();
            let from = fake.log().len();
            let done = fake.run_with(step, Some(other));
            assert_eq!(
                done.report.reason,
                Some(Reason::BadParams),
                "{step}: {done:?}"
            );
            assert_eq!(fake.snapshot(), before);
            let calls = &fake.log()[from..];
            assert!(
                !calls
                    .iter()
                    .any(|c| c.starts_with("service.") || c.starts_with("links.")),
                "{calls:?}"
            );
        }
    }
    let fake = SingleFake::new();
    fake.advance(&[Prepare]);
    let newer = params_for(NEWER, "0.5.0");
    assert_eq!(
        fake.run_with(Prepare, Some(&newer)).outcome,
        Outcome::Success
    );
    assert_eq!(
        fake.record(),
        SingleRecord::updating(PathBuf::from(OLD), PathBuf::from(NEWER), "0.5.0".to_owned())
    );
}

/// Asserts that a `rollback` whose release launchd starts but which never answers `/readyz`
/// fails with `not_ready` and keeps the update on record and the link `previous`, so that a
/// second `rollback`, once it answers, finishes; and that a failed `cutover` whose own rollback
/// meets the same says so in its detail. It would catch a rollback that clears the way back
/// before the release it went back to serves.
#[test]
fn a_rollback_whose_release_does_not_answer_keeps_the_update() {
    let fake = SingleFake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.mute(OLD);
    let done = fake.run(Rollback);
    assert_eq!(done.report.reason, Some(Reason::NotReady), "{done:?}");
    assert_eq!(
        done.report.detail.as_deref(),
        Some(format!("{OLD} does not answer /readyz on {PORT}").as_str())
    );
    assert!(fake.record().updating);
    assert_eq!(fake.snapshot().previous, path(OLD));
    fake.unmute(OLD);
    fake.advance(&[Rollback]);
    assert_eq!(fake.record(), SingleRecord::idle());
    let fake = SingleFake::new();
    fake.lie(NEW, "9.9.9");
    fake.mute(OLD);
    fake.advance(&[Prepare]);
    let done = fake.run(Cutover);
    let detail = done.report.detail.unwrap();
    assert!(
        detail.ends_with(&format!(
            "; the rollback to {OLD} failed: {OLD} does not answer /readyz on {PORT}"
        )),
        "{detail}"
    );
}

/// Asserts that a `cutover` run again after `swapped` looks again: with the service stopped since
/// (a reboot) it starts it and sees it serve, with no stop; with `current` moved to another
/// release it swaps back; and with the instance no longer answering it swaps again, fails, rolls
/// back to the release that ran and clears `swapped`, so that the record says no swap. It would
/// catch a re-run that reports a swap it no longer sees.
#[test]
fn a_cutover_run_again_looks_again() {
    let fake = SingleFake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.reboot();
    let (done, calls) = calls_of(&fake, Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!calls.contains(&"service.stop".to_owned()), "{calls:?}");
    assert_eq!(fake.snapshot().running, path(NEW));
    let fake = SingleFake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.set_link(Link::Current, Some(NEWER));
    fake.advance(&[Cutover]);
    assert_eq!(fake.snapshot().current, path(NEW));
    let fake = SingleFake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.mute(NEW);
    let done = fake.run(Cutover);
    assert_eq!(done.report.reason, Some(Reason::NotReady), "{done:?}");
    assert_eq!(fake.record(), in_flight());
    let end = fake.snapshot();
    assert_eq!((end.current, end.running), (path(OLD), path(OLD)));
}
