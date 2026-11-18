//! The stripe descriptor and the code of a stripe: the shape rules `check` enforces, the byte layout
//! and every way a decode can refuse bytes.
//!
//! Catches:
//! - a code with no data shard, no parity or no copy that is accepted, or a width and a data block
//!   count that disagree with `k + m` and `1`;
//! - a stripe with no block, more blocks than `k`, or two blocks in the replicas mode;
//! - shard indexes that repeat, descend, fall outside the code, or name a virtual zero shard of a
//!   partial stripe, and are accepted (and the replicas copy indexes, which may exceed the block count,
//!   that are refused);
//! - a descriptor that does not round trip, or whose bytes differ from the documented layout;
//! - a short buffer, a wrong magic, a wrong version, an unknown code, a flipped byte or a body that
//!   ends early that decodes, and a decode that skips the shape check after a valid crc.

use tierstore_format::FormatError;
use tierstore_format::crc32c;
use tierstore_format::descriptor::{
    Code, DESCRIPTOR_MAGIC, DESCRIPTOR_VERSION, PlacedShard, ShardLoc, StripeDescriptor,
};
use tierstore_format::ids::{BlockId, DriveId, ExpiryClass, NodeId, StripeId};

const WHAT: &str = "stripe descriptor";

fn shard(index: u8) -> PlacedShard {
    PlacedShard {
        index,
        loc: ShardLoc {
            node: NodeId(index.wrapping_add(10)),
            drive: DriveId([index.wrapping_add(1); 16]),
        },
    }
}

fn blocks(n: u64) -> Vec<(BlockId, u64)> {
    (0..n)
        .map(|i| (BlockId::from_raw(0x0100_0000_0000_0000 + i), 1000 * (i + 1)))
        .collect()
}

fn desc(code: Code, nblocks: u64, shard_indexes: &[u8]) -> StripeDescriptor {
    StripeDescriptor {
        stripe: StripeId::from_raw(0x0200_0000_0000_0042),
        revision: 3,
        code,
        blocks: blocks(nblocks),
        shards: shard_indexes.iter().map(|i| shard(*i)).collect(),
        expiry: ExpiryClass(5),
        coordinator: NodeId(2),
    }
}

fn full() -> StripeDescriptor {
    desc(Code::Ec { k: 4, m: 2 }, 4, &[0, 1, 2, 3, 4, 5])
}

fn recrc(mut bytes: Vec<u8>) -> Vec<u8> {
    let n = bytes.len() - 4;
    let crc = crc32c(&bytes[..n]);
    bytes[n..].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn is_invalid(r: Result<(), FormatError>) -> bool {
    matches!(r, Err(FormatError::Invalid { what: WHAT, .. }))
}

#[test]
fn a_code_knows_its_width_and_its_data_blocks() {
    assert_eq!(Code::Ec { k: 4, m: 2 }.width(), 6);
    assert_eq!(Code::Ec { k: 4, m: 2 }.data_blocks(), 4);
    assert_eq!(Code::Ec { k: 255, m: 255 }.width(), 510);
    assert_eq!(Code::Ec { k: 255, m: 255 }.data_blocks(), 255);
    assert_eq!(Code::Replicas(3).width(), 3);
    assert_eq!(Code::Replicas(255).width(), 255);
    assert_eq!(Code::Replicas(3).data_blocks(), 1);
}

#[test]
fn a_full_stripe_a_partial_stripe_and_a_replicated_block_pass_the_check() {
    assert!(full().check().is_ok());
    assert!(
        desc(Code::Ec { k: 4, m: 2 }, 2, &[0, 1, 4, 5])
            .check()
            .is_ok()
    );
    assert!(desc(Code::Ec { k: 4, m: 2 }, 1, &[0, 4]).check().is_ok());
    assert!(desc(Code::Replicas(3), 1, &[0, 1, 2]).check().is_ok());
    assert!(desc(Code::Replicas(3), 1, &[1, 2]).check().is_ok());
    assert!(desc(Code::Ec { k: 4, m: 2 }, 4, &[]).check().is_ok());
    assert!(
        desc(Code::Ec { k: 255, m: 1 }, 255, &[0, 254, 255])
            .check()
            .is_ok()
    );
}

#[test]
fn a_code_without_a_data_shard_or_without_redundancy_is_refused() {
    for code in [
        Code::Ec { k: 0, m: 2 },
        Code::Ec { k: 4, m: 0 },
        Code::Ec { k: 0, m: 0 },
        Code::Replicas(0),
    ] {
        let d = StripeDescriptor { code, ..full() };
        assert!(is_invalid(d.check()), "{code:?}");
        assert!(d.encode().is_err(), "{code:?}");
    }
    let err = StripeDescriptor {
        code: Code::Replicas(0),
        ..full()
    }
    .check()
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "stripe descriptor: a code needs at least one data shard and one redundant shard or copy"
    );
}

