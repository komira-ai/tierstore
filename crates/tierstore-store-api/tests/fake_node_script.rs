//! Proves the fake storage node's scripts and its cluster: a step's service time is spent on the
//! injected clock and nothing else, a hang never resolves, a crash makes every route unreachable
//! until a restart that keeps the durable state, the node's stamp merges the caller's, and the
//! cluster routes each call to the node it names.
//!
//! Catches:
//! - an answer released before its deadline, or one that needs more than the deadline;
//! - a hang that a clock advance releases;
//! - a failing step that skips its service time, or a script step used for the wrong route;
//! - a crashed node that consumes scripts, answers some route, or loses durable state on restart;
//! - a call counter that misses failed or crashed calls;
//! - an answer whose stamp is not above the stamp of the message it answers;
//! - a cluster call that reaches the wrong node, or an unknown node that is not `Unreachable`.

mod fake_node_support;

use std::sync::Arc;

use bytes::Bytes;
use fake_node_support::*;
use futures::FutureExt;
use tierstore_fetch_api::clock::{Clock, ManualClock};
use tierstore_format::crc32c;
use tierstore_format::ids::NodeId;
use tierstore_format::seal::SealProposal;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{BlockDrop, LogEntry, LogRecord};
use tierstore_store_api::fake::{FakeCluster, FakeStorageNode, Outcome, Route, Step};
use tierstore_store_api::peer::{PeerClient, PeerError};
use tierstore_store_api::proto::{
    KeyCheck, LocateAnswer, LocateRequest, SealAnswer, SealMessage, TombPull,
};

fn refused(node: u8) -> PeerError {
    PeerError::Unavailable {
        node: NodeId(node),
        retry_after_ms: 5,
        signals: Default::default(),
    }
}

#[test]
fn step_constructors_name_their_service_time_and_outcome() {
    assert_eq!(
        Step::answer(30),
        Step {
            service_ms: 30,
            outcome: Outcome::Answer
        }
    );
    assert_eq!(Step::hang().outcome, Outcome::Hang);
    assert_eq!(Step::crash().outcome, Outcome::Crash);
    assert_eq!(
        Step::fail(4, refused(1)),
        Step {
            service_ms: 4,
            outcome: Outcome::Fail(refused(1))
        }
    );
}

#[tokio::test]
async fn an_unscripted_call_answers_at_once() {
    let (clock, node) = node(1);
    assert!(node.ping().now_or_never().expect("ready").is_ok());
    assert_eq!(clock.sleepers(), 0);
    assert_eq!(node.calls(Route::Ping), 1);
    assert_eq!(node.id(), NodeId(1));
}

#[tokio::test]
async fn a_scripted_service_time_holds_the_call_until_the_clock_reaches_its_deadline() {
    let (clock, node) = node(1);
    clock.advance(100);
    node.script(Route::Ping, Step::answer(50));
    let mut call = Box::pin(node.ping());
    assert!((&mut call).now_or_never().is_none());
    assert_eq!(clock.sleepers(), 1);
    clock.advance(49);
    assert!((&mut call).now_or_never().is_none());
    assert_eq!(clock.sleepers(), 1);
    clock.advance(1);
    assert!((&mut call).now_or_never().expect("released").is_ok());
    assert_eq!(clock.sleepers(), 0);
}

#[tokio::test]
async fn steps_are_taken_in_order_per_route_and_an_empty_queue_answers() {
    let (_clock, node) = node(1);
    node.script(Route::Ping, Step::fail(0, refused(1)));
    node.script(Route::Ping, Step::answer(0));
    let first = node.ping().now_or_never().expect("ready");
    assert_eq!(first.unwrap_err(), refused(1));
    assert!(node.ping().now_or_never().expect("ready").is_ok());
    assert!(node.ping().now_or_never().expect("ready").is_ok());
    assert_eq!(node.calls(Route::Ping), 3);
    assert_eq!(node.calls(Route::Locate), 0);
}

