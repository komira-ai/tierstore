//! The replica's check of a batch (10.3 step 2), Choose (10.3 step 4) and the settings of a
//! scenario of the seal model.

use std::collections::BTreeMap;

use tierstore_format::ids::NodeId;
use tierstore_format::seal::Ballot;
use tierstore_harness::models::seal::{
    Answer, Batch, Config, Cp, Prop, Reject, ReplicaView, SealModel, Variant, check_batch, choose,
};
use tierstore_store_api::hot::ReplicaReject;

const A: NodeId = NodeId(1);
const B: NodeId = NodeId(2);

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

// ---------------------------------------------------------------------------------------------
// The replica's check (10.3 step 2)
// ---------------------------------------------------------------------------------------------

fn view() -> ReplicaView {
    ReplicaView {
        committed: 4,
        copy_crc: crc32c::crc32c(&[1, 2, 3, 4]),
        promised: ballot(2, B),
        sealed: false,
    }
}

fn batch(bytes: &[u8]) -> Batch<'_> {
    let mut all = vec![1, 2, 3, 4];
    all.extend_from_slice(bytes);
    Batch {
        ballot: ballot(2, B),
        start: 4,
        running_crc: crc32c::crc32c(&all),
        bytes,
    }
}

/// A batch at the committed length, with the right running crc and a ballot at or above the
/// promise, is appended and the new committed length is its end.
#[test]
fn a_replica_appends_a_good_batch() {
    assert_eq!(check_batch(&view(), true, &batch(&[5, 6])), Ok(6));
    let higher = Batch {
        ballot: ballot(3, A),
        ..batch(&[5])
    };
    assert_eq!(check_batch(&view(), true, &higher), Ok(5));
}

/// A replica rejects a batch whose start offset is not its committed length, naming both.
///
/// Catches: a replica that appends at the wrong offset and holds a hole or an overlap.
#[test]
fn a_replica_rejects_a_batch_at_the_wrong_offset() {
    let b = Batch {
        start: 6,
        ..batch(&[5])
    };
    assert_eq!(
        check_batch(&view(), true, &b),
        Err(ReplicaReject::Offset {
            start: 6,
            committed: 4
        })
    );
}

/// A replica rejects a batch whose running crc does not match its bytes plus the batch, naming
/// what was sent and what it computed.
///
/// Catches: a replica that appends after a torn or corrupted copy.
#[test]
fn a_replica_rejects_a_batch_with_a_wrong_running_crc() {
    let good = batch(&[5, 6]);
    let b = Batch {
        running_crc: good.running_crc ^ 1,
        ..good
    };
    assert_eq!(
        check_batch(&view(), true, &b),
        Err(ReplicaReject::Crc {
            sent: good.running_crc ^ 1,
            computed: good.running_crc
        })
    );
}

/// A replica rejects a batch whose ballot is below its promise (the old writer is fenced); with
/// the fence off the same batch passes, which is what the `NoFence` variant models.
#[test]
fn a_replica_rejects_a_batch_below_its_promise() {
    let b = Batch {
        ballot: ballot(1, A),
        ..batch(&[5])
    };
    assert_eq!(
        check_batch(&view(), true, &b),
        Err(ReplicaReject::Ballot {
            ballot: ballot(1, A),
            promised: ballot(2, B)
        })
    );
    assert_eq!(check_batch(&view(), false, &b), Ok(5));
}

/// A sealed copy rejects every batch, before any other check.
#[test]
fn a_sealed_replica_rejects_every_batch() {
    let sealed = ReplicaView {
        sealed: true,
        ..view()
    };
    assert_eq!(
        check_batch(&sealed, true, &batch(&[5])),
        Err(ReplicaReject::Sealed)
    );
}

/// The kinds the model carries are the kinds of the store API's reject.
#[test]
fn the_model_keeps_the_kind_of_each_reject() {
    let all = [
        (
            ReplicaReject::Offset {
                start: 1,
                committed: 2,
            },
            Reject::Offset,
        ),
        (
            ReplicaReject::Crc {
                sent: 1,
                computed: 2,
            },
            Reject::Crc,
        ),
        (
            ReplicaReject::Ballot {
                ballot: ballot(1, A),
                promised: ballot(2, A),
            },
            Reject::Ballot,
        ),
        (ReplicaReject::Sealed, Reject::Sealed),
    ];
    for (api, kind) in all {
        assert_eq!(Reject::from(api), kind);
    }
}

// ---------------------------------------------------------------------------------------------
// Choose (10.3 step 4)
// ---------------------------------------------------------------------------------------------

fn promises(list: &[Answer]) -> BTreeMap<NodeId, Answer> {
    (1..).map(NodeId).zip(list.iter().copied()).collect()
}

