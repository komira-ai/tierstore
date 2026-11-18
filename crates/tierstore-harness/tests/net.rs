//! The simulated network: delivery is the driver's choice, cut links and crashes lose messages,
//! and the state does not depend on the order of sends.

use std::collections::HashSet;

use tierstore_format::ids::NodeId;
use tierstore_harness::net::{Envelope, SimNet};

const A: NodeId = NodeId(1);
const B: NodeId = NodeId(2);
const C: NodeId = NodeId(3);

/// Messages stay in flight until taken; `take` returns exactly the message chosen and counts it
/// delivered; an index past the end is `None` for `take` and `lose`.
#[test]
fn a_message_arrives_only_when_the_driver_takes_it() {
    let mut net = SimNet::new();
    assert!(net.is_idle());
    assert!(net.send(A, B, 7u32));
    assert!(net.send(A, C, 3u32));
    assert_eq!(net.in_flight().len(), 2);
    let got = net.take(1).unwrap();
    assert_eq!(
        got,
        Envelope {
            from: A,
            to: C,
            msg: 3
        }
    );
    assert_eq!(net.delivered(), 1);
    assert_eq!(net.take(5), None);
    assert_eq!(net.lose(5), None);
    let lost = net.lose(0).unwrap();
    assert_eq!(lost.msg, 7);
    assert_eq!(net.lost(), 1);
    assert!(net.is_idle());
}

/// The same messages sent in two orders are one state (equal and with one hash), and the counts of
/// delivered and lost messages take no part in equality.
#[test]
fn the_order_of_sends_does_not_make_another_state() {
    let mut x = SimNet::new();
    x.send(A, B, 1u8);
    x.send(B, C, 2u8);
    x.send(A, B, 1u8);
    let mut y = SimNet::new();
    y.send(A, B, 1u8);
    y.send(A, B, 1u8);
    y.send(B, C, 2u8);
    y.send(C, A, 9u8);
    y.lose(y.in_flight().iter().position(|e| e.msg == 9).unwrap());
    assert_eq!(x, y);
    let set: HashSet<SimNet<u8>> = [x.clone(), y].into_iter().collect();
    assert_eq!(set.len(), 1);
    let mut z = x.clone();
    z.send(A, C, 1u8);
    assert_ne!(x, z, "another message in flight is another state");
    x.cut(C, A);
    assert!(!set.contains(&x), "a cut link is part of the state");
}

/// A cut link loses what was in flight on it and every later send, in that direction only; healing
/// restores it.
#[test]
fn a_cut_link_loses_messages_in_one_direction_until_healed() {
    let mut net = SimNet::new();
    net.send(A, B, 1u8);
    net.send(B, A, 2u8);
    net.cut(A, B);
    assert!(net.is_cut(A, B));
    assert!(!net.is_cut(B, A));
    assert_eq!(
        net.in_flight().len(),
        1,
        "the message on the cut link is lost"
    );
    assert_eq!(net.lost(), 1);
    assert!(!net.send(A, B, 3u8));
    assert!(net.send(B, A, 4u8));
    assert_eq!(net.lost(), 2);
    net.heal(A, B);
    assert!(net.send(A, B, 5u8));
    net.cut(A, C);
    net.cut(C, B);
    net.heal_all();
    assert!(!net.is_cut(A, C) && !net.is_cut(C, B));
}

/// A symmetric partition cuts both directions between the sides and nothing inside a side;
/// broadcast counts what got through.
#[test]
fn a_partition_cuts_both_ways_between_the_sides_only() {
    let mut net = SimNet::new();
    net.partition(&[A], &[B, C]);
    for (f, t) in [(A, B), (B, A), (A, C), (C, A)] {
        assert!(net.is_cut(f, t));
    }
    assert!(!net.is_cut(B, C));
    assert_eq!(net.broadcast(B, [A, C], &1u8), 1);
    assert_eq!(net.broadcast(A, [B, C], &2u8), 0);
    assert_eq!(
        net.in_flight(),
        &[Envelope {
            from: B,
            to: C,
            msg: 1
        }]
    );
}

/// A crash loses what was in flight to the node and keeps what it sent.
#[test]
fn a_crash_loses_the_messages_in_flight_to_the_node() {
    let mut net = SimNet::new();
    net.send(A, B, 1u8);
    net.send(C, B, 2u8);
    net.send(B, A, 3u8);
    net.crash(B);
    assert_eq!(
        net.in_flight(),
        &[Envelope {
            from: B,
            to: A,
            msg: 3
        }]
    );
    assert_eq!(net.lost(), 2);
    assert_eq!(SimNet::<u8>::default(), SimNet::new());
}
