//! The cases against the model of a node (`model.rs`): each passes against a healthy node, and
//! fails, saying what failed, against a node with the defect it is the test of.

use std::time::Duration;

use super::model::{Model, unavailable};
use super::*;

fn refusal(code: Code, message: &str) -> Refusal {
    Refusal {
        code,
        message: message.to_owned(),
    }
}

/// Runs `case` against `model` with the plan's numbers.
async fn against(case: Case, model: &mut Model) -> Outcome {
    run(case, model, &Params::defaults()).await
}

/// Runs `case` against `model` with `params`.
async fn with(case: Case, model: &mut Model, params: &Params) -> Outcome {
    run(case, model, params).await
}

fn has(out: &Outcome, failure: &str) -> bool {
    out.failures.iter().any(|f| f == failure)
}

const LEFT: &str = "placements left 60s after the last Release";
const TOO_MANY_1024: &str = "the tree was not made: too many placements: 1024 are alive";

/// Asserts the names of the cases, in the order the driver runs them, and that each is found
/// by its name and nothing else is.
#[test]
fn every_case_has_its_name() {
    let names: Vec<&str> = Case::ALL.iter().map(|case| case.name()).collect();
    assert_eq!(
        names,
        [
            "t1-cycles",
            "t2-placements",
            "t3-stall",
            "restart",
            "upstream-errors",
            "disk-full"
        ]
    );
    for case in Case::ALL {
        assert_eq!(Case::of(case.name()), Some(case));
    }
    assert_eq!(Case::of("t4"), None);
}

/// Asserts that every case passes against a healthy node, at the plan's numbers, with what it
/// saw noted: T1's cycles, T2's refusals of the whole burst, T3's held requests and its idle
/// queues, the restart's two kills, the waits of the case of errors, the refusals of a full disk.
#[tokio::test]
async fn every_case_passes_against_a_healthy_node() {
    for case in Case::ALL {
        let mut model = Model::healthy();
        let out = against(case, &mut model).await;
        assert!(out.passed(), "{}: {:?}", case.name(), out.failures);
        assert!(
            out.notes
                .iter()
                .any(|n| n.starts_with("no placement left 0ns")),
            "{}: {:?}",
            case.name(),
            out.notes
        );
    }
    let notes = |case| async move { against(case, &mut Model::healthy()).await.notes };
    assert_eq!(notes(Case::Cycles).await[0], "2100 cycles in 0ns");
    assert!(notes(Case::Placements).await.contains(
        &"64 of 64 Mounts past 1024 placements were unavailable for too many placements".to_owned()
    ));
    let stall = notes(Case::Stall).await;
    assert_eq!(stall[0], "the idle resident memory: 200 MiB");
    assert!(
        stall.contains(&"the stall held 3000 requests of the node".to_owned()),
        "{stall:?}"
    );
    assert!(
        stall.contains(&"the queues were idle 0ns after the stall".to_owned()),
        "{stall:?}"
    );
    assert!(
        stall.contains(&"the peak resident memory: 456 MiB over 61 samples".to_owned()),
        "{stall:?}"
    );
    assert!(
        notes(Case::Restart)
            .await
            .contains(&"killed right after a Prepare, and again once a tree was placed".to_owned())
    );
    let errors = notes(Case::Errors).await;
    assert!(
        errors[0].starts_with("480 answers were unavailable while the upstream failed"),
        "{errors:?}"
    );
    let disk = notes(Case::Disk).await;
    assert_eq!(disk[0], "the cache's disk filled with 3 MiB");
    assert_eq!(
        disk[1],
        "with the disk full: 0 trees read, 0 reads failed, 5 refused with a reason"
    );
}

