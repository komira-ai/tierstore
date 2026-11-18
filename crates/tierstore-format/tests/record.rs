//! The record: the 64-byte header, the framing of key and data with padding to 16 bytes, and the
//! scan of a segment that stops at the last valid record.
//!
//! Catches:
//! - a header whose layout differs from the documented offsets, or one that does not round trip every
//!   field and the flag bits this release does not know;
//! - a record length that is off at a 16-byte boundary, or that overflows (and panics) for a header
//!   claiming a data length near `u64::MAX`;
//! - a key longer than 65535 bytes that is accepted and truncated;
//! - a flipped header byte, a flipped data byte, a wrong magic, a torn header or a torn body that
//!   decodes as a record;
//! - a scan that reads past a torn or flipped record, drops a valid one, reports the wrong committed
//!   length, or takes the zero tail of a preallocated segment for an error.

use tierstore_format::FormatError;
use tierstore_format::crc32c;
use tierstore_format::ids::Generation;
use tierstore_format::record::{
    RECORD_ALIGN, RECORD_HEADER_LEN, RECORD_MAGIC, RecordFlags, RecordHeader, RecordIn,
    decode_record, encode_record, record_len, scan,
};
use tierstore_format::stamp::Stamp;

const GEN: Generation = Generation([0x5a; 16]);

fn rec<'a>(key: &'a [u8], data: &'a [u8], stamp: u64) -> RecordIn<'a> {
    RecordIn {
        flags: RecordFlags::default(),
        key,
        data,
        generation: GEN,
        stamp: Stamp::from_raw(stamp),
        object_offset: 0,
    }
}

fn header() -> RecordHeader {
    RecordHeader {
        flags: RecordFlags::RELOCATE.with(RecordFlags::CHAIN_SLICE),
        key_len: 0x0102,
        data_len: 0x0807_0605_0403_0201,
        generation: Generation([9; 16]),
        stamp: Stamp::from_raw(0x1111_2222_3333_4444),
        object_offset: 0x5555_6666_7777_8888,
        data_crc: 0xdead_beef,
        key_crc: 0x0bad_f00d,
    }
}

#[test]
fn the_header_is_64_bytes_with_the_documented_offsets() {
    assert_eq!(RECORD_HEADER_LEN, 64);
    assert_eq!(RECORD_ALIGN, 16);
    assert_eq!(RECORD_MAGIC.to_le_bytes(), *b"TSR1");
    let bytes = header().encode();
    assert_eq!(&bytes[0..4], b"TSR1");
    assert_eq!(&bytes[4..6], &3u16.to_le_bytes());
    assert_eq!(&bytes[6..8], &0x0102u16.to_le_bytes());
    assert_eq!(&bytes[8..16], &0x0807_0605_0403_0201u64.to_le_bytes());
    assert_eq!(&bytes[16..32], &[9u8; 16]);
    assert_eq!(&bytes[32..40], &0x1111_2222_3333_4444u64.to_le_bytes());
    assert_eq!(&bytes[40..48], &0x5555_6666_7777_8888u64.to_le_bytes());
    assert_eq!(&bytes[48..52], &0xdead_beefu32.to_le_bytes());
    assert_eq!(&bytes[52..56], &0x0bad_f00du32.to_le_bytes());
    assert_eq!(&bytes[56..60], &[0u8; 4]);
    assert_eq!(&bytes[60..64], &crc32c(&bytes[..60]).to_le_bytes());
}

#[test]
fn a_header_round_trips_every_field() {
    let h = header();
    assert_eq!(RecordHeader::decode(&h.encode()).unwrap(), h);
}

#[test]
fn a_header_decode_reads_the_start_of_a_longer_buffer() {
    let h = header();
    let mut buf = h.encode().to_vec();
    buf.extend_from_slice(b"more bytes after the header");
    assert_eq!(RecordHeader::decode(&buf).unwrap(), h);
}

