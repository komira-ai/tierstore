//! Records and the segment scan.

use super::*;

// Record

#[test]
fn record_header_round_trips_and_every_flipped_bit_is_an_error() {
    let mut r = Rng(1);
    for case in 0..300 {
        let h = gen_header(&mut r);
        let enc = h.encode();
        assert_eq!(RecordHeader::decode(&enc).unwrap(), h, "case {case}");
        let mut longer = enc.to_vec();
        longer.extend(r.bytes(30));
        assert_eq!(RecordHeader::decode(&longer).unwrap(), h, "case {case}");
        let crc = crc32c(&enc[..60]);
        for bit in 0..RecordHeader::encode(&h).len() * 8 {
            let got = RecordHeader::decode(&flipped(&enc, bit));
            let mask = 1u32 << (bit % 32);
            match bit {
                0..32 => assert_eq!(
                    got,
                    Err(FormatError::BadMagic {
                        what: "record",
                        found: RECORD_MAGIC ^ mask
                    })
                ),
                480.. => assert_eq!(
                    got,
                    Err(FormatError::BadCrc {
                        what: "record",
                        stored: crc ^ mask,
                        computed: crc
                    })
                ),
                _ => assert!(got.is_err(), "case {case} bit {bit}"),
            }
        }
    }
}

#[test]
fn record_header_prefixes_are_short_and_random_bytes_never_decode() {
    let mut r = Rng(2);
    let enc = gen_header(&mut r).encode();
    for have in 0..64 {
        let want = FormatError::Short {
            what: "record",
            need: 64,
            have: have as u64,
        };
        assert_eq!(RecordHeader::decode(&enc[..have]), Err(want));
    }
    for case in 0..3000 {
        let n = r.range(0, 150);
        let junk = r.bytes(n);
        assert!(RecordHeader::decode(&junk).is_err(), "case {case}");
        assert!(decode_record(&junk).is_err(), "case {case}");
    }
}

#[test]
fn records_round_trip_and_only_padding_bytes_may_flip() {
    let mut r = Rng(3);
    for case in 0..400 {
        let key = gen_key(&mut r);
        let n = r.len(400);
        let data = r.bytes(n);
        let rec = RecordIn {
            flags: RecordFlags(r.u16()),
            key: &key,
            data: &data,
            generation: Generation(r.array()),
            stamp: Stamp::from_raw(r.u64()),
            object_offset: r.u64(),
        };
        let enc = encode_record(&rec).unwrap();
        let covered = 64 + key.len() + data.len();
        assert_eq!(enc.len() % 16, 0, "case {case}");
        assert!(enc.len() - covered < 16 && enc[covered..].iter().all(|b| *b == 0));
        let (got, used) = decode_record(&enc).unwrap();
        assert_eq!(used, enc.len() as u64);
        assert_eq!((got.key, got.data), (&key[..], &data[..]));
        let h = got.header;
        assert_eq!(
            (h.flags, h.generation, h.stamp),
            (rec.flags, rec.generation, rec.stamp)
        );
        assert_eq!(
            (h.object_offset, h.key_len),
            (rec.object_offset, key.len() as u16)
        );
        assert_eq!(
            (h.data_len, h.data_crc, h.key_crc),
            (data.len() as u64, crc32c(&data), crc32c(&key))
        );
        assert_eq!(h.record_len(), used);
        assert_eq!(h.encode()[..], enc[..64]);
        let mut longer = enc.clone();
        longer.extend(r.bytes(20));
        assert_eq!(decode_record(&longer).unwrap().1, used);
        for _ in 0..40 {
            let bit = r.below(enc.len() as u64 * 8) as usize;
            let damaged = flipped(&enc, bit);
            let got = decode_record(&damaged);
            if bit / 8 < covered {
                assert!(got.is_err(), "case {case} covered bit {bit}");
            } else {
                let (rec2, used2) = got.unwrap();
                assert_eq!(
                    (rec2.header, rec2.key, rec2.data, used2),
                    (h, &key[..], &data[..], used)
                );
            }
        }
    }
}

#[test]
fn record_truncations_are_short_and_damage_never_panics() {
    let mut r = Rng(4);
    for case in 0..150 {
        let key = gen_key(&mut r);
        let n = r.len(200);
        let data = r.bytes(n);
        let enc = encode_record(&RecordIn {
            flags: RecordFlags::default(),
            key: &key,
            data: &data,
            generation: Generation(r.array()),
            stamp: Stamp::from_raw(r.u64()),
            object_offset: 0,
        })
        .unwrap();
        for have in 0..enc.len() {
            let need = if have < 64 { 64 } else { enc.len() as u64 };
            let want = FormatError::Short {
                what: "record",
                need,
                have: have as u64,
            };
            assert_eq!(
                decode_record(&enc[..have]).map(|_| ()),
                Err(want),
                "case {case}"
            );
        }
        for _ in 0..40 {
            let damaged = mutate(&mut r, &enc);
            if let Ok((rec, used)) = decode_record(&damaged) {
                assert!(used <= damaged.len() as u64 && used.is_multiple_of(16));
                assert_eq!(crc32c(rec.key), rec.header.key_crc);
                assert_eq!(crc32c(rec.data), rec.header.data_crc);
            }
        }
    }
}

