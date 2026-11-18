//! The journal and snapshot formats (section 8; this crate's `format` module), and the cell
//! sets and runs they carry.
//!
//! Catches: a field written at another offset or in another byte order (golden bytes); a torn
//! or flipped byte of a header, a record or a snapshot read as a value (every byte flipped is
//! refused); a record of an earlier epoch read under a new header; an unknown record kind or an
//! empty run read as a record; a later release's flag bits, reserved bytes or snapshot
//! extension refused (they are additive and ignored); a bitmap with a bit past the pack
//! accepted; runs split or merged wrongly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use tierstore_fetch_api::ids::{CellRun, PackId, PackKey, PackRoot};
use tierstore_format::ids::Generation;
use tierstore_journal::format::{
    FormatError, HEADER_LEN, JournalHeader, JournalImage, RECORD_LEN, Record, RecordKind,
    SNAPSHOT_FIXED_LEN, Slot, Snapshot, cell_count, decode_record,
};
use tierstore_journal::{BootId, CellSet, runs_of};

const BOOT: BootId = BootId([0x5a; 16]);

fn header(epoch: u64, boot: Option<BootId>) -> JournalHeader {
    JournalHeader { epoch, boot }
}

fn set(cells: u32, members: &[u32]) -> CellSet {
    let mut s = CellSet::new(cells);
    for &c in members {
        s.insert(c);
    }
    s
}

fn snapshot(root: PackRoot) -> Snapshot {
    Snapshot {
        epoch: 9,
        boot: Some(BOOT),
        pack: PackId {
            key: PackKey("k/ü".to_owned()),
            root,
        },
        size: 10 << 20,
        last_use: 123_456,
        present: set(10, &[0, 1, 9]),
        recheck: set(10, &[4]),
    }
}

#[test]
fn a_journal_header_has_the_documented_bytes_and_round_trips() {
    let b = header(0x0102_0304_0506_0708, Some(BOOT)).encode();
    assert_eq!(&b[0..8], b"TSJOURNL");
    assert_eq!(&b[8..10], &[1, 0]);
    assert_eq!(&b[10..12], &[1, 0], "boot known");
    assert_eq!(&b[12..16], &[0; 4]);
    assert_eq!(&b[16..24], &[8, 7, 6, 5, 4, 3, 2, 1]);
    assert_eq!(&b[24..40], &[0x5a; 16]);
    assert_eq!(&b[40..44], &[0; 4]);
    assert_eq!(&b[44..48], &crc32c::crc32c(&b[..44]).to_le_bytes());
    assert_eq!(
        JournalHeader::decode(&b),
        Ok(header(0x0102_0304_0506_0708, Some(BOOT)))
    );
    let none = header(3, None).encode();
    assert_eq!(&none[10..12], &[0, 0]);
    assert_eq!(&none[24..40], &[0; 16]);
    assert_eq!(JournalHeader::decode(&none), Ok(header(3, None)));
}

#[test]
fn every_flipped_byte_of_a_journal_header_is_refused_and_a_short_one_too() {
    let b = header(5, Some(BOOT)).encode();
    for i in 0..HEADER_LEN {
        let mut c = b;
        c[i] ^= 0x10;
        assert!(JournalHeader::decode(&c).is_err(), "byte {i}");
    }
    assert_eq!(
        JournalHeader::decode(&b[..47]),
        Err(FormatError::Short {
            what: "journal header",
            need: 48,
            got: 47
        })
    );
    let mut magic = b;
    magic[0] = b'X';
    assert_eq!(
        JournalHeader::decode(&magic),
        Err(FormatError::Magic {
            what: "journal header"
        })
    );
    let mut version = b;
    version[8] = 2;
    assert_eq!(
        JournalHeader::decode(&version),
        Err(FormatError::Version {
            what: "journal header",
            version: 2
        })
    );
}

#[test]
fn a_later_release_s_flag_bits_and_reserved_bytes_are_ignored() {
    let mut b = header(5, Some(BOOT)).encode();
    b[11] = 0x80; // a flag bit this release does not define
    b[12] = 7; // reserved
    b[41] = 7; // reserved
    let crc = crc32c::crc32c(&b[..44]);
    b[44..48].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(JournalHeader::decode(&b), Ok(header(5, Some(BOOT))));
}

