//! The block trailer: the sorted entries, the cell table and the 96-byte footer that locates them
//! from the end of a block.
//!
//! Catches:
//! - a footer whose fields are not at the documented offsets, or an entry that is not padded to 8;
//! - an encode that depends on the order the entries were given in (two copies of one block must have
//!   byte-identical trailers), or a decode that accepts entries out of order or twice;
//! - a cell table that is accepted when it does not hold one crc per 1 MiB of the data (checked at
//!   exactly 1 MiB, at one byte more and at none);
//! - a version, a cell shift, a magic, a crc or any single flipped byte of the trailer that is not an
//!   error, or a crc that does not cover the whole trailer;
//! - an entry count, an entry length or a trailer length that disagrees with the bytes and is not
//!   refused, including a length that overflows;
//! - a trailer that is not found when block bytes precede it, or a kind this release does not know
//!   that is refused instead of kept.

use tierstore_format::FormatError;
use tierstore_format::ids::Generation;
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_format::trailer::{
    BlockKind, TRAILER_FOOTER_LEN, TRAILER_MAGIC, TRAILER_VERSION, Trailer, TrailerEntry, cell_crcs,
};
use tierstore_format::{CELL_BYTES, cells_for, crc32c};

const WHAT: &str = "block trailer";

fn entry(key: &[u8], stamp: u64, offset: u64) -> TrailerEntry {
    TrailerEntry {
        key: key.to_vec(),
        stamp: Stamp::from_raw(stamp),
        offset,
        length: 100 + offset,
        generation: Generation([key.len() as u8; 16]),
        crc32c: 0x0a0b_0c0d,
        flags: RecordFlags::default(),
    }
}

fn trailer(entries: Vec<TrailerEntry>, data_len: u64) -> Trailer {
    Trailer {
        kind: BlockKind::Segment,
        entries,
        cell_crcs: (0..cells_for(data_len))
            .map(|i| 0x1000 + i as u32)
            .collect(),
        data_len,
        blake3: [0xb3; 32],
    }
}

fn sample() -> Trailer {
    trailer(
        vec![
            entry(b"alpha", 1, 0),
            entry(b"beta", 2, 128),
            entry(b"gamma", 3, 256),
        ],
        1000,
    )
}

/// The fields of a footer, so a test can forge a trailer with a valid crc and one wrong field.
#[derive(Clone)]
struct Footer {
    version: u16,
    kind: u8,
    shift: u8,
    count: u64,
    entries_len: u64,
    cells: u64,
    data_len: u64,
    reserved: [u8; 20],
}

impl Footer {
    fn good(count: u64, entries_len: u64, cells: u64, data_len: u64) -> Self {
        Self {
            version: 1,
            kind: 1,
            shift: 20,
            count,
            entries_len,
            cells,
            data_len,
            reserved: [0; 20],
        }
    }
}

/// The 48-byte entry header and key, padded to 8 bytes, as the trailer stores an entry.
fn entry_bytes(key: &[u8], stamp: u64, offset: u64) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(key.len() as u16).to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&offset.to_le_bytes());
    b.extend_from_slice(&10u64.to_le_bytes());
    b.extend_from_slice(&stamp.to_le_bytes());
    b.extend_from_slice(&[0; 16]);
    b.extend_from_slice(key);
    while b.len() % 8 != 0 {
        b.push(0);
    }
    b
}

