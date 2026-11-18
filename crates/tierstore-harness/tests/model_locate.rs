//! The definitive-miss rule and the `LOCATE` coordinator: the cases of the section 14 row one by
//! one through the coordinator on the store-api fake cluster, the exhaustive check of the rule
//! for each configuration, and the check's teeth (a wrong rule has a counterexample).

#![expect(
    clippy::expect_used,
    reason = "helpers of the tests fail the test on a setup error; clippy.toml exempts only #[test] fns"
)]

use bytes::Bytes;
use futures::executor::block_on;
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_fetch_api::upstream::{DeviceRef, Located, NotDefinitive, Source, SourceKind};
use tierstore_format::ids::{BlockId, DriveId, NodeId};
use tierstore_harness::models::locate::{
    Checked, Cond, Config, Fate, Fill, Locator, NodeAnswer, Scenario, SettingsError, Violation,
    check_rule, decide, decide_with_threshold, fate, miss_threshold, node_answer, sample_copy,
    sample_generation, sample_hit, scenarios,
};
use tierstore_store_api::fake::{FakeCluster, Route};
use tierstore_store_api::peer::PeerError;
use tierstore_store_api::proto::{ColdExtent, Envelope, HotCopy, LocateResponse};
use tierstore_store_api::view::NodeState;

const KEY: &str = "packs/p1";

fn id(n: u8) -> NodeId {
    NodeId(n)
}

/// A cluster of `n` nodes, `holders` of them (1-based ids) holding the key.
fn cluster(n: u8, holders: &[u8]) -> FakeCluster {
    let mut c = FakeCluster::new(ManualClock::new());
    for i in 1..=n {
        let node = c.spawn(id(i));
        if holders.contains(&i) {
            node.put_hot(
                KEY,
                sample_generation(),
                Bytes::from_static(b"x"),
                vec![sample_copy(id(i))],
            );
        }
    }
    c
}

fn view(n: u8) -> Vec<(NodeId, NodeState)> {
    (1..=n).map(|i| (id(i), NodeState::Up)).collect()
}

fn calls(c: &FakeCluster, n: u8) -> usize {
    c.node(id(n))
        .map_or(usize::MAX, |node| node.calls(Route::Locate))
}

fn locate(c: &FakeCluster, nodes: Vec<(NodeId, NodeState)>, min_acks: u8) -> Located {
    let locator = Locator::new(c, nodes, min_acks).map_err(|e| e.to_string());
    block_on(locator.expect("settings").locate(KEY))
}

fn other_routes_untouched(c: &FakeCluster, n: u8) {
    for i in 1..=n {
        let node = c.node(id(i)).expect("node");
        for route in [
            Route::Get,
            Route::Replicate,
            Route::Seal,
            Route::Commit,
            Route::TombPull,
        ] {
            assert_eq!(node.calls(route), 0, "node {i} saw {route:?}");
        }
    }
}

/// With three nodes and `min_acks = 2`, a hit at the first node asked is final: `Found` with that
/// node's location after one `LOCATE` call, and the other nodes are never asked.
/// Catches: a coordinator that polls every node before answering.
#[test]
fn a_hit_at_the_first_node_is_final_with_one_locate_call() {
    let c = cluster(3, &[1, 2, 3]);
    assert_eq!(locate(&c, view(3), 2), Located::Found(sample_hit(id(1))));
    assert_eq!((calls(&c, 1), calls(&c, 2), calls(&c, 3)), (1, 0, 0));
}

/// A hit is final wherever it comes: a node ahead of it that answered absent or indexing does
/// not stop the search, and the first hit in the order asked is the location.
#[test]
fn a_later_hit_overrides_earlier_absent_answers() {
    let c = cluster(3, &[2, 3]);
    assert_eq!(locate(&c, view(3), 2), Located::Found(sample_hit(id(2))));
    assert_eq!((calls(&c, 1), calls(&c, 2), calls(&c, 3)), (1, 1, 0));
    let c = cluster(3, &[2, 3]);
    c.node(id(1)).expect("node").set_indexing(true);
    assert_eq!(locate(&c, view(3), 2), Located::Found(sample_hit(id(2))));
}

