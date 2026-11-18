//! What a node of the seal model does with a message that tells it nothing new, and which
//! messages in flight the model drops as dead.

use tierstore_format::ids::NodeId;
use tierstore_format::seal::Ballot;
use tierstore_harness::models::seal::{
    Act, Answer, Config, Cp, Msg, Prop, Reject, SealModel, Sealer, World,
};
use tierstore_model::explore::Model;

const A: NodeId = NodeId(1);
const B: NodeId = NodeId(2);
const C: NodeId = NodeId(3);

fn ballot(round: u32, node: NodeId) -> Ballot {
    Ballot { round, node }
}

fn prop(length: u64, crc: u32) -> Prop {
    Prop { length, crc }
}

/// An answer from a copy with `committed` bytes of which `verified` check out.
fn copy(committed: u64, verified: u64, crc: u32) -> Answer {
    Answer {
        accepted: None,
        copy: Some(Cp {
            committed,
            verified,
            crc,
        }),
    }
}

// What a node does with a message that tells it nothing new
// ---------------------------------------------------------------------------------------------

fn start() -> (SealModel, World) {
    let model = SealModel::new(Config::default());
    let world = model.init().remove(0);
    (model, world)
}

/// The state after the first enabled step `pick` accepts.
fn advance(model: &SealModel, w: &World, pick: impl Fn(&Act) -> bool) -> World {
    let mut out = Vec::new();
    model.successors(w, &mut out);
    let Some(step) = out.into_iter().find(|s| pick(&s.act)) else {
        panic!("no such step is enabled");
    };
    step.to.unwrap_or_else(|e| panic!("{e}"))
}

fn sent(w: &World, from: NodeId, to: NodeId, msg: Msg) -> bool {
    w.net
        .in_flight()
        .iter()
        .any(|e| e.from == from && e.to == to && e.msg == msg)
}

/// B has started a seal attempt under ballot (2, B).
fn b_preparing(model: &SealModel, idle: &World) -> World {
    advance(model, idle, |a| matches!(a, Act::StartSeal(n) if *n == B))
}

/// A promise or an accept for an attempt that is not the one in progress (none, or another
/// ballot), and a refusal that raises nothing and ends nothing, change nothing.
#[test]
fn a_stale_reply_changes_nothing() {
    let (model, idle) = start();
    let other = ballot(9, C);
    let promise = Msg::Promise {
        ballot: ballot(2, A),
        answer: copy(0, 0, 0),
    };
    assert_eq!(model.receive(&idle, B, A, promise), idle);
    assert_eq!(
        model.receive(&idle, B, A, Msg::Accepted { ballot: other }),
        idle
    );
    let refusal = Msg::Refused {
        ballot: other,
        promised: ballot(1, A),
    };
    assert_eq!(model.receive(&idle, B, A, refusal), idle);

    let preparing = b_preparing(&model, &idle);
    let stale = Msg::Promise {
        ballot: other,
        answer: copy(0, 0, 0),
    };
    assert_eq!(model.receive(&preparing, C, B, stale), preparing);
    let accepted = Msg::Accepted {
        ballot: ballot(2, B),
    };
    assert_eq!(model.receive(&preparing, C, B, accepted), preparing);
    // A sealer already in its accept phase ignores an accepted reply of another ballot.
    let mut accepting = preparing.clone();
    accepting.nodes[1].sealer = Sealer::Accepting {
        ballot: ballot(2, B),
        prop: prop(3, 7),
        accepts: std::collections::BTreeSet::new(),
    };
    let old = Msg::Accepted {
        ballot: ballot(1, B),
    };
    assert_eq!(model.receive(&accepting, C, B, old), accepting);
}

/// A refusal for the attempt in progress ends it and raises the ballot the sealer picks next.
#[test]
fn a_refusal_ends_the_attempt_and_raises_the_next_ballot() {
    let (model, idle) = start();
    let preparing = b_preparing(&model, &idle);
    let refusal = Msg::Refused {
        ballot: ballot(2, B),
        promised: ballot(5, C),
    };
    let after = model.receive(&preparing, C, B, refusal);
    assert_eq!(after.nodes[1].sealer, Sealer::Idle);
    assert_eq!(after.nodes[1].seen, ballot(5, C));
}