#[test]
fn flag_bits_this_release_does_not_know_are_kept() {
    let mut h = header();
    h.flags = RecordFlags(0x8003);
    let back = RecordHeader::decode(&h.encode()).unwrap();
    assert_eq!(back.flags, RecordFlags(0x8003));
    assert!(back.flags.contains(RecordFlags::RELOCATE));
    assert!(back.flags.contains(RecordFlags::CHAIN_SLICE));
    assert!(!RecordFlags(1).contains(RecordFlags::CHAIN_SLICE));
    assert!(RecordFlags(0).contains(RecordFlags(0)));
    assert_eq!(
        RecordFlags::RELOCATE.with(RecordFlags::CHAIN_SLICE),
        RecordFlags(3)
    );
}

#[test]
fn the_reserved_bytes_are_covered_by_the_header_crc_but_not_read() {
    let mut bytes = header().encode();
    bytes[57] = 1;
    assert!(matches!(
        RecordHeader::decode(&bytes),
        Err(FormatError::BadCrc { what: "record", .. })
    ));
    // A later release that uses the reserved bytes writes a valid crc; this release reads it.
    let crc = crc32c(&bytes[..60]);
    bytes[60..].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(RecordHeader::decode(&bytes).unwrap(), header());
}

#[test]
fn a_flipped_header_byte_is_a_bad_magic_or_a_bad_crc() {
    let good = header().encode();
    for i in 0..RECORD_HEADER_LEN {
        let mut bad = good;
        bad[i] ^= 0x10;
        let err = RecordHeader::decode(&bad).unwrap_err();
        if i < 4 {
            assert!(
                matches!(err, FormatError::BadMagic { what: "record", .. }),
                "byte {i}: {err}"
            );
        } else {
            assert!(
                matches!(err, FormatError::BadCrc { what: "record", .. }),
                "byte {i}: {err}"
            );
        }
    }
}

#[test]
fn a_bad_magic_reports_the_four_bytes_found() {
    let mut bytes = header().encode();
    bytes[..4].copy_from_slice(b"XXXX");
    let err = RecordHeader::decode(&bytes).unwrap_err();
    assert_eq!(
        err,
        FormatError::BadMagic {
            what: "record",
            found: u32::from_le_bytes(*b"XXXX")
        }
    );
}

#[test]
fn a_torn_header_is_short_at_every_length_below_64() {
    let bytes = header().encode();
    for n in [0usize, 1, 4, 59, 60, 63] {
        let err = RecordHeader::decode(&bytes[..n]).unwrap_err();
        assert_eq!(
            err,
            FormatError::Short {
                what: "record",
                need: 64,
                have: n as u64
            },
            "n = {n}"
        );
    }
}

#[test]
fn the_record_length_pads_to_16_bytes() {
    assert_eq!(record_len(0, 0), 64);
    assert_eq!(record_len(0, 1), 80);
    assert_eq!(record_len(0, 16), 80);
    assert_eq!(record_len(0, 17), 96);
    assert_eq!(record_len(1, 15), 80);
    assert_eq!(record_len(1, 16), 96);
    assert_eq!(record_len(16, 0), 80);
    assert_eq!(record_len(4, 12), 80);
    assert_eq!(
        record_len(u16::MAX, 0),
        (64 + 65535u64).next_multiple_of(16)
    );
    let mut h = header();
    h.key_len = 3;
    h.data_len = 20;
    assert_eq!(h.record_len(), 96);
    assert_eq!(record_len(0, 1 << 40), (1u64 << 40) + 64);
}

#[test]
fn the_record_length_of_a_huge_data_length_saturates_instead_of_overflowing() {
    for data_len in [u64::MAX, u64::MAX - 70, u64::MAX - 79, u64::MAX - 14] {
        let len = record_len(u16::MAX, data_len);
        assert!(len >= u64::MAX - 15, "data_len {data_len}: {len}");
        assert_eq!(len % 16, 0);
    }
}