/// One node down (the view says so, so it is not asked) and the two live nodes answering absent
/// is a definitive miss, decided on the second absent answer.
#[test]
fn one_node_down_and_two_absent_is_a_definitive_miss() {
    let c = cluster(3, &[]);
    let nodes = vec![
        (id(1), NodeState::Up),
        (id(2), NodeState::Down),
        (id(3), NodeState::Up),
    ];
    assert_eq!(locate(&c, nodes, 2), Located::DefinitiveMiss);
    assert_eq!((calls(&c, 1), calls(&c, 2), calls(&c, 3)), (1, 0, 1));
}

/// A node the view calls up but that does not answer (crashed) is asked once and counts as no
/// answer; the other two absent answers still decide.
#[test]
fn a_crashed_node_that_the_view_still_calls_up_is_no_answer() {
    let c = cluster(3, &[]);
    c.node(id(1)).expect("node").crash();
    assert_eq!(locate(&c, view(3), 2), Located::DefinitiveMiss);
    assert_eq!((calls(&c, 1), calls(&c, 2), calls(&c, 3)), (1, 1, 1));
}

/// A single absent answer decides nothing: the rule on one absent answer is not a miss, and the
/// coordinator does not stop at the first absent answer but asks the next node.
/// Catches: a miss from the least-loaded node's answer alone.
#[test]
fn a_single_absent_is_not_a_definitive_miss() {
    assert_eq!(
        decide(3, 2, &[NodeAnswer::Absent]),
        Ok(Located::NotDefinitive(NotDefinitive::TooFewAnswers))
    );
    let c = cluster(3, &[]);
    c.node(id(2)).expect("node").crash();
    c.node(id(3)).expect("node").crash();
    assert_eq!(
        locate(&c, view(3), 2),
        Located::NotDefinitive(NotDefinitive::TooFewAnswers)
    );
    assert_eq!((calls(&c, 1), calls(&c, 2), calls(&c, 3)), (1, 1, 1));
}

/// Two nodes down leave one live node: not a miss, answer 503 (`Fate::Wait`), and nothing but
/// `LOCATE` is called on any node (no import, no repair).
#[test]
fn two_nodes_down_is_not_definitive_and_nothing_is_repaired() {
    let c = cluster(3, &[]);
    let nodes = vec![
        (id(1), NodeState::Down),
        (id(2), NodeState::Down),
        (id(3), NodeState::Up),
    ];
    let located = locate(&c, nodes, 2);
    assert_eq!(
        located,
        Located::NotDefinitive(NotDefinitive::TooFewAnswers)
    );
    assert_eq!(fate(located), Fate::Wait(NotDefinitive::TooFewAnswers));
    assert_eq!((calls(&c, 1), calls(&c, 2), calls(&c, 3)), (0, 0, 1));
    other_routes_untouched(&c, 3);
}

/// A node that answers `indexing` is never a miss: with one node indexing, one absent and one
/// down the answer is `Indexing`, and two indexing nodes with one absent are the same.
#[test]
fn a_deciding_node_that_is_indexing_is_not_definitive() {
    let c = cluster(3, &[]);
    c.node(id(1)).expect("node").set_indexing(true);
    let nodes = vec![
        (id(1), NodeState::Up),
        (id(2), NodeState::Up),
        (id(3), NodeState::Down),
    ];
    let located = locate(&c, nodes, 2);
    assert_eq!(located, Located::NotDefinitive(NotDefinitive::Indexing));
    assert_eq!(fate(located), Fate::Wait(NotDefinitive::Indexing));
    other_routes_untouched(&c, 3);
    let c = cluster(3, &[]);
    c.node(id(1)).expect("node").set_indexing(true);
    c.node(id(2)).expect("node").set_indexing(true);
    assert_eq!(
        locate(&c, view(3), 2),
        Located::NotDefinitive(NotDefinitive::Indexing)
    );
}