/// Catches a Release that leaves the place counted while the mount holds the tree
/// (komira-ai/tierstore#619 undone): T1 fails at cycle 1025, the Mount refused for too many
/// placements, and T2 cannot give the burst its trees within 60 s of the Releases; every case
/// finds placements left after its last Release.
#[tokio::test]
async fn a_release_that_keeps_the_place_fails_t1_and_t2() {
    let keeps = || {
        let mut model = Model::healthy();
        model.release_keeps_place = true;
        model
    };
    let t1 = against(Case::Cycles, &mut keeps()).await;
    assert_eq!(
        t1.failures[0],
        format!("cycle 1025 of 2100: Mount: Unavailable: {TOO_MANY_1024}")
    );
    assert_eq!(t1.failures[1], format!("1024 {LEFT}"));

    let t2 = against(Case::Placements, &mut keeps()).await;
    assert_eq!(
        t2.failures[0],
        format!(
            "js.t2-burst-0 within 60s of the Releases: Mount: Unavailable: no tree by the agent's deadline; the last answer: {TOO_MANY_1024}"
        )
    );
    assert_eq!(t2.failures.len(), 65, "{:?}", t2.failures);

    for case in Case::ALL {
        let out = against(case, &mut keeps()).await;
        assert!(
            out.failures.iter().any(|f| f.ends_with(LEFT)),
            "{}: {:?}",
            case.name(),
            out.failures
        );
    }
}

/// Catches a burst past `max_placements` that is not refused as the agent must see it: a Mount
/// that succeeds past the limit, and a refusal that is not "unavailable" for too many placements.
#[tokio::test]
async fn t2_fails_a_burst_mount_that_is_not_refused_unavailable_for_too_many() {
    let mut roomy = Model::healthy();
    roomy.max_placements = 2048;
    let out = against(Case::Placements, &mut roomy).await;
    assert_eq!(
        out.failures[0],
        "the burst's Mount of js.t2-burst-0 past 1024 placements answered /m/js.t2-burst-0"
    );
    assert!(out.notes.contains(
        &"0 of 64 Mounts past 1024 placements were unavailable for too many placements".to_owned()
    ));

    let mut refusing = Model::healthy();
    refusing.too_many_answer = Some(refusal(Code::FailedPrecondition, "no room"));
    let out = against(Case::Placements, &mut refusing).await;
    assert_eq!(
        out.failures[0],
        "the burst's Mount of js.t2-burst-0 past 1024 placements: FailedPrecondition: no room; not unavailable for too many placements"
    );
}

/// Asserts that T2 says which step failed when holding its sources or starting its burst: a
/// held source's Prepare refused, its read not the tree's, a burst's Prepare refused; and each
/// Release refused.
#[tokio::test]
async fn t2_names_the_step_that_failed() {
    let wrong = TreeFault::Differs {
        path: "d0000/f0000000".to_owned(),
        what: "0 bytes".to_owned(),
    };
    let mut model = Model::healthy();
    model
        .next
        .push_back(("prepare", "", refusal(Code::FailedPrecondition, "no")));
    let out = against(Case::Placements, &mut model).await;
    assert_eq!(
        out.failures,
        ["held source 1 of 1024: Prepare: FailedPrecondition: no"]
    );

    let mut model = Model::healthy();
    model.read_fault = Some(wrong);
    let out = against(Case::Placements, &mut model).await;
    assert_eq!(
        out.failures,
        ["held source 1 of 1024: the read: d0000/f0000000: 0 bytes"]
    );

    let mut model = Model::healthy();
    model
        .next
        .push_back(("prepare", "js.t2-burst-3", unavailable("busy")));
    let out = against(Case::Placements, &mut model).await;
    assert_eq!(
        out.failures,
        ["the burst's Prepare of js.t2-burst-3: Unavailable: busy"]
    );

    let mut model = Model::healthy();
    model
        .next
        .push_back(("release", "js.t2-held-7", unavailable("later")));
    model
        .next
        .push_back(("release", "js.t2-burst-9", unavailable("later")));
    let out = against(Case::Placements, &mut model).await;
    assert!(
        has(&out, "the Release of js.t2-held-7: Unavailable: later"),
        "{:?}",
        out.failures
    );
    assert!(
        has(&out, "the Release of js.t2-burst-9: Unavailable: later"),
        "{:?}",
        out.failures
    );
}

