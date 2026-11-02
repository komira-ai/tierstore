//! The single-instance steps of a launchd host, over the fake of `single_testing`: the order of
//! the calls of an update, the rollback of a failed probe, what is skipped for want of a
//! hand-over, the gate of the commit, and the refusals of the session.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::record::{Marker, Record};
use super::single_testing::{
    DECLARATION, NEW, NEWER, OLD, PORT, Promotion, SingleFake, Snapshot, params_for,
};
use super::steps::{PROBE_ATTEMPTS, PROBE_HITS, READY_ATTEMPTS};
use super::testing::DECLARATION as SLOT_DECLARATION;
use super::{
    Clock, Host, Link, Outcome, Reason, RecordError, SKIPPED_CLEANUP, SKIPPED_CUTOVER,
    SKIPPED_PREPARE, SingleRecord, Step,
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
            "links.target current",
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
        "links.target previous".to_owned(),
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
                ..SingleRecord::updating()
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
                markers: markers(&[Marker::Swapped, Marker::Verified, Marker::Stopping]),
                ..SingleRecord::idle()
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
            "links.target previous",
            "links.target current",
            "service.stop",
            &format!("links.point current {OLD}"),
            "service.start",
        ]
    );
    assert_eq!(
        fake.snapshot(),
        Snapshot {
            record: Some(SingleRecord::updating()),
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
    // The cutover's eight calls up to its probe (the lock, the declaration, the record, the link
    // previous, the stop, the link current, the start and /readyz), every look of the probe, and
    // the rollback's two looks at the links: its stop is the call that fails.
    let to_stop = 8 + PROBE_ATTEMPTS as usize + 3;
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
/// the one port, the declaration and the state directory: no boot, no conntrack, no
/// capabilities. It would catch a hand-over step brought onto this path.
#[test]
fn the_steps_skip_what_needs_a_hand_over_and_say_so() {
    let fake = SingleFake::new();
    let prepared = fake.run(Prepare);
    let detail = prepared.report.detail.unwrap();
    assert_eq!(
        detail,
        format!(
            "prepared {NEW} to follow {OLD}, mode restart (single_instance); {SKIPPED_PREPARE}"
        )
    );
    let cut = fake.run(Cutover).report.detail.unwrap();
    assert_eq!(
        cut,
        format!("the service runs {NEW} on {PORT}; {SKIPPED_CUTOVER}")
    );
    fake.advance(&[Verify]);
    let clean = fake.run(Cleanup).report.detail.unwrap();
    assert_eq!(clean, format!("{NEW} serves alone; {SKIPPED_CLEANUP}"));
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
                "get 9300 ",
            ]
            .iter()
            .any(|prefix| call.starts_with(prefix));
            assert!(allowed, "{call}");
        }
    }
}