#[test]
fn a_record_round_trips_key_data_header_and_length() {
    let key = b"some/key";
    let data = b"hello record";
    let mut input = rec(key, data, 77);
    input.flags = RecordFlags::CHAIN_SLICE;
    input.object_offset = 4096;
    let bytes = encode_record(&input).unwrap();
    assert_eq!(bytes.len(), 96);
    assert_eq!(bytes.len() % RECORD_ALIGN, 0);
    let (r, used) = decode_record(&bytes).unwrap();
    assert_eq!(used, 96);
    assert_eq!(r.key, key);
    assert_eq!(r.data, data);
    assert_eq!(r.header.flags, RecordFlags::CHAIN_SLICE);
    assert_eq!(r.header.key_len, 8);
    assert_eq!(r.header.data_len, 12);
    assert_eq!(r.header.generation, GEN);
    assert_eq!(r.header.stamp, Stamp::from_raw(77));
    assert_eq!(r.header.object_offset, 4096);
    assert_eq!(r.header.data_crc, crc32c(data));
    assert_eq!(&bytes[..64], &r.header.encode());
    assert!(bytes[64 + 8 + 12..].iter().all(|b| *b == 0));
}

#[test]
fn an_empty_key_and_empty_data_make_a_64_byte_record() {
    let bytes = encode_record(&rec(b"", b"", 1)).unwrap();
    assert_eq!(bytes.len(), 64);
    let (r, used) = decode_record(&bytes).unwrap();
    assert_eq!(used, 64);
    assert!(r.key.is_empty());
    assert!(r.data.is_empty());
    assert_eq!(r.header.data_crc, 0);
}

#[test]
fn record_sizes_at_the_16_byte_boundaries_pad_to_the_next_multiple() {
    for (key, data, total) in [
        (0usize, 15usize, 80usize),
        (0, 16, 80),
        (0, 17, 96),
        (5, 11, 80),
        (5, 12, 96),
    ] {
        let k = vec![b'k'; key];
        let d = vec![b'd'; data];
        let bytes = encode_record(&rec(&k, &d, 1)).unwrap();
        assert_eq!(bytes.len(), total, "key {key} data {data}");
        let (r, used) = decode_record(&bytes).unwrap();
        assert_eq!(used as usize, total);
        assert_eq!(r.key, &k[..]);
        assert_eq!(r.data, &d[..]);
    }
}

#[test]
fn a_key_of_65535_bytes_is_kept_and_one_of_65536_is_refused() {
    let key = vec![b'k'; 65535];
    let bytes = encode_record(&rec(&key, b"x", 1)).unwrap();
    let (r, _) = decode_record(&bytes).unwrap();
    assert_eq!(r.key.len(), 65535);
    assert_eq!(r.header.key_len, u16::MAX);
    let err = encode_record(&rec(&vec![b'k'; 65536], b"x", 1)).unwrap_err();
    assert_eq!(err.to_string(), "record: key longer than 65535 bytes");
}

#[test]
fn decode_record_leaves_the_bytes_after_the_record_alone() {
    let a = encode_record(&rec(b"a", b"1", 1)).unwrap();
    let b = encode_record(&rec(b"b", b"2", 2)).unwrap();
    let mut buf = a.clone();
    buf.extend_from_slice(&b);
    let (r, used) = decode_record(&buf).unwrap();
    assert_eq!(r.key, b"a");
    assert_eq!(used as usize, a.len());
    let (r2, _) = decode_record(&buf[used as usize..]).unwrap();
    assert_eq!(r2.key, b"b");
}

#[test]
fn a_flipped_data_byte_is_a_bad_crc_naming_both_crcs() {
    let data = b"0123456789abcdef";
    let mut bytes = encode_record(&rec(b"key", data, 1)).unwrap();
    bytes[64 + 3 + 5] ^= 1;
    let err = decode_record(&bytes).unwrap_err();
    let mut flipped = data.to_vec();
    flipped[5] ^= 1;
    assert_eq!(
        err,
        FormatError::BadCrc {
            what: "record",
            stored: crc32c(data),
            computed: crc32c(&flipped)
        }
    );
}

