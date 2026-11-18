//! Proves the write routes of the fake storage node: replication batches (append, and each reason
//! to refuse one), the majority seal's voter (promise, accept, the fence of a lower ballot, the
//! committed seal that never changes) and the commit of descriptors and frees.
//!
//! Catches:
//! - a batch appended at the wrong offset, with a wrong running crc, under a ballot below the
//!   promise, or onto a sealed copy;
//! - a refused batch that still creates or changes a copy, or the wrong rule winning when two
//!   apply;
//! - a voter that lets a lower ballot prepare or accept after a higher promise, forgets an
//!   accepted proposal, or changes a committed seal;
//! - a sealed broadcast that leaves bytes past the sealed length, or creates a copy from nothing;
//! - a descriptor accepted over a free or at a stale revision, a record that does not decode that
//!   is answered as durable, a free that is not kept.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod fake_node_support;

use fake_node_support::*;
use tierstore_format::crc32c;
use tierstore_format::descriptor::{Code, PlacedShard, ShardLoc, StripeDescriptor};
use tierstore_format::free::StripeFree;
use tierstore_format::ids::{ExpiryClass, NodeId, StripeId};
use tierstore_format::seal::SealProposal;
use tierstore_format::stamp::Stamp;
use tierstore_store_api::peer::PeerError;
use tierstore_store_api::proto::{
    CommitAnswer, CommitRecord, CopyWire, HexBytes, ReplicaAnswer, SealAnswer, SealMessage,
};

fn stripe(counter: u64) -> StripeId {
    StripeId::new(NodeId(1), counter).unwrap()
}

fn descriptor(stripe: StripeId, revision: u32) -> StripeDescriptor {
    StripeDescriptor {
        stripe,
        revision,
        code: Code::Ec { k: 2, m: 1 },
        blocks: vec![(block(1, 1), 100)],
        shards: vec![
            PlacedShard {
                index: 0,
                loc: ShardLoc {
                    node: NodeId(1),
                    drive: drive(1),
                },
            },
            PlacedShard {
                index: 2,
                loc: ShardLoc {
                    node: NodeId(2),
                    drive: drive(2),
                },
            },
        ],
        expiry: ExpiryClass(0),
        coordinator: NodeId(1),
    }
}

fn free(stripe: StripeId, revision: u32) -> StripeFree {
    StripeFree {
        stripe,
        revision,
        owner: NodeId(1),
        stamp: Stamp::from_raw(5),
    }
}

// Replication.

#[tokio::test]
async fn contiguous_batches_are_appended_and_acked_with_the_committed_length() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    let first = node
        .replicate(batch(seg, ballot(1, 1), 0, crc32c(b"hel"), b"hel"))
        .await
        .unwrap();
    assert_eq!(first.body, ReplicaAnswer::Acked { committed: 3 });
    let second = node
        .replicate(batch(seg, ballot(1, 1), 3, crc32c(b"hello"), b"lo"))
        .await
        .unwrap();
    assert_eq!(second.body, ReplicaAnswer::Acked { committed: 5 });
    assert_eq!(node.copy(seg), Some(b"hello".to_vec()));
    assert_eq!(node.copy(block(1, 2)), None);
}

#[tokio::test]
async fn a_batch_that_does_not_start_at_the_committed_length_is_refused_with_both_numbers() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.replicate(batch(seg, ballot(1, 1), 0, crc32c(b"hello"), b"hello"))
        .await
        .unwrap();
    for start in [0, 4, 6] {
        let refused = node
            .replicate(batch(seg, ballot(1, 1), start, 0, b"x"))
            .await
            .unwrap();
        assert_eq!(
            refused.body,
            ReplicaAnswer::Offset {
                start,
                committed: 5
            }
        );
    }
    assert_eq!(node.copy(seg), Some(b"hello".to_vec()));
}

