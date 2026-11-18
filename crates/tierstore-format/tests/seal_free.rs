//! The seal state with its ballots, and the stripe-free record: the byte layouts and every way a
//! decode can refuse bytes.
//!
//! Catches:
//! - a ballot order that is not round first, then node, or an opening ballot that is not round 1
//!   under the writer;
//! - a seal state that does not round trip with and without an accepted proposal and with and
//!   without a committed one, or whose bytes differ from the documented layout;
//! - a flag byte other than 0 or 1 that is read as a flag;
//! - a wrong magic, a wrong version, a flipped byte, a cut buffer or a body that ends early that
//!   decodes, for either record;
//! - ballots and proposals that are not plain JSON objects of their fields.

use tierstore_format::FormatError;
use tierstore_format::crc32c;
use tierstore_format::free::{FREE_MAGIC, FREE_VERSION, StripeFree};
use tierstore_format::ids::{BlockId, NodeId, StripeId};
use tierstore_format::seal::{Ballot, SEAL_MAGIC, SEAL_VERSION, SealProposal, SealState};
use tierstore_format::stamp::Stamp;

const SEAL: &str = "seal state";
const FREE: &str = "stripe free";

fn recrc(mut bytes: Vec<u8>) -> Vec<u8> {
    let n = bytes.len() - 4;
    let crc = crc32c(&bytes[..n]);
    bytes[n..].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn with_crc(prefix: &[u8]) -> Vec<u8> {
    let mut v = prefix.to_vec();
    v.extend_from_slice(&crc32c(prefix).to_le_bytes());
    v
}

fn proposal(length: u64, crc: u32) -> SealProposal {
    SealProposal {
        length,
        running_crc: crc,
    }
}

fn state(accepted: bool, committed: bool) -> SealState {
    SealState {
        block: BlockId::from_raw(0x0500_0000_0000_0009),
        promised: Ballot {
            round: 7,
            node: NodeId(2),
        },
        accepted: accepted.then(|| {
            (
                Ballot {
                    round: 6,
                    node: NodeId(1),
                },
                proposal(4096, 0xaabb_ccdd),
            )
        }),
        committed: committed.then(|| proposal(8192, 0x1122_3344)),
    }
}

fn free() -> StripeFree {
    StripeFree {
        stripe: StripeId::from_raw(0x0600_0000_0000_0001),
        revision: 4,
        owner: NodeId(3),
        stamp: Stamp::from_raw(0x0102_0304_0506_0708),
    }
}

#[test]
fn ballots_order_by_round_then_node() {
    let b = |round, node| Ballot {
        round,
        node: NodeId(node),
    };
    assert!(b(1, 200) < b(2, 0));
    assert!(b(2, 3) < b(2, 4));
    assert!(b(2, 4) > b(2, 3));
    assert_eq!(b(2, 3), b(2, 3));
    assert!(b(u32::MAX, 0) > b(u32::MAX - 1, 255));
    let mut all = vec![b(2, 1), b(1, 9), b(2, 0), b(1, 2)];
    all.sort();
    assert_eq!(all, vec![b(1, 2), b(1, 9), b(2, 0), b(2, 1)]);
}

#[test]
fn a_segment_opens_at_round_one_under_its_writer() {
    assert_eq!(
        Ballot::opening(NodeId(7)),
        Ballot {
            round: 1,
            node: NodeId(7)
        }
    );
    assert!(
        Ballot::opening(NodeId(7))
            < Ballot {
                round: 2,
                node: NodeId(0)
            }
    );
}

#[test]
fn ballots_and_proposals_are_json_objects_of_their_fields() {
    let b = Ballot {
        round: 3,
        node: NodeId(4),
    };
    assert_eq!(
        serde_json::to_string(&b).unwrap(),
        r#"{"round":3,"node":4}"#
    );
    assert_eq!(
        serde_json::from_str::<Ballot>(r#"{"round":3,"node":4}"#).unwrap(),
        b
    );
    let p = proposal(10, 11);
    assert_eq!(
        serde_json::to_string(&p).unwrap(),
        r#"{"length":10,"running_crc":11}"#
    );
    assert_eq!(
        serde_json::from_str::<SealProposal>(r#"{"length":10,"running_crc":11}"#).unwrap(),
        p
    );
}

#[test]
fn a_seal_state_round_trips_with_and_without_accepted_and_committed() {
    for (accepted, committed) in [(false, false), (true, false), (false, true), (true, true)] {
        let s = state(accepted, committed);
        let bytes = s.encode();
        assert_eq!(
            SealState::decode(&bytes).unwrap(),
            s,
            "{accepted} {committed}"
        );
        assert_eq!(
            bytes.len(),
            25 + if accepted { 17 } else { 0 } + if committed { 12 } else { 0 }
        );
    }
}

#[test]
fn the_seal_bytes_follow_the_documented_layout() {
    assert_eq!(SEAL_MAGIC.to_le_bytes(), *b"TSSL");
    assert_eq!(SEAL_VERSION, 1);
    let b = state(true, true).encode();
    assert_eq!(&b[0..4], b"TSSL");
    assert_eq!(&b[4..6], &1u16.to_le_bytes());
    assert_eq!(&b[6..14], &0x0500_0000_0000_0009u64.to_le_bytes());
    assert_eq!(&b[14..18], &7u32.to_le_bytes());
    assert_eq!(b[18], 2);
    assert_eq!(b[19], 1);
    assert_eq!(&b[20..24], &6u32.to_le_bytes());
    assert_eq!(b[24], 1);
    assert_eq!(&b[25..33], &4096u64.to_le_bytes());
    assert_eq!(&b[33..37], &0xaabb_ccddu32.to_le_bytes());
    assert_eq!(b[37], 1);
    assert_eq!(&b[38..46], &8192u64.to_le_bytes());
    assert_eq!(&b[46..50], &0x1122_3344u32.to_le_bytes());
    assert_eq!(&b[50..54], &crc32c(&b[..50]).to_le_bytes());
    assert_eq!(b.len(), 54);
    let none = state(false, false).encode();
    assert_eq!(&none[19..21], &[0, 0]);
}

#[test]
fn a_flag_byte_that_is_neither_zero_nor_one_is_refused() {
    for flag in [2u8, 3, 255] {
        let mut b = state(false, false).encode();
        b[19] = flag;
        let err = SealState::decode(&recrc(b)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "seal state: a flag is neither 0 nor 1",
            "accepted flag {flag}"
        );
        let mut b = state(false, false).encode();
        b[20] = flag;
        let err = SealState::decode(&recrc(b)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "seal state: a flag is neither 0 nor 1",
            "committed flag {flag}"
        );
    }
}

#[test]
fn a_seal_state_with_a_wrong_magic_or_version_is_refused() {
    let mut b = state(true, true).encode();
    b[..4].copy_from_slice(b"XXXX");
    assert_eq!(
        SealState::decode(&recrc(b)).unwrap_err(),
        FormatError::BadMagic {
            what: SEAL,
            found: u32::from_le_bytes(*b"XXXX")
        }
    );
    for version in [0u16, 2] {
        let mut b = state(true, true).encode();
        b[4..6].copy_from_slice(&version.to_le_bytes());
        assert_eq!(
            SealState::decode(&recrc(b)).unwrap_err(),
            FormatError::UnsupportedVersion {
                what: SEAL,
                version: u32::from(version)
            }
        );
    }
}

#[test]
fn a_flipped_byte_anywhere_in_a_seal_state_is_a_bad_crc() {
    let good = state(true, true).encode();
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x08;
        assert!(
            matches!(
                SealState::decode(&bad),
                Err(FormatError::BadCrc { what: SEAL, .. })
            ),
            "byte {i}"
        );
    }
}

#[test]
fn a_cut_seal_state_is_an_error_at_every_length() {
    let good = state(true, true).encode();
    for n in 0..good.len() {
        assert!(SealState::decode(&good[..n]).is_err(), "length {n}");
    }
    assert_eq!(
        SealState::decode(&good[..3]).unwrap_err(),
        FormatError::Short {
            what: SEAL,
            need: 4,
            have: 3
        }
    );
}

#[test]
fn a_seal_body_that_ends_early_with_a_valid_crc_is_short() {
    let good = state(true, true).encode();
    // Every prefix of the body up to the last field, given its own crc.
    for n in [4usize, 5, 10, 14, 17, 18, 19, 22, 24, 30, 36, 37, 40, 49] {
        let err = SealState::decode(&with_crc(&good[..n])).unwrap_err();
        assert!(
            matches!(err, FormatError::Short { what: SEAL, .. }),
            "prefix {n}: {err}"
        );
    }
    assert!(SealState::decode(&with_crc(&good[..50])).is_ok());
}

#[test]
fn a_stripe_free_round_trips_and_follows_the_layout() {
    assert_eq!(FREE_MAGIC.to_le_bytes(), *b"TSFR");
    assert_eq!(FREE_VERSION, 1);
    let f = free();
    let b = f.encode();
    assert_eq!(StripeFree::decode(&b).unwrap(), f);
    assert_eq!(b.len(), 31);
    assert_eq!(&b[0..4], b"TSFR");
    assert_eq!(&b[4..6], &1u16.to_le_bytes());
    assert_eq!(&b[6..14], &0x0600_0000_0000_0001u64.to_le_bytes());
    assert_eq!(&b[14..18], &4u32.to_le_bytes());
    assert_eq!(b[18], 3);
    assert_eq!(&b[19..27], &0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(&b[27..31], &crc32c(&b[..27]).to_le_bytes());
    let edge = StripeFree {
        stripe: StripeId::from_raw(u64::MAX),
        revision: u32::MAX,
        owner: NodeId(255),
        stamp: Stamp::from_raw(u64::MAX),
    };
    assert_eq!(StripeFree::decode(&edge.encode()).unwrap(), edge);
}

#[test]
fn a_stripe_free_with_a_wrong_magic_or_version_is_refused() {
    let mut b = free().encode();
    b[..4].copy_from_slice(b"ZZZZ");
    assert_eq!(
        StripeFree::decode(&recrc(b)).unwrap_err(),
        FormatError::BadMagic {
            what: FREE,
            found: u32::from_le_bytes(*b"ZZZZ")
        }
    );
    for version in [0u16, 2] {
        let mut b = free().encode();
        b[4..6].copy_from_slice(&version.to_le_bytes());
        assert_eq!(
            StripeFree::decode(&recrc(b)).unwrap_err(),
            FormatError::UnsupportedVersion {
                what: FREE,
                version: u32::from(version)
            }
        );
    }
}

#[test]
fn a_flipped_byte_anywhere_in_a_stripe_free_is_a_bad_crc() {
    let good = free().encode();
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x02;
        assert!(
            matches!(
                StripeFree::decode(&bad),
                Err(FormatError::BadCrc { what: FREE, .. })
            ),
            "byte {i}"
        );
    }
}

#[test]
fn a_cut_stripe_free_is_an_error_at_every_length() {
    let good = free().encode();
    for n in 0..good.len() {
        assert!(StripeFree::decode(&good[..n]).is_err(), "length {n}");
    }
    assert_eq!(
        StripeFree::decode(&[]).unwrap_err(),
        FormatError::Short {
            what: FREE,
            need: 4,
            have: 0
        }
    );
}

#[test]
fn a_stripe_free_body_that_ends_early_with_a_valid_crc_is_short() {
    let good = free().encode();
    for n in [4usize, 5, 8, 14, 17, 18, 26] {
        let err = StripeFree::decode(&with_crc(&good[..n])).unwrap_err();
        assert!(
            matches!(err, FormatError::Short { what: FREE, .. }),
            "prefix {n}: {err}"
        );
    }
}