/// Asserts that T1 names the cycle and the step that failed: a read that is not the tree's, and
/// a Release refused.
#[tokio::test]
async fn t1_names_the_cycle_and_the_step_that_failed() {
    let mut model = Model::healthy();
    model.read_fault = Some(TreeFault::Unreadable {
        path: "link".to_owned(),
        why: "Input/output error (os error 5)".to_owned(),
    });
    let out = against(Case::Cycles, &mut model).await;
    assert_eq!(
        out.failures,
        [
            "cycle 1 of 2100: the read: link: Input/output error (os error 5)",
            "1 placements left 60s after the last Release",
        ]
    );
    let mut model = Model::healthy();
    model
        .next
        .push_back(("release", "js.t1-41", refusal(Code::Other(13), "a fault")));
    let out = against(Case::Cycles, &mut model).await;
    assert_eq!(
        out.failures,
        [
            "cycle 42 of 2100: Release: Other(13): a fault",
            "1 placements left 60s after the last Release",
        ]
    );
}

/// Catches a node with no bound on the heap of the reads it queues while the upstream stalls
/// (komira-ai/tierstore#613 undone): its resident memory passes idle plus 1 GiB.
#[tokio::test]
async fn t3_fails_a_node_whose_queued_reads_have_no_bound() {
    let mut model = Model::healthy();
    model.heap_bound = None;
    let out = against(Case::Stall, &mut model).await;
    assert!(!out.passed());
    assert!(
        out.failures[0].ends_with("MiB, over the idle 200 MiB and 1024 MiB"),
        "{:?}",
        out.failures
    );
    assert!(out.failures[0].starts_with("at "), "{:?}", out.failures);
}

/// Catches a memory that keeps growing to the end of the stall, under the bound: the growth over
/// the stall's last minute passes 32 MiB.
#[tokio::test]
async fn t3_fails_a_memory_that_does_not_level_off() {
    let mut model = Model::healthy();
    model.heap_bound = None;
    model.heap_per_prepare = 100 << 10;
    let params = Params {
        arrival: Duration::from_secs(300),
        ..Params::defaults()
    };
    let out = with(Case::Stall, &mut model, &params).await;
    assert_eq!(
        out.failures,
        [
            "the resident memory grew 58 MiB over the last 60s of the stall, over 32 MiB: it did not level off"
        ]
    );
}

/// Asserts that T3 proves nothing, and says so, when the stall held no request of the node; and
/// that it fails when the queues are not idle 120 s after the stall, or a Prepare is refused.
#[tokio::test]
async fn t3_fails_an_unreached_stall_busy_queues_and_refused_prepares() {
    let mut model = Model::healthy();
    model.stall_unreached = true;
    let out = against(Case::Stall, &mut model).await;
    assert_eq!(
        out.failures,
        ["the upstream held no request: the stall never reached the node's reads"]
    );

    let mut model = Model::healthy();
    model.queues_stuck = true;
    let out = against(Case::Stall, &mut model).await;
    assert_eq!(
        out.failures,
        [
            "the queues were not idle 120s after the stall: tierstore_provider_prefetch_queued_bytes 1"
        ]
    );

    let mut model = Model::healthy();
    model
        .next
        .push_back(("prepare", "js.t3-4", unavailable("full")));
    model
        .next
        .push_back(("prepare", "js.t3-9", unavailable("full")));
    model
        .next
        .push_back(("release", "js.t3-0", unavailable("later")));
    let out = against(Case::Stall, &mut model).await;
    assert_eq!(
        out.failures,
        [
            "Prepare 5 under the stall: Unavailable: full",
            "2 of 3000 Prepares refused under the stall",
            "the Release of js.t3-0: Unavailable: later",
            "1024 placements left 60s after the last Release",
        ]
    );
}