/// An indexing node outside the absent answers that decide does not weaken the miss: with
/// `min_acks = 2` two absent answers still prove no acknowledged copy exists (it would sit on two
/// of the three nodes).
#[test]
fn two_absent_answers_decide_even_with_the_third_node_indexing() {
    let c = cluster(3, &[]);
    c.node(id(1)).expect("node").set_indexing(true);
    assert_eq!(locate(&c, view(3), 2), Located::DefinitiveMiss);
}

/// The threshold is `N - min_acks + 1` of the frozen rule: 2 for three nodes with `min_acks = 2`
/// (as with `hot.replicas = 2`: the rule reads `min_acks` and `N`, not the replica count), 1 for
/// `min_acks = 3`, 5 for the six nodes of the NOW profile with `min_acks = 2`; and four absent
/// answers of six are not a miss while five are.
#[test]
fn the_miss_needs_n_minus_min_acks_plus_one_absent_answers() {
    assert_eq!(miss_threshold(3, 2), Ok(2));
    assert_eq!(miss_threshold(3, 3), Ok(1));
    assert_eq!(miss_threshold(6, 2), Ok(5));
    let c = cluster(3, &[]);
    c.node(id(3)).expect("node").crash();
    assert_eq!(locate(&c, view(3), 2), Located::DefinitiveMiss);
    let c = cluster(6, &[]);
    for n in 5..=6 {
        c.node(id(n)).expect("node").crash();
    }
    assert_eq!(
        locate(&c, view(6), 2),
        Located::NotDefinitive(NotDefinitive::TooFewAnswers),
        "four absent answers of six"
    );
    let c = cluster(6, &[]);
    c.node(id(6)).expect("node").crash();
    assert_eq!(locate(&c, view(6), 2), Located::DefinitiveMiss);
    assert_eq!(
        calls(&c, 6),
        0,
        "the sixth is not asked once five are absent"
    );
}

/// Settings no cluster can meet (`min_acks` 0 or above `N`) are an error from every entry point.
#[test]
fn unmeetable_settings_are_an_error() {
    let bad = |n, m| SettingsError {
        storage_nodes: n,
        min_acks: m,
    };
    assert_eq!(miss_threshold(3, 0), Err(bad(3, 0)));
    assert_eq!(miss_threshold(3, 4), Err(bad(3, 4)));
    assert_eq!(decide(3, 4, &[]), Err(bad(3, 4)));
    assert_eq!(
        SettingsError {
            storage_nodes: 3,
            min_acks: 4
        }
        .to_string(),
        "min_acks 4 cannot be met by 3 storage nodes"
    );
    let c = cluster(3, &[]);
    assert_eq!(Locator::new(&c, view(3), 4).err(), Some(bad(3, 4)));
    let cfg = Config {
        storage_nodes: 3,
        min_acks: 0,
    };
    assert_eq!(scenarios(cfg), Err(bad(3, 0)));
    assert_eq!(
        check_rule(cfg, &|_| Located::DefinitiveMiss).err(),
        Some(bad(3, 0))
    );
}