#[tokio::test]
async fn a_batch_with_the_wrong_running_crc_is_refused_with_the_computed_one() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.replicate(batch(seg, ballot(1, 1), 0, crc32c(b"ab"), b"ab"))
        .await
        .unwrap();
    let bad = crc32c(b"abc") ^ 1;
    let refused = node
        .replicate(batch(seg, ballot(1, 1), 2, bad, b"cd"))
        .await
        .unwrap();
    assert_eq!(
        refused.body,
        ReplicaAnswer::Crc {
            sent: bad,
            computed: crc32c(b"abcd")
        }
    );
    assert_eq!(node.copy(seg), Some(b"ab".to_vec()));
}

#[tokio::test]
async fn a_batch_below_the_promised_ballot_is_refused_and_a_higher_ballot_raises_the_promise() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.replicate(batch(seg, ballot(2, 1), 0, crc32c(b"a"), b"a"))
        .await
        .unwrap();
    let lower = node
        .replicate(batch(seg, ballot(1, 5), 1, crc32c(b"ab"), b"b"))
        .await
        .unwrap();
    assert_eq!(
        lower.body,
        ReplicaAnswer::Ballot {
            ballot: ballot(1, 5),
            promised: ballot(2, 1)
        }
    );
    // The same round under a lower node id is lower too.
    let same_round = node
        .replicate(batch(seg, ballot(2, 0), 1, crc32c(b"ab"), b"b"))
        .await
        .unwrap();
    assert_eq!(
        same_round.body,
        ReplicaAnswer::Ballot {
            ballot: ballot(2, 0),
            promised: ballot(2, 1)
        }
    );
    let higher = node
        .replicate(batch(seg, ballot(3, 0), 1, crc32c(b"ab"), b"b"))
        .await
        .unwrap();
    assert_eq!(higher.body, ReplicaAnswer::Acked { committed: 2 });
    let after = node
        .replicate(batch(seg, ballot(2, 1), 2, crc32c(b"abc"), b"c"))
        .await
        .unwrap();
    assert_eq!(
        after.body,
        ReplicaAnswer::Ballot {
            ballot: ballot(2, 1),
            promised: ballot(3, 0)
        }
    );
    assert_eq!(node.copy(seg), Some(b"ab".to_vec()));
}

#[tokio::test]
async fn a_refused_first_batch_creates_no_copy() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    let refused = node
        .replicate(batch(seg, ballot(5, 1), 3, 0, b"x"))
        .await
        .unwrap();
    assert_eq!(
        refused.body,
        ReplicaAnswer::Offset {
            start: 3,
            committed: 0
        }
    );
    assert_eq!(node.copy(seg), None);
    // A copy that was never created has no promise: a lower ballot opens it.
    let opened = node
        .replicate(batch(seg, ballot(1, 1), 0, crc32c(b"x"), b"x"))
        .await
        .unwrap();
    assert_eq!(opened.body, ReplicaAnswer::Acked { committed: 1 });
}

#[tokio::test]
async fn the_ballot_rule_is_checked_before_the_offset_and_the_offset_before_the_crc() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.replicate(batch(seg, ballot(2, 1), 0, crc32c(b"a"), b"a"))
        .await
        .unwrap();
    // Wrong ballot, wrong offset and wrong crc at once: the ballot is named.
    let all_wrong = node
        .replicate(batch(seg, ballot(1, 1), 9, 0, b"x"))
        .await
        .unwrap();
    assert!(matches!(all_wrong.body, ReplicaAnswer::Ballot { .. }));
    // Wrong offset and wrong crc: the offset is named.
    let offset_and_crc = node
        .replicate(batch(seg, ballot(2, 1), 9, 0, b"x"))
        .await
        .unwrap();
    assert!(matches!(offset_and_crc.body, ReplicaAnswer::Offset { .. }));
}

#[tokio::test]
async fn a_sealed_copy_refuses_every_batch_before_any_other_rule() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.replicate(batch(seg, ballot(1, 1), 0, crc32c(b"abc"), b"abc"))
        .await
        .unwrap();
    let proposal = SealProposal {
        length: 3,
        running_crc: crc32c(b"abc"),
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
    for refused in [
        batch(seg, ballot(1, 1), 3, crc32c(b"abcd"), b"d"),
        batch(seg, ballot(0, 0), 99, 0, b"d"),
    ] {
        let answer = node.replicate(refused).await.unwrap();
        assert_eq!(answer.body, ReplicaAnswer::Sealed);
    }
    assert_eq!(node.copy(seg), Some(b"abc".to_vec()));
}