#[test]
fn the_block_count_must_be_between_one_and_k() {
    assert!(is_invalid(
        desc(Code::Ec { k: 4, m: 2 }, 0, &[4, 5]).check()
    ));
    assert!(is_invalid(desc(Code::Ec { k: 4, m: 2 }, 5, &[]).check()));
    assert!(desc(Code::Ec { k: 4, m: 2 }, 4, &[]).check().is_ok());
    assert!(desc(Code::Ec { k: 4, m: 2 }, 1, &[]).check().is_ok());
    assert!(is_invalid(desc(Code::Replicas(3), 0, &[0]).check()));
    assert!(is_invalid(desc(Code::Replicas(3), 2, &[0]).check()));
    let err = desc(Code::Ec { k: 4, m: 2 }, 0, &[]).check().unwrap_err();
    assert_eq!(
        err.to_string(),
        "stripe descriptor: block count outside 1..=k"
    );
}

#[test]
fn shard_indexes_must_be_ascending_unique_stored_and_inside_the_code() {
    let ec = Code::Ec { k: 4, m: 2 };
    // Repeated, descending.
    assert!(is_invalid(desc(ec, 4, &[0, 0]).check()));
    assert!(is_invalid(desc(ec, 4, &[1, 0]).check()));
    assert!(is_invalid(desc(ec, 4, &[0, 1, 2, 3, 5, 4]).check()));
    // Outside the width: 6 is the first index past `k + m`.
    assert!(is_invalid(desc(ec, 4, &[0, 1, 2, 3, 4, 5, 6]).check()));
    assert!(is_invalid(desc(ec, 4, &[6]).check()));
    assert!(is_invalid(desc(ec, 4, &[255]).check()));
    assert!(desc(ec, 4, &[5]).check().is_ok());
    // Replicas: width 3.
    assert!(is_invalid(desc(Code::Replicas(3), 1, &[0, 1, 3]).check()));
    assert!(is_invalid(desc(Code::Replicas(3), 1, &[1, 1]).check()));
    assert!(is_invalid(desc(Code::Replicas(3), 1, &[2, 1]).check()));
    let err = desc(ec, 4, &[1, 0]).check().unwrap_err();
    assert_eq!(
        err.to_string(),
        "stripe descriptor: shard indexes not ascending, unique, stored and inside the code"
    );
}

#[test]
fn a_virtual_zero_data_shard_of_a_partial_stripe_is_not_stored() {
    let ec = Code::Ec { k: 4, m: 2 };
    // Two blocks: data shards 2 and 3 are virtual zeros.
    assert!(is_invalid(desc(ec, 2, &[0, 1, 2, 4, 5]).check()));
    assert!(is_invalid(desc(ec, 2, &[0, 1, 3]).check()));
    assert!(is_invalid(desc(ec, 2, &[2]).check()));
    assert!(desc(ec, 2, &[1, 4]).check().is_ok());
    // The boundaries of the virtual range: index j (2) and k - 1 (3) are refused, k (4) is parity.
    assert!(desc(ec, 2, &[4]).check().is_ok());
    assert!(is_invalid(desc(ec, 3, &[3]).check()));
    assert!(desc(ec, 3, &[2]).check().is_ok());
}