/// Asserts that the restart case passes when the restart drops the placements too (the agent's
/// asks prepare them again), and fails, naming the step, when a source after the restart is
/// refused, when the second source is never placed, and when a Prepare or a Release is refused.
#[tokio::test]
async fn the_restart_case_gets_each_tree_or_names_what_failed() {
    let mut model = Model::healthy();
    model.restart_drops_places = true;
    let out = against(Case::Restart, &mut model).await;
    assert!(out.passed(), "{:?}", out.failures);
    assert_eq!(
        model.calls.iter().filter(|c| *c == "restart").count(),
        2,
        "{:?}",
        model.calls
    );

    let mut model = Model::healthy();
    let released = refusal(Code::FailedPrecondition, "the source was released");
    model.next.push_back(("mount", "js.restart-0", released));
    model
        .next
        .push_back(("release", "js.restart-1", unavailable("later")));
    let out = against(Case::Restart, &mut model).await;
    assert_eq!(
        out.failures,
        [
            "js.restart-0 after the restart: Mount: FailedPrecondition: the source was released",
            "the Release of js.restart-1: Unavailable: later",
            "1 placements left 60s after the last Release",
        ]
    );

    let mut model = Model::healthy();
    model.max_placements = 1;
    let out = against(Case::Restart, &mut model).await;
    assert_eq!(
        out.failures,
        ["the second source was not placed within 60s of its Prepare"]
    );

    for (run, said) in [
        ("js.restart-0", "the first Prepare: Unavailable: no"),
        ("js.restart-1", "the second Prepare: Unavailable: no"),
    ] {
        let mut model = Model::healthy();
        model.next.push_back(("prepare", run, unavailable("no")));
        let out = against(Case::Restart, &mut model).await;
        assert_eq!(out.failures, [said]);
    }
}

/// Catches a node that, while the upstream answers only errors, fails a start instead of letting
/// it wait, or hands out a tree, or grows its memory past the bound; and asserts that a case
/// that asked nothing proves nothing and says so, and that a tree that never comes after the
/// errors, and a Release refused, are named.
#[tokio::test]
async fn the_case_of_errors_fails_a_start_that_would_not_wait() {
    let mut model = Model::healthy();
    model.failing_answer = refusal(Code::FailedPrecondition, "no tree list at cas/l/x");
    let out = against(Case::Errors, &mut model).await;
    assert_eq!(
        out.failures,
        [
            "js.errors-0 while the upstream failed: Mount: FailedPrecondition: no tree list at cas/l/x; not unavailable, so the start would not wait"
        ]
    );

    let mut model = Model::healthy();
    model.failing_serves = true;
    let out = against(Case::Errors, &mut model).await;
    assert_eq!(
        out.failures,
        ["js.errors-0 got a tree at /m/js.errors-0 while every read of the upstream failed"]
    );

    let mut model = Model::healthy();
    model.leak_per_read = 100 << 20;
    let out = against(Case::Errors, &mut model).await;
    assert!(
        out.failures
            .iter()
            .all(|f| f.ends_with("over the idle 300 MiB and 1024 MiB")),
        "{:?}",
        out.failures
    );
    assert!(!out.passed());

    let params = Params {
        errors_for: Duration::ZERO,
        ..Params::defaults()
    };
    let out = with(Case::Errors, &mut Model::healthy(), &params).await;
    assert_eq!(
        out.failures,
        ["no start waited: nothing asked while the upstream failed"]
    );

    let mut model = Model::healthy();
    model
        .next
        .push_back(("release", "js.errors-2", unavailable("later")));
    let out = against(Case::Errors, &mut model).await;
    assert_eq!(
        out.failures,
        [
            "the Release of js.errors-2: Unavailable: later",
            "1 placements left 60s after the last Release",
        ]
    );
}

