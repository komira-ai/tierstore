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
//! - a key or a name longer than its length field that is accepted and truncated;
//! - an entry whose origin or sequence is not carried by its frame (every kind, the unknown kind
//!   included), whose frame bytes outside the body are not covered by the crc, or whose sequence 0 is
//!   accepted;
//! - a log file read with a record of another origin, a gap, a repeat or a wrong first sequence.

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
    BlockDrop, LargeFile, LogEntry, LogRecord, Named, TOMB_FRAME_LEN, TOMB_MAGIC, TombDrop,
    Tombstone, scan_log, scan_log_after,
};

const WHAT: &str = "tomb log entry";

/// A frame of origin 1 and sequence 6, byte by byte.
fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    frame_at(kind, 1, 6, body)
}

fn frame_at(kind: u8, origin: u8, seq: u64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&TOMB_MAGIC.to_le_bytes());
    out.push(kind);
    out.push(origin);
    out.extend_from_slice(&[0; 2]);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    let mut covered = out.clone();
    covered.extend_from_slice(body);
    out.extend_from_slice(&crc32c(&covered).to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn rec(origin: u8, seq: u64, entry: LogEntry) -> LogRecord {
    LogRecord {
        origin: NodeId(origin),
        seq,
        entry,
    }
}

fn drop_entry(target_origin: u8, target_seq: u64) -> LogEntry {
    LogEntry::TombDrop(TombDrop {
        target_origin: NodeId(target_origin),
        target_seq,
    })
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
    b.extend_from_slice(&count.to_le_bytes());
    b.extend_from_slice(names);
    b
}

fn round_trip(entry: &LogEntry) -> Vec<u8> {
    let record = rec(2, 99, entry.clone());
    let bytes = record.encode().unwrap();
    let (back, used) = LogRecord::decode(&bytes).unwrap();
    assert_eq!(back, record);
    assert_eq!(used as usize, bytes.len());
    assert_eq!(back.encode().unwrap(), bytes);
    bytes
}

fn decode_entry(wire: &[u8]) -> Result<(LogEntry, u64), FormatError> {
    LogRecord::decode(wire).map(|(r, used)| (r.entry, used))
}

fn encode_entry(entry: LogEntry) -> Result<Vec<u8>, FormatError> {
    rec(1, 6, entry).encode()
}

#[test]
fn a_tombstone_round_trips_with_every_kind_of_name() {
    let bytes = round_trip(&LogEntry::Tombstone(tombstone()));
    assert_eq!(&bytes[..4], b"TSTL");
    assert_eq!(bytes[4], 1);
    assert_eq!(bytes[5], 2);
    assert_eq!(&bytes[6..8], &[0, 0]);
    assert_eq!(&bytes[8..16], &99u64.to_le_bytes());
    let body = &bytes[TOMB_FRAME_LEN..];
    assert_eq!(&bytes[16..20], &(body.len() as u32).to_le_bytes());
    let crc = crc32c(&[&bytes[..20], body].concat());
    assert_eq!(&bytes[20..24], &crc.to_le_bytes());
    assert_eq!(TOMB_FRAME_LEN, 24);
}

#[test]
fn a_tombstone_with_no_key_and_no_names_round_trips() {
    let t = Tombstone {
        key: vec![],
        generation: Generation([0; 16]),
        stamp: Stamp::ZERO,
        names: vec![],
    };
    let bytes = round_trip(&LogEntry::Tombstone(t));
    assert_eq!(bytes.len(), TOMB_FRAME_LEN + 2 + 16 + 8 + 4);
}

#[test]
fn a_block_drop_round_trips_with_its_documented_body() {
    let entry = LogEntry::BlockDrop(BlockDrop {
        block: BlockId::from_raw(0x0102_0304_0506_0708),
        stamp: Stamp::from_raw(0x2222),
    });
    let bytes = round_trip(&entry);
    assert_eq!(bytes[4], 2);
    let body = &bytes[TOMB_FRAME_LEN..];
    assert_eq!(body.len(), 8 + 8);
    assert_eq!(&body[..8], &0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(&body[8..], &0x2222u64.to_le_bytes());
}

#[test]
fn a_tomb_drop_round_trips_with_its_documented_body() {
    let bytes = round_trip(&drop_entry(5, 600));
    assert_eq!(bytes[4], 3);
    let body = &bytes[TOMB_FRAME_LEN..];
    assert_eq!(body.len(), 1 + 8);
    assert_eq!(body[0], 5);
    assert_eq!(&body[1..], &600u64.to_le_bytes());
}

#[test]
fn every_kind_carries_the_origin_and_sequence_of_its_frame() {
    let entries = [
        LogEntry::Tombstone(tombstone()),
        LogEntry::BlockDrop(BlockDrop {
            block: BlockId::from_raw(1),
            stamp: Stamp::ZERO,
        }),
        drop_entry(3, 4),
        LogEntry::Unknown {
            kind: 77,
            body: vec![1, 2],
        },
    ];
    for entry in entries {
        let wire = rec(200, 0x0A0B_0C0D_0E0F_1011, entry.clone())
            .encode()
            .unwrap();
        assert_eq!(wire[5], 200);
        assert_eq!(&wire[8..16], &0x0A0B_0C0D_0E0F_1011u64.to_le_bytes());
        let (back, _) = LogRecord::decode(&wire).unwrap();
        assert_eq!(back.origin, NodeId(200));
        assert_eq!(back.seq, 0x0A0B_0C0D_0E0F_1011);
        assert_eq!(back.entry, entry);
    }
}

#[test]
fn sequence_zero_is_refused_on_encode_and_decode() {
    let err = rec(1, 0, drop_entry(1, 1)).encode().unwrap_err();
    assert_eq!(
        err.to_string(),
        "tomb log entry: sequence 0 is not a position"
    );
    let wire = frame_at(3, 1, 0, &[0; 9]);
    assert_eq!(LogRecord::decode(&wire).unwrap_err(), err);
}

#[test]
fn an_entry_of_an_unknown_kind_is_kept_with_its_body_and_re_encodes_byte_identically() {
    for kind in [0u8, 4, 5, 200, 255] {
        let wire = frame(kind, b"a body this release cannot read");
        let (record, used) = LogRecord::decode(&wire).unwrap();
        assert_eq!(used as usize, wire.len());
        assert_eq!(
            record,
            rec(
                1,
                6,
                LogEntry::Unknown {
                    kind,
                    body: b"a body this release cannot read".to_vec()
                }
            )
        );
        assert_eq!(record.encode().unwrap(), wire, "kind {kind}");
    }
    let (empty, used) = decode_entry(&frame(9, b"")).unwrap();
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
    let mut log = frame_at(42, 1, 5, b"future");
    log.extend_from_slice(&rec(1, 6, drop_entry(2, 3)).encode().unwrap());
    let (first, used) = LogRecord::decode(&log).unwrap();
    assert_eq!(first.seq, 5);
    assert!(matches!(first.entry, LogEntry::Unknown { kind: 42, .. }));
    let (second, _) = LogRecord::decode(&log[used as usize..]).unwrap();
    assert_eq!(second, rec(1, 6, drop_entry(2, 3)));
}

#[test]
fn a_name_of_an_unknown_kind_is_kept_and_re_encodes_byte_identically() {
    let mut names = vec![50u8];
    names.extend_from_slice(&3u16.to_le_bytes());
    names.extend_from_slice(&[9, 8, 7]);
    let wire = frame(1, &tombstone_body(b"k", 1, &names));
    let (record, _) = LogRecord::decode(&wire).unwrap();
    match &record.entry {
        LogEntry::Tombstone(t) => {
            assert_eq!(
                t.names,
                vec![Named::Unknown {
                    kind: 50,
                    bytes: vec![9, 8, 7]
                }]
            );
            assert_eq!(t.key, b"k");
            assert_eq!(t.stamp, Stamp::from_raw(5));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!((record.origin, record.seq), (NodeId(1), 6));
    assert_eq!(record.encode().unwrap(), wire);
}

#[test]
fn a_known_name_longer_than_its_fields_reads_the_fields_and_ignores_the_rest() {
    // A later release may append to a name; the 8 bytes of a block name still read.
    let mut names = vec![1u8];
    names.extend_from_slice(&12u16.to_le_bytes());
    names.extend_from_slice(&7u64.to_le_bytes());
    names.extend_from_slice(&[0xee; 4]);
    let wire = frame(1, &tombstone_body(b"", 1, &names));
    let (entry, _) = decode_entry(&wire).unwrap();
    match entry {
        LogEntry::Tombstone(t) => assert_eq!(t.names, vec![Named::Block(BlockId::from_raw(7))]),
        other => panic!("{other:?}"),
    }
}

fn is_short(r: Result<(LogEntry, u64), FormatError>) -> bool {
    matches!(r, Err(FormatError::Short { what: WHAT, .. }))
}

#[test]
fn a_known_name_with_too_few_bytes_is_short() {
    let mut block = vec![1u8];
    block.extend_from_slice(&4u16.to_le_bytes());
    block.extend_from_slice(&[0; 4]);
    assert!(is_short(decode_entry(&frame(
        1,
        &tombstone_body(b"", 1, &block)
    ))));
    let mut file = vec![2u8];
    file.extend_from_slice(&39u16.to_le_bytes());
    file.extend_from_slice(&[0; 39]);
    assert!(is_short(decode_entry(&frame(
        1,
        &tombstone_body(b"", 1, &file)
    ))));
}

#[test]
fn a_name_count_larger_than_the_names_present_is_short() {
    let mut one = vec![1u8];
    one.extend_from_slice(&8u16.to_le_bytes());
    one.extend_from_slice(&[0; 8]);
    assert!(is_short(decode_entry(&frame(
        1,
        &tombstone_body(b"", 2, &one)
    ))));
    // A name whose length runs past the body.
    let mut long = vec![9u8];
    long.extend_from_slice(&100u16.to_le_bytes());
    long.extend_from_slice(&[0; 3]);
    assert!(is_short(decode_entry(&frame(
        1,
        &tombstone_body(b"", 1, &long)
    ))));
    // A count of 2^32 - 1 over an empty list ends at the first missing name.
    assert!(is_short(decode_entry(&frame(
        1,
        &tombstone_body(b"", u32::MAX, &[])
    ))));
}

#[test]
fn a_tombstone_body_that_ends_inside_a_field_is_short() {
    let full = tombstone_body(b"key", 0, &[]);
    for cut in [0usize, 1, 2, 4, 10, 25, 30, full.len() - 1] {
        assert!(is_short(decode_entry(&frame(1, &full[..cut]))), "cut {cut}");
    }
    assert!(decode_entry(&frame(1, &full)).is_ok());
    // A key length that runs past the body.
    let mut bad = full.clone();
    bad[..2].copy_from_slice(&1000u16.to_le_bytes());
    assert!(is_short(decode_entry(&frame(1, &bad))));
}

#[test]
fn a_block_drop_or_tomb_drop_body_that_is_too_short_is_short() {
    for len in [0usize, 8, 15] {
        assert!(
            is_short(decode_entry(&frame(2, &vec![0u8; len]))),
            "block drop {len}"
        );
    }
    assert!(decode_entry(&frame(2, &[0u8; 16])).is_ok());
    assert!(decode_entry(&frame(2, &[0u8; 17])).is_ok());
    for len in [0usize, 1, 8] {
        assert!(
            is_short(decode_entry(&frame(3, &vec![0u8; len]))),
            "tomb drop {len}"
        );
    }
    assert!(decode_entry(&frame(3, &[0u8; 9])).is_ok());
}

#[test]
fn a_bad_magic_is_refused() {
    let mut wire = frame(3, &[0; 9]);
    wire[0] ^= 1;
    let err = LogRecord::decode(&wire).unwrap_err();
    assert!(matches!(err, FormatError::BadMagic { what: WHAT, .. }));
}

#[test]
fn a_flipped_byte_of_the_frame_or_body_is_a_bad_crc() {
    let good = rec(2, 99, LogEntry::Tombstone(tombstone()))
        .encode()
        .unwrap();
    // The kind, the origin, a reserved byte, the sequence, the length's high byte is not flipped
    // (it would make the body run past the buffer), the body and the crc itself.
    for i in [
        4,
        5,
        6,
        7,
        8,
        15,
        20,
        23,
        TOMB_FRAME_LEN,
        TOMB_FRAME_LEN + 7,
        good.len() - 1,
    ] {
        let mut bad = good.clone();
        bad[i] ^= 0x40;
        let err = LogRecord::decode(&bad).unwrap_err();
        assert!(
            matches!(err, FormatError::BadCrc { what: WHAT, .. }),
            "byte {i}: {err}"
        );
    }
}

#[test]
fn a_short_header_or_a_cut_body_is_short_with_the_bytes_needed() {
    let good = rec(
        1,
        1,
        LogEntry::BlockDrop(BlockDrop {
            block: BlockId::from_raw(1),
            stamp: Stamp::ZERO,
        }),
    )
    .encode()
    .unwrap();
    assert_eq!(good.len(), 24 + 16);
    for n in [0usize, 3, 4, 5, 6, 8, 16, 20, 23] {
        let err = LogRecord::decode(&good[..n]).unwrap_err();
        assert!(
            matches!(err, FormatError::Short { what: WHAT, .. }),
            "n {n}: {err}"
        );
    }
    let err = LogRecord::decode(&good[..good.len() - 1]).unwrap_err();
    assert_eq!(
        err,
        FormatError::Short {
            what: WHAT,
            need: 40,
            have: 39
        }
    );
}

#[test]
fn a_key_of_65535_bytes_is_kept_and_one_of_65536_is_refused() {
    let mut t = tombstone();
    t.key = vec![b'k'; 65535];
    round_trip(&LogEntry::Tombstone(t.clone()));
    t.key = vec![b'k'; 65536];
    let err = encode_entry(LogEntry::Tombstone(t)).unwrap_err();
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
    let err = encode_entry(LogEntry::Tombstone(t)).unwrap_err();
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
        let err = encode_entry(LogEntry::Unknown {
            kind,
            body: vec![0; 4],
        })
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
        let err = encode_entry(LogEntry::Tombstone(t)).unwrap_err();
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

fn log_of(records: &[LogRecord]) -> Vec<u8> {
    records.iter().flat_map(|r| r.encode().unwrap()).collect()
}

#[test]
fn a_log_file_scans_to_its_records_in_order_from_any_first_sequence() {
    let records = vec![
        rec(4, 17, LogEntry::Tombstone(tombstone())),
        rec(4, 18, drop_entry(2, 9)),
        rec(
            4,
            19,
            LogEntry::Unknown {
                kind: 50,
                body: vec![1],
            },
        ),
    ];
    let buf = log_of(&records);
    assert_eq!(scan_log(&buf, NodeId(4)).unwrap(), records);
    assert_eq!(scan_log_after(&buf, NodeId(4), 16).unwrap(), records);
    assert_eq!(scan_log(&[], NodeId(4)).unwrap(), vec![]);
    assert_eq!(scan_log_after(&[], NodeId(4), 16).unwrap(), vec![]);
}

#[test]
fn a_log_file_with_a_record_of_another_origin_a_gap_or_a_repeat_is_refused() {
    let gap = "tomb log entry: a sequence that is not the previous plus one";
    let other = "tomb log entry: a record of another origin";
    let ok = rec(4, 1, drop_entry(2, 9));
    let cases = [
        (vec![ok.clone(), rec(5, 2, drop_entry(2, 9))], other),
        (vec![rec(5, 1, drop_entry(2, 9))], other),
        (vec![ok.clone(), rec(4, 3, drop_entry(2, 9))], gap),
        (vec![ok.clone(), rec(4, 1, drop_entry(2, 9))], gap),
        (vec![ok.clone(), rec(4, 0xff, drop_entry(2, 9))], gap),
        (
            vec![
                rec(4, u64::MAX, drop_entry(2, 9)),
                rec(4, 1, drop_entry(2, 9)),
            ],
            gap,
        ),
    ];
    for (records, message) in cases {
        let err = scan_log(&log_of(&records), NodeId(4)).unwrap_err();
        assert_eq!(err.to_string(), message);
    }
}

#[test]
fn a_run_that_continues_a_log_must_start_at_the_cursor_plus_one() {
    let gap = "tomb log entry: a sequence that is not the previous plus one";
    let buf = log_of(&[rec(4, 6, drop_entry(2, 9)), rec(4, 7, drop_entry(2, 9))]);
    assert_eq!(scan_log_after(&buf, NodeId(4), 5).unwrap().len(), 2);
    for after in [0u64, 4, 6, 7, u64::MAX] {
        let err = scan_log_after(&buf, NodeId(4), after).unwrap_err();
        assert_eq!(err.to_string(), gap, "after {after}");
    }
}

#[test]
fn a_log_file_with_a_damaged_or_cut_record_is_refused() {
    let mut buf = log_of(&[rec(4, 1, drop_entry(2, 9)), rec(4, 2, drop_entry(2, 9))]);
    let cut = &buf[..buf.len() - 1];
    assert!(matches!(
        scan_log(cut, NodeId(4)),
        Err(FormatError::Short { what: WHAT, .. })
    ));
    let last = buf.len() - 1;
    buf[last] ^= 1;
    assert!(matches!(
        scan_log(&buf, NodeId(4)),
        Err(FormatError::BadCrc { what: WHAT, .. })
    ));
}