fn forge(entries: &[u8], cells: &[u32], f: &Footer) -> Vec<u8> {
    let mut b = entries.to_vec();
    for c in cells {
        b.extend_from_slice(&c.to_le_bytes());
    }
    b.extend_from_slice(&TRAILER_MAGIC.to_le_bytes());
    b.extend_from_slice(&f.version.to_le_bytes());
    b.push(f.kind);
    b.push(f.shift);
    b.extend_from_slice(&f.count.to_le_bytes());
    b.extend_from_slice(&f.entries_len.to_le_bytes());
    b.extend_from_slice(&f.cells.to_le_bytes());
    b.extend_from_slice(&f.data_len.to_le_bytes());
    b.extend_from_slice(&[0xb3; 32]);
    b.extend_from_slice(&f.reserved);
    let crc = crc32c(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}

fn invalid(err: &FormatError, reason: &str) -> bool {
    matches!(err, FormatError::Invalid { what: WHAT, reason: r } if *r == reason)
}

#[test]
fn the_footer_is_96_bytes_with_the_documented_offsets() {
    assert_eq!(TRAILER_FOOTER_LEN, 96);
    assert_eq!(TRAILER_VERSION, 1);
    assert_eq!(TRAILER_MAGIC.to_le_bytes(), *b"TSBT");
    let t = sample();
    let bytes = t.encode().unwrap();
    let f = &bytes[bytes.len() - 96..];
    assert_eq!(&f[0..4], b"TSBT");
    assert_eq!(&f[4..6], &1u16.to_le_bytes());
    assert_eq!(f[6], 1);
    assert_eq!(f[7], 20);
    assert_eq!(&f[8..16], &3u64.to_le_bytes());
    // Three entries of 48 + key bytes padded to 8: 56, 56, 56.
    assert_eq!(&f[16..24], &168u64.to_le_bytes());
    assert_eq!(&f[24..32], &1u64.to_le_bytes());
    assert_eq!(&f[32..40], &1000u64.to_le_bytes());
    assert_eq!(&f[40..72], &[0xb3u8; 32]);
    assert_eq!(&f[72..92], &[0u8; 20]);
    assert_eq!(&f[92..96], &crc32c(&bytes[..bytes.len() - 4]).to_le_bytes());
    assert_eq!(bytes.len(), 168 + 4 + 96);
    assert_eq!(&bytes[168..172], &0x1000u32.to_le_bytes());
}

#[test]
fn an_entry_is_laid_out_and_padded_to_eight_bytes() {
    let t = trailer(vec![entry(b"k", 7, 64)], 10);
    let bytes = t.encode().unwrap();
    let mut expected = Vec::new();
    expected.extend_from_slice(&1u16.to_le_bytes());
    expected.extend_from_slice(&0u16.to_le_bytes());
    expected.extend_from_slice(&0x0a0b_0c0du32.to_le_bytes());
    expected.extend_from_slice(&64u64.to_le_bytes());
    expected.extend_from_slice(&164u64.to_le_bytes());
    expected.extend_from_slice(&7u64.to_le_bytes());
    expected.extend_from_slice(&[1u8; 16]);
    expected.extend_from_slice(b"k");
    expected.extend_from_slice(&[0u8; 7]);
    assert_eq!(&bytes[..56], &expected[..]);
    for (key_len, total) in [
        (0usize, 48usize),
        (1, 56),
        (8, 56),
        (9, 64),
        (16, 64),
        (17, 72),
    ] {
        let key = vec![b'x'; key_len];
        let bytes = trailer(vec![entry(&key, 0, 0)], 0).encode().unwrap();
        let at = bytes.len() - 96 + 16;
        let entries_len = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        assert_eq!(entries_len as usize, total, "key {key_len}");
        assert_eq!(Trailer::decode(&bytes).unwrap().entries[0].key, key);
    }
}

#[test]
fn a_trailer_round_trips_every_field() {
    let mut t = sample();
    t.entries[0].flags = RecordFlags(0x8003);
    t.entries[1].crc32c = u32::MAX;
    t.entries[2].length = u64::MAX;
    t.entries[2].generation = Generation([0xee; 16]);
    t.cell_crcs = vec![0xfeed_f00d];
    let back = Trailer::decode(&t.encode().unwrap()).unwrap();
    assert_eq!(back, t);
}

#[test]
fn every_block_kind_round_trips_including_one_this_release_does_not_know() {
    for (kind, code) in [
        (BlockKind::Segment, 1u8),
        (BlockKind::Pack, 2),
        (BlockKind::ChainSlice, 3),
        (BlockKind::Other(0), 0),
        (BlockKind::Other(4), 4),
        (BlockKind::Other(200), 200),
    ] {
        let mut t = sample();
        t.kind = kind;
        let bytes = t.encode().unwrap();
        assert_eq!(bytes[bytes.len() - 96 + 6], code);
        let back = Trailer::decode(&bytes).unwrap();
        assert_eq!(back.kind, kind);
        assert_eq!(back, t);
    }
}

#[test]
fn an_empty_trailer_is_the_footer_alone() {
    let t = trailer(vec![], 0);
    let bytes = t.encode().unwrap();
    assert_eq!(bytes.len(), 96);
    assert_eq!(Trailer::decode(&bytes).unwrap(), t);
}

#[test]
fn encode_sorts_so_two_orders_of_the_entries_give_the_same_bytes() {
    let a = entry(b"a", 5, 0);
    let b = entry(b"b", 1, 64);
    let c = entry(b"b", 2, 32);
    let d = entry(b"b", 2, 96);
    let sorted = trailer(vec![a.clone(), b.clone(), c.clone(), d.clone()], 500);
    let shuffled = trailer(vec![d.clone(), a.clone(), c.clone(), b.clone()], 500);
    let reversed = trailer(vec![d.clone(), c.clone(), b.clone(), a.clone()], 500);
    let bytes = sorted.encode().unwrap();
    assert_eq!(shuffled.encode().unwrap(), bytes);
    assert_eq!(reversed.encode().unwrap(), bytes);
    let back = Trailer::decode(&bytes).unwrap();
    assert_eq!(back.entries, vec![a, b, c, d]);
}

#[test]
fn entries_sort_by_key_bytes_then_stamp_then_offset() {
    let entries = vec![
        entry(b"b", 1, 0),
        entry(b"a", 9, 0),
        entry(b"ab", 0, 0),
        entry(b"a", 2, 500),
        entry(b"a", 2, 400),
    ];
    let back = Trailer::decode(&trailer(entries, 10).encode().unwrap()).unwrap();
    let order: Vec<(&[u8], u64, u64)> = back
        .entries
        .iter()
        .map(|e| (&e.key[..], e.stamp.raw(), e.offset))
        .collect();
    assert_eq!(
        order,
        vec![
            (&b"a"[..], 2, 400),
            (&b"a"[..], 2, 500),
            (&b"a"[..], 9, 0),
            (&b"ab"[..], 0, 0),
            (&b"b"[..], 1, 0)
        ]
    );
}

#[test]
fn a_decode_refuses_entries_out_of_order() {
    let mut area = entry_bytes(b"b", 1, 0);
    area.extend_from_slice(&entry_bytes(b"a", 1, 0));
    let bytes = forge(&area, &[], &Footer::good(2, area.len() as u64, 0, 0));
    let err = Trailer::decode(&bytes).unwrap_err();
    assert!(invalid(&err, "entries are not sorted"), "{err}");
    // The same key and stamp with a lower offset second.
    let mut area = entry_bytes(b"a", 1, 64);
    area.extend_from_slice(&entry_bytes(b"a", 1, 0));
    let bytes = forge(&area, &[], &Footer::good(2, area.len() as u64, 0, 0));
    assert!(invalid(
        &Trailer::decode(&bytes).unwrap_err(),
        "entries are not sorted"
    ));
}

#[test]
fn a_decode_refuses_the_same_entry_twice() {
    let mut area = entry_bytes(b"a", 1, 0);
    area.extend_from_slice(&entry_bytes(b"a", 1, 0));
    let bytes = forge(&area, &[], &Footer::good(2, area.len() as u64, 0, 0));
    assert!(invalid(
        &Trailer::decode(&bytes).unwrap_err(),
        "entries are not sorted"
    ));
    // The sorted pair is accepted.
    let mut area = entry_bytes(b"a", 1, 0);
    area.extend_from_slice(&entry_bytes(b"a", 1, 8));
    let bytes = forge(&area, &[], &Footer::good(2, area.len() as u64, 0, 0));
    assert_eq!(Trailer::decode(&bytes).unwrap().entries.len(), 2);
}

#[test]
fn the_cell_table_must_cover_the_data_length_at_every_boundary() {
    for (data_len, cells) in [
        (0u64, 0u64),
        (1, 1),
        (CELL_BYTES - 1, 1),
        (CELL_BYTES, 1),
        (CELL_BYTES + 1, 2),
        (2 * CELL_BYTES, 2),
        (2 * CELL_BYTES + 1, 3),
    ] {
        let mut t = trailer(vec![], data_len);
        assert_eq!(t.cell_crcs.len() as u64, cells);
        let bytes = t.encode().unwrap();
        assert_eq!(Trailer::decode(&bytes).unwrap(), t, "data_len {data_len}");
        // One cell too many and one too few are refused by the encode ...
        t.cell_crcs.push(0);
        let err = t.encode().unwrap_err();
        assert!(
            invalid(&err, "cell table does not cover the block"),
            "data_len {data_len}: {err}"
        );
        t.cell_crcs.pop();
        if cells > 0 {
            t.cell_crcs.pop();
            assert!(t.encode().is_err(), "data_len {data_len}");
        }
        // ... and by the decode, whose footer carries a valid crc.
        let crcs = vec![1u32; cells as usize + 1];
        let forged = forge(&[], &crcs, &Footer::good(0, 0, cells + 1, data_len));
        assert!(invalid(
            &Trailer::decode(&forged).unwrap_err(),
            "cell table does not cover the block"
        ));
        if cells > 0 {
            let crcs = vec![1u32; cells as usize - 1];
            let forged = forge(&[], &crcs, &Footer::good(0, 0, cells - 1, data_len));
            assert!(invalid(
                &Trailer::decode(&forged).unwrap_err(),
                "cell table does not cover the block"
            ));
        }
    }
}

#[test]
fn a_trailer_found_after_block_bytes_is_the_trailer_of_the_block() {
    let data: Vec<u8> = (0..(2 * CELL_BYTES as usize + 10))
        .map(|i| (i % 251) as u8)
        .collect();
    let t = Trailer {
        kind: BlockKind::Pack,
        entries: vec![entry(b"x", 1, 0)],
        cell_crcs: cell_crcs(&data),
        data_len: data.len() as u64,
        blake3: [7; 32],
    };
    assert_eq!(t.cell_crcs.len(), 3);
    let mut block = data.clone();
    block.extend_from_slice(&t.encode().unwrap());
    let back = Trailer::decode(&block).unwrap();
    assert_eq!(back, t);
    assert_eq!(back.data_len as usize, data.len());
    // The cell table is the crc of each 1 MiB of the record bytes.
    let c = CELL_BYTES as usize;
    assert_eq!(back.cell_crcs[0], crc32c(&data[..c]));
    assert_eq!(back.cell_crcs[1], crc32c(&data[c..2 * c]));
    assert_eq!(back.cell_crcs[2], crc32c(&data[2 * c..]));
}

#[test]
fn a_buffer_shorter_than_the_footer_is_short() {
    let bytes = sample().encode().unwrap();
    for n in [0usize, 1, 95] {
        let err = Trailer::decode(&bytes[..n]).unwrap_err();
        assert_eq!(
            err,
            FormatError::Short {
                what: WHAT,
                need: 96,
                have: n as u64
            }
        );
    }
}

#[test]
fn a_buffer_cut_inside_the_trailer_is_short_with_the_whole_length_needed() {
    let bytes = sample().encode().unwrap();
    // Dropping the first byte keeps the footer at the end but loses part of the trailer.
    let err = Trailer::decode(&bytes[1..]).unwrap_err();
    assert_eq!(
        err,
        FormatError::Short {
            what: WHAT,
            need: bytes.len() as u64,
            have: bytes.len() as u64 - 1
        }
    );
}

#[test]
fn a_wrong_magic_is_refused() {
    let mut bytes = sample().encode().unwrap();
    let at = bytes.len() - 96;
    bytes[at] ^= 1;
    assert!(matches!(
        Trailer::decode(&bytes).unwrap_err(),
        FormatError::BadMagic { what: WHAT, .. }
    ));
}

#[test]
fn another_version_is_unsupported() {
    for version in [0u16, 2, 0x0100] {
        let f = Footer {
            version,
            ..Footer::good(0, 0, 0, 0)
        };
        let err = Trailer::decode(&forge(&[], &[], &f)).unwrap_err();
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
fn another_cell_shift_is_refused() {
    for shift in [0u8, 19, 21] {
        let f = Footer {
            shift,
            ..Footer::good(0, 0, 0, 0)
        };
        let err = Trailer::decode(&forge(&[], &[], &f)).unwrap_err();
        assert!(
            invalid(&err, "cell shift is not 20"),
            "shift {shift}: {err}"
        );
    }
}

#[test]
fn a_flipped_byte_anywhere_in_the_trailer_is_an_error() {
    let good = trailer(
        vec![entry(b"alpha", 1, 0), entry(b"b", 2, 64)],
        CELL_BYTES + 5,
    )
    .encode()
    .unwrap();
    assert!(Trailer::decode(&good).is_ok());
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x01;
        assert!(
            Trailer::decode(&bad).is_err(),
            "byte {i} of {} flipped without an error",
            good.len()
        );
    }
}

#[test]
fn the_crc_covers_the_entries_the_cell_table_and_the_footer_fields() {
    let good = sample().encode().unwrap();
    let footer = good.len() - 96;
    // An entry byte, a cell crc byte, the blake3, the reserved bytes and the stored crc itself.
    for i in [10usize, 170, footer + 41, footer + 80, footer + 93] {
        let mut bad = good.clone();
        bad[i] ^= 0x80;
        let err = Trailer::decode(&bad).unwrap_err();
        assert!(
            matches!(err, FormatError::BadCrc { what: WHAT, .. }),
            "byte {i}: {err}"
        );
    }
}

#[test]
fn the_reserved_footer_bytes_are_not_read() {
    let f = Footer {
        reserved: [0xff; 20],
        ..Footer::good(0, 0, 0, 0)
    };
    assert!(Trailer::decode(&forge(&[], &[], &f)).is_ok());
}

#[test]
fn a_trailer_length_that_overflows_is_refused() {
    for (entries_len, cells) in [
        (u64::MAX, 0u64),
        (u64::MAX - 95, 0),
        (0, u64::MAX),
        (0, 1 << 62),
        (1, u64::MAX / 4),
    ] {
        let f = Footer::good(0, entries_len, cells, 0);
        let err = Trailer::decode(&forge(&[], &[], &f)).unwrap_err();
        assert!(
            invalid(&err, "trailer length overflows"),
            "{entries_len} {cells}: {err}"
        );
    }
}

#[test]
fn a_trailer_longer_than_the_buffer_is_short() {
    let f = Footer::good(0, 1000, 0, 0);
    let err = Trailer::decode(&forge(&[], &[], &f)).unwrap_err();
    assert_eq!(
        err,
        FormatError::Short {
            what: WHAT,
            need: 1096,
            have: 96
        }
    );
}

#[test]
fn an_entry_count_that_disagrees_with_the_entries_is_refused() {
    let mut area = entry_bytes(b"a", 1, 0);
    area.extend_from_slice(&entry_bytes(b"b", 1, 0));
    for count in [0u64, 1, 3, u64::MAX] {
        let bytes = forge(&area, &[], &Footer::good(count, area.len() as u64, 0, 0));
        let err = Trailer::decode(&bytes).unwrap_err();
        assert!(
            invalid(&err, "entry count does not match"),
            "count {count}: {err}"
        );
    }
    let ok = forge(&area, &[], &Footer::good(2, area.len() as u64, 0, 0));
    assert_eq!(Trailer::decode(&ok).unwrap().entries.len(), 2);
}

#[test]
fn an_entries_length_that_runs_past_the_last_entry_is_refused() {
    for entries_len in [57u64, 60, 64, 104] {
        let mut area = entry_bytes(b"a", 1, 0);
        area.resize(entries_len as usize, 0);
        let bytes = forge(&area, &[], &Footer::good(1, entries_len, 0, 0));
        assert!(Trailer::decode(&bytes).is_err(), "len {entries_len}");
    }
    let area = entry_bytes(b"a", 1, 0);
    assert!(Trailer::decode(&forge(&area, &[], &Footer::good(1, 56, 0, 0))).is_ok());
}

#[test]
fn an_entries_length_that_ends_inside_the_last_entry_padding_is_refused() {
    // The entry is 50 bytes with its two-byte key; the padding to 56 is read from the footer's
    // bytes, so the entries end at 56 while the footer says they end at 50.
    let mut area = entry_bytes(b"ab", 1, 0);
    area.truncate(50);
    let bytes = forge(&area, &[], &Footer::good(1, 50, 0, 0));
    let err = Trailer::decode(&bytes).unwrap_err();
    assert!(invalid(&err, "entry count does not match"), "{err}");
}

#[test]
fn an_entry_whose_key_runs_past_the_trailer_is_short() {
    let mut area = entry_bytes(b"", 1, 0);
    area[..2].copy_from_slice(&60000u16.to_le_bytes());
    let bytes = forge(&area, &[], &Footer::good(1, area.len() as u64, 0, 0));
    let err = Trailer::decode(&bytes).unwrap_err();
    assert!(
        matches!(err, FormatError::Short { what: WHAT, .. }),
        "{err}"
    );
}

#[test]
fn a_key_longer_than_65535_bytes_is_refused_by_the_encode() {
    let t = trailer(vec![entry(&vec![b'k'; 65536], 1, 0)], 0);
    assert_eq!(
        t.encode().unwrap_err().to_string(),
        "block trailer: key longer than 65535 bytes"
    );
    let ok = trailer(vec![entry(&vec![b'k'; 65535], 1, 0)], 0);
    assert_eq!(Trailer::decode(&ok.encode().unwrap()).unwrap(), ok);
}

#[test]
fn the_cell_table_helper_is_the_crc_of_each_mebibyte() {
    let data = vec![1u8; CELL_BYTES as usize + 1];
    assert_eq!(
        cell_crcs(&data),
        vec![crc32c(&data[..CELL_BYTES as usize]), crc32c(&[1])]
    );
    assert!(cell_crcs(&[]).is_empty());
}

#[test]
fn encode_refuses_two_entries_with_the_same_key_stamp_and_offset() {
    let mut dup = entry(b"a", 5, 0);
    dup.length = 7;
    let t = trailer(vec![entry(b"a", 5, 0), entry(b"b", 1, 64), dup], 0);
    let err = t.encode().unwrap_err();
    assert_eq!(err.to_string(), "block trailer: duplicate entries");
    // Same key and stamp at different offsets, or same key and offset at different stamps, are fine.
    let ok = trailer(
        vec![entry(b"a", 5, 0), entry(b"a", 5, 64), entry(b"a", 6, 0)],
        0,
    );
    assert_eq!(
        Trailer::decode(&ok.encode().unwrap())
            .unwrap()
            .entries
            .len(),
        3
    );
}