// The seal voter.

fn prepare(seg: tierstore_format::ids::BlockId, round: u32, node: u8) -> SealMessage {
    SealMessage::Prepare {
        segment: seg,
        ballot: ballot(round, node),
    }
}

fn accept(
    seg: tierstore_format::ids::BlockId,
    round: u32,
    node: u8,
    proposal: SealProposal,
) -> SealMessage {
    SealMessage::Accept {
        segment: seg,
        ballot: ballot(round, node),
        proposal,
    }
}

const P5: SealProposal = SealProposal {
    length: 5,
    running_crc: 55,
};
const P7: SealProposal = SealProposal {
    length: 7,
    running_crc: 77,
};

#[tokio::test]
async fn a_prepare_on_a_fresh_voter_promises_with_nothing_accepted_and_no_copy() {
    let (_clock, node) = node(2);
    let answer = node
        .seal(env(1, 1, prepare(block(1, 1), 1, 1)))
        .await
        .unwrap();
    assert_eq!(
        answer.body,
        SealAnswer::Promise {
            accepted: None,
            copy: None
        }
    );
}

#[tokio::test]
async fn a_promise_describes_the_nodes_copy() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.replicate(batch(seg, ballot(1, 1), 0, crc32c(b"hello"), b"hello"))
        .await
        .unwrap();
    let answer = node.seal(env(3, 1, prepare(seg, 2, 3))).await.unwrap();
    assert_eq!(
        answer.body,
        SealAnswer::Promise {
            accepted: None,
            copy: Some(CopyWire {
                committed: 5,
                verified: 5,
                running_crc: crc32c(b"hello")
            }),
        }
    );
    // Another segment is another voter with no copy.
    let other = node
        .seal(env(3, 1, prepare(block(1, 2), 1, 3)))
        .await
        .unwrap();
    assert_eq!(
        other.body,
        SealAnswer::Promise {
            accepted: None,
            copy: None
        }
    );
}

#[tokio::test]
async fn a_prepare_below_the_promise_is_refused_and_an_equal_or_higher_one_is_promised() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.seal(env(1, 1, prepare(seg, 2, 1))).await.unwrap();
    let lower = node.seal(env(1, 1, prepare(seg, 1, 9))).await.unwrap();
    assert_eq!(
        lower.body,
        SealAnswer::Promised {
            ballot: ballot(2, 1)
        }
    );
    let lower_node = node.seal(env(1, 1, prepare(seg, 2, 0))).await.unwrap();
    assert_eq!(
        lower_node.body,
        SealAnswer::Promised {
            ballot: ballot(2, 1)
        }
    );
    let equal = node.seal(env(1, 1, prepare(seg, 2, 1))).await.unwrap();
    assert!(matches!(equal.body, SealAnswer::Promise { .. }));
    let higher = node.seal(env(1, 1, prepare(seg, 3, 0))).await.unwrap();
    assert!(matches!(higher.body, SealAnswer::Promise { .. }));
    let after = node.seal(env(1, 1, prepare(seg, 2, 1))).await.unwrap();
    assert_eq!(
        after.body,
        SealAnswer::Promised {
            ballot: ballot(3, 0)
        }
    );
}

#[tokio::test]
async fn an_accepted_proposal_is_returned_by_the_next_promise() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.seal(env(1, 1, prepare(seg, 1, 1))).await.unwrap();
    let ok = node.seal(env(1, 1, accept(seg, 1, 1, P5))).await.unwrap();
    assert_eq!(ok.body, SealAnswer::Ok);
    let promise = node.seal(env(2, 1, prepare(seg, 2, 2))).await.unwrap();
    assert_eq!(
        promise.body,
        SealAnswer::Promise {
            accepted: Some((ballot(1, 1), P5)),
            copy: None
        }
    );
}