/// A node that receives an accept above its own attempt's ballot gives that attempt up, accepts
/// durably and answers; one that promised higher refuses and says so.
#[test]
fn an_accept_above_the_nodes_own_attempt_supersedes_it() {
    let (model, idle) = start();
    let preparing = b_preparing(&model, &idle);
    let value = prop(3, 7);
    let above = Msg::Accept {
        ballot: ballot(3, C),
        prop: value,
    };
    let after = model.receive(&preparing, C, B, above);
    assert_eq!(after.nodes[1].sealer, Sealer::Idle);
    assert_eq!(after.nodes[1].accepted, Some((ballot(3, C), value)));
    assert_eq!(after.nodes[1].promised, ballot(3, C));
    let below = Msg::Accept {
        ballot: ballot(1, A),
        prop: value,
    };
    let refused = model.receive(&preparing, A, B, below);
    assert_eq!(refused.nodes[1].accepted, None);
    let refusal = Msg::Refused {
        ballot: ballot(1, A),
        promised: ballot(2, B),
    };
    assert!(sent(&refused, B, A, refusal));
}

/// A node that holds the committed seal answers a prepare or an accept with it (the later
/// sealer adopts it); a prepare below its promise is refused; the same seal twice changes
/// nothing, and a different one is flagged as S2.
#[test]
fn a_node_with_the_committed_seal_hands_it_to_a_later_sealer() {
    let (model, idle) = start();
    let sealed = prop(3, 7);
    let held = model.receive(&idle, B, A, Msg::Sealed(sealed));
    assert_eq!(held.nodes[0].committed, Some(sealed));
    assert!(!held.writer.writing);
    assert_eq!(model.receive(&held, C, A, Msg::Sealed(sealed)), held);
    let prepare = Msg::Prepare {
        ballot: ballot(5, B),
    };
    let answered = model.receive(&held, B, A, prepare);
    assert!(sent(&answered, A, B, Msg::Committed(sealed)));
    let accept = Msg::Accept {
        ballot: ballot(5, B),
        prop: prop(1, 1),
    };
    let answered = model.receive(&held, B, A, accept);
    assert!(sent(&answered, A, B, Msg::Committed(sealed)));
    let changed = model.receive(&held, C, A, Msg::Committed(prop(5, 9)));
    let rule = model.check(&changed).unwrap();
    assert!(rule.starts_with("S2"), "{rule}");
    let low = Msg::Prepare {
        ballot: ballot(1, A),
    };
    let mut promised = idle.clone();
    promised.nodes[0].promised = ballot(4, C);
    let refused = model.receive(&promised, B, A, low);
    let refusal = Msg::Refused {
        ballot: ballot(1, A),
        promised: ballot(4, C),
    };
    assert!(sent(&refused, A, B, refusal));
}

/// A refusal of the writer's batch for its ballot or the seal stops the writer; an
/// acknowledgement from a replica is remembered, and the best one is kept.
#[test]
fn the_writer_stops_on_a_refusal_and_remembers_acknowledgements() {
    let (model, idle) = start();
    assert!(idle.writer.writing);
    let stopped = model.receive(&idle, B, A, Msg::Reject(Reject::Ballot));
    assert!(!stopped.writer.writing);
    let acked = model.receive(&idle, B, A, Msg::Ack { committed: 5 });
    assert_eq!(acked.writer.rep, [0, 5, 0]);
    let lower = model.receive(&acked, B, A, Msg::Ack { committed: 3 });
    assert_eq!(lower.writer.rep, [0, 5, 0]);
}