/// Asserts that a full disk passes when each answer is a tree, a read the job sees fail, or a
/// refusal with its reason; and catches a wrong read, no answer, a node that dies, a Release
/// refused, and a node that gives no tree once the disk has room.
#[tokio::test]
async fn a_full_disk_is_a_clear_answer_and_no_crash() {
    let mut serving = Model::healthy();
    serving.full_answer = None;
    let out = against(Case::Disk, &mut serving).await;
    assert!(out.passed(), "{:?}", out.failures);
    assert_eq!(
        out.notes[1],
        "with the disk full: 5 trees read, 0 reads failed, 0 refused with a reason"
    );

    let mut unreadable = Model::healthy();
    unreadable.full_answer = None;
    unreadable.full_read = Some(TreeFault::Unreadable {
        path: "empty".to_owned(),
        why: "No space left on device (os error 28)".to_owned(),
    });
    let out = against(Case::Disk, &mut unreadable).await;
    assert!(out.passed(), "{:?}", out.failures);
    assert_eq!(
        out.notes[1],
        "with the disk full: 0 trees read, 5 reads failed, 0 refused with a reason"
    );

    let mut wrong = Model::healthy();
    wrong.full_answer = None;
    wrong.full_read = Some(TreeFault::Differs {
        path: "d0000/f0000001".to_owned(),
        what: "0 bytes that are not the tree's 65536".to_owned(),
    });
    let out = against(Case::Disk, &mut wrong).await;
    assert_eq!(out.failures.len(), 5);
    assert_eq!(
        out.failures[0],
        "js.disk-full-0 with the disk full: a wrong read: d0000/f0000001: 0 bytes that are not the tree's 65536"
    );

    let mut silent = Model::healthy();
    silent.full_answer = Some(refusal(Code::Unavailable, ""));
    let out = against(Case::Disk, &mut silent).await;
    assert_eq!(
        out.failures[0],
        "js.disk-full-0 with the disk full: Mount: Unavailable: ; not a refusal with its reason"
    );

    let mut dying = Model::healthy();
    dying.full_kills = true;
    let out = against(Case::Disk, &mut dying).await;
    assert_eq!(
        out.failures,
        [
            "js.disk-full-0 with the disk full: Mount: NoAnswer: no node; not a refusal with its reason",
            "the Release of js.disk-full-0 with the disk full: NoAnswer: no node",
            "the node died with its disk full, at cycle 1 of 5",
            "the node is not running at the end of the case",
        ]
    );

    let mut sticky = Model::healthy();
    sticky.full_sticks = true;
    sticky
        .next
        .push_back(("release", "js.disk-full-5", unavailable("later")));
    let out = against(Case::Disk, &mut sticky).await;
    assert_eq!(
        out.failures,
        [
            "js.disk-full-5 once the disk had room: Mount: Unavailable: no tree by the agent's deadline; the last answer: the tree was not made: No space left on device (os error 28)",
            "the Release of js.disk-full-5: Unavailable: later",
        ]
    );
}

/// Asserts that a failure of the rig itself ends the case as a failure naming it, whichever step
/// of the rig failed.
#[tokio::test]
async fn a_failure_of_the_rig_fails_the_case() {
    for (case, call) in [
        (Case::Cycles, "put"),
        (Case::Cycles, "placements"),
        (Case::Stall, "resident"),
        (Case::Stall, "serve_as"),
        (Case::Stall, "stalled"),
        (Case::Stall, "gauges"),
        (Case::Restart, "restart"),
        (Case::Disk, "fill_disk"),
        (Case::Disk, "free_disk"),
    ] {
        let mut model = Model::healthy();
        model.rig_fails = Some(call);
        let out = against(case, &mut model).await;
        assert_eq!(out.failures, [format!("the rig: {call} failed")], "{call}");
    }
}

/// Asserts what the memory makes of its samples: the growth from a time on is the largest sample
/// then less the first, 0 with no sample; the peak is the idle size with no sample.
#[test]
fn the_memory_judges_its_samples() {
    let mut memory = Memory {
        idle: 10,
        samples: Vec::new(),
    };
    assert_eq!(memory.growth_from(Duration::ZERO), 0);
    assert_eq!(memory.peak(), 10);
    let second = Duration::from_secs(1);
    memory.samples = vec![
        (second, 50),
        (2 * second, 40),
        (3 * second, 70),
        (4 * second, 60),
    ];
    assert_eq!(memory.growth_from(2 * second), 30);
    assert_eq!(memory.growth_from(5 * second), 0);
    assert_eq!(memory.peak(), 70);
    assert_eq!(
        memory.over(55),
        ["at 3s the resident memory was 0 MiB, over the idle 0 MiB and 0 MiB"]
    );
}