#[tokio::test]
async fn an_accept_without_a_prepare_is_taken_and_raises_the_promise() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    let ok = node.seal(env(1, 1, accept(seg, 3, 1, P5))).await.unwrap();
    assert_eq!(ok.body, SealAnswer::Ok);
    let below = node.seal(env(2, 1, prepare(seg, 2, 2))).await.unwrap();
    assert_eq!(
        below.body,
        SealAnswer::Promised {
            ballot: ballot(3, 1)
        }
    );
}

#[tokio::test]
async fn two_sealers_a_higher_ballot_fences_the_lower_ones_prepare_and_accept() {
    let (_clock, node) = node(3);
    let seg = block(1, 1);
    // Sealer one (ballot 1.1) prepares and accepts its proposal.
    assert!(matches!(
        node.seal(env(1, 1, prepare(seg, 1, 1))).await.unwrap().body,
        SealAnswer::Promise { accepted: None, .. }
    ));
    assert_eq!(
        node.seal(env(1, 1, accept(seg, 1, 1, P5)))
            .await
            .unwrap()
            .body,
        SealAnswer::Ok
    );
    // Sealer two (ballot 1.2) prepares above it and learns the accepted proposal.
    assert_eq!(
        node.seal(env(2, 1, prepare(seg, 1, 2))).await.unwrap().body,
        SealAnswer::Promise {
            accepted: Some((ballot(1, 1), P5)),
            copy: None
        }
    );
    // Sealer one is fenced: its accept of another proposal and its re-prepare are refused.
    let fenced = SealAnswer::Promised {
        ballot: ballot(1, 2),
    };
    assert_eq!(
        node.seal(env(1, 1, accept(seg, 1, 1, P7)))
            .await
            .unwrap()
            .body,
        fenced
    );
    assert_eq!(
        node.seal(env(1, 1, prepare(seg, 1, 1))).await.unwrap().body,
        fenced
    );
    // Sealer two accepts, and what the node accepted is its proposal under its ballot.
    assert_eq!(
        node.seal(env(2, 1, accept(seg, 1, 2, P7)))
            .await
            .unwrap()
            .body,
        SealAnswer::Ok
    );
    assert_eq!(
        node.seal(env(3, 1, prepare(seg, 2, 3))).await.unwrap().body,
        SealAnswer::Promise {
            accepted: Some((ballot(1, 2), P7)),
            copy: None
        }
    );
}

#[tokio::test]
async fn a_committed_seal_is_returned_unchanged_to_every_prepare_and_accept() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.seal(env(
        1,
        1,
        SealMessage::Sealed {
            segment: seg,
            proposal: P5,
        },
    ))
    .await
    .unwrap();
    let committed = SealAnswer::Committed { proposal: P5 };
    // Not even a ballot above every earlier one gets through.
    assert_eq!(
        node.seal(env(2, 1, prepare(seg, 99, 2)))
            .await
            .unwrap()
            .body,
        committed
    );
    assert_eq!(
        node.seal(env(2, 1, accept(seg, 99, 2, P7)))
            .await
            .unwrap()
            .body,
        committed
    );
    assert_eq!(
        node.seal(env(2, 1, prepare(seg, 1, 1))).await.unwrap().body,
        committed
    );
    // A committed segment is independent of another.
    let other = node
        .seal(env(2, 1, prepare(block(1, 2), 1, 2)))
        .await
        .unwrap();
    assert!(matches!(other.body, SealAnswer::Promise { .. }));
}

#[tokio::test]
async fn the_sealed_broadcast_truncates_a_longer_copy_and_marks_it_sealed() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.replicate(batch(
        seg,
        ballot(1, 1),
        0,
        crc32c(b"hello world"),
        b"hello world",
    ))
    .await
    .unwrap();
    let proposal = SealProposal {
        length: 5,
        running_crc: crc32c(b"hello"),
    };
    let ok = node
        .seal(env(
            1,
            1,
            SealMessage::Sealed {
                segment: seg,
                proposal,
            },
        ))
        .await
        .unwrap();
    assert_eq!(ok.body, SealAnswer::Ok);
    assert_eq!(node.copy(seg), Some(b"hello".to_vec()));
    let refused = node
        .replicate(batch(seg, ballot(1, 1), 5, crc32c(b"hello!"), b"!"))
        .await
        .unwrap();
    assert_eq!(refused.body, ReplicaAnswer::Sealed);
}