#[test]
fn a_record_has_the_documented_bytes_and_its_crc_is_seeded_with_the_epoch() {
    let r = Record {
        kind: RecordKind::Punch,
        run: CellRun {
            first: 0x0a0b,
            count: 3,
        },
    };
    let b = r.encode(7);
    assert_eq!(b[0], 2);
    assert_eq!(&b[1..4], &[0; 3]);
    assert_eq!(&b[4..8], &[0x0b, 0x0a, 0, 0]);
    assert_eq!(&b[8..12], &[3, 0, 0, 0]);
    let crc = crc32c::crc32c_append(crc32c::crc32c(&7u64.to_le_bytes()), &b[..12]);
    assert_eq!(&b[12..16], &crc.to_le_bytes());
    assert_eq!(decode_record(7, &b), Slot::Record(r));
    assert_eq!(
        decode_record(8, &b),
        Slot::Invalid,
        "a record of another epoch"
    );
    assert_eq!(RecordKind::Add.byte(), 1);
    for i in 0..RECORD_LEN {
        let mut c = b;
        c[i] ^= 1;
        assert_eq!(decode_record(7, &c), Slot::Invalid, "byte {i}");
    }
}

fn with_crc(epoch: u64, mut b: [u8; RECORD_LEN]) -> [u8; RECORD_LEN] {
    let crc = crc32c::crc32c_append(crc32c::crc32c(&epoch.to_le_bytes()), &b[..12]);
    b[12..16].copy_from_slice(&crc.to_le_bytes());
    b
}

#[test]
fn an_unknown_kind_or_an_empty_run_is_not_a_record() {
    let mut b = Record {
        kind: RecordKind::Add,
        run: CellRun { first: 1, count: 1 },
    }
    .encode(0);
    b[0] = 3;
    assert_eq!(decode_record(0, &with_crc(0, b)), Slot::Unknown(3));
    let mut empty = Record {
        kind: RecordKind::Add,
        run: CellRun { first: 1, count: 1 },
    }
    .encode(0);
    empty[8] = 0;
    assert_eq!(decode_record(0, &with_crc(0, empty)), Slot::Unknown(1));
    // A reserved byte set by a later release does not change the record.
    let mut reserved = Record {
        kind: RecordKind::Add,
        run: CellRun { first: 1, count: 1 },
    }
    .encode(0);
    reserved[2] = 9;
    assert_eq!(
        decode_record(0, &with_crc(0, reserved)),
        Slot::Record(Record {
            kind: RecordKind::Add,
            run: CellRun { first: 1, count: 1 }
        })
    );
}

#[test]
fn a_journal_image_reads_records_up_to_a_torn_stale_or_unknown_slot() {
    let h = header(4, None).encode().to_vec();
    let add = Record {
        kind: RecordKind::Add,
        run: CellRun { first: 0, count: 2 },
    };
    let punch = Record {
        kind: RecordKind::Punch,
        run: CellRun { first: 1, count: 1 },
    };
    let good = [h.clone(), add.encode(4).to_vec(), punch.encode(4).to_vec()].concat();
    let img = JournalImage::decode(&good);
    assert_eq!(img.header, Ok(header(4, None)));
    assert_eq!(img.records, [add, punch]);
    assert_eq!((img.valid_len, img.tail_len, img.unknown), (80, 0, None));

    let torn = [good.clone(), vec![1, 2, 3]].concat();
    let img = JournalImage::decode(&torn);
    assert_eq!((img.records.len(), img.valid_len, img.tail_len), (2, 80, 3));

    let stale = [h.clone(), add.encode(3).to_vec(), punch.encode(4).to_vec()].concat();
    let img = JournalImage::decode(&stale);
    assert!(
        img.records.is_empty(),
        "a stale slot ends the replay, whatever follows"
    );
    assert_eq!((img.valid_len, img.tail_len), (48, 32));

    let mut unknown = add.encode(4);
    unknown[0] = 0xee;
    let with_unknown = [
        h.clone(),
        add.encode(4).to_vec(),
        with_crc(4, unknown).to_vec(),
    ]
    .concat();
    let img = JournalImage::decode(&with_unknown);
    assert_eq!(
        (img.records.len(), img.unknown, img.valid_len),
        (1, Some(0xee), 64)
    );

    for bytes in [Vec::new(), b"nope".to_vec()] {
        let img = JournalImage::decode(&bytes);
        assert!(img.header.is_err());
        assert!(img.records.is_empty());
        assert_eq!((img.valid_len, img.tail_len), (0, bytes.len() as u64));
    }
}