/// One fill asks each node once per key: a second `locate` of the same key answers from the fill
/// with no call, another key is its own round, and `locate_again` (4.6) is a new round whose
/// answer replaces the kept one.
#[test]
fn one_locate_per_key_per_node_per_fill() {
    let c = cluster(3, &[]);
    let locator = Locator::new(&c, view(3), 2)
        .map_err(|e| e.to_string())
        .expect("settings");
    let mut fill = Fill::new(&locator);
    assert_eq!(block_on(fill.locate(KEY)), Located::DefinitiveMiss);
    assert_eq!(block_on(fill.locate(KEY)), Located::DefinitiveMiss);
    assert_eq!((calls(&c, 1), calls(&c, 2), calls(&c, 3)), (1, 1, 0));
    assert_eq!(block_on(fill.locate("packs/p2")), Located::DefinitiveMiss);
    assert_eq!((calls(&c, 1), calls(&c, 2), calls(&c, 3)), (2, 2, 0));
    assert_eq!(block_on(fill.locate(KEY)), Located::DefinitiveMiss);
    assert_eq!(calls(&c, 1), 2);
    // The pack appears on two nodes; the second round of the same fill finds it.
    for n in [1, 2] {
        c.node(id(n)).expect("node").put_hot(
            KEY,
            sample_generation(),
            Bytes::from_static(b"x"),
            vec![sample_copy(id(n))],
        );
    }
    assert_eq!(block_on(fill.locate(KEY)), Located::DefinitiveMiss, "kept");
    assert_eq!(
        block_on(fill.locate_again(KEY)),
        Located::Found(sample_hit(id(1)))
    );
    assert_eq!(
        block_on(fill.locate(KEY)),
        Located::Found(sample_hit(id(1)))
    );
    assert_eq!(calls(&c, 1), 3);
}

/// A `suspect` node is asked after the others and a `down` node is not asked: with the first node
/// suspect and the key on every node, the hit is the second node's and the suspect one is never
/// called; when the others cannot decide, the suspect node is asked last.
#[test]
fn a_suspect_node_is_asked_last_and_a_down_node_not_at_all() {
    let nodes = vec![
        (id(1), NodeState::Suspect),
        (id(2), NodeState::Up),
        (id(3), NodeState::Up),
    ];
    let c = cluster(3, &[1, 2, 3]);
    assert_eq!(
        locate(&c, nodes.clone(), 2),
        Located::Found(sample_hit(id(2)))
    );
    assert_eq!(calls(&c, 1), 0);
    let c = cluster(3, &[1]);
    c.node(id(2)).expect("node").crash();
    assert_eq!(locate(&c, nodes, 2), Located::Found(sample_hit(id(1))));
    assert_eq!((calls(&c, 1), calls(&c, 2), calls(&c, 3)), (1, 1, 1));
    let c = cluster(3, &[]);
    let down_first = vec![
        (id(1), NodeState::Down),
        (id(2), NodeState::Suspect),
        (id(3), NodeState::Up),
    ];
    assert_eq!(locate(&c, down_first, 2), Located::DefinitiveMiss);
    assert_eq!((calls(&c, 1), calls(&c, 2), calls(&c, 3)), (0, 1, 1));
}

/// Pack lost, the coordinator's part, with a single index node: down or indexing, the fill waits
/// (never lost) and resumes with the location when the node returns; with the node answering
/// absent the pack is lost, and then `locate_again` still says lost.
#[test]
fn a_lone_index_node_down_or_indexing_waits_and_absent_is_lost() {
    let c = cluster(1, &[1]);
    let node = c.node(id(1)).expect("node");
    let locator = Locator::new(&c, view(1), 1)
        .map_err(|e| e.to_string())
        .expect("settings");
    let mut fill = Fill::new(&locator);
    node.crash();
    let down = block_on(fill.locate(KEY));
    assert_eq!(fate(down), Fate::Wait(NotDefinitive::TooFewAnswers));
    node.restart();
    node.set_indexing(true);
    let indexing = block_on(fill.locate_again(KEY));
    assert_eq!(fate(indexing), Fate::Wait(NotDefinitive::Indexing));
    node.set_indexing(false);
    assert_eq!(
        fate(block_on(fill.locate_again(KEY))),
        Fate::Resolved(sample_hit(id(1)))
    );
    node.delete(KEY, tierstore_format::stamp::Stamp::from_raw(9));
    assert_eq!(fate(block_on(fill.locate_again(KEY))), Fate::Lost);
    assert_eq!(fate(block_on(fill.locate_again(KEY))), Fate::Lost);
}