#[tokio::test]
async fn the_sealed_broadcast_keeps_a_shorter_copy_and_creates_none() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.replicate(batch(seg, ballot(1, 1), 0, crc32c(b"abc"), b"abc"))
        .await
        .unwrap();
    let proposal = SealProposal {
        length: 10,
        running_crc: 1,
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
    assert_eq!(node.copy(seg), Some(b"abc".to_vec()));
    let none = block(1, 2);
    node.seal(env(
        1,
        1,
        SealMessage::Sealed {
            segment: none,
            proposal,
        },
    ))
    .await
    .unwrap();
    assert_eq!(node.copy(none), None);
}

// Commit.

#[tokio::test]
async fn a_descriptor_is_stored_and_a_higher_revision_replaces_it() {
    let (_clock, node) = node(2);
    let s = stripe(1);
    let record = |rev| env(1, 1, CommitRecord::descriptor(&descriptor(s, rev)).unwrap());
    assert_eq!(node.descriptor(s), None);
    assert_eq!(
        node.commit(record(0)).await.unwrap().body,
        CommitAnswer::Durable
    );
    assert_eq!(node.descriptor(s), Some(descriptor(s, 0)));
    assert_eq!(
        node.commit(record(3)).await.unwrap().body,
        CommitAnswer::Durable
    );
    assert_eq!(node.descriptor(s), Some(descriptor(s, 3)));
}

#[tokio::test]
async fn a_descriptor_at_or_below_the_held_revision_is_stale() {
    let (_clock, node) = node(2);
    let s = stripe(1);
    let record = |rev| env(1, 1, CommitRecord::descriptor(&descriptor(s, rev)).unwrap());
    node.commit(record(4)).await.unwrap();
    for revision in [3, 0] {
        let answer = node.commit(record(revision)).await.unwrap();
        assert_eq!(answer.body, CommitAnswer::StaleRevision { held: 4 });
    }
    // Another descriptor at the held revision is stale too.
    let mut other = descriptor(s, 4);
    other.coordinator = NodeId(2);
    let answer = node
        .commit(env(1, 1, CommitRecord::descriptor(&other).unwrap()))
        .await
        .unwrap();
    assert_eq!(answer.body, CommitAnswer::StaleRevision { held: 4 });
    assert_eq!(node.descriptor(s), Some(descriptor(s, 4)));
}

#[tokio::test]
async fn a_resend_of_the_descriptor_held_is_durable_again() {
    let (_clock, node) = node(2);
    let s = stripe(1);
    let record = |rev| env(1, 1, CommitRecord::descriptor(&descriptor(s, rev)).unwrap());
    assert_eq!(
        node.commit(record(4)).await.unwrap().body,
        CommitAnswer::Durable
    );
    assert_eq!(
        node.commit(record(4)).await.unwrap().body,
        CommitAnswer::Durable
    );
    assert_eq!(node.descriptor(s), Some(descriptor(s, 4)));
}

#[tokio::test]
async fn a_prepare_or_accept_that_raises_the_promise_refuses_a_later_lower_batch() {
    let (_clock, node) = node(2);
    for use_accept in [false, true] {
        let seg = block(1, if use_accept { 2 } else { 1 });
        node.replicate(batch(seg, ballot(1, 1), 0, crc32c(b"a"), b"a"))
            .await
            .unwrap();
        let message = if use_accept {
            accept(seg, 2, 1, P5)
        } else {
            prepare(seg, 2, 1)
        };
        node.seal(env(1, 1, message)).await.unwrap();
        let late = node
            .replicate(batch(seg, ballot(1, 1), 1, crc32c(b"ab"), b"b"))
            .await
            .unwrap();
        assert_eq!(
            late.body,
            ReplicaAnswer::Ballot {
                ballot: ballot(1, 1),
                promised: ballot(2, 1)
            }
        );
        assert_eq!(node.copy(seg), Some(b"a".to_vec()));
        // A batch at the promised ballot is still accepted.
        let ok = node
            .replicate(batch(seg, ballot(2, 1), 1, crc32c(b"ab"), b"b"))
            .await
            .unwrap();
        assert_eq!(ok.body, ReplicaAnswer::Acked { committed: 2 });
    }
}

