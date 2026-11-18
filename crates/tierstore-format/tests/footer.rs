//! The footer of a shard file or a copy file: its layout, its self-locating tail, the stripe it
//! carries and every way a decode can refuse bytes.
//!
//! Catches:
//! - a footer whose length is not a multiple of 4 KiB (at least 4 KiB), or that grows by a whole
//!   4 KiB at the wrong byte count;
//! - a stripe or copy subject that does not round trip, a descriptor that cannot be rebuilt from a
//!   stripe shard's footer, or one that is invented for a copy;
//! - a decode of a whole shard file (data, padding, footer) that does not find the footer at
//!   `footer_offset(data_len)`;
//! - a tail that is too short, a length that is not a multiple of 4 KiB, a wrong magic, a wrong crc,
//!   a wrong version, an unknown subject, a cell table that does not cover the data and a body that
//!   ends early, all of which must be refused;
//! - a stripe subject whose embedded descriptor fails the shape check yet decodes.

use tierstore_format::descriptor::{Code, PlacedShard, ShardLoc, StripeDescriptor};
use tierstore_format::footer::{
    FOOTER_ALIGN, FOOTER_MAGIC, FOOTER_TAIL_LEN, FOOTER_VERSION, FooterSubject, ShardFooter,
    footer_offset,
};
use tierstore_format::ids::{BlockId, DriveId, ExpiryClass, NodeId, StripeId};
use tierstore_format::{CELL_BYTES, FormatError, cells_for, crc32c};

const WHAT: &str = "shard footer";

fn placed(index: u8) -> PlacedShard {
    PlacedShard {
        index,
        loc: ShardLoc {
            node: NodeId(index + 1),
            drive: DriveId([index + 7; 16]),
        },
    }
}

fn placed_any(index: u8) -> PlacedShard {
    PlacedShard {
        index,
        loc: ShardLoc {
            node: NodeId(1),
            drive: DriveId([index; 16]),
        },
    }
}

fn stripe() -> StripeDescriptor {
    StripeDescriptor {
        stripe: StripeId::from_raw(0x0300_0000_0000_0007),
        revision: 2,
        code: Code::Ec { k: 4, m: 2 },
        blocks: vec![(BlockId::from_raw(1), 5000), (BlockId::from_raw(2), 4096)],
        shards: vec![placed(0), placed(1), placed(4), placed(5)],
        expiry: ExpiryClass(1),
        coordinator: NodeId(3),
    }
}

fn data_cells(data_len: u64) -> Vec<u32> {
    (0..cells_for(data_len))
        .map(|i| 0x7000 + i as u32)
        .collect()
}

fn stripe_footer(index: u8, data_len: u64) -> ShardFooter {
    ShardFooter {
        subject: FooterSubject::Stripe {
            stripe: stripe(),
            index,
        },
        data_len,
        cell_crcs: data_cells(data_len),
        blake3: [0x33; 32],
    }
}

fn copy_footer(copies: Vec<PlacedShard>, data_len: u64) -> ShardFooter {
    ShardFooter {
        subject: FooterSubject::Copy {
            block: BlockId::from_raw(0x0900_0000_0000_0001),
            length: data_len,
            expiry: ExpiryClass(9),
            copies,
        },
        data_len,
        cell_crcs: data_cells(data_len),
        blake3: [0x44; 32],
    }
}

fn recrc(mut bytes: Vec<u8>) -> Vec<u8> {
    let n = bytes.len() - 4;
    let crc = crc32c(&bytes[..n]);
    bytes[n..].copy_from_slice(&crc.to_le_bytes());
    bytes
}

#[test]
fn the_constants_are_the_documented_ones() {
    assert_eq!(FOOTER_ALIGN, 4096);
    assert_eq!(FOOTER_TAIL_LEN, 16);
    assert_eq!(FOOTER_VERSION, 1);
    assert_eq!(FOOTER_MAGIC.to_le_bytes(), *b"TSSF");
}