#[test]
fn a_record_whose_body_is_cut_is_short_with_the_length_it_needed() {
    let bytes = encode_record(&rec(b"key", &[7u8; 100], 1)).unwrap();
    assert_eq!(bytes.len(), 176);
    for n in [64usize, 100, 166, 175] {
        let err = decode_record(&bytes[..n]).unwrap_err();
        assert_eq!(
            err,
            FormatError::Short {
                what: "record",
                need: 176,
                have: n as u64
            },
            "n = {n}"
        );
    }
    assert!(decode_record(&bytes).is_ok());
    // The padding may be cut off the last record of a buffer only if the buffer says it is whole;
    // a decode needs the padding too.
    assert!(decode_record(&bytes[..170]).is_err());
}

#[test]
fn a_header_claiming_a_data_length_near_u64_max_is_short_not_a_panic() {
    for data_len in [u64::MAX, u64::MAX - 1, u64::MAX - 63, 1 << 63] {
        let mut h = header();
        h.key_len = 0;
        h.data_len = data_len;
        let mut buf = h.encode().to_vec();
        buf.extend_from_slice(&[0u8; 64]);
        let err = decode_record(&buf).unwrap_err();
        match err {
            FormatError::Short {
                what: "record",
                need,
                have,
            } => {
                assert!(need > have);
                assert_eq!(have, 128);
            }
            other => panic!("data_len {data_len}: {other}"),
        }
    }
}

#[test]
fn a_scan_of_nothing_finds_nothing() {
    let s = scan(&[]);
    assert!(s.records.is_empty());
    assert_eq!(s.valid_len, 0);
    assert_eq!(s.stop, None);
}

#[test]
fn a_scan_lists_every_record_with_its_offset_and_the_committed_length() {
    let a = encode_record(&rec(b"a", b"first", 1)).unwrap();
    let b = encode_record(&rec(b"bb", &[1u8; 100], 2)).unwrap();
    let c = encode_record(&rec(b"", b"", 3)).unwrap();
    let mut buf = a.clone();
    buf.extend_from_slice(&b);
    buf.extend_from_slice(&c);
    let s = scan(&buf);
    assert_eq!(s.stop, None);
    assert_eq!(s.valid_len as usize, buf.len());
    let offsets: Vec<u64> = s.records.iter().map(|(o, _)| *o).collect();
    assert_eq!(offsets, vec![0, a.len() as u64, (a.len() + b.len()) as u64]);
    let stamps: Vec<u64> = s.records.iter().map(|(_, h)| h.stamp.raw()).collect();
    assert_eq!(stamps, vec![1, 2, 3]);
    assert_eq!(s.records[1].1.key_len, 2);
    assert_eq!(s.records[1].1.data_len, 100);
}

#[test]
fn a_scan_takes_a_zero_tail_as_the_end_of_the_records() {
    let a = encode_record(&rec(b"a", b"x", 1)).unwrap();
    let mut buf = a.clone();
    buf.extend_from_slice(&[0u8; 4096]);
    let s = scan(&buf);
    assert_eq!(s.stop, None);
    assert_eq!(s.valid_len as usize, a.len());
    assert_eq!(s.records.len(), 1);
    // One zero byte is a tail too.
    let mut buf = a.clone();
    buf.push(0);
    let s = scan(&buf);
    assert_eq!(s.stop, None);
    assert_eq!(s.valid_len as usize, a.len());
}

#[test]
fn a_scan_of_only_zero_bytes_is_empty_and_not_an_error() {
    let s = scan(&[0u8; 100]);
    assert!(s.records.is_empty());
    assert_eq!(s.valid_len, 0);
    assert_eq!(s.stop, None);
}

#[test]
fn a_scan_does_not_read_a_record_that_follows_a_zero_gap() {
    let a = encode_record(&rec(b"a", b"x", 1)).unwrap();
    let b = encode_record(&rec(b"b", b"y", 2)).unwrap();
    let mut buf = a.clone();
    buf.extend_from_slice(&[0u8; 32]);
    buf.extend_from_slice(&b);
    let s = scan(&buf);
    // The zeros do not make the rest all zero, so the scan tries to decode them as a header.
    assert_eq!(s.records.len(), 1);
    assert_eq!(s.valid_len as usize, a.len());
    assert!(matches!(
        s.stop,
        Some(FormatError::BadMagic {
            what: "record",
            found: 0
        })
    ));
}