#[test]
fn a_header_claiming_a_huge_data_length_is_short_not_an_overflow() {
    let mut r = Rng(5);
    for case in 0..300 {
        let mut h = gen_header(&mut r);
        h.data_len = if case % 3 == 0 {
            u64::MAX - r.below(40)
        } else {
            r.u64() | 1 << 40
        };
        let need = record_len(h.key_len, h.data_len);
        assert!(need > 64);
        let want = FormatError::Short {
            what: "record",
            need,
            have: 64,
        };
        assert_eq!(
            decode_record(&h.encode()).map(|_| ()),
            Err(want),
            "case {case}"
        );
    }
    let long = vec![7u8; 65535];
    let mk = |key: &[u8]| {
        let rec = RecordIn {
            flags: RecordFlags::default(),
            key,
            data: b"d",
            generation: Generation([1; 16]),
            stamp: Stamp::ZERO,
            object_offset: 0,
        };
        encode_record(&rec)
    };
    assert_eq!(decode_record(&mk(&long).unwrap()).unwrap().0.key, &long[..]);
    let too_long = vec![7u8; 65536];
    assert_eq!(
        mk(&too_long),
        Err(invalid("record", "key longer than 65535 bytes"))
    );
}

fn segment_of(r: &mut Rng, count: usize) -> (Vec<u8>, Vec<(u64, RecordHeader)>) {
    let mut buf = Vec::new();
    let mut listed = Vec::new();
    for _ in 0..count {
        let key = gen_key(r);
        let n = r.len(300);
        let data = r.bytes(n);
        let enc = encode_record(&RecordIn {
            flags: RecordFlags(r.u16()),
            key: &key,
            data: &data,
            generation: Generation(r.array()),
            stamp: Stamp::from_raw(r.u64()),
            object_offset: r.u64(),
        })
        .unwrap();
        listed.push((buf.len() as u64, RecordHeader::decode(&enc).unwrap()));
        buf.extend(enc);
    }
    (buf, listed)
}

#[test]
fn scan_lists_every_record_and_stops_at_the_first_damaged_one() {
    let mut r = Rng(6);
    for case in 0..200 {
        let count = r.range(0, 6);
        let (mut buf, listed) = segment_of(&mut r, count);
        let total = buf.len() as u64;
        buf.extend(vec![0u8; r.range(0, 70)]);
        let s = scan(&buf);
        assert_eq!(
            (s.records.as_slice(), s.valid_len, s.stop),
            (listed.as_slice(), total, None)
        );
        // A flipped bit in a record's covered bytes ends the scan right before that record.
        for _ in 0..8 {
            if listed.is_empty() {
                break;
            }
            let i = r.below(listed.len() as u64) as usize;
            let start = listed[i].0 as usize;
            let rec_end = listed.get(i + 1).map_or(total, |next| next.0) as usize;
            let h = listed[i].1;
            let covered = 64 + usize::from(h.key_len) + h.data_len as usize;
            let bit = start * 8 + r.below(covered as u64 * 8) as usize;
            assert!(covered <= rec_end - start);
            let s = scan(&flipped(&buf, bit));
            assert_eq!(
                (s.records.len(), s.valid_len),
                (i, start as u64),
                "case {case}"
            );
            assert!(s.stop.is_some());
        }
        if buf.len() > total as usize {
            let bit = r.range(total as usize, buf.len() - 1) * 8 + r.range(0, 7);
            let s = scan(&flipped(&buf, bit));
            assert_eq!((s.records.len(), s.valid_len), (listed.len(), total));
            assert!(s.stop.is_some());
        }
    }
    let s = scan(&[]);
    assert_eq!((s.records.len(), s.valid_len, s.stop), (0, 0, None));
}

#[test]
fn scan_of_every_truncation_and_of_damage_keeps_its_invariants() {
    let mut r = Rng(7);
    for case in 0..60 {
        let count = r.range(1, 5);
        let (buf, listed) = segment_of(&mut r, count);
        let ends: Vec<u64> = listed
            .iter()
            .skip(1)
            .map(|(p, _)| *p)
            .chain([buf.len() as u64])
            .collect();
        for cut in 0..=buf.len() {
            let s = scan(&buf[..cut]);
            let whole = ends.iter().filter(|e| **e <= cut as u64).count();
            let valid = if whole == 0 { 0 } else { ends[whole - 1] };
            assert_eq!(s.records.len(), whole, "case {case} cut {cut}");
            assert_eq!(
                (s.valid_len, s.stop.is_none()),
                (valid, valid == cut as u64)
            );
        }
        for _ in 0..40 {
            let damaged = mutate(&mut r, &buf);
            let s = scan(&damaged);
            assert!(s.valid_len <= damaged.len() as u64 && s.valid_len.is_multiple_of(16));
            let rest = &damaged[s.valid_len as usize..];
            assert_eq!(s.stop.is_none(), rest.iter().all(|b| *b == 0));
            let mut next = 0;
            for (pos, header) in &s.records {
                assert_eq!(*pos, next);
                let (rec, used) = decode_record(&damaged[*pos as usize..]).unwrap();
                assert_eq!(rec.header, *header);
                next += used;
            }
            assert_eq!(next, s.valid_len);
        }
    }
}
