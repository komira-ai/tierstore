//! The tombstone log: the framing of an entry, every kind it carries, and how a kind this release
//! does not know is kept and forwarded.
//!
//! Catches:
//! - an entry of any kind that does not round trip every field, or whose frame header differs from
//!   the documented layout, or whose consumed length is wrong so the next entry is misread;
//! - an entry of an unknown kind, or a tombstone name of an unknown kind, that is refused, loses its
//!   bytes, or re-encodes to different bytes (an older node must forward a newer node's log);
//! - a bad magic, a flipped body or crc byte, a short header or a cut body that decodes as an entry;
//! - a body whose fields end early (a name with too few bytes, a count larger than the names present)
//!   that decodes instead of failing as `Short`;
//! - a key or a name longer than its length field that is accepted and truncated.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use tierstore_format::FormatError;
use tierstore_format::crc32c;
use tierstore_format::ids::{BlockId, Generation, KeyHash, NodeId};
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{
    BlockDrop, LargeFile, LogEntry, Named, TOMB_FRAME_LEN, TOMB_MAGIC, TombDrop, Tombstone,
};

const WHAT: &str = "tomb log entry";

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&TOMB_MAGIC.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32c(body).to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn large() -> LargeFile {
    LargeFile {
        key_hash: KeyHash([3; 16]),
        generation: Generation([4; 16]),
        size: 1 << 33,
    }
}

fn tombstone() -> Tombstone {
    Tombstone {
        key: b"bucket/object".to_vec(),
        generation: Generation([7; 16]),
        stamp: Stamp::from_raw(0x0102_0304_0506_0708),
        origin: NodeId(2),
        origin_seq: 99,
        names: vec![
            Named::Block(BlockId::from_raw(0xaa00_0000_0000_0001)),
            Named::LargeFile(large()),
            Named::Unknown {
                kind: 77,
                bytes: vec![1, 2, 3, 4, 5],
            },
            Named::Unknown {
                kind: 78,
                bytes: vec![],
            },
        ],
    }
}

/// The body of a tombstone with the given key and names, field by field.
fn tombstone_body(key: &[u8], count: u32, names: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(key.len() as u16).to_le_bytes());
    b.extend_from_slice(key);
    b.extend_from_slice(&[0; 16]);
    b.extend_from_slice(&5u64.to_le_bytes());
    b.push(1);
    b.extend_from_slice(&6u64.to_le_bytes());
    b.extend_from_slice(&count.to_le_bytes());
    b.extend_from_slice(names);
    b
}

fn round_trip(entry: &LogEntry) -> Vec<u8> {
    let bytes = entry.encode().unwrap();
    let (back, used) = LogEntry::decode(&bytes).unwrap();
    assert_eq!(&back, entry);
    assert_eq!(used as usize, bytes.len());
    assert_eq!(back.encode().unwrap(), bytes);
    bytes
}

#[test]
fn a_tombstone_round_trips_with_every_kind_of_name() {
    let bytes = round_trip(&LogEntry::Tombstone(tombstone()));
    assert_eq!(&bytes[..4], b"TSTL");
    assert_eq!(bytes[4], 1);
    assert_eq!(&bytes[5..8], &[0, 0, 0]);
    let body = &bytes[TOMB_FRAME_LEN..];
    assert_eq!(&bytes[8..12], &(body.len() as u32).to_le_bytes());
    assert_eq!(&bytes[12..16], &crc32c(body).to_le_bytes());
    assert_eq!(TOMB_FRAME_LEN, 16);
}

#[test]
fn a_tombstone_with_no_key_and_no_names_round_trips() {
    let t = Tombstone {
        key: vec![],
        generation: Generation([0; 16]),
        stamp: Stamp::ZERO,
        origin: NodeId(0),
        origin_seq: 0,
        names: vec![],
    };
    let bytes = round_trip(&LogEntry::Tombstone(t));
    assert_eq!(bytes.len(), TOMB_FRAME_LEN + 2 + 16 + 8 + 1 + 8 + 4);
}