#[test]
fn a_snapshot_has_the_documented_bytes_and_round_trips_with_either_root() {
    let s = snapshot(PackRoot::Digest([3; 32]));
    let b = s.encode();
    assert_eq!(&b[0..8], b"TSSNAPSH");
    assert_eq!(&b[8..10], &[1, 0]);
    assert_eq!(&b[10..12], &[1, 0]);
    assert_eq!(&b[12..16], &[0; 4], "no extension");
    assert_eq!(&b[16..24], &9u64.to_le_bytes());
    assert_eq!(&b[24..40], &[0x5a; 16]);
    assert_eq!(&b[40..48], &(10u64 << 20).to_le_bytes());
    assert_eq!(&b[48..56], &123_456u64.to_le_bytes());
    assert_eq!(&b[56..60], &10u32.to_le_bytes());
    assert_eq!(b[60], 1);
    assert_eq!(&b[64..96], &[3; 32]);
    assert_eq!(&b[96..100], &4u32.to_le_bytes());
    assert_eq!(&b[100..104], "k/ü".as_bytes());
    assert_eq!(&b[104..106], &[0b0000_0011, 0b0000_0010], "present 0, 1, 9");
    assert_eq!(&b[106..108], &[0b0001_0000, 0], "recheck 4");
    assert_eq!(b.len(), 112);
    assert_eq!(&b[108..], &crc32c::crc32c(&b[..108]).to_le_bytes());
    assert_eq!(Snapshot::decode(&b), Ok(s));

    let etag = snapshot(PackRoot::Etag(Generation([0xcd; 16])));
    let b = etag.encode();
    assert_eq!(b[60], 2);
    assert_eq!(&b[64..80], &[0xcd; 16]);
    assert_eq!(&b[80..96], &[0; 16]);
    assert_eq!(Snapshot::decode(&b), Ok(etag));

    let mut unknown_boot = snapshot(PackRoot::Digest([3; 32]));
    unknown_boot.boot = None;
    assert_eq!(Snapshot::decode(&unknown_boot.encode()), Ok(unknown_boot));
}

/// Re-crcs a snapshot's bytes after a test changed them.
fn recrc(mut b: Vec<u8>) -> Vec<u8> {
    let n = b.len() - 4;
    let crc = crc32c::crc32c(&b[..n]);
    b[n..].copy_from_slice(&crc.to_le_bytes());
    b
}

#[test]
fn every_flipped_byte_of_a_snapshot_is_refused() {
    let b = snapshot(PackRoot::Digest([3; 32])).encode();
    for i in 0..b.len() {
        let mut c = b.clone();
        c[i] ^= 0x04;
        assert!(Snapshot::decode(&c).is_err(), "byte {i}");
    }
    assert_eq!(
        Snapshot::decode(&b[..103]),
        Err(FormatError::Short {
            what: "snapshot",
            need: 104,
            got: 103
        })
    );
}

#[test]
fn a_snapshot_whose_fields_disagree_is_refused_even_with_a_good_crc() {
    let b = snapshot(PackRoot::Digest([3; 32])).encode();
    let mut magic = b.clone();
    magic[1] = b'X';
    assert_eq!(
        Snapshot::decode(&recrc(magic)),
        Err(FormatError::Magic { what: "snapshot" })
    );
    let mut version = b.clone();
    version[9] = 1;
    assert_eq!(
        Snapshot::decode(&recrc(version)),
        Err(FormatError::Version {
            what: "snapshot",
            version: 0x0101
        })
    );
    let mut cells = b.clone();
    cells[56] = 11;
    assert_eq!(
        Snapshot::decode(&recrc(cells)),
        Err(FormatError::Layout("cell count is not the size's"))
    );
    let mut root = b.clone();
    root[60] = 3;
    assert_eq!(
        Snapshot::decode(&recrc(root)),
        Err(FormatError::RootKind(3))
    );
    let mut key_len = b.clone();
    key_len[96] = 200;
    assert_eq!(
        Snapshot::decode(&recrc(key_len)),
        Err(FormatError::Layout(
            "lengths do not add up to the snapshot's"
        ))
    );
    let mut huge = b.clone();
    huge[96..100].copy_from_slice(&u32::MAX.to_le_bytes());
    huge[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Snapshot::decode(&recrc(huge)).is_err());
    let mut utf8 = b.clone();
    utf8[101] = 0xff;
    assert_eq!(
        Snapshot::decode(&recrc(utf8)),
        Err(FormatError::Layout("key is not UTF-8"))
    );
    // Bit 10 of a 10-cell pack is past the pack.
    let mut present = b.clone();
    present[105] |= 0b100;
    assert_eq!(
        Snapshot::decode(&recrc(present)),
        Err(FormatError::Layout(
            "present bitmap sets a bit past the pack"
        ))
    );
    let mut recheck = b.clone();
    recheck[107] |= 0b1000_0000;
    assert_eq!(
        Snapshot::decode(&recrc(recheck)),
        Err(FormatError::Layout(
            "recheck bitmap sets a bit past the pack"
        ))
    );
}

