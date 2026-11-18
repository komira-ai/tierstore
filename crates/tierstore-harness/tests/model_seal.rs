//! The seal model (design 10.3): the exhaustive scenarios of the replication row, the
//! two-sealers row, the weaker-acknowledgement row and the minority part of seal and roll, and
//! the variants that show the model has teeth. Every scenario is bounded (records, nodes,
//! losses, attempts: see the module documentation of `tierstore_harness::models::seal`) and
//! explored to the end.

use tierstore_format::ids::NodeId;
use tierstore_harness::models::seal::{Act, Config, SealModel, Sealer, Variant, World};
use tierstore_model::explore::{Limits, Model, Outcome, Succ, Trace, explore};

const A: NodeId = NodeId(1);
const B: NodeId = NodeId(2);
const C: NodeId = NodeId(3);

const MAX_STATES: usize = 2_000_000;

/// Explores `cfg` to the end with liveness; every rule is judged.
fn run(cfg: Config) -> Outcome {
    explore(&SealModel::new(cfg), Limits::new(MAX_STATES, true))
}

/// Explores `cfg` judging only the rule `only`.
fn run_only(cfg: Config, only: &'static str) -> Outcome {
    let mut limits = Limits::new(MAX_STATES, false);
    limits.only = Some(only);
    explore(&SealModel::new(cfg), limits)
}

/// Asserts the design's protocol passed the scenario completely: states explored, none left
/// out, no violation.
fn passes(cfg: Config) -> Outcome {
    let out = run(cfg);
    assert!(out.states > 0);
    assert!(!out.truncated, "the scenario outgrew its state bound");
    assert!(out.trace.is_none(), "violation: {:?}", out.trace);
    out
}

/// The violation the explorer reports.
fn violation(out: Outcome) -> Trace {
    assert!(!out.truncated, "the scenario outgrew its state bound");
    let Some(trace) = out.trace else {
        panic!("the explorer found no violation");
    };
    trace
}

/// The model with a goal: a state that satisfies `goal` is reported as rule `G`, so a test can
/// assert that a situation of the design is reachable (a scenario that passes only because the
/// interesting step never happens proves nothing).
struct Reach<'a> {
    inner: &'a SealModel,
    goal: fn(&World) -> bool,
}

impl Model for Reach<'_> {
    type S = World;
    type A = Act;

    fn init(&self) -> Vec<World> {
        self.inner.init()
    }

    fn successors(&self, s: &World, out: &mut Vec<Succ<World, Act>>) {
        self.inner.successors(s, out);
    }

    fn check(&self, s: &World) -> Option<String> {
        (self.goal)(s)
            .then(|| "G goal reached".to_string())
            .or_else(|| self.inner.check(s))
    }

    fn terminal(&self, s: &World) -> Option<String> {
        self.inner.terminal(s)
    }

    fn quiescent(&self, s: &World) -> bool {
        self.inner.quiescent(s)
    }

    fn converged(&self, s: &World) -> bool {
        self.inner.converged(s)
    }

    fn show_act(&self, a: &Act) -> String {
        self.inner.show_act(a)
    }

    fn show_state(&self, s: &World) -> String {
        self.inner.show_state(s)
    }
}

/// Asserts that a state satisfying `goal` is reachable in `cfg`, and returns the steps to it.
fn reaches(cfg: Config, goal: fn(&World) -> bool) -> Vec<(String, String)> {
    let model = SealModel::new(cfg);
    let mut limits = Limits::new(MAX_STATES, false);
    limits.only = Some("G");
    let out = explore(
        &Reach {
            inner: &model,
            goal,
        },
        limits,
    );
    let Some(trace) = out.trace else {
        panic!("the situation is not reachable");
    };
    assert_eq!(trace.rule(), "G");
    trace.steps
}

fn sym() -> Vec<(NodeId, NodeId)> {
    vec![(A, B), (B, A)]
}

/// Three copies, one record, the writer may crash and return, node 2 may take over.
fn crash3() -> Config {
    Config {
        records: 1,
        crash_writer: true,
        attempts: [0, 1, 0],
        ..Config::default()
    }
}