/// With every one of three nodes absent the pack is lost (a definitive miss), decided after two
/// absent answers.
#[test]
fn every_node_absent_is_lost() {
    let c = cluster(3, &[]);
    assert_eq!(fate(locate(&c, view(3), 2)), Fate::Lost);
}

/// A hot answer becomes a location with the generation and size of the answer and one source per
/// copy, a promoted copy as `HotClean`; a cold answer one `Cold` source per extent on its data
/// shard's drive; an answer from an error, an empty response, absent and indexing are as named.
#[test]
fn node_answers_become_locations_and_failures_no_answer() {
    let c = cluster(1, &[]);
    let node = c.node(id(1)).expect("node");
    let copies = vec![
        HotCopy {
            node: id(2),
            file: "cache-1".into(),
            offset: 7,
            length: 3,
            promoted: true,
        },
        sample_copy(id(1)),
    ];
    node.put_hot(
        "hot",
        sample_generation(),
        Bytes::from_static(b"abc"),
        copies,
    );
    let block = BlockId::new(id(1), 1).expect("block");
    let drive = DriveId([5; 16]);
    let extent = ColdExtent {
        block,
        object_offset: 10,
        length: 4,
        block_offset: 20,
        node: id(1),
        drive,
        shard: "shard-1".into(),
        reconstructor: id(1),
        sources: Vec::new(),
        margin: 1,
    };
    node.put_cold(
        "cold",
        sample_generation(),
        Bytes::from_static(b"wxyz"),
        vec![extent],
    );
    let ask = |key: &str| {
        let req = tierstore_store_api::proto::LocateRequest {
            keys: vec![key.to_owned()],
        };
        node_answer(block_on(tierstore_store_api::peer::PeerClient::locate(
            &c,
            id(1),
            req,
        )))
    };
    let NodeAnswer::Hit(hot) = ask("hot") else {
        panic!("hot is a hit")
    };
    assert_eq!(
        (hot.generation, hot.size),
        (Some(sample_generation()), Some(3))
    );
    assert_eq!(
        hot.sources,
        vec![
            Source {
                device: DeviceRef::Hot(id(2)),
                kind: SourceKind::HotClean {
                    file: "cache-1".into(),
                    offset: 7
                },
                extent: ByteRange { offset: 0, len: 3 },
            },
            Source {
                device: DeviceRef::Hot(id(1)),
                kind: SourceKind::Hot {
                    file: "seg-1".into(),
                    offset: 0
                },
                extent: ByteRange { offset: 0, len: 1 },
            },
        ]
    );
    let NodeAnswer::Hit(cold) = ask("cold") else {
        panic!("cold is a hit")
    };
    assert_eq!(cold.size, Some(4));
    assert_eq!(
        cold.sources,
        vec![Source {
            device: DeviceRef::Drive(id(1), drive),
            kind: SourceKind::Cold {
                block,
                shard: "shard-1".into(),
                offset: 20
            },
            extent: ByteRange { offset: 10, len: 4 },
        }]
    );
    assert_eq!(ask("nothing"), NodeAnswer::Absent);
    node.set_indexing(true);
    assert_eq!(ask("nothing"), NodeAnswer::Indexing);
    let empty = Envelope {
        from: id(1),
        stamp: tierstore_format::stamp::Stamp::from_raw(1),
        mem_epoch: 1,
        body: LocateResponse {
            answers: Vec::new(),
        },
    };
    assert_eq!(node_answer(Ok(empty)), NodeAnswer::NoAnswer);
    let err = PeerError::NotFound { node: id(1) };
    assert_eq!(node_answer(Err(err)), NodeAnswer::NoAnswer);
}