/// Catches a tree that comes, but past the deadline it was asked by: each Mount of a node that
/// takes 70 s to answer gives T2's burst its trees after the 60 s it has from the Releases.
#[tokio::test]
async fn a_tree_that_comes_past_its_deadline_fails() {
    let mut slow = Model::healthy();
    slow.mount_takes = Duration::from_secs(70);
    let params = Params {
        max_placements: 2,
        burst: 1,
        ..Params::defaults()
    };
    slow.max_placements = 2;
    let out = with(Case::Placements, &mut slow, &params).await;
    assert_eq!(
        out.failures,
        ["js.t2-burst-0 within 60s of the Releases: the tree came 10s past the deadline"]
    );
}

/// Asserts that a tree the cases cannot make (a shape out of bounds) fails each case as a
/// failure of the rig, naming the shape, before anything is asked of the node.
#[tokio::test]
async fn a_tree_that_cannot_be_made_fails_the_case() {
    let params = Params {
        cycle_files: 0,
        stall_files: 0,
        disk_files: 0,
        ..Params::defaults()
    };
    for case in Case::ALL {
        let mut model = Model::healthy();
        let out = with(case, &mut model, &params).await;
        assert_eq!(out.failures.len(), 1, "{}: {:?}", case.name(), out.failures);
        assert!(
            out.failures[0].starts_with("the rig: a tree of 0 files of "),
            "{}: {:?}",
            case.name(),
            out.failures
        );
        assert!(model.calls.is_empty(), "{}: {:?}", case.name(), model.calls);
    }
}

/// Asserts that a Prepare answered "unavailable" is one the agent waits on and asks again, as a
/// Mount so answered is: the case of errors counts it as a wait, and the tree comes after it once
/// the disk has room again.
#[tokio::test]
async fn a_prepare_answered_unavailable_is_asked_again() {
    let mut model = Model::healthy();
    model
        .next
        .push_back(("prepare", "js.errors-0", unavailable("busy")));
    let out = against(Case::Errors, &mut model).await;
    assert!(out.passed(), "{:?}", out.failures);

    let mut model = Model::healthy();
    model
        .next
        .push_back(("prepare", "js.disk-full-5", unavailable("busy")));
    let out = against(Case::Disk, &mut model).await;
    assert!(out.passed(), "{:?}", out.failures);
    let asked = model
        .calls
        .iter()
        .filter(|c| *c == "Prepare js.disk-full-5")
        .count();
    assert_eq!(asked, 2, "{:?}", model.calls);
}

/// Catches an upstream that goes on failing after the case of errors has it serve again: no
/// source gets its tree within the agent's budget.
#[tokio::test]
async fn the_case_of_errors_fails_when_no_tree_comes_after_the_errors() {
    let mut model = Model::healthy();
    model.failing_sticks = true;
    let out = against(Case::Errors, &mut model).await;
    assert_eq!(out.failures.len(), 8, "{:?}", out.failures);
    assert_eq!(
        out.failures[0],
        "js.errors-0 once the upstream served: Mount: Unavailable: no tree by the agent's deadline; the last answer: the read of cas/l/... failed: 503 SlowDown"
    );
}

/// Catches a burst Mount refused "unavailable" for a reason other than too many placements: the
/// agent waits, but the node did not say why it cannot place the tree.
#[tokio::test]
async fn t2_fails_a_burst_refused_unavailable_for_another_reason() {
    let mut model = Model::healthy();
    model.too_many_answer = Some(unavailable("busy"));
    let out = against(Case::Placements, &mut model).await;
    assert_eq!(
        out.failures[0],
        "the burst's Mount of js.t2-burst-0 past 1024 placements: Unavailable: busy; not unavailable for too many placements"
    );
}

/// Asserts that after the restart the case reads each tree the node answers the agent's Mount
/// with, and names a read that is not the tree's.
#[tokio::test]
async fn the_restart_case_reads_each_tree_it_is_answered() {
    let mut model = Model::healthy();
    model.read_fault = Some(TreeFault::Differs {
        path: "empty".to_owned(),
        what: "1 bytes, not an empty file".to_owned(),
    });
    let out = against(Case::Restart, &mut model).await;
    assert_eq!(
        out.failures,
        [
            "js.restart-0 after the restart: the read: empty: 1 bytes, not an empty file",
            "js.restart-1 after the restart: the read: empty: 1 bytes, not an empty file",
        ]
    );
}