#[test]
fn a_snapshot_with_a_later_release_s_extension_and_flag_bits_is_read() {
    let s = snapshot(PackRoot::Digest([3; 32]));
    let mut b = s.encode();
    b.truncate(b.len() - 4);
    b.extend_from_slice(b"future!");
    b[12..16].copy_from_slice(&7u32.to_le_bytes());
    b[11] |= 0x40;
    b[61] = 5;
    b.extend_from_slice(&[0; 4]);
    assert_eq!(Snapshot::decode(&recrc(b)), Ok(s));
}

#[test]
fn errors_name_the_format_and_the_problem() {
    let cases = [
        (
            FormatError::Short {
                what: "snapshot",
                need: 104,
                got: 3,
            },
            "snapshot: 3 bytes, at least 104 needed",
        ),
        (
            FormatError::Magic {
                what: "journal header",
            },
            "journal header: bad magic",
        ),
        (
            FormatError::Version {
                what: "snapshot",
                version: 9,
            },
            "snapshot: version 9, this release reads 1",
        ),
        (
            FormatError::Crc { what: "snapshot" },
            "snapshot: crc mismatch",
        ),
        (FormatError::Layout("x"), "snapshot: x"),
        (FormatError::RootKind(4), "snapshot: root kind 4"),
    ];
    for (e, text) in cases {
        assert_eq!(e.to_string(), text);
    }
    assert_eq!(SNAPSHOT_FIXED_LEN, 100);
}

#[test]
fn a_pack_has_one_cell_per_mib_rounded_up() {
    assert_eq!(cell_count(0), Some(0));
    assert_eq!(cell_count(1), Some(1));
    assert_eq!(cell_count(1 << 20), Some(1));
    assert_eq!(cell_count((1 << 20) + 1), Some(2));
    assert_eq!(cell_count(u64::from(u32::MAX) << 20), Some(u32::MAX));
    assert_eq!(cell_count((u64::from(u32::MAX) << 20) + 1), None);
}

#[test]
fn a_cell_set_holds_the_cells_of_its_pack_only() {
    let mut s = CellSet::new(10);
    assert!(s.is_empty());
    assert_eq!((s.cell_count(), s.bits().len()), (10, 2));
    for c in [0, 3, 9, 10, 400] {
        s.insert(c);
    }
    assert_eq!(s.iter().collect::<Vec<_>>(), [0, 3, 9]);
    assert_eq!(s.len(), 3);
    assert!(s.contains(9) && !s.contains(10) && !s.contains(1));
    s.remove(3);
    s.remove(10);
    assert_eq!(s.iter().collect::<Vec<_>>(), [0, 9]);
    assert!(!s.is_empty());
    assert_eq!(CellSet::from_bits(10, s.bits()), Some(s.clone()));
    assert_eq!(CellSet::from_bits(10, &[0]), None, "too short");
    assert_eq!(CellSet::from_bits(10, &[0, 0b100]), None, "bit 10");
    assert_eq!(
        CellSet::from_bits(16, &[0xff, 0xff]).map(|s| s.len()),
        Some(16)
    );
    assert_eq!(CellSet::from_bits(0, &[]), Some(CellSet::new(0)));
    assert_eq!(CellSet::bytes_for(17), 3);
}

#[test]
fn runs_are_the_maximal_sequences_of_consecutive_cells() {
    let run = |first, count| CellRun { first, count };
    assert_eq!(runs_of(&[]), []);
    assert_eq!(runs_of(&[4]), [run(4, 1)]);
    assert_eq!(
        runs_of(&[0, 1, 2, 5, 6, 9]),
        [run(0, 3), run(5, 2), run(9, 1)]
    );
    assert_eq!(
        runs_of(&[1, 1, 2, 2]),
        [run(1, 2)],
        "a repeat joins its run"
    );
    assert_eq!(runs_of(&[u32::MAX - 1, u32::MAX]), [run(u32::MAX - 1, 2)]);
    assert_eq!(
        runs_of(&[3, 1]),
        [run(3, 1), run(1, 1)],
        "descending cells are separate runs"
    );
}