/// `Scenario::answers` gives a down node no answer, an indexing node `Indexing` even when it holds
/// a copy, and an up node a hit or absent by whether it holds one; the counts of scenarios are
/// `3^N` per truth, truths being the sets of at least `min_acks` holders and the empty set.
#[test]
fn scenarios_enumerate_every_condition_against_every_truth() {
    let s = Scenario {
        conds: vec![Cond::Down, Cond::Indexing, Cond::Up, Cond::Up],
        holders: vec![true, true, true, false],
    };
    assert!(s.copy_exists());
    assert_eq!(
        s.answers(),
        vec![
            NodeAnswer::NoAnswer,
            NodeAnswer::Indexing,
            NodeAnswer::Hit(sample_hit(id(3))),
            NodeAnswer::Absent
        ]
    );
    let none = Scenario {
        conds: vec![Cond::Up],
        holders: vec![false],
    };
    assert!(!none.copy_exists());
    let count = |n, m| {
        scenarios(Config {
            storage_nodes: n,
            min_acks: m,
        })
        .map(|s| s.len())
    };
    assert_eq!(count(3, 2), Ok(27 * 5));
    assert_eq!(count(3, 3), Ok(27 * 2));
    assert_eq!(count(2, 2), Ok(9 * 2));
    assert_eq!(count(6, 2), Ok(729 * 58));
    let all = scenarios(Config {
        storage_nodes: 3,
        min_acks: 2,
    })
    .expect("settings");
    assert!(all[..all.len() - 27].iter().all(Scenario::copy_exists));
    assert!(all[all.len() - 27..].iter().all(|s| !s.copy_exists()));
}

fn exhaustive(n: usize, m: u8) -> Checked {
    let t = miss_threshold(n, m).expect("settings");
    check_rule(
        Config {
            storage_nodes: n,
            min_acks: m,
        },
        &|answers| decide_with_threshold(t, answers),
    )
    .expect("settings")
    .map_err(|c| format!("{c:?}"))
    .expect("the rule holds in every scenario")
}

/// For three nodes with `min_acks = 2` (also `hot.replicas = 2`, which the rule does not read) the
/// rule is safe (no miss while an acknowledged copy exists), decisive, and gives the right reason,
/// in every assignment of node conditions against every truth: 135 scenarios.
#[test]
fn the_rule_holds_exhaustively_for_three_nodes_and_min_acks_two() {
    assert_eq!(exhaustive(3, 2).scenarios, 135);
}

/// The same for three nodes with `min_acks = 3`: one absent answer is a miss, 54 scenarios.
#[test]
fn the_rule_holds_exhaustively_for_three_nodes_and_min_acks_three() {
    assert_eq!(exhaustive(3, 3).scenarios, 54);
}

/// The same for two nodes with `min_acks = 2` (`replicas = 2` on a two-node cluster): 18 scenarios.
#[test]
fn the_rule_holds_exhaustively_for_two_nodes_and_min_acks_two() {
    assert_eq!(exhaustive(2, 2).scenarios, 18);
}

/// The same for the six nodes of the NOW profile (`hot.replicas = 2`, `min_acks = 2`): five absent
/// answers decide, 42282 scenarios.
#[test]
fn the_rule_holds_exhaustively_for_the_six_node_now_profile() {
    assert_eq!(exhaustive(6, 2).scenarios, 42_282);
}

/// The coordinator against the real fake cluster decides exactly as the reference rule does on
/// every scenario of each configuration, whether the view calls a down node up (it is asked and
/// fails) or down (it is not asked at all).
#[test]
fn the_coordinator_agrees_with_the_rule_on_every_scenario() {
    for (n, m) in [(2u8, 2u8), (3, 2), (3, 3), (6, 2)] {
        let cfg = Config {
            storage_nodes: usize::from(n),
            min_acks: m,
        };
        for scenario in scenarios(cfg).expect("settings") {
            let expected = decide(usize::from(n), m, &scenario.answers()).expect("settings");
            let c = scenario.cluster(ManualClock::new(), KEY);
            assert_eq!(locate(&c, view(n), m), expected, "{scenario:?}");
            let by_view: Vec<_> = scenario
                .conds
                .iter()
                .zip(1u8..)
                .map(|(cond, i)| {
                    let state = if *cond == Cond::Down {
                        NodeState::Down
                    } else {
                        NodeState::Up
                    };
                    (id(i), state)
                })
                .collect();
            let c = scenario.cluster(ManualClock::new(), KEY);
            assert_eq!(locate(&c, by_view, m), expected, "{scenario:?}");
            for (cond, i) in scenario.conds.iter().zip(1u8..) {
                if *cond == Cond::Down {
                    assert_eq!(calls(&c, i), 0, "a down node is not asked: {scenario:?}");
                }
            }
        }
    }
}