#[tokio::test]
async fn a_script_for_one_route_leaves_the_others_alone() {
    let (_clock, node) = node(1);
    node.script(Route::Locate, Step::fail(0, refused(1)));
    for route in ROUTES {
        if route != Route::Locate {
            let result = call(&node, route).now_or_never().expect("ready");
            assert!(
                !matches!(result, Err(PeerError::Unavailable { .. })),
                "{route:?}"
            );
        }
    }
    let located = call(&node, Route::Locate).now_or_never().expect("ready");
    assert_eq!(located.unwrap_err(), refused(1));
}

#[tokio::test]
async fn a_failing_step_returns_its_error_only_after_its_service_time() {
    let (clock, node) = node(1);
    let not_found = PeerError::NotFound { node: NodeId(1) };
    node.script(Route::Locate, Step::fail(10, not_found.clone()));
    let mut call = Box::pin(node.locate(LocateRequest { keys: vec![] }));
    assert!((&mut call).now_or_never().is_none());
    clock.advance(9);
    assert!((&mut call).now_or_never().is_none());
    clock.advance(1);
    let result = (&mut call).now_or_never().expect("released");
    assert_eq!(result.unwrap_err(), not_found);
    assert_eq!(node.calls(Route::Locate), 1);
}

#[tokio::test]
async fn a_hang_never_resolves_whatever_the_clock_does() {
    let (clock, node) = node(1);
    node.script(Route::Ping, Step::hang());
    let mut hung = Box::pin(node.ping());
    assert!((&mut hung).now_or_never().is_none());
    clock.advance(1_000_000);
    assert!((&mut hung).now_or_never().is_none());
    assert_eq!(clock.sleepers(), 0);
    assert_eq!(node.calls(Route::Ping), 1);
    // The hang was one step: the next call answers, and other routes are not held.
    assert!(node.ping().now_or_never().expect("ready").is_ok());
    assert!(
        node.locate(LocateRequest { keys: vec![] })
            .now_or_never()
            .expect("ready")
            .is_ok()
    );
}

#[tokio::test]
async fn a_crash_step_makes_every_route_unreachable_without_consuming_scripts() {
    let (_clock, node) = node(4);
    node.script(Route::Ping, Step::crash());
    for route in ROUTES {
        if route != Route::Ping {
            node.script(
                route,
                Step::fail(
                    0,
                    PeerError::Protocol {
                        node: NodeId(4),
                        status: 599,
                        reason: "scripted".to_owned(),
                    },
                ),
            );
        }
    }
    let crashed = call(&node, Route::Ping)
        .now_or_never()
        .expect("ready")
        .unwrap_err();
    assert!(matches!(crashed, PeerError::Unreachable { node, .. } if node == NodeId(4)));
    assert!(node.is_crashed());
    for route in ROUTES {
        let result = call(&node, route).now_or_never().expect("ready");
        assert!(
            matches!(result, Err(PeerError::Unreachable { node, .. }) if node == NodeId(4)),
            "{route:?}: {result:?}"
        );
        assert_eq!(node.calls(route), if route == Route::Ping { 2 } else { 1 });
    }
    node.restart();
    assert!(!node.is_crashed());
    for route in ROUTES {
        if route != Route::Ping {
            let result = call(&node, route).now_or_never().expect("ready");
            assert!(
                matches!(result, Err(PeerError::Protocol { status: 599, .. })),
                "{route:?}: {result:?}"
            );
        }
    }
    assert!(node.ping().now_or_never().expect("ready").is_ok());
}

#[tokio::test]
async fn crash_stops_a_node_by_hand_and_restart_starts_it() {
    let (_clock, node) = node(1);
    node.crash();
    assert!(node.ping().now_or_never().expect("ready").is_err());
    node.restart();
    assert!(node.ping().now_or_never().expect("ready").is_ok());
}