/// With no accepted proposal the sealer proposes the `r`-th longest verified length,
/// `r = max(1, q - replicas + min_acks)`: the shortest of the answering copies at the default
/// `min_acks`, the longest when one acknowledging copy may be missing, never an unanswering one.
#[test]
fn choose_takes_the_rth_longest_verified_copy() {
    let all = promises(&[copy(9, 9, 90), copy(7, 7, 70), copy(3, 3, 30)]);
    let design = Variant::Design;
    assert_eq!(choose(&all, 3, 3, 0, design), Some(prop(3, 30)));
    assert_eq!(choose(&all, 3, 2, 0, design), Some(prop(7, 70)));
    assert_eq!(choose(&all, 3, 1, 0, design), Some(prop(9, 90)));
    // Two of three copies answered (the third node is down or silent).
    let two = promises(&[copy(9, 9, 90), copy(7, 7, 70)]);
    assert_eq!(choose(&two, 3, 3, 0, design), Some(prop(7, 70)));
    assert_eq!(choose(&two, 3, 2, 0, design), Some(prop(9, 90)));
    // A voter with no copy does not count among the answering copies.
    let voter = Answer {
        accepted: None,
        copy: None,
    };
    let with_voter = promises(&[copy(9, 9, 90), voter]);
    assert_eq!(choose(&with_voter, 2, 2, 0, design), Some(prop(9, 90)));
}

/// A copy whose verified length is below its committed length counts as not answering: it is
/// neither proposed nor counted in `q`. The `Longest` variant counts it and proposes its length
/// with the crc it reports, which does not describe the bytes.
#[test]
fn choose_excludes_a_copy_with_damage() {
    let damaged = promises(&[copy(9, 0, 0), copy(7, 7, 70), copy(3, 3, 30)]);
    assert_eq!(
        choose(&damaged, 3, 3, 0, Variant::Design),
        Some(prop(3, 30))
    );
    assert_eq!(
        choose(&damaged, 3, 2, 0, Variant::Design),
        Some(prop(7, 70))
    );
    assert_eq!(
        choose(&damaged, 3, 3, 0, Variant::Longest),
        Some(prop(9, 0))
    );
    let only_damaged = promises(&[copy(9, 0, 0)]);
    assert_eq!(choose(&only_damaged, 3, 3, 0, Variant::Design), None);
}

/// A sealer that hears of an accepted proposal re-proposes the one with the highest ballot,
/// whatever its copies say; `IgnoreAccepted` skips this and proposes by the copies.
#[test]
fn choose_re_proposes_the_highest_accepted_proposal() {
    let mut a = copy(9, 9, 90);
    a.accepted = Some((ballot(2, A), prop(4, 40)));
    let mut b = copy(7, 7, 70);
    b.accepted = Some((ballot(3, B), prop(5, 50)));
    let all = promises(&[a, b, copy(3, 3, 30)]);
    assert_eq!(choose(&all, 3, 3, 0, Variant::Design), Some(prop(5, 50)));
    assert_eq!(
        choose(&all, 3, 3, 0, Variant::IgnoreAccepted),
        Some(prop(3, 30))
    );
}

/// The proposal is never below the sealer's own last acknowledged offset: at the floor it is
/// made, below it the sealer makes none, and with no copy answering there is none either.
#[test]
fn choose_never_proposes_below_the_floor() {
    let all = promises(&[copy(9, 9, 90), copy(3, 3, 30)]);
    assert_eq!(choose(&all, 3, 3, 3, Variant::Design), Some(prop(3, 30)));
    assert_eq!(choose(&all, 3, 3, 4, Variant::Design), None);
    assert_eq!(choose(&BTreeMap::new(), 3, 3, 0, Variant::Design), None);
}

// ---------------------------------------------------------------------------------------------
// The scenario settings
// ---------------------------------------------------------------------------------------------

/// A scenario of the design is bounded: `replicas` 2 or 3, `min_acks` 1 to `replicas`, 1 to 3
/// records; anything else is a test bug and the constructor says so.
#[test]
#[should_panic(expected = "2 or 3 replicas")]
fn a_scenario_with_four_replicas_is_refused() {
    let _ = SealModel::new(Config {
        replicas: 4,
        ..Config::default()
    });
}

/// `min_acks` above `replicas` is refused.
#[test]
#[should_panic(expected = "2 or 3 replicas")]
fn a_scenario_with_more_acks_than_replicas_is_refused() {
    let _ = SealModel::new(Config {
        replicas: 2,
        min_acks: 3,
        ..Config::default()
    });
}

/// No records is refused.
#[test]
#[should_panic(expected = "1 to 3 records")]
fn a_scenario_with_no_records_is_refused() {
    let _ = SealModel::new(Config {
        records: 0,
        ..Config::default()
    });
}

/// The model reports its scenario and the crc32c of any prefix of its segment: the crc of the
/// empty prefix is 0, a longer one differs, and a length past the end is clipped to the segment.
#[test]
fn the_model_reports_its_scenario_and_prefix_crcs() {
    let model = SealModel::new(Config {
        records: 3,
        ..Config::default()
    });
    assert_eq!(model.config().records, 3);
    assert_eq!(model.crc_at(0), 0);
    assert_ne!(model.crc_at(3), model.crc_at(5));
    assert_eq!(model.crc_at(9), model.crc_at(1000));
}