fn caught(n: usize, m: u8, rule: &dyn Fn(&[NodeAnswer]) -> Located) -> (Violation, Scenario) {
    let cx = check_rule(
        Config {
            storage_nodes: n,
            min_acks: m,
        },
        rule,
    )
    .expect("settings")
    .expect_err("the check finds a counterexample");
    (cx.violation, cx.scenario)
}

/// A rule too weak, a miss on `N - min_acks` absent answers, is caught in every configuration:
/// the explorer reports a definitive miss while an acknowledged copy exists, with the nodes that
/// hold it down or indexing and a single absent answer (three nodes, `min_acks = 2`).
#[test]
fn a_rule_that_is_too_weak_has_a_safety_counterexample() {
    for (n, m) in [(2usize, 2u8), (3, 2), (3, 3), (6, 2)] {
        let t = miss_threshold(n, m).expect("settings") - 1;
        let (violation, scenario) = caught(n, m, &|a| decide_with_threshold(t, a));
        assert_eq!(violation, Violation::MissWhileCopyExists, "{n}/{m}");
        assert!(scenario.copy_exists());
    }
    let (_, s) = caught(3, 2, &|a| decide_with_threshold(1, a));
    let absent = s
        .answers()
        .iter()
        .filter(|a| **a == NodeAnswer::Absent)
        .count();
    assert_eq!(absent, 1);
}

/// A rule too strong, a miss only on `N - min_acks + 2` absent answers, is caught as not decisive:
/// no copy and enough absent answers, and no miss.
#[test]
fn a_rule_that_is_too_strong_is_not_decisive() {
    let t = miss_threshold(3, 2).expect("settings") + 1;
    let (violation, scenario) = caught(3, 2, &|a| decide_with_threshold(t, a));
    assert_eq!(violation, Violation::NotDecisive);
    assert!(!scenario.copy_exists());
}

/// A rule that reports `TooFewAnswers` where a node answered `indexing` is caught: the reason of
/// a non-definitive answer must name the indexing node.
#[test]
fn a_rule_that_hides_the_indexing_reason_is_caught() {
    let (violation, _) = caught(3, 2, &|a| match decide_with_threshold(2, a) {
        Located::NotDefinitive(NotDefinitive::Indexing) => {
            Located::NotDefinitive(NotDefinitive::TooFewAnswers)
        }
        other => other,
    });
    assert_eq!(violation, Violation::WrongReason);
}

/// A rule that ignores a hit, or invents one, is caught: a hit is final, and only a hit is
/// `Found`.
#[test]
fn a_rule_that_ignores_or_invents_a_hit_is_caught() {
    let (violation, _) = caught(3, 2, &|_| {
        Located::NotDefinitive(NotDefinitive::TooFewAnswers)
    });
    assert_eq!(violation, Violation::HitNotFinal);
    let (violation, _) = caught(3, 2, &|_| Located::Found(sample_hit(id(1))));
    assert_eq!(violation, Violation::FoundWithoutHit);
}

/// The fate of an answer: a location resolves, a definitive miss is lost, anything less waits.
#[test]
fn a_fill_resolves_waits_or_is_lost() {
    assert_eq!(
        fate(Located::Found(sample_hit(id(1)))),
        Fate::Resolved(sample_hit(id(1)))
    );
    assert_eq!(fate(Located::DefinitiveMiss), Fate::Lost);
    assert_eq!(
        fate(Located::NotDefinitive(NotDefinitive::Indexing)),
        Fate::Wait(NotDefinitive::Indexing)
    );
}