#[tokio::test]
async fn a_prepare_below_the_copys_promise_leaves_it_and_a_prepare_without_a_copy_makes_none() {
    let (_clock, node) = node(2);
    let seg = block(1, 1);
    node.replicate(batch(seg, ballot(3, 1), 0, crc32c(b"a"), b"a"))
        .await
        .unwrap();
    node.seal(env(1, 1, prepare(seg, 1, 1))).await.unwrap();
    let late = node
        .replicate(batch(seg, ballot(2, 1), 1, crc32c(b"ab"), b"b"))
        .await
        .unwrap();
    assert_eq!(
        late.body,
        ReplicaAnswer::Ballot {
            ballot: ballot(2, 1),
            promised: ballot(3, 1)
        }
    );
    let none = block(1, 9);
    node.seal(env(1, 1, prepare(none, 4, 1))).await.unwrap();
    assert_eq!(node.copy(none), None);
}

#[tokio::test]
async fn a_free_is_stored_and_wins_over_a_later_descriptor_of_its_stripe_only() {
    let (_clock, node) = node(2);
    let (s, other) = (stripe(1), stripe(2));
    assert_eq!(node.freed(s), None);
    let freed = node
        .commit(env(1, 1, CommitRecord::free(&free(s, 2))))
        .await
        .unwrap();
    assert_eq!(freed.body, CommitAnswer::Durable);
    assert_eq!(node.freed(s), Some(free(s, 2)));
    let refused = node
        .commit(env(
            1,
            1,
            CommitRecord::descriptor(&descriptor(s, 9)).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(refused.body, CommitAnswer::FreeWins);
    assert_eq!(node.descriptor(s), None);
    let unaffected = node
        .commit(env(
            1,
            1,
            CommitRecord::descriptor(&descriptor(other, 0)).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(unaffected.body, CommitAnswer::Durable);
}

#[tokio::test]
async fn a_free_after_a_descriptor_is_durable_and_keeps_the_descriptor() {
    let (_clock, node) = node(2);
    let s = stripe(1);
    node.commit(env(
        1,
        1,
        CommitRecord::descriptor(&descriptor(s, 1)).unwrap(),
    ))
    .await
    .unwrap();
    let freed = node
        .commit(env(1, 1, CommitRecord::free(&free(s, 1))))
        .await
        .unwrap();
    assert_eq!(freed.body, CommitAnswer::Durable);
    assert_eq!(node.descriptor(s), Some(descriptor(s, 1)));
    assert_eq!(node.freed(s), Some(free(s, 1)));
}

#[tokio::test]
async fn a_record_that_does_not_decode_is_a_400_and_stores_nothing() {
    let (_clock, node) = node(2);
    let garbage = |make: fn(HexBytes) -> CommitRecord| make(HexBytes(vec![1, 2, 3]));
    let descriptor_err = node
        .commit(env(
            1,
            1,
            garbage(|bytes| CommitRecord::Descriptor { bytes }),
        ))
        .await
        .unwrap_err();
    let free_err = node
        .commit(env(1, 1, garbage(|bytes| CommitRecord::Free { bytes })))
        .await
        .unwrap_err();
    for err in [descriptor_err, free_err] {
        assert!(
            matches!(err, PeerError::Protocol { status: 400, node, .. } if node == NodeId(2)),
            "{err:?}"
        );
    }
    assert_eq!(node.descriptor(stripe(1)), None);
    assert_eq!(node.freed(stripe(1)), None);
}