/// A replica that is behind (a lost batch) or whose copy fails its crc refuses the next batch
/// and tells the writer nothing: the writer stalls and the early seal follows.
#[test]
fn a_batch_refused_for_its_offset_or_crc_is_not_answered() {
    let (model, idle) = start();
    let late = Msg::Replicate {
        ballot: ballot(1, A),
        start: 3,
        len: 2,
        crc: model.crc_at(5),
    };
    let after = model.receive(&idle, A, B, late);
    assert!(after.net.is_idle());
    assert_eq!(after.nodes[1].len, 0);
    let mut damaged = idle.clone();
    damaged.nodes[1].bad = true;
    let first = Msg::Replicate {
        ballot: ballot(1, A),
        start: 0,
        len: 3,
        crc: model.crc_at(3),
    };
    let after = model.receive(&damaged, A, B, first);
    assert!(after.net.is_idle());
    assert_eq!(after.nodes[1].len, 0);
}

/// Dropping dead messages keeps what can still change a state and removes what cannot: a
/// stop, an acknowledgement, a promise, an accept, a refusal and a seal that tell the node
/// nothing new; a batch, a prepare, an accept, and the replies of the attempt in progress stay.
#[test]
fn settling_drops_only_messages_that_change_nothing() {
    let (model, idle) = start();
    let mut w = b_preparing(&model, &idle);
    let sealed = prop(3, 7);
    w.nodes[0].committed = Some(sealed);
    w.writer.writing = false;
    let mine = ballot(2, B);
    let old = ballot(1, B);
    let none = copy(0, 0, 0);
    let live = [
        (
            C,
            B,
            Msg::Promise {
                ballot: mine,
                answer: none,
            },
        ),
        (
            B,
            A,
            Msg::Replicate {
                ballot: old,
                start: 0,
                len: 3,
                crc: 1,
            },
        ),
        (C, A, Msg::Prepare { ballot: mine }),
        (
            C,
            A,
            Msg::Accept {
                ballot: mine,
                prop: sealed,
            },
        ),
        (
            C,
            B,
            Msg::Refused {
                ballot: mine,
                promised: ballot(2, C),
            },
        ),
        (
            C,
            B,
            Msg::Refused {
                ballot: old,
                promised: ballot(3, C),
            },
        ),
        (C, A, Msg::Ack { committed: 3 }),
        (C, A, Msg::Sealed(prop(5, 9))),
    ];
    let dead = [
        (
            C,
            B,
            Msg::Promise {
                ballot: old,
                answer: none,
            },
        ),
        (A, B, Msg::Accepted { ballot: mine }),
        (
            C,
            B,
            Msg::Refused {
                ballot: old,
                promised: old,
            },
        ),
        (C, A, Msg::Ack { committed: 0 }),
        (C, A, Msg::Sealed(sealed)),
        (C, A, Msg::Committed(sealed)),
        (B, A, Msg::Reject(Reject::Sealed)),
    ];
    for (from, to, msg) in live.iter().chain(dead.iter()) {
        assert!(w.net.send(*from, *to, *msg));
    }
    let settled = model.settle(&w);
    for (from, to, msg) in &live {
        assert!(sent(&settled, *from, *to, *msg), "{msg:?} is kept");
    }
    for (from, to, msg) in &dead {
        assert!(!sent(&settled, *from, *to, *msg), "{msg:?} is dropped");
    }
    // While the writer still writes, a refusal of its batch is not dead.
    w.writer.writing = true;
    let kept = model.settle(&w);
    assert!(sent(&kept, B, A, Msg::Reject(Reject::Sealed)));
    // A sealer in its accept phase takes the accepted replies of its own ballot only.
    let accepted = Msg::Accepted { ballot: mine };
    let mut acc = b_preparing(&model, &idle);
    acc.nodes[1].sealer = Sealer::Accepting {
        ballot: mine,
        prop: sealed,
        accepts: std::collections::BTreeSet::new(),
    };
    assert!(acc.net.send(C, B, accepted));
    assert!(sent(&model.settle(&acc), C, B, accepted));
    let stale = Msg::Accepted { ballot: old };
    assert!(acc.net.send(C, B, stale));
    assert!(!sent(&model.settle(&acc), C, B, stale));
}