#[test]
fn a_block_drop_round_trips_with_its_documented_body() {
    let entry = LogEntry::BlockDrop(BlockDrop {
        block: BlockId::from_raw(0x0102_0304_0506_0708),
        origin: NodeId(9),
        origin_seq: 0x1111,
        stamp: Stamp::from_raw(0x2222),
    });
    let bytes = round_trip(&entry);
    assert_eq!(bytes[4], 2);
    let body = &bytes[TOMB_FRAME_LEN..];
    assert_eq!(body.len(), 8 + 1 + 8 + 8);
    assert_eq!(&body[..8], &0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(body[8], 9);
    assert_eq!(&body[9..17], &0x1111u64.to_le_bytes());
    assert_eq!(&body[17..], &0x2222u64.to_le_bytes());
}

#[test]
fn a_tomb_drop_round_trips_with_its_documented_body() {
    let entry = LogEntry::TombDrop(TombDrop {
        reporter: NodeId(4),
        origin: NodeId(5),
        origin_seq: 600,
    });
    let bytes = round_trip(&entry);
    assert_eq!(bytes[4], 3);
    let body = &bytes[TOMB_FRAME_LEN..];
    assert_eq!(body.len(), 1 + 1 + 8);
    assert_eq!(&body[..2], &[4, 5]);
    assert_eq!(&body[2..], &600u64.to_le_bytes());
}

#[test]
fn an_entry_of_an_unknown_kind_is_kept_with_its_body_and_re_encodes_byte_identically() {
    for kind in [0u8, 4, 5, 200, 255] {
        let wire = frame(kind, b"a body this release cannot read");
        let (entry, used) = LogEntry::decode(&wire).unwrap();
        assert_eq!(used as usize, wire.len());
        assert_eq!(
            entry,
            LogEntry::Unknown {
                kind,
                body: b"a body this release cannot read".to_vec()
            }
        );
        assert_eq!(entry.encode().unwrap(), wire, "kind {kind}");
    }
    let (empty, used) = LogEntry::decode(&frame(9, b"")).unwrap();
    assert_eq!(
        empty,
        LogEntry::Unknown {
            kind: 9,
            body: vec![]
        }
    );
    assert_eq!(used as usize, TOMB_FRAME_LEN);
}

#[test]
fn an_unknown_entry_does_not_hide_the_entries_after_it() {
    let mut log = frame(42, b"future");
    log.extend_from_slice(
        &LogEntry::TombDrop(TombDrop {
            reporter: NodeId(1),
            origin: NodeId(2),
            origin_seq: 3,
        })
        .encode()
        .unwrap(),
    );
    let (first, used) = LogEntry::decode(&log).unwrap();
    assert!(matches!(first, LogEntry::Unknown { kind: 42, .. }));
    let (second, _) = LogEntry::decode(&log[used as usize..]).unwrap();
    assert_eq!(
        second,
        LogEntry::TombDrop(TombDrop {
            reporter: NodeId(1),
            origin: NodeId(2),
            origin_seq: 3
        })
    );
}

#[test]
fn a_name_of_an_unknown_kind_is_kept_and_re_encodes_byte_identically() {
    let mut names = vec![50u8];
    names.extend_from_slice(&3u16.to_le_bytes());
    names.extend_from_slice(&[9, 8, 7]);
    let wire = frame(1, &tombstone_body(b"k", 1, &names));
    let (entry, _) = LogEntry::decode(&wire).unwrap();
    match &entry {
        LogEntry::Tombstone(t) => {
            assert_eq!(
                t.names,
                vec![Named::Unknown {
                    kind: 50,
                    bytes: vec![9, 8, 7]
                }]
            );
            assert_eq!(t.key, b"k");
            assert_eq!(t.origin, NodeId(1));
            assert_eq!(t.origin_seq, 6);
            assert_eq!(t.stamp, Stamp::from_raw(5));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(entry.encode().unwrap(), wire);
}

#[test]
fn a_known_name_longer_than_its_fields_reads_the_fields_and_ignores_the_rest() {
    // A later release may append to a name; the 8 bytes of a block name still read.
    let mut names = vec![1u8];
    names.extend_from_slice(&12u16.to_le_bytes());
    names.extend_from_slice(&7u64.to_le_bytes());
    names.extend_from_slice(&[0xee; 4]);
    let wire = frame(1, &tombstone_body(b"", 1, &names));
    let (entry, _) = LogEntry::decode(&wire).unwrap();
    match entry {
        LogEntry::Tombstone(t) => assert_eq!(t.names, vec![Named::Block(BlockId::from_raw(7))]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_known_name_with_too_few_bytes_is_short() {
    let mut block = vec![1u8];
    block.extend_from_slice(&4u16.to_le_bytes());
    block.extend_from_slice(&[0; 4]);
    let err = LogEntry::decode(&frame(1, &tombstone_body(b"", 1, &block))).unwrap_err();
    assert!(
        matches!(err, FormatError::Short { what: WHAT, .. }),
        "{err}"
    );
    let mut file = vec![2u8];
    file.extend_from_slice(&39u16.to_le_bytes());
    file.extend_from_slice(&[0; 39]);
    let err = LogEntry::decode(&frame(1, &tombstone_body(b"", 1, &file))).unwrap_err();
    assert!(
        matches!(err, FormatError::Short { what: WHAT, .. }),
        "{err}"
    );
}

#[test]
fn a_name_count_larger_than_the_names_present_is_short() {
    let mut one = vec![1u8];
    one.extend_from_slice(&8u16.to_le_bytes());
    one.extend_from_slice(&[0; 8]);
    let err = LogEntry::decode(&frame(1, &tombstone_body(b"", 2, &one))).unwrap_err();
    assert!(
        matches!(err, FormatError::Short { what: WHAT, .. }),
        "{err}"
    );
    // A name whose length runs past the body.
    let mut long = vec![9u8];
    long.extend_from_slice(&100u16.to_le_bytes());
    long.extend_from_slice(&[0; 3]);
    let err = LogEntry::decode(&frame(1, &tombstone_body(b"", 1, &long))).unwrap_err();
    assert!(
        matches!(err, FormatError::Short { what: WHAT, .. }),
        "{err}"
    );
    // A count of 2^32 - 1 over an empty list ends at the first missing name.
    let err = LogEntry::decode(&frame(1, &tombstone_body(b"", u32::MAX, &[]))).unwrap_err();
    assert!(
        matches!(err, FormatError::Short { what: WHAT, .. }),
        "{err}"
    );
}

#[test]
fn a_tombstone_body_that_ends_inside_a_field_is_short() {
    let full = tombstone_body(b"key", 0, &[]);
    for cut in [0usize, 1, 2, 4, 10, 25, 30, 38, full.len() - 1] {
        let err = LogEntry::decode(&frame(1, &full[..cut])).unwrap_err();
        assert!(
            matches!(err, FormatError::Short { what: WHAT, .. }),
            "cut {cut}: {err}"
        );
    }
    assert!(LogEntry::decode(&frame(1, &full)).is_ok());
    // A key length that runs past the body.
    let mut bad = full.clone();
    bad[..2].copy_from_slice(&1000u16.to_le_bytes());
    assert!(matches!(
        LogEntry::decode(&frame(1, &bad)),
        Err(FormatError::Short { .. })
    ));
}

#[test]
fn a_block_drop_or_tomb_drop_body_that_is_too_short_is_short() {
    for len in [0usize, 8, 16, 24] {
        let err = LogEntry::decode(&frame(2, &vec![0u8; len])).unwrap_err();
        assert!(
            matches!(err, FormatError::Short { what: WHAT, .. }),
            "block drop {len}: {err}"
        );
    }
    assert!(LogEntry::decode(&frame(2, &[0u8; 25])).is_ok());
    for len in [0usize, 1, 2, 9] {
        let err = LogEntry::decode(&frame(3, &vec![0u8; len])).unwrap_err();
        assert!(
            matches!(err, FormatError::Short { what: WHAT, .. }),
            "tomb drop {len}: {err}"
        );
    }
    assert!(LogEntry::decode(&frame(3, &[0u8; 10])).is_ok());
}

#[test]
fn a_bad_magic_is_refused() {
    let mut wire = frame(3, &[0; 10]);
    wire[0] ^= 1;
    let err = LogEntry::decode(&wire).unwrap_err();
    assert!(matches!(err, FormatError::BadMagic { what: WHAT, .. }));
}

#[test]
fn a_flipped_body_byte_or_crc_byte_is_a_bad_crc() {
    let good = LogEntry::Tombstone(tombstone()).encode().unwrap();
    for i in [TOMB_FRAME_LEN, TOMB_FRAME_LEN + 7, good.len() - 1, 12, 15] {
        let mut bad = good.clone();
        bad[i] ^= 0x40;
        let err = LogEntry::decode(&bad).unwrap_err();
        assert!(
            matches!(err, FormatError::BadCrc { what: WHAT, .. }),
            "byte {i}: {err}"
        );
    }
}

#[test]
fn a_short_header_or_a_cut_body_is_short_with_the_bytes_needed() {
    let good = LogEntry::BlockDrop(BlockDrop {
        block: BlockId::from_raw(1),
        origin: NodeId(1),
        origin_seq: 1,
        stamp: Stamp::ZERO,
    })
    .encode()
    .unwrap();
    assert_eq!(good.len(), 16 + 25);
    for n in [0usize, 3, 4, 5, 8, 12, 15] {
        let err = LogEntry::decode(&good[..n]).unwrap_err();
        assert!(
            matches!(err, FormatError::Short { what: WHAT, .. }),
            "n {n}: {err}"
        );
    }
    let err = LogEntry::decode(&good[..good.len() - 1]).unwrap_err();
    assert_eq!(
        err,
        FormatError::Short {
            what: WHAT,
            need: 41,
            have: 40
        }
    );
}

#[test]
fn a_key_of_65535_bytes_is_kept_and_one_of_65536_is_refused() {
    let mut t = tombstone();
    t.key = vec![b'k'; 65535];
    round_trip(&LogEntry::Tombstone(t.clone()));
    t.key = vec![b'k'; 65536];
    let err = LogEntry::Tombstone(t).encode().unwrap_err();
    assert_eq!(
        err.to_string(),
        "tomb log entry: key longer than 65535 bytes"
    );
}

#[test]
fn an_unknown_name_of_65535_bytes_is_kept_and_one_of_65536_is_refused() {
    let mut t = tombstone();
    t.names = vec![Named::Unknown {
        kind: 60,
        bytes: vec![1; 65535],
    }];
    round_trip(&LogEntry::Tombstone(t.clone()));
    t.names = vec![Named::Unknown {
        kind: 60,
        bytes: vec![1; 65536],
    }];
    let err = LogEntry::Tombstone(t).encode().unwrap_err();
    assert_eq!(
        err.to_string(),
        "tomb log entry: name longer than 65535 bytes"
    );
}

#[test]
fn names_order_by_kind_then_content() {
    let a = Named::Block(BlockId::from_raw(1));
    let b = Named::Block(BlockId::from_raw(2));
    let c = Named::LargeFile(large());
    assert!(a < b);
    assert!(b < c);
}

#[test]
fn an_unknown_entry_or_name_that_uses_a_known_kind_code_is_refused_on_encode() {
    for kind in [1u8, 2, 3] {
        let err = LogEntry::Unknown {
            kind,
            body: vec![0; 4],
        }
        .encode()
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "tomb log entry: an unknown kind uses a known code"
        );
    }
    for kind in [1u8, 2] {
        let mut t = tombstone();
        t.names = vec![Named::Unknown {
            kind,
            bytes: vec![0; 4],
        }];
        let err = LogEntry::Tombstone(t).encode().unwrap_err();
        assert_eq!(
            err.to_string(),
            "tomb log entry: an unknown kind uses a known code"
        );
    }
    // The first codes past the known ones are still accepted.
    round_trip(&LogEntry::Unknown {
        kind: 4,
        body: vec![9],
    });
    let mut t = tombstone();
    t.names = vec![Named::Unknown {
        kind: 3,
        bytes: vec![9],
    }];
    round_trip(&LogEntry::Tombstone(t));
}