#[test]
fn a_descriptor_round_trips_in_every_mode() {
    for d in [
        full(),
        desc(Code::Ec { k: 4, m: 2 }, 2, &[0, 1, 4, 5]),
        desc(Code::Replicas(3), 1, &[0, 1, 2]),
        desc(Code::Ec { k: 4, m: 2 }, 4, &[]),
        StripeDescriptor {
            revision: u32::MAX,
            expiry: ExpiryClass(255),
            coordinator: NodeId(255),
            ..full()
        },
        StripeDescriptor {
            revision: 0,
            expiry: ExpiryClass(0),
            coordinator: NodeId(0),
            ..full()
        },
    ] {
        let bytes = d.encode().unwrap();
        assert_eq!(StripeDescriptor::decode(&bytes).unwrap(), d);
        assert_eq!(
            StripeDescriptor::decode(&bytes).unwrap().encode().unwrap(),
            bytes
        );
    }
}

#[test]
fn the_bytes_of_a_descriptor_follow_the_documented_layout() {
    assert_eq!(DESCRIPTOR_MAGIC.to_le_bytes(), *b"TSSD");
    assert_eq!(DESCRIPTOR_VERSION, 1);
    let d = desc(Code::Ec { k: 4, m: 2 }, 1, &[0, 4]);
    let b = d.encode().unwrap();
    assert_eq!(&b[0..4], b"TSSD");
    assert_eq!(&b[4..6], &1u16.to_le_bytes());
    assert_eq!(&b[6..9], &[1, 4, 2]);
    assert_eq!(&b[9..17], &0x0200_0000_0000_0042u64.to_le_bytes());
    assert_eq!(&b[17..21], &3u32.to_le_bytes());
    assert_eq!(b[21], 5);
    assert_eq!(b[22], 2);
    assert_eq!(&b[23..25], &1u16.to_le_bytes());
    assert_eq!(&b[25..33], &0x0100_0000_0000_0000u64.to_le_bytes());
    assert_eq!(&b[33..41], &1000u64.to_le_bytes());
    assert_eq!(&b[41..43], &2u16.to_le_bytes());
    assert_eq!(&b[43..45], &[0, 10]);
    assert_eq!(&b[45..61], &[1u8; 16]);
    assert_eq!(&b[61..63], &[4, 14]);
    assert_eq!(&b[63..79], &[5u8; 16]);
    assert_eq!(b.len(), 83);
    assert_eq!(&b[79..], &crc32c(&b[..79]).to_le_bytes());
    let r = desc(Code::Replicas(3), 1, &[0]).encode().unwrap();
    assert_eq!(&r[6..8], &[2, 3]);
    assert_eq!(&r[8..16], &0x0200_0000_0000_0042u64.to_le_bytes());
}

#[test]
fn encode_refuses_a_descriptor_that_fails_the_check() {
    assert!(desc(Code::Ec { k: 4, m: 2 }, 0, &[]).encode().is_err());
    assert!(desc(Code::Ec { k: 4, m: 2 }, 4, &[1, 1]).encode().is_err());
}

#[test]
fn a_buffer_shorter_than_a_crc_is_short() {
    for n in [0usize, 1, 3] {
        let err = StripeDescriptor::decode(&vec![0u8; n]).unwrap_err();
        assert_eq!(
            err,
            FormatError::Short {
                what: WHAT,
                need: 4,
                have: n as u64
            }
        );
    }
}

#[test]
fn a_flipped_byte_anywhere_in_a_descriptor_is_an_error() {
    let good = full().encode().unwrap();
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x04;
        assert!(StripeDescriptor::decode(&bad).is_err(), "byte {i}");
    }
    for i in 0..good.len() - 4 {
        let mut bad = good.clone();
        bad[i] ^= 0x04;
        assert!(
            matches!(
                StripeDescriptor::decode(&bad),
                Err(FormatError::BadCrc { what: WHAT, .. })
            ),
            "byte {i}"
        );
    }
}