#[test]
fn a_torn_last_record_stops_the_scan_at_the_last_valid_one() {
    let a = encode_record(&rec(b"a", b"first", 1)).unwrap();
    let b = encode_record(&rec(b"b", &[2u8; 200], 2)).unwrap();
    let mut buf = a.clone();
    buf.extend_from_slice(&b);
    for cut in [
        a.len() + 1,
        a.len() + 63,
        a.len() + 64,
        a.len() + 100,
        buf.len() - 1,
    ] {
        let s = scan(&buf[..cut]);
        assert_eq!(s.records.len(), 1, "cut {cut}");
        assert_eq!(s.valid_len as usize, a.len(), "cut {cut}");
        assert!(
            matches!(s.stop, Some(FormatError::Short { what: "record", .. })),
            "cut {cut}"
        );
    }
}

#[test]
fn a_flipped_byte_in_a_later_record_stops_the_scan_before_it() {
    let a = encode_record(&rec(b"a", b"first", 1)).unwrap();
    let b = encode_record(&rec(b"b", b"second", 2)).unwrap();
    let c = encode_record(&rec(b"c", b"third", 3)).unwrap();
    let mut buf = a.clone();
    buf.extend_from_slice(&b);
    buf.extend_from_slice(&c);
    let mut data_flip = buf.clone();
    data_flip[a.len() + 64 + 1 + 2] ^= 0xff;
    let s = scan(&data_flip);
    assert_eq!(s.records.len(), 1);
    assert_eq!(s.valid_len as usize, a.len());
    assert!(matches!(
        s.stop,
        Some(FormatError::BadCrc { what: "record", .. })
    ));
    let mut header_flip = buf.clone();
    header_flip[a.len() + 20] ^= 1;
    let s = scan(&header_flip);
    assert_eq!(s.records.len(), 1);
    assert_eq!(s.valid_len as usize, a.len());
    assert!(matches!(s.stop, Some(FormatError::BadCrc { .. })));
}

#[test]
fn a_flipped_byte_in_the_first_record_gives_an_empty_committed_prefix() {
    let mut buf = encode_record(&rec(b"a", b"first", 1)).unwrap();
    buf[0] ^= 1;
    let s = scan(&buf);
    assert!(s.records.is_empty());
    assert_eq!(s.valid_len, 0);
    assert!(matches!(s.stop, Some(FormatError::BadMagic { .. })));
}

#[test]
fn a_few_nonzero_bytes_after_the_records_are_a_short_record_not_a_valid_end() {
    let a = encode_record(&rec(b"a", b"x", 1)).unwrap();
    let mut buf = a.clone();
    buf.extend_from_slice(&[1, 2, 3, 4, 5]);
    let s = scan(&buf);
    assert_eq!(s.records.len(), 1);
    assert_eq!(s.valid_len as usize, a.len());
    assert_eq!(
        s.stop,
        Some(FormatError::Short {
            what: "record",
            need: 64,
            have: 5
        })
    );
}

#[test]
fn a_flipped_key_byte_is_a_bad_crc_and_never_a_record_under_another_key() {
    let rec = RecordIn {
        flags: RecordFlags::default(),
        key: b"packs/abc",
        data: b"payload",
        generation: Generation([1; 16]),
        stamp: Stamp::from_raw(7),
        object_offset: 0,
    };
    let good = encode_record(&rec).unwrap();
    let (r, _) = decode_record(&good).unwrap();
    assert_eq!(r.header.key_crc, crc32c(b"packs/abc"));
    for i in RECORD_HEADER_LEN..RECORD_HEADER_LEN + rec.key.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x01;
        assert!(
            matches!(
                decode_record(&bad),
                Err(FormatError::BadCrc { what: "record", .. })
            ),
            "key byte {i}"
        );
        let scanned = scan(&bad);
        assert!(scanned.records.is_empty());
        assert_eq!(scanned.valid_len, 0);
    }
}