/// Asserts that the fake host refuses the three questions a single-instance step never asks,
/// so that the test above would see a step that asked fail: the boot, the conntrack and the
/// capabilities each fail with a host error and are logged.
#[test]
fn the_fake_host_refuses_what_a_single_step_never_asks() {
    let fake = SingleFake::new();
    assert!(fake.boot_id().is_err());
    assert!(fake.conntrack().is_err());
    assert!(fake.capabilities(&PathBuf::from(NEW)).is_err());
    assert_eq!(
        fake.log(),
        [
            "host.boot_id",
            "host.conntrack",
            &format!("host.capabilities {NEW}")
        ]
    );
    assert_eq!(fake.now_ms(), 0);
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

/// Asserts that a `rollback` after `cutover` (and after `verify`) puts `previous` back as
/// `current`, restarts the service on it, removes `previous` and clears the update; and that one
/// that finds no `previous` (a crash of `prepare` before it made the link) only makes sure the
/// service runs. It would catch a rollback that leaves the new release running.
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
    fake.set_record(&SingleRecord::updating());
    let done = fake.run(Rollback);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("rolled back: current was never changed, and the service runs")
    );
    assert_eq!(fake.snapshot().record, Some(SingleRecord::idle()));
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
            assert_eq!(fake.record(), SingleRecord::updating());
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
/// declaration of the two-slot hook (`state_invalid`), a staged release inside the state
/// directory, no record, a record of the two-slot hook, a record whose markers are out of order,
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
            "markers out of order",
            Box::new(|f| {
                f.set_record(&SingleRecord {
                    markers: markers(&[Marker::Verified]),
                    ..SingleRecord::updating()
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

/// Asserts the record's bytes and its rules: the idle record is exactly the JSON the docs show,
/// it reads back as itself, a record of the two-slot hook is refused here and this one there, a
/// newer schema is refused as newer, and the marker rules refuse `verified` without `swapped`,
/// `stopping` without `verified`, and markers with no update in flight and no commit. It would
/// catch a hook that acts on the other platform's record.
#[test]
fn the_single_record_reads_back_and_refuses_the_other_kind() {
    let idle = SingleRecord::idle();
    assert_eq!(
        String::from_utf8(idle.encode()).unwrap(),
        "{\n  \"kind\": \"single\",\n  \"markers\": [],\n  \"schema\": 1,\n  \"updating\": false\n}\n"
    );
    assert_eq!(SingleRecord::decode(&idle.encode()), Ok(idle.clone()));
    assert!(matches!(
        SingleRecord::decode(&Record::idle(9301).encode()),
        Err(RecordError::Corrupt(_))
    ));
    assert!(matches!(
        Record::decode(&idle.encode()),
        Err(RecordError::Corrupt(_))
    ));
    assert_eq!(
        SingleRecord::decode(b"{\"schema\":2,\"kind\":\"single\"}"),
        Err(RecordError::Newer { found: 2 })
    );
    let rules = [
        (
            vec![Marker::Verified],
            true,
            "verified is set but swapped is not",
        ),
        (
            vec![Marker::Swapped, Marker::Stopping],
            true,
            "stopping is set but verified is not",
        ),
        (
            vec![Marker::Swapped],
            false,
            "markers are set with no update in flight",
        ),
    ];
    for (set, updating, why) in rules {
        let record = SingleRecord {
            updating,
            markers: markers(&set),
            ..SingleRecord::idle()
        };
        assert_eq!(record.check().unwrap_err().to_string(), why);
    }
    let committed = SingleRecord {
        markers: markers(&[Marker::Swapped, Marker::Verified, Marker::Stopping]),
        ..SingleRecord::idle()
    };
    assert_eq!(committed.check(), Ok(()));
    assert!(DECLARATION.contains("[9300]"));
}

/// Asserts that `cutover` and `verify` refuse the records they do not follow from: a `cutover`
/// with no update in flight, one whose update has no `previous` to go back to (a `prepare` that
/// crashed before making it), and one on a record a crashed `cleanup` left committed but still in
/// flight; a `verify` with nothing in flight. Each is `out_of_order` and changes nothing. It
/// would catch a swap with no way back, and a restart after the commit.
#[test]
fn cutover_and_verify_refuse_what_they_do_not_follow_from() {
    let fake = SingleFake::new();
    assert_eq!(fake.run(Cutover).report.reason, Some(Reason::OutOfOrder));
    assert_eq!(fake.run(Verify).report.reason, Some(Reason::OutOfOrder));
    fake.set_record(&SingleRecord::updating());
    let before = fake.snapshot();
    let done = fake.run(Cutover);
    assert_eq!(done.report.reason, Some(Reason::OutOfOrder));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the link previous names no release: prepare first")
    );
    assert_eq!(fake.snapshot(), before);
    let half_cleaned = SingleRecord {
        markers: markers(&[Marker::Swapped, Marker::Verified, Marker::Stopping]),
        ..SingleRecord::updating()
    };
    fake.set_record(&half_cleaned);
    let done = fake.run(Cutover);
    assert_eq!(done.report.reason, Some(Reason::OutOfOrder));
    assert!(!done.report.reversible);
    assert_eq!(fake.record(), half_cleaned);
}