#[test]
fn a_stripe_shard_footer_round_trips_and_rebuilds_the_descriptor() {
    for index in [0u8, 1, 4, 5] {
        let f = stripe_footer(index, 9000);
        let bytes = f.encode().unwrap();
        assert_eq!(bytes.len(), 4096);
        let back = ShardFooter::decode(&bytes).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.descriptor(), Some(&stripe()));
        match back.subject {
            FooterSubject::Stripe { index: i, .. } => assert_eq!(i, index),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_copy_footer_round_trips_and_has_no_descriptor() {
    let f = copy_footer(vec![placed(0), placed(1), placed(2)], 12345);
    let bytes = f.encode().unwrap();
    let back = ShardFooter::decode(&bytes).unwrap();
    assert_eq!(back, f);
    assert!(back.descriptor().is_none());
    let none = copy_footer(vec![], 0);
    let back = ShardFooter::decode(&none.encode().unwrap()).unwrap();
    assert_eq!(back, none);
}

#[test]
fn the_tail_locates_the_footer_from_the_end() {
    let bytes = stripe_footer(0, 100).encode().unwrap();
    let n = bytes.len();
    assert_eq!(&bytes[n - 16..n - 8], &(n as u64).to_le_bytes());
    assert_eq!(&bytes[n - 8..n - 4], b"TSSF");
    assert_eq!(&bytes[n - 4..], &crc32c(&bytes[..n - 4]).to_le_bytes());
    assert_eq!(&bytes[0..2], &1u16.to_le_bytes());
    assert_eq!(bytes[2], 1);
    let copy = copy_footer(vec![], 0).encode().unwrap();
    assert_eq!(copy[2], 2);
}

#[test]
fn the_footer_grows_by_whole_4_kib_at_the_exact_byte() {
    // One copy entry: the body is 86 + 18 + 4 * cells bytes with the 16-byte tail counted.
    let exact = copy_footer(vec![placed(0)], 998 * CELL_BYTES);
    assert_eq!(exact.cell_crcs.len(), 998);
    let bytes = exact.encode().unwrap();
    assert_eq!(bytes.len(), 4096);
    assert_eq!(ShardFooter::decode(&bytes).unwrap(), exact);
    let over = copy_footer(vec![placed(0)], 998 * CELL_BYTES + 1);
    assert_eq!(over.cell_crcs.len(), 999);
    let bytes = over.encode().unwrap();
    assert_eq!(bytes.len(), 8192);
    assert_eq!(ShardFooter::decode(&bytes).unwrap(), over);
    let under = copy_footer(vec![placed(0)], 997 * CELL_BYTES);
    assert_eq!(under.encode().unwrap().len(), 4096);
}

#[test]
fn a_footer_is_always_a_multiple_of_4_kib_and_at_least_4_kib() {
    for data_len in [
        0u64,
        1,
        CELL_BYTES,
        CELL_BYTES + 1,
        500 * CELL_BYTES,
        1500 * CELL_BYTES,
    ] {
        let bytes = stripe_footer(0, data_len).encode().unwrap();
        assert!(bytes.len() >= 4096);
        assert_eq!(bytes.len() % 4096, 0, "data_len {data_len}");
        assert_eq!(ShardFooter::decode(&bytes).unwrap().data_len, data_len);
    }
    assert!(stripe_footer(0, 1500 * CELL_BYTES).encode().unwrap().len() > 4096);
}

#[test]
fn the_footer_offset_is_the_data_length_rounded_up_to_4_kib() {
    assert_eq!(footer_offset(0), 0);
    assert_eq!(footer_offset(1), 4096);
    assert_eq!(footer_offset(4095), 4096);
    assert_eq!(footer_offset(4096), 4096);
    assert_eq!(footer_offset(4097), 8192);
}

#[test]
fn a_whole_shard_file_decodes_from_its_end() {
    for data_len in [0usize, 1, 4095, 4096, 4097, 10_000] {
        let f = stripe_footer(1, data_len as u64);
        let footer = f.encode().unwrap();
        let mut file = vec![0xabu8; data_len];
        file.resize(footer_offset(data_len as u64) as usize, 0);
        let at = file.len();
        file.extend_from_slice(&footer);
        assert_eq!(file.len() % 4096, 0);
        assert_eq!(
            ShardFooter::decode(&file).unwrap(),
            f,
            "data_len {data_len}"
        );
        assert_eq!(at as u64, footer_offset(data_len as u64));
    }
}

#[test]
fn a_tail_shorter_than_sixteen_bytes_is_short() {
    for n in [0usize, 1, 15] {
        let err = ShardFooter::decode(&vec![0u8; n]).unwrap_err();
        assert_eq!(
            err,
            FormatError::Short {
                what: WHAT,
                need: 16,
                have: n as u64
            }
        );
    }
}

#[test]
fn a_footer_length_that_is_not_a_positive_multiple_of_4_kib_is_refused() {
    let good = stripe_footer(0, 10).encode().unwrap();
    for total in [0u64, 4095, 4097, 6144, 100] {
        let mut b = good.clone();
        let n = b.len();
        b[n - 16..n - 8].copy_from_slice(&total.to_le_bytes());
        let err = ShardFooter::decode(&b).unwrap_err();
        assert_eq!(
            err.to_string(),
            "shard footer: footer length is not a positive multiple of 4 KiB",
            "total {total}"
        );
    }
}

#[test]
fn a_footer_longer_than_the_buffer_is_short() {
    let good = stripe_footer(0, 10).encode().unwrap();
    let mut b = good.clone();
    let n = b.len();
    b[n - 16..n - 8].copy_from_slice(&8192u64.to_le_bytes());
    let err = ShardFooter::decode(&b).unwrap_err();
    assert_eq!(
        err,
        FormatError::Short {
            what: WHAT,
            need: 8192,
            have: 4096
        }
    );
    // A huge claimed length does not allocate or overflow.
    b[n - 16..n - 8].copy_from_slice(&(1u64 << 62).to_le_bytes());
    assert!(matches!(
        ShardFooter::decode(&b),
        Err(FormatError::Short { what: WHAT, .. })
    ));
}

#[test]
fn a_wrong_magic_is_refused() {
    let mut b = stripe_footer(0, 10).encode().unwrap();
    let n = b.len();
    b[n - 8] ^= 1;
    assert!(matches!(
        ShardFooter::decode(&b),
        Err(FormatError::BadMagic { what: WHAT, .. })
    ));
}

#[test]
fn a_flipped_byte_anywhere_in_the_footer_is_an_error() {
    let good = stripe_footer(0, 10).encode().unwrap();
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x20;
        assert!(ShardFooter::decode(&bad).is_err(), "byte {i}");
    }
    // The padding is covered by the crc too.
    let mut bad = good.clone();
    bad[2000] ^= 1;
    assert!(matches!(
        ShardFooter::decode(&bad),
        Err(FormatError::BadCrc { what: WHAT, .. })
    ));
}

#[test]
fn another_version_with_a_valid_crc_is_unsupported() {
    for version in [0u16, 2] {
        let mut b = stripe_footer(0, 10).encode().unwrap();
        b[..2].copy_from_slice(&version.to_le_bytes());
        let err = ShardFooter::decode(&recrc(b)).unwrap_err();
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
fn an_unknown_subject_kind_with_a_valid_crc_is_refused() {
    for kind in [0u8, 3, 255] {
        let mut b = stripe_footer(0, 10).encode().unwrap();
        b[2] = kind;
        let err = ShardFooter::decode(&recrc(b)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "shard footer: unknown subject kind",
            "kind {kind}"
        );
    }
}

#[test]
fn a_stripe_subject_with_a_bad_code_with_a_valid_crc_is_refused() {
    let mut b = stripe_footer(0, 10).encode().unwrap();
    b[4] = 9;
    let err = ShardFooter::decode(&recrc(b)).unwrap_err();
    assert_eq!(err.to_string(), "shard footer: unknown code");
    let mut b = stripe_footer(0, 10).encode().unwrap();
    b[5] = 0;
    assert!(matches!(
        ShardFooter::decode(&recrc(b)),
        Err(FormatError::Invalid { what: WHAT, .. })
    ));
}

#[test]
fn a_stripe_subject_whose_descriptor_fails_the_shape_check_is_refused() {
    // Header: version 2, kind 1, index 1, code 3, stripe 8, revision 4, expiry 1, coordinator 1,
    // blocks 2 + 2 * 16, shards count 2, then 18 bytes per shard.
    let mut b = stripe_footer(0, 10).encode().unwrap();
    let first = 2 + 1 + 1 + 3 + 8 + 4 + 1 + 1 + 2 + 32 + 2;
    assert_eq!(b[first], 0);
    assert_eq!(b[first + 18], 1);
    b[first + 18] = 0;
    let err = ShardFooter::decode(&recrc(b)).unwrap_err();
    // The shape check is the descriptor's own, and names the descriptor.
    assert!(
        matches!(
            err,
            FormatError::Invalid {
                what: "stripe descriptor",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn a_cell_table_that_does_not_cover_the_data_region_is_refused() {
    // A copy with no copies: data length at 22, cell count at 30, cells from 38.
    let good = copy_footer(vec![], 2 * CELL_BYTES).encode().unwrap();
    assert_eq!(&good[22..30], &(2 * CELL_BYTES).to_le_bytes());
    assert_eq!(&good[30..38], &2u64.to_le_bytes());
    for cells in [0u64, 1, 3] {
        let mut b = good.clone();
        b[30..38].copy_from_slice(&cells.to_le_bytes());
        let err = ShardFooter::decode(&recrc(b)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "shard footer: cell table does not cover the data region",
            "{cells}"
        );
    }
    let mut b = good.clone();
    b[22..30].copy_from_slice(&(2 * CELL_BYTES + 1).to_le_bytes());
    assert!(ShardFooter::decode(&recrc(b)).is_err());
    let mut b = good.clone();
    b[22..30].copy_from_slice(&(2 * CELL_BYTES).to_le_bytes());
    assert!(ShardFooter::decode(&recrc(b)).is_ok());
}

#[test]
fn a_cell_table_longer_than_the_footer_is_short() {
    let mut b = copy_footer(vec![], 0).encode().unwrap();
    b[22..30].copy_from_slice(&u64::MAX.to_le_bytes());
    b[30..38].copy_from_slice(&(1u64 << 44).to_le_bytes());
    let err = ShardFooter::decode(&recrc(b)).unwrap_err();
    assert!(
        matches!(err, FormatError::Short { what: WHAT, .. }),
        "{err}"
    );
}

#[test]
fn a_shard_count_that_runs_past_the_footer_is_short() {
    let mut b = copy_footer(vec![], 0).encode().unwrap();
    // The copy count is at 20.
    b[20..22].copy_from_slice(&65535u16.to_le_bytes());
    let err = ShardFooter::decode(&recrc(b)).unwrap_err();
    assert!(
        matches!(err, FormatError::Short { what: WHAT, .. }),
        "{err}"
    );
}

#[test]
fn encode_refuses_a_cell_table_that_does_not_cover_the_data() {
    for (data_len, cells) in [(0u64, 1usize), (1, 0), (CELL_BYTES, 2), (CELL_BYTES + 1, 1)] {
        let mut f = stripe_footer(0, 10);
        f.data_len = data_len;
        f.cell_crcs = vec![0; cells];
        let err = f.encode().unwrap_err();
        assert_eq!(
            err.to_string(),
            "shard footer: cell table does not cover the data region"
        );
    }
    let mut ok = stripe_footer(0, 10);
    ok.data_len = CELL_BYTES;
    ok.cell_crcs = vec![0];
    assert!(ok.encode().is_ok());
}

#[test]
fn encode_refuses_a_stripe_that_fails_the_shape_check() {
    let mut f = stripe_footer(0, 10);
    if let FooterSubject::Stripe { stripe, .. } = &mut f.subject {
        stripe.shards = vec![placed(1), placed(1)];
    }
    assert!(matches!(f.encode(), Err(FormatError::Invalid { .. })));
}

#[test]
fn a_copy_footer_with_more_than_255_copies_or_unsorted_indexes_is_refused_on_encode() {
    let reason = "shard footer: copies are more than 255 or their indexes are not ascending";
    let all: Vec<PlacedShard> = (0..=255u8).map(placed_any).collect();
    assert_eq!(all.len(), 256);
    let err = copy_footer(all.clone(), 0).encode().unwrap_err();
    assert_eq!(err.to_string(), reason);
    let ok = copy_footer(all[..255].to_vec(), 0).encode().unwrap();
    assert_eq!(ShardFooter::decode(&ok).unwrap().descriptor(), None);
    for copies in [vec![placed(1), placed(0)], vec![placed(2), placed(2)]] {
        let err = copy_footer(copies, 0).encode().unwrap_err();
        assert_eq!(err.to_string(), reason);
    }
}

#[test]
fn a_copy_footer_whose_stored_indexes_are_not_ascending_is_refused_on_decode() {
    let good = copy_footer(vec![placed(0), placed(1)], 0).encode().unwrap();
    // The copy count is at 20; entries of 18 bytes follow, each starting with its index.
    assert_eq!(&good[20..22], &2u16.to_le_bytes());
    assert_eq!(good[22], 0);
    assert_eq!(good[40], 1);
    {
        let mut b = good.clone();
        b[40] = 0;
        let err = ShardFooter::decode(&recrc(b)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "shard footer: copies are more than 255 or their indexes are not ascending"
        );
    }
}

#[allow(
    clippy::unwrap_used,
    reason = "a test helper: a failed unwrap is the failure it reports"
)]
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A footer's bytes must be: the body, zeros up to the tail, then footer length `u64`, magic and
/// crc32c.
#[allow(
    clippy::unwrap_used,
    reason = "a test helper: a failed unwrap is the failure it reports"
)]
fn assert_golden(footer: &ShardFooter, body_hex: &str, crc_hex: &str) {
    let bytes = footer.encode().unwrap();
    let body = unhex(body_hex);
    assert_eq!(bytes.len(), 4096);
    assert_eq!(&bytes[..body.len()], &body[..]);
    assert!(bytes[body.len()..4096 - 16].iter().all(|b| *b == 0));
    assert_eq!(&bytes[4096 - 16..4096 - 8], &4096u64.to_le_bytes());
    assert_eq!(&bytes[4096 - 8..4096 - 4], b"TSSF");
    assert_eq!(&bytes[4096 - 4..], &unhex(crc_hex)[..]);
    assert_eq!(&ShardFooter::decode(&bytes).unwrap(), footer);
}

#[test]
fn a_stripe_shard_footer_has_exactly_these_bytes() {
    // version 1 | kind 1 | shard index 2 | code Ec k 2 m 1 | stripe id | revision 3 | expiry 4 |
    // coordinator 5 | one block (id 0x11, length 100) | two shards (index, node, drive) |
    // data length 5 | one cell | cell crc | blake3
    let footer = ShardFooter {
        subject: FooterSubject::Stripe {
            stripe: StripeDescriptor {
                stripe: StripeId::from_raw(0x0102_0304_0506_0708),
                revision: 3,
                code: Code::Ec { k: 2, m: 1 },
                blocks: vec![(BlockId::from_raw(0x11), 100)],
                shards: vec![
                    PlacedShard {
                        index: 0,
                        loc: ShardLoc {
                            node: NodeId(1),
                            drive: DriveId([0xa0; 16]),
                        },
                    },
                    PlacedShard {
                        index: 2,
                        loc: ShardLoc {
                            node: NodeId(3),
                            drive: DriveId([0xb0; 16]),
                        },
                    },
                ],
                expiry: ExpiryClass(4),
                coordinator: NodeId(5),
            },
            index: 2,
        },
        data_len: 5,
        cell_crcs: vec![0x0102_0304],
        blake3: [0x55; 32],
    };
    assert_golden(
        &footer,
        "01000102010201080706050403020103000000040501001100000000000000640000000000000002000001a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a00203b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b005000000000000000100000000000000040302015555555555555555555555555555555555555555555555555555555555555555",
        "5e5e30aa",
    );
}

#[test]
fn a_copy_footer_has_exactly_these_bytes() {
    // version 1 | kind 2 | block id | length 5 | expiry 9 | two copies (index, node, drive) |
    // data length 5 | one cell | cell crc | blake3
    let footer = ShardFooter {
        subject: FooterSubject::Copy {
            block: BlockId::from_raw(0x0900_0000_0000_0001),
            length: 5,
            expiry: ExpiryClass(9),
            copies: vec![
                PlacedShard {
                    index: 0,
                    loc: ShardLoc {
                        node: NodeId(1),
                        drive: DriveId([0xa0; 16]),
                    },
                },
                PlacedShard {
                    index: 1,
                    loc: ShardLoc {
                        node: NodeId(2),
                        drive: DriveId([0xb0; 16]),
                    },
                },
            ],
        },
        data_len: 5,
        cell_crcs: vec![0x0102_0304],
        blake3: [0x66; 32],
    };
    assert_golden(
        &footer,
        "010002010000000000000905000000000000000902000001a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a00102b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b005000000000000000100000000000000040302016666666666666666666666666666666666666666666666666666666666666666",
        "bcbc7893",
    );
}