/// Two copies and a voter, two records, the writer may crash and return, node 2 may take over.
fn crash2() -> Config {
    Config {
        replicas: 2,
        min_acks: 2,
        crash_writer: true,
        attempts: [0, 1, 0],
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Replication under a crash of the writer (10.3 steps 1 to 3, take-over)
// ---------------------------------------------------------------------------------------------

/// With `min_acks = replicas` and three copies, a crash of the writer at every step (and its
/// return) with node 2 taking over by majority never breaks a rule: every acknowledged record is
/// within the committed length, one seal, the fence holds, and every copy converges.
///
/// Catches: a lost acknowledged write, a split brain, a resurrected tail.
#[test]
fn a_crash_of_the_writer_at_each_step_loses_no_acknowledged_record_with_three_copies() {
    passes(crash3());
}

/// The same with two copies and a voter, two records, and one lost message.
#[test]
fn a_crash_of_the_writer_at_each_step_loses_no_acknowledged_record_with_two_copies() {
    passes(crash2());
    passes(Config {
        records: 1,
        max_losses: 1,
        ..crash2()
    });
}

/// The three crash points of the row are states of the explored graph: the writer crashes
/// before its local sync (a replica already holds the record), after its local sync and
/// before any acknowledgement, and after every copy synced and before the client's
/// acknowledgement.
#[test]
fn the_crash_points_of_the_writer_are_all_explored() {
    reaches(crash3(), |w| {
        w.crashed && w.nodes[0].len == 0 && w.nodes[1].len > 0
    });
    reaches(crash3(), |w| {
        w.crashed && w.nodes[0].len > 0 && w.acked == 0
    });
    reaches(crash3(), |w| {
        w.crashed && w.acked == 0 && w.nodes.iter().all(|n| n.len > 0)
    });
}

/// A replica that takes over seals by majority while an acknowledged record exists, and the
/// seal covers it (the scenario passes S3 and reaches this state).
#[test]
fn a_replica_taking_over_seals_by_majority_and_keeps_every_acknowledged_record() {
    let steps = reaches(crash3(), |w| w.acked > 0 && w.nodes[1].committed.is_some());
    assert!(steps.iter().any(|(act, _)| act.starts_with("StartSeal")));
}

/// The returned old writer adopts the committed seal, and a batch under its old ballot is
/// rejected by a replica that promised the taking-over ballot (the fence engaged).
#[test]
fn the_returned_old_writer_adopts_the_seal_and_its_old_ballot_batches_are_rejected() {
    reaches(crash3(), |w| {
        w.crashed && w.nodes[0].up && w.nodes[0].committed.is_some()
    });
    reaches(crash3(), |w| w.fenced);
    reaches(crash3(), |w| w.crashed && w.nodes[0].up && w.fenced);
}

/// The model with three records runs to the end too (offsets 0, 3, 5, 9).
#[test]
fn three_records_under_the_partition_pass() {
    passes(Config {
        records: 3,
        cuts: sym(),
        ..Config::default()
    });
}

// ---------------------------------------------------------------------------------------------
// Two sealers under a partition (10.3 step 4)
// ---------------------------------------------------------------------------------------------

/// Writer A and replica B cut off from each other, each sealing, with every ordering of their
/// prepare and accept messages to C and one lost message: at most one seal commits, every
/// acknowledged record is within it, a destage would see only the committed proposal, and every
/// copy converges; at the default `min_acks` and with `min_acks = 2`.
#[test]
fn two_sealers_cut_off_from_each_other_commit_at_most_one_seal() {
    passes(Config {
        cuts: sym(),
        max_losses: 1,
        ..Config::default()
    });
    passes(Config {
        cuts: sym(),
        max_losses: 1,
        min_acks: 2,
        ..Config::default()
    });
}

/// The two sealers really propose different things: a state exists in which A and B each
/// accepted its own proposal, of different lengths, and still only one can commit.
#[test]
fn the_two_sealers_propose_different_lengths() {
    let cfg = Config {
        cuts: sym(),
        min_acks: 2,
        ..Config::default()
    };
    reaches(cfg, |w| {
        let (a, b) = (w.nodes[0].accepted, w.nodes[1].accepted);
        a.is_some() && b.is_some() && a.map(|x| x.1) != b.map(|x| x.1)
    });
}

/// Asymmetric partitions (A reaches C and B does not; B reaches C and A does not; C reaches
/// only one of them), with one lost message: the same properties hold.
#[test]
fn asymmetric_partitions_keep_one_seal() {
    let cuts = [
        vec![(A, B), (B, A), (B, C)],
        vec![(A, B), (B, A), (A, C)],
        vec![(A, B), (B, A), (C, B), (B, C)],
        vec![(A, B), (B, A), (C, A), (A, C)],
        vec![(A, B), (B, A), (C, A)],
        vec![(A, B), (B, A), (C, B)],
    ];
    for cuts in cuts {
        for min_acks in [3, 2] {
            passes(Config {
                cuts: cuts.clone(),
                max_losses: 1,
                min_acks,
                ..Config::default()
            });
        }
    }
}

/// A later sealer adopts the committed proposal and never changes it: A seals twice and B once
/// in a partitioned cluster, and a node that started its second attempt ends with the one seal.
#[test]
fn a_later_sealer_adopts_the_committed_proposal() {
    let cfg = Config {
        cuts: sym(),
        attempts: [2, 1, 0],
        records: 1,
        ..Config::default()
    };
    passes(cfg.clone());
    reaches(cfg, |w| {
        w.nodes[0].attempts == 2 && w.nodes[0].committed.is_some()
    });
}

/// With `replicas = 3, min_acks = 2` and one acknowledging copy silent (C's replies never reach
/// A), a take-over by B still serves every acknowledged record.
#[test]
fn a_silent_acknowledging_copy_does_not_lose_a_record() {
    passes(Config {
        cuts: vec![(C, A)],
        min_acks: 2,
        attempts: [0, 1, 0],
        ..Config::default()
    });
}

/// With one copy whose crc fails inside its acknowledged prefix, the copy is excluded from
/// Choose and is repaired: the scenario passes (every copy converges) and reaches a state in
/// which the damaged copy was repaired after the commit.
#[test]
fn a_copy_with_damage_is_excluded_from_choose_and_repaired() {
    let cfg = Config {
        cuts: sym(),
        damage: Some(C),
        ..Config::default()
    };
    passes(cfg.clone());
    reaches(cfg, |w| {
        w.damaged && !w.nodes[2].bad && w.nodes[2].committed.is_some() && w.nodes[2].len > 0
    });
}

/// A node alone in its partition seals nothing and acknowledges nothing: B cut from both others
/// starts a seal and never gets a majority; A cut from both others never acknowledges (its
/// client gets 503). No rule is broken and no seal commits.
#[test]
fn a_node_alone_in_its_partition_seals_nothing_and_acknowledges_nothing() {
    let alone_b = Config {
        cuts: vec![(A, B), (B, A), (B, C), (C, B)],
        attempts: [0, 1, 0],
        ..Config::default()
    };
    passes(alone_b.clone());
    let preparing = reaches(alone_b, |w| {
        w.nodes[1].attempts == 1 && matches!(w.nodes[1].sealer, Sealer::Preparing { .. })
    });
    assert!(!preparing.is_empty());
    let alone_a = Config {
        cuts: vec![(A, B), (B, A), (A, C), (C, A)],
        attempts: [1, 0, 0],
        ..Config::default()
    };
    passes(alone_a.clone());
    reaches(alone_a, |w| w.nodes[0].len > 0);
}

// ---------------------------------------------------------------------------------------------
// Destage waits for the commit (S5)
// ---------------------------------------------------------------------------------------------

/// Destage is a step of the model once the seal committed and the copy equals it: the scenario
/// passes and a state exists in which the destager read the committed proposal.
#[test]
fn destage_starts_only_after_the_commit() {
    let cfg = Config {
        records: 1,
        attempts: [0, 1, 0],
        destage: true,
        ..Config::default()
    };
    passes(cfg.clone());
    let steps = reaches(cfg, |w| w.destaged.is_some());
    let destage = steps
        .iter()
        .position(|(act, _)| act.starts_with("Destage"))
        .expect("a destage step");
    assert!(
        steps[..destage]
            .iter()
            .any(|(act, _)| act.starts_with("Choose"))
    );
}

// ---------------------------------------------------------------------------------------------
// Weaker acknowledgement is documented behaviour
// ---------------------------------------------------------------------------------------------

/// With `min_acks = 1` (two copies), a record acknowledged on the writer's copy alone is lost
/// when the node that takes over seals without it: the explorer reports S3 with the exact
/// length, and the design's own default (below) does not.
#[test]
fn with_one_ack_a_record_acknowledged_on_one_copy_is_cut_by_the_seal() {
    let trace = violation(run(Config {
        replicas: 2,
        min_acks: 1,
        ..crash2()
    }));
    assert_eq!(trace.rule(), "S3");
    assert_eq!(
        trace.message,
        "S3 acknowledged length 3 is beyond the committed seal 0"
    );
}

/// The row itself: a writer lost between its local sync and the replica's receipt loses the
/// acknowledged record. The steps to the loss are: the writer syncs and acknowledges, the
/// writer crashes with the batch not yet received by the replica, and the replica seals at
/// its own, shorter, length.
#[test]
fn with_one_ack_a_writer_lost_between_its_local_sync_and_the_replicas_receipt_loses_the_record() {
    let cfg = Config {
        replicas: 2,
        min_acks: 1,
        ..crash2()
    };
    let steps = reaches(cfg, |w| {
        w.crashed
            && w.acked > 0
            && w.nodes
                .iter()
                .any(|n| n.committed.is_some_and(|p| p.length < w.acked))
    });
    let acts: Vec<&str> = steps.iter().map(|(a, _)| a.as_str()).collect();
    let crash = acts.iter().position(|a| *a == "Crash").expect("a crash");
    let local = acts
        .iter()
        .position(|a| a.starts_with("LocalSync"))
        .expect("a local sync");
    assert!(local < crash, "the writer synced locally before it crashed");
    assert!(
        !acts[..crash]
            .iter()
            .any(|a| a.starts_with("Deliver") && a.contains("Replicate")),
        "the replica had not received the batch"
    );
}

/// With the default `min_acks = replicas` the same scenario loses nothing.
#[test]
fn with_the_default_no_acknowledged_record_is_lost() {
    passes(crash2());
    passes(Config {
        replicas: 3,
        min_acks: 3,
        records: 1,
        ..crash2()
    });
}

// ---------------------------------------------------------------------------------------------
// The model has teeth: each variant is caught by the matching rule
// ---------------------------------------------------------------------------------------------

fn variant(variant: Variant, cfg: Config) -> Config {
    Config { variant, ..cfg }
}

/// A sealer that ignores accepted proposals in Choose lets a second seal commit with another
/// length: S1.
#[test]
fn a_sealer_that_ignores_accepted_proposals_commits_a_second_seal() {
    let cfg = variant(
        Variant::IgnoreAccepted,
        Config {
            cuts: sym(),
            min_acks: 2,
            ..Config::default()
        },
    );
    let trace = violation(run_only(cfg, "S1"));
    assert_eq!(trace.rule(), "S1");
    assert!(
        trace
            .message
            .starts_with("S1 two different seals committed")
    );
}

/// A replica that ignores the ballot fence appends a batch of the old writer below its
/// promise: S4.
#[test]
fn a_replica_that_ignores_the_ballot_fence_appends_for_the_old_writer() {
    let cfg = variant(
        Variant::NoFence,
        Config {
            records: 1,
            attempts: [0, 1, 0],
            ..Config::default()
        },
    );
    assert_eq!(violation(run_only(cfg, "S4")).rule(), "S4");
}

/// A seal that needs one node instead of a majority commits two different seals in a partition
/// (S1), changes a node's committed seal when both reach it (S2), and commits in a minority (S6).
#[test]
fn a_seal_that_needs_one_node_breaks_the_seal_rules() {
    let split = variant(
        Variant::CommitOnOne,
        Config {
            cuts: sym(),
            min_acks: 2,
            max_losses: 1,
            ..Config::default()
        },
    );
    assert_eq!(violation(run_only(split.clone(), "S1")).rule(), "S1");
    assert_eq!(violation(run_only(split, "S2")).rule(), "S2");
    let alone = variant(
        Variant::CommitOnOne,
        Config {
            cuts: vec![(A, B), (B, A), (B, C), (C, B)],
            attempts: [0, 1, 0],
            ..Config::default()
        },
    );
    assert_eq!(violation(run_only(alone, "S6")).rule(), "S6");
}

/// A writer that acknowledges on its own copy alone acknowledges in a minority: S6.
#[test]
fn a_writer_that_acknowledges_on_its_own_copy_acknowledges_in_a_minority() {
    let cfg = variant(
        Variant::AckOnOne,
        Config {
            cuts: vec![(A, B), (B, A), (A, C), (C, A)],
            attempts: [1, 0, 0],
            ..Config::default()
        },
    );
    let trace = violation(run(cfg));
    assert_eq!(
        trace.message,
        "S6 the writer alone in its partition acknowledged"
    );
}

/// Choose that counts a damaged copy and proposes the longest proposes a length whose crc does
/// not describe the segment: S7.
#[test]
fn counting_a_damaged_copy_proposes_a_crc_that_does_not_describe_the_segment() {
    let cfg = variant(
        Variant::Longest,
        Config {
            cuts: sym(),
            damage: Some(C),
            ..Config::default()
        },
    );
    assert_eq!(violation(run_only(cfg, "S7")).rule(), "S7");
}

/// Destage that may read an open segment destages before any seal committed: S5.
#[test]
fn destage_of_an_open_segment_is_caught() {
    let cfg = variant(
        Variant::DestageEarly,
        Config {
            records: 1,
            attempts: [0, 1, 0],
            destage: true,
            ..Config::default()
        },
    );
    let trace = violation(run(cfg));
    assert_eq!(trace.message, "S5 destaged before any seal committed");
}

/// A cluster in which no copy converges after the commit cannot reach the converged state from
/// quiescence: the explorer reports its liveness failure (rule `A4`).
#[test]
fn without_converge_the_copies_never_equal_the_seal() {
    let cfg = variant(
        Variant::NoConverge,
        Config {
            records: 1,
            attempts: [0, 1, 0],
            ..Config::default()
        },
    );
    let trace = violation(run(cfg));
    assert_eq!(trace.rule(), "A4");
}

/// A trace reads as steps: each names the action and shows the state after it.
#[test]
fn a_counterexample_prints_the_steps_and_the_states() {
    let cfg = variant(
        Variant::NoFence,
        Config {
            records: 1,
            attempts: [0, 1, 0],
            ..Config::default()
        },
    );
    let trace = violation(run_only(cfg, "S4"));
    assert!(
        trace
            .steps
            .iter()
            .any(|(act, _)| act.starts_with("StartSeal"))
    );
    for (_, state) in &trace.steps {
        assert!(state.starts_with("n1[up=true"), "{state}");
        assert!(state.contains("writer="));
    }
}

/// With the writer's own copy damaged after an acknowledgement (`min_acks = 2`), the writer's
/// early seal may find that the rule puts the proposal below its last acknowledged offset: it
/// makes no proposal and the segment waits. The scenario breaks no rule and reaches that
/// state.
#[test]
fn a_writer_whose_rule_gives_less_than_it_acknowledged_makes_no_proposal() {
    let cfg = Config {
        min_acks: 2,
        damage: Some(A),
        attempts: [1, 0, 0],
        ..Config::default()
    };
    passes(cfg.clone());
    reaches(cfg, |w| {
        w.damaged
            && w.acked > 0
            && w.nodes[0].attempts == 1
            && w.nodes[0].sealer == Sealer::Idle
            && w.nodes[0].committed.is_none()
    });
}

/// With `min_acks = 1` a writer alone in its partition does acknowledge: the minority rule of
/// S6 holds only when an acknowledgement needs a second copy, and the weaker setting is the
/// documented exception.
#[test]
fn with_one_ack_a_writer_alone_in_its_partition_acknowledges() {
    let cfg = Config {
        min_acks: 1,
        cuts: vec![(A, B), (B, A), (A, C), (C, A)],
        attempts: [0, 0, 0],
        ..Config::default()
    };
    passes(cfg.clone());
    reaches(cfg, |w| w.acked > 0);
}

/// Destage with two copies and a voter: the voter holds nothing to destage, the copies do once
/// they equal the seal, and no rule is broken.
#[test]
fn destage_with_a_voter_reads_only_a_converged_copy() {
    let cfg = Config {
        replicas: 2,
        min_acks: 2,
        destage: true,
        attempts: [0, 1, 0],
        records: 1,
        ..Config::default()
    };
    passes(cfg.clone());
    reaches(cfg, |w| w.destaged.is_some());
}

/// Destage that does not wait for the copy to converge reads bytes beyond the seal: S5, with the
/// two proposals named.
#[test]
fn destage_of_an_unconverged_copy_differs_from_the_seal() {
    let cfg = variant(
        Variant::DestageUnconverged,
        Config {
            destage: true,
            attempts: [0, 1, 0],
            ..Config::default()
        },
    );
    let trace = violation(run_only(cfg, "S5"));
    assert!(
        trace.message.contains("but the seal is"),
        "{}",
        trace.message
    );
}

// ---------------------------------------------------------------------------------------------