#[test]
fn a_cut_descriptor_is_an_error_at_every_length() {
    let good = full().encode().unwrap();
    for n in 0..good.len() {
        assert!(StripeDescriptor::decode(&good[..n]).is_err(), "length {n}");
    }
}

#[test]
fn a_wrong_magic_with_a_valid_crc_is_a_bad_magic() {
    let mut b = full().encode().unwrap();
    b[..4].copy_from_slice(b"NOPE");
    let err = StripeDescriptor::decode(&recrc(b)).unwrap_err();
    assert_eq!(
        err,
        FormatError::BadMagic {
            what: WHAT,
            found: u32::from_le_bytes(*b"NOPE")
        }
    );
}

#[test]
fn another_version_with_a_valid_crc_is_unsupported() {
    for version in [0u16, 2, 65535] {
        let mut b = full().encode().unwrap();
        b[4..6].copy_from_slice(&version.to_le_bytes());
        let err = StripeDescriptor::decode(&recrc(b)).unwrap_err();
        assert_eq!(
            err,
            FormatError::UnsupportedVersion {
                what: WHAT,
                version: u32::from(version)
            }
        );
    }
}

#[test]
fn an_unknown_code_kind_with_a_valid_crc_is_refused() {
    for kind in [0u8, 3, 255] {
        let mut b = full().encode().unwrap();
        b[6] = kind;
        let err = StripeDescriptor::decode(&recrc(b)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "stripe descriptor: unknown code",
            "kind {kind}"
        );
    }
}

#[test]
fn a_code_with_zero_fields_with_a_valid_crc_is_refused_on_decode() {
    for (k, m) in [(0u8, 2u8), (4, 0)] {
        let mut b = full().encode().unwrap();
        b[7] = k;
        b[8] = m;
        let err = StripeDescriptor::decode(&recrc(b)).unwrap_err();
        assert!(
            matches!(err, FormatError::Invalid { what: WHAT, .. }),
            "{k},{m}: {err}"
        );
    }
    let mut r = desc(Code::Replicas(3), 1, &[0, 1, 2]).encode().unwrap();
    r[7] = 0;
    assert!(matches!(
        StripeDescriptor::decode(&recrc(r)),
        Err(FormatError::Invalid { what: WHAT, .. })
    ));
}

#[test]
fn a_decode_runs_the_shape_check_after_the_crc() {
    // The full stripe's shard entries start at 91 and are 18 bytes each; make the second index 0.
    let mut b = full().encode().unwrap();
    assert_eq!(b[91], 0);
    assert_eq!(b[91 + 18], 1);
    b[91 + 18] = 0;
    let err = StripeDescriptor::decode(&recrc(b)).unwrap_err();
    assert!(
        matches!(err, FormatError::Invalid { what: WHAT, .. }),
        "{err}"
    );
    // A block count of zero.
    let mut b = desc(Code::Ec { k: 4, m: 2 }, 1, &[0, 4]).encode().unwrap();
    b[23..25].copy_from_slice(&0u16.to_le_bytes());
    assert!(StripeDescriptor::decode(&recrc(b)).is_err());
}

#[test]
fn a_body_whose_counts_run_past_its_end_is_short() {
    // The block count claims 65535 blocks.
    let mut b = full().encode().unwrap();
    b[23..25].copy_from_slice(&65535u16.to_le_bytes());
    let err = StripeDescriptor::decode(&recrc(b)).unwrap_err();
    assert!(
        matches!(err, FormatError::Short { what: WHAT, .. }),
        "{err}"
    );
    // The shard count claims 65535 shards.
    let mut b = full().encode().unwrap();
    b[89..91].copy_from_slice(&65535u16.to_le_bytes());
    let err = StripeDescriptor::decode(&recrc(b)).unwrap_err();
    assert!(
        matches!(err, FormatError::Short { what: WHAT, .. }),
        "{err}"
    );
}