#[tokio::test]
async fn restart_keeps_objects_copies_seal_states_commits_and_tomb_logs() {
    let (_clock, node) = node(1);
    let seg = block(1, 1);
    let bytes = b"hello";
    node.put_hot(
        "k",
        generation(1),
        Bytes::from_static(b"v"),
        vec![hot_copy("f", 0, 1)],
    );
    node.replicate(batch(seg, ballot(1, 1), 0, crc32c(bytes), bytes))
        .await
        .unwrap();
    let proposal = SealProposal {
        length: 5,
        running_crc: crc32c(bytes),
    };
    node.seal(env(
        1,
        1,
        SealMessage::Sealed {
            segment: seg,
            proposal,
        },
    ))
    .await
    .unwrap();
    node.append_tomb(LogRecord {
        origin: NodeId(2),
        seq: 1,
        entry: LogEntry::BlockDrop(BlockDrop {
            block: seg,
            stamp: Stamp::from_raw(9),
        }),
    })
    .unwrap();

    node.crash();
    node.restart();

    let located = node
        .locate(LocateRequest {
            keys: vec!["k".to_owned()],
        })
        .await
        .unwrap();
    assert!(matches!(located.body.answers[0], LocateAnswer::Hot { .. }));
    assert_eq!(node.copy(seg), Some(bytes.to_vec()));
    let prepared = node
        .seal(env(
            1,
            1,
            SealMessage::Prepare {
                segment: seg,
                ballot: ballot(9, 9),
            },
        ))
        .await
        .unwrap();
    assert_eq!(prepared.body, SealAnswer::Committed { proposal });
    let page = node
        .tomb_pull(TombPull {
            origin: NodeId(2),
            after: 0,
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(page.body.last, 1);
}

#[tokio::test]
async fn calls_count_every_call_including_failed_and_crashed_ones() {
    let (_clock, node) = node(1);
    node.script(Route::KeyCheck, Step::fail(0, refused(1)));
    node.script(Route::KeyCheck, Step::crash());
    let check = || {
        env(
            2,
            1,
            KeyCheck {
                key: "k".to_owned(),
            },
        )
    };
    assert!(node.key_check(check()).await.is_err());
    assert!(node.key_check(check()).await.is_err());
    assert!(node.key_check(check()).await.is_err());
    node.restart();
    assert!(node.key_check(check()).await.is_ok());
    assert_eq!(node.calls(Route::KeyCheck), 4);
    assert_eq!(node.calls(Route::Seal), 0);
}

#[tokio::test]
async fn an_answer_carries_a_stamp_above_the_callers_and_the_nodes_own_epoch_and_id() {
    let (_clock, node) = node(3);
    node.set_mem_epoch(12);
    let first = node
        .key_check(env(
            2,
            100,
            KeyCheck {
                key: "k".to_owned(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(first.from, NodeId(3));
    assert_eq!(first.mem_epoch, 12);
    assert_eq!(first.stamp, Stamp::from_raw(101));
    assert_eq!(node.stamp(), 101);
    // A lower stamp does not move the clock back: the node ticks its own.
    let second = node
        .key_check(env(
            2,
            5,
            KeyCheck {
                key: "k".to_owned(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(second.stamp, Stamp::from_raw(102));
    // A call without an envelope ticks it too.
    let ping = node.ping().await.unwrap();
    assert_eq!(ping.stamp, Stamp::from_raw(103));
    assert_eq!(ping.from, NodeId(3));
}

#[tokio::test]
async fn a_put_stamps_the_record_with_the_nodes_ticked_clock() {
    let (_clock, node) = node(1);
    node.put_hot("a", generation(1), Bytes::new(), vec![]);
    node.put_cold("b", generation(2), Bytes::new(), vec![]);
    assert_eq!(node.stamp(), 2);
    let a = node
        .key_check(env(
            2,
            1,
            KeyCheck {
                key: "a".to_owned(),
            },
        ))
        .await
        .unwrap();
    let b = node
        .key_check(env(
            2,
            1,
            KeyCheck {
                key: "b".to_owned(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(a.body.records[0].stamp, Stamp::from_raw(1));
    assert_eq!(b.body.records[0].stamp, Stamp::from_raw(2));
}

#[tokio::test]
async fn the_cluster_routes_each_call_to_the_node_it_names() {
    let clock = ManualClock::new();
    let mut cluster = FakeCluster::new(clock.clone());
    let one = cluster.spawn(NodeId(1));
    let two = Arc::new(FakeStorageNode::new(NodeId(2), clock.clone()));
    cluster.add(Arc::clone(&two));
    one.put_hot(
        "k",
        generation(1),
        Bytes::from_static(b"x"),
        vec![hot_copy("f", 0, 1)],
    );
    let ask = |node: u8| {
        let cluster = &cluster;
        async move {
            cluster
                .locate(
                    NodeId(node),
                    LocateRequest {
                        keys: vec!["k".to_owned()],
                    },
                )
                .await
                .unwrap()
        }
    };
    let from_one = ask(1).await;
    let from_two = ask(2).await;
    assert_eq!(from_one.from, NodeId(1));
    assert!(matches!(from_one.body.answers[0], LocateAnswer::Hot { .. }));
    assert_eq!(from_two.from, NodeId(2));
    assert_eq!(from_two.body.answers, vec![LocateAnswer::Absent]);
    assert_eq!(one.calls(Route::Locate), 1);
    assert_eq!(two.calls(Route::Locate), 1);
    assert!(cluster.node(NodeId(2)).is_some());
    assert!(cluster.node(NodeId(3)).is_none());
    assert_eq!(cluster.clock().now_ms(), 0);
}

#[tokio::test]
async fn a_node_the_cluster_does_not_hold_is_unreachable_on_every_route() {
    let mut cluster = FakeCluster::new(ManualClock::new());
    cluster.spawn(NodeId(1));
    let ghost = NodeId(9);
    let unreachable =
        |e: PeerError| matches!(e, PeerError::Unreachable { node, .. } if node == ghost);
    assert!(unreachable(
        cluster
            .locate(ghost, LocateRequest { keys: vec![] })
            .await
            .unwrap_err()
    ));
    assert!(unreachable(
        cluster
            .get(ghost, get(hot("f"), 0, 0, None))
            .await
            .unwrap_err()
    ));
    let seg = block(1, 1);
    assert!(unreachable(
        cluster
            .replicate(ghost, batch(seg, ballot(1, 1), 0, 0, b""))
            .await
            .unwrap_err()
    ));
    let prepare = SealMessage::Prepare {
        segment: seg,
        ballot: ballot(1, 1),
    };
    assert!(unreachable(
        cluster.seal(ghost, env(1, 1, prepare)).await.unwrap_err()
    ));
    let check = KeyCheck {
        key: "k".to_owned(),
    };
    assert!(unreachable(
        cluster
            .key_check(ghost, env(1, 1, check))
            .await
            .unwrap_err()
    ));
    let record = tierstore_store_api::proto::CommitRecord::Free {
        bytes: tierstore_store_api::proto::HexBytes(vec![]),
    };
    assert!(unreachable(
        cluster.commit(ghost, env(1, 1, record)).await.unwrap_err()
    ));
    let pull = TombPull {
        origin: NodeId(1),
        after: 0,
        limit: 1,
    };
    assert!(unreachable(
        cluster.tomb_pull(ghost, pull).await.unwrap_err()
    ));
    assert!(unreachable(cluster.ping(ghost).await.unwrap_err()));
    assert!(cluster.ping(NodeId(1)).await.is_ok());
}

#[tokio::test]
async fn adding_a_node_of_the_same_id_replaces_it() {
    let clock = ManualClock::new();
    let mut cluster = FakeCluster::new(clock.clone());
    let first = cluster.spawn(NodeId(1));
    let second = Arc::new(FakeStorageNode::new(NodeId(1), clock));
    cluster.add(Arc::clone(&second));
    cluster.ping(NodeId(1)).await.unwrap();
    assert_eq!(first.calls(Route::Ping), 0);
    assert_eq!(second.calls(Route::Ping), 1);
}

#[tokio::test]
async fn a_scripted_delay_through_the_cluster_waits_on_the_clusters_clock() {
    let mut cluster = FakeCluster::new(ManualClock::new());
    let one = cluster.spawn(NodeId(1));
    one.script(Route::Ping, Step::answer(20));
    let clock = cluster.clock().clone();
    let mut pending = Box::pin(cluster.ping(NodeId(1)));
    assert!((&mut pending).now_or_never().is_none());
    assert_eq!(clock.sleepers(), 1);
    clock.advance(20);
    assert!((&mut pending).now_or_never().expect("released").is_ok());
}
