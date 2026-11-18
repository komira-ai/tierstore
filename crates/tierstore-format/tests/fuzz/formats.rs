//! Trailer, footer, descriptor, seal, free and the tombstone log.

use super::*;

// Trailer, footer, descriptor, seal, free

#[test]
fn trailers_round_trip_sorted_and_encode_independently_of_entry_order() {
    let mut r = Rng(8);
    for case in 0..400 {
        let t = gen_trailer(&mut r);
        let enc = t.encode().unwrap();
        let mut want = t.clone();
        want.entries.sort();
        assert_eq!(Trailer::decode(&enc).unwrap(), want, "case {case}");
        assert_eq!(want.encode().unwrap(), enc, "case {case}");
        let mut shuffled = t.clone();
        r.shuffle(&mut shuffled.entries);
        assert_eq!(shuffled.encode().unwrap(), enc);
    }
    let long = TrailerEntry {
        key: vec![9; 65535],
        stamp: Stamp::ZERO,
        offset: 0,
        length: 1,
        generation: Generation([3; 16]),
        crc32c: 5,
        flags: RecordFlags::default(),
    };
    let mut t = Trailer {
        kind: BlockKind::Pack,
        entries: vec![long],
        cell_crcs: vec![],
        data_len: 0,
        blake3: [1; 32],
    };
    assert_eq!(Trailer::decode(&t.encode().unwrap()).unwrap(), t);
    t.entries[0].key.push(0);
    assert_eq!(
        t.encode(),
        Err(invalid("block trailer", "key longer than 65535 bytes"))
    );
}

#[test]
fn a_trailer_is_found_from_the_end_and_any_damage_to_it_is_an_error() {
    let mut r = Rng(9);
    for case in 0..250 {
        let t = gen_trailer(&mut r);
        let enc = t.encode().unwrap();
        for _ in 0..50 {
            let bit = r.below(enc.len() as u64 * 8) as usize;
            assert!(
                Trailer::decode(&flipped(&enc, bit)).is_err(),
                "case {case} bit {bit}"
            );
        }
        // Bytes before the trailer are allowed; any byte taken from it, or added after it, is not.
        let prefix = r.vec(0, 40);
        let all = [prefix.as_slice(), &enc].concat();
        let want = Trailer::decode(&enc).unwrap();
        for from in 0..=prefix.len() {
            assert_eq!(Trailer::decode(&all[from..]).as_ref(), Ok(&want));
        }
        for from in prefix.len() + 1..all.len() {
            assert!(
                Trailer::decode(&all[from..]).is_err(),
                "case {case} from {from}"
            );
        }
        for have in 0..96 {
            let want = FormatError::Short {
                what: "block trailer",
                need: 96,
                have: have as u64,
            };
            assert_eq!(Trailer::decode(&enc[enc.len() - have..]), Err(want));
        }
        for extra in [vec![0], r.bytes(7), r.bytes(100)] {
            assert!(Trailer::decode(&[enc.as_slice(), &extra].concat()).is_err());
        }
    }
}

#[test]
fn crc_repaired_trailer_damage_is_an_error_or_a_trailer_that_encodes_to_itself() {
    let mut r = Rng(10);
    let mut accepted = 0;
    for case in 0..250 {
        let enc = gen_trailer(&mut r).encode().unwrap();
        for _ in 0..12 {
            if let Ok(t) = Trailer::decode(&mutate_refixed(&mut r, &enc)) {
                accepted += 1;
                assert_eq!(Trailer::decode(&t.encode().unwrap()), Ok(t), "case {case}");
            }
        }
        // The named failures, each behind a repaired crc.
        let foot = enc.len() - 96;
        let at = |off: usize, bytes: &[u8]| {
            let mut v = enc.clone();
            v[foot + off..foot + off + bytes.len()].copy_from_slice(bytes);
            refix_tail(&mut v);
            Trailer::decode(&v)
        };
        assert_eq!(
            at(0, b"XSBT"),
            Err(FormatError::BadMagic {
                what: "block trailer",
                found: u32::from_le_bytes(*b"XSBT")
            })
        );
        assert_eq!(
            at(4, &[2, 0]),
            Err(FormatError::UnsupportedVersion {
                what: "block trailer",
                version: 2
            })
        );
        assert_eq!(
            at(7, &[19]),
            Err(invalid("block trailer", "cell shift is not 20"))
        );
    }
    assert!(
        accepted > 0,
        "no repaired mutation was accepted, the check is vacuous"
    );
    for _ in 0..3000 {
        let n = r.range(0, 400);
        let junk = r.bytes(n);
        assert!(Trailer::decode(&junk).is_err());
    }
}

#[test]
fn footers_round_trip_and_are_found_from_the_end() {
    let mut r = Rng(11);
    for case in 0..200 {
        let f = gen_footer(&mut r);
        let enc = f.encode().unwrap();
        assert!(
            enc.len() >= 4096 && enc.len().is_multiple_of(4096),
            "case {case}"
        );
        assert_eq!(ShardFooter::decode(&enc).unwrap(), f, "case {case}");
        assert_eq!(
            f.descriptor().is_some(),
            matches!(f.subject, FooterSubject::Stripe { .. })
        );
        let data = r.vec(0, 5000);
        assert_eq!(
            ShardFooter::decode(&[data.as_slice(), &enc].concat()).unwrap(),
            f
        );
    }
}

#[test]
fn any_damage_to_a_footer_is_an_error() {
    let mut r = Rng(12);
    for case in 0..120 {
        let enc = gen_footer(&mut r).encode().unwrap();
        for _ in 0..40 {
            let bit = r.below(enc.len() as u64 * 8) as usize;
            assert!(
                ShardFooter::decode(&flipped(&enc, bit)).is_err(),
                "case {case} bit {bit}"
            );
        }
        for cut in 0..enc.len() {
            assert!(
                ShardFooter::decode(&enc[..cut]).is_err(),
                "case {case} cut {cut}"
            );
        }
        for skip in 1..=enc.len() {
            let have = enc.len() - skip;
            let need = if have < 16 { 16 } else { enc.len() as u64 };
            let want = FormatError::Short {
                what: "shard footer",
                need,
                have: have as u64,
            };
            assert_eq!(ShardFooter::decode(&enc[skip..]), Err(want), "case {case}");
        }
        assert!(ShardFooter::decode(&[enc.as_slice(), &[0]].concat()).is_err());
    }
}

#[test]
fn crc_repaired_footer_damage_is_an_error_or_a_footer_that_encodes_to_itself() {
    let mut r = Rng(13);
    let mut accepted = 0;
    for case in 0..200 {
        let enc = gen_footer(&mut r).encode().unwrap();
        for _ in 0..15 {
            if let Ok(f) = ShardFooter::decode(&mutate_refixed(&mut r, &enc)) {
                accepted += 1;
                assert_eq!(
                    ShardFooter::decode(&f.encode().unwrap()),
                    Ok(f),
                    "case {case}"
                );
            }
        }
        let at = |off: usize, byte: u8| {
            let mut v = enc.clone();
            v[off] = byte;
            refix_tail(&mut v);
            ShardFooter::decode(&v)
        };
        assert_eq!(
            at(0, 2),
            Err(FormatError::UnsupportedVersion {
                what: "shard footer",
                version: 2
            })
        );
        assert_eq!(
            at(2, 3),
            Err(invalid("shard footer", "unknown subject kind"))
        );
    }
    assert!(
        accepted > 0,
        "no repaired mutation was accepted, the check is vacuous"
    );
    for _ in 0..2000 {
        let n = r.range(0, 5000);
        let junk = r.bytes(n);
        assert!(ShardFooter::decode(&junk).is_err());
    }
}

#[test]
fn descriptors_round_trip_and_any_damage_is_an_error() {
    let mut r = Rng(14);
    for case in 0..300 {
        let d = gen_desc(&mut r);
        let enc = d.encode().unwrap();
        assert_eq!(StripeDescriptor::decode(&enc).unwrap(), d, "case {case}");
        for bit in 0..enc.len() * 8 {
            assert!(
                StripeDescriptor::decode(&flipped(&enc, bit)).is_err(),
                "case {case} bit {bit}"
            );
        }
        for cut in 0..enc.len() {
            let got = StripeDescriptor::decode(&enc[..cut]);
            if cut < 4 {
                let want = FormatError::Short {
                    what: "stripe descriptor",
                    need: 4,
                    have: cut as u64,
                };
                assert_eq!(got, Err(want));
            } else {
                assert!(got.is_err(), "case {case} cut {cut}");
            }
        }
        assert!(StripeDescriptor::decode(&[enc.as_slice(), &[0]].concat()).is_err());
    }
}

#[test]
fn crc_repaired_descriptor_damage_is_an_error_or_a_valid_descriptor() {
    let mut r = Rng(15);
    let mut accepted = 0;
    for case in 0..400 {
        let enc = gen_desc(&mut r).encode().unwrap();
        for _ in 0..10 {
            if let Ok(d) = StripeDescriptor::decode(&mutate_refixed(&mut r, &enc)) {
                accepted += 1;
                d.check().unwrap();
                assert_eq!(
                    StripeDescriptor::decode(&d.encode().unwrap()),
                    Ok(d),
                    "case {case}"
                );
            }
        }
        let mut v = enc.clone();
        v[..4].copy_from_slice(b"ZSSD");
        refix_tail(&mut v);
        assert_eq!(
            StripeDescriptor::decode(&v),
            Err(FormatError::BadMagic {
                what: "stripe descriptor",
                found: u32::from_le_bytes(*b"ZSSD")
            })
        );
        v[..4].copy_from_slice(&enc[..4]);
        v[4] = 2;
        refix_tail(&mut v);
        let want = FormatError::UnsupportedVersion {
            what: "stripe descriptor",
            version: 2,
        };
        assert_eq!(StripeDescriptor::decode(&v), Err(want));
    }
    assert!(
        accepted > 0,
        "no repaired mutation was accepted, the check is vacuous"
    );
    for _ in 0..3000 {
        let n = r.range(0, 200);
        assert!(StripeDescriptor::decode(&r.bytes(n)).is_err());
    }
}

#[test]
fn a_descriptor_that_breaks_its_rules_is_refused_by_encode_and_by_footer_encode() {
    let mut r = Rng(16);
    let blocks_err = invalid("stripe descriptor", "block count outside 1..=k");
    let shards_err = invalid(
        "stripe descriptor",
        "shard indexes not ascending, unique, stored and inside the code",
    );
    let mut seen = 0;
    while seen < 100 {
        let d = gen_desc(&mut r);
        if d.shards.is_empty() || d.code.width() >= 256 {
            continue;
        }
        seen += 1;
        let mut no_blocks = d.clone();
        no_blocks.blocks.clear();
        assert_eq!(no_blocks.encode(), Err(blocks_err.clone()));
        let mut dup = d.clone();
        dup.shards.insert(0, d.shards[0]);
        assert_eq!(dup.encode(), Err(shards_err.clone()));
        let mut outside = d.clone();
        outside.shards.push(PlacedShard {
            index: d.code.width() as u8,
            loc: gen_loc(&mut r),
        });
        assert_eq!(outside.encode(), Err(shards_err.clone()));
        let footer = ShardFooter {
            subject: FooterSubject::Stripe {
                stripe: dup,
                index: 0,
            },
            data_len: 0,
            cell_crcs: vec![],
            blake3: [0; 32],
        };
        assert_eq!(footer.encode(), Err(shards_err.clone()));
    }
}

#[test]
fn seal_states_and_free_records_round_trip_and_any_damage_is_an_error() {
    let mut r = Rng(17);
    for case in 0..300 {
        let s = gen_seal(&mut r);
        let enc = s.encode();
        assert_eq!(SealState::decode(&enc), Ok(s), "case {case}");
        let f = gen_free(&mut r);
        let fenc = f.encode();
        assert_eq!(StripeFree::decode(&fenc), Ok(f), "case {case}");
        for bit in 0..enc.len() * 8 {
            assert!(
                SealState::decode(&flipped(&enc, bit)).is_err(),
                "case {case} bit {bit}"
            );
        }
        for bit in 0..fenc.len() * 8 {
            assert!(
                StripeFree::decode(&flipped(&fenc, bit)).is_err(),
                "case {case} bit {bit}"
            );
        }
        for cut in 0..enc.len() {
            assert!(SealState::decode(&enc[..cut]).is_err());
        }
        for cut in 0..fenc.len() {
            assert!(StripeFree::decode(&fenc[..cut]).is_err());
        }
        assert!(SealState::decode(&[enc.as_slice(), &[0]].concat()).is_err());
        assert!(StripeFree::decode(&[fenc.as_slice(), &[0]].concat()).is_err());
        // The named failures behind a repaired crc: a flag byte of 2, a version, a magic.
        let mut v = enc.clone();
        v[19] = 2;
        refix_tail(&mut v);
        assert_eq!(
            SealState::decode(&v),
            Err(invalid("seal state", "a flag is neither 0 nor 1"))
        );
        let mut v = enc.clone();
        v[4] = 9;
        refix_tail(&mut v);
        assert_eq!(
            SealState::decode(&v),
            Err(FormatError::UnsupportedVersion {
                what: "seal state",
                version: 9
            })
        );
        let mut v = fenc.clone();
        v[4] = 9;
        refix_tail(&mut v);
        assert_eq!(
            StripeFree::decode(&v),
            Err(FormatError::UnsupportedVersion {
                what: "stripe free",
                version: 9
            })
        );
        let mut v = fenc.clone();
        v[0] = 0;
        refix_tail(&mut v);
        let found = u32::from_le_bytes([0, b'S', b'F', b'R']);
        assert_eq!(
            StripeFree::decode(&v),
            Err(FormatError::BadMagic {
                what: "stripe free",
                found
            })
        );
    }
    let mut r = Rng(18);
    for case in 0..3000 {
        let n = r.range(0, 60);
        let junk = r.bytes(n);
        assert!(
            SealState::decode(&junk).is_err() && StripeFree::decode(&junk).is_err(),
            "{case}"
        );
    }
    for case in 0..400 {
        let enc = gen_seal(&mut r).encode();
        for _ in 0..10 {
            if let Ok(s) = SealState::decode(&mutate_refixed(&mut r, &enc)) {
                assert_eq!(SealState::decode(&s.encode()), Ok(s), "case {case}");
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tombstone log

fn frame(kind: u8, origin: u8, seq: u64, body: &[u8]) -> Vec<u8> {
    let mut head = Vec::new();
    head.extend(TOMB_MAGIC.to_le_bytes());
    head.extend([kind, origin, 0, 0]);
    head.extend(seq.to_le_bytes());
    head.extend((body.len() as u32).to_le_bytes());
    let crc = crc32c(&[head.as_slice(), body].concat());
    head.extend(crc.to_le_bytes());
    head.extend(body);
    head
}

fn refix_frame(v: &mut [u8]) {
    if v.len() >= 24 {
        let len = u32::from_le_bytes([v[16], v[17], v[18], v[19]]) as usize;
        if v.len() - 24 >= len {
            let crc = crc32c(&[&v[..20], &v[24..24 + len]].concat());
            v[20..24].copy_from_slice(&crc.to_le_bytes());
        }
    }
}

#[test]
fn log_records_round_trip_and_any_damage_is_an_error() {
    let mut r = Rng(19);
    for case in 0..500 {
        let rec = gen_any_log_record(&mut r);
        let enc = rec.encode().unwrap();
        assert_eq!(
            LogRecord::decode(&enc),
            Ok((rec.clone(), enc.len() as u64)),
            "case {case}"
        );
        let longer = [enc.as_slice(), &r.bytes(9)].concat();
        assert_eq!(LogRecord::decode(&longer).unwrap().1, enc.len() as u64);
        for _ in 0..40 {
            let bit = r.below(enc.len() as u64 * 8) as usize;
            assert!(
                LogRecord::decode(&flipped(&enc, bit)).is_err(),
                "case {case} bit {bit}"
            );
        }
        for cut in 0..enc.len() {
            assert!(
                LogRecord::decode(&enc[..cut]).is_err(),
                "case {case} cut {cut}"
            );
        }
    }
}

#[test]
fn log_frames_have_the_documented_layout_and_refuse_sequence_zero_and_known_unknown_kinds() {
    let mut r = Rng(20);
    for _ in 0..200 {
        let (block, stamp, origin, seq) = (r.u64(), r.u64(), r.u8(), r.u64().max(1));
        let body = [block.to_le_bytes(), stamp.to_le_bytes()].concat();
        let rec = LogRecord {
            origin: NodeId(origin),
            seq,
            entry: LogEntry::BlockDrop(BlockDrop {
                block: BlockId::from_raw(block),
                stamp: Stamp::from_raw(stamp),
            }),
        };
        assert_eq!(rec.encode().unwrap(), frame(2, origin, seq, &body));
        let drop_target = (r.u8(), r.u64());
        let body = [&[drop_target.0][..], &drop_target.1.to_le_bytes()].concat();
        let rec = LogRecord {
            origin: NodeId(origin),
            seq,
            entry: LogEntry::TombDrop(TombDrop {
                target_origin: NodeId(drop_target.0),
                target_seq: drop_target.1,
            }),
        };
        assert_eq!(rec.encode().unwrap(), frame(3, origin, seq, &body));
        let body = r.vec(0, 30);
        let kind = unknown_kind(&mut r, &[1, 2, 3]);
        let unknown = LogEntry::Unknown {
            kind,
            body: body.clone(),
        };
        let rec = LogRecord {
            origin: NodeId(origin),
            seq,
            entry: unknown,
        };
        assert_eq!(rec.encode().unwrap(), frame(kind, origin, seq, &body));
        let zero = frame(kind, origin, 0, &body);
        assert_eq!(
            LogRecord::decode(&zero),
            Err(invalid("tomb log entry", "sequence 0 is not a position"))
        );
        let rec = LogRecord { seq: 0, ..rec };
        assert_eq!(
            rec.encode(),
            Err(invalid("tomb log entry", "sequence 0 is not a position"))
        );
        for known in 1..=3 {
            let entry = LogEntry::Unknown {
                kind: known,
                body: vec![],
            };
            let rec = LogRecord {
                origin: NodeId(origin),
                seq,
                entry,
            };
            assert_eq!(
                rec.encode(),
                Err(invalid(
                    "tomb log entry",
                    "an unknown kind uses a known code"
                ))
            );
        }
    }
}

#[test]
fn crc_repaired_log_damage_is_an_error_or_a_record_that_encodes_to_itself() {
    let mut r = Rng(21);
    let mut accepted = 0;
    for case in 0..1500 {
        let rec = gen_any_log_record(&mut r);
        let enc = rec.encode().unwrap();
        let mut v = mutate(&mut r, &enc);
        refix_frame(&mut v);
        if let Ok((got, used)) = LogRecord::decode(&v) {
            accepted += 1;
            assert!(used <= v.len() as u64);
            let again = got.encode().unwrap();
            assert_eq!(
                LogRecord::decode(&again),
                Ok((got, again.len() as u64)),
                "case {case}"
            );
        }
    }
    assert!(
        accepted > 100,
        "too few repaired mutations were accepted: {accepted}"
    );
    for _ in 0..3000 {
        let n = r.range(0, 120);
        assert!(LogRecord::decode(&r.bytes(n)).is_err());
    }
}

#[test]
fn scan_log_returns_the_records_in_order_and_refuses_gaps_and_foreign_origins() {
    let mut r = Rng(22);
    let seq_err = invalid(
        "tomb log entry",
        "a sequence that is not the previous plus one",
    );
    let origin_err = invalid("tomb log entry", "a record of another origin");
    for case in 0..200 {
        let origin = NodeId(r.u8());
        let count = r.range(3, 7);
        let first = if r.chance(4) {
            u64::MAX - count as u64 + 1 - r.below(3)
        } else {
            r.below(1000) + 1
        };
        let recs: Vec<LogRecord> = (0..count)
            .map(|i| gen_log_record(&mut r, origin, first + i as u64))
            .collect();
        let encs: Vec<Vec<u8>> = recs.iter().map(|x| x.encode().unwrap()).collect();
        let buf = encs.concat();
        assert_eq!(scan_log(&buf, origin).as_ref(), Ok(&recs), "case {case}");
        assert_eq!(scan_log_after(&buf, origin, first - 1).as_ref(), Ok(&recs));
        assert_eq!(scan_log_after(&buf, origin, first), Err(seq_err.clone()));
        assert_eq!(scan_log_after(&buf, origin, 0).is_ok(), first == 1);
        assert_eq!(
            scan_log(&buf, NodeId(origin.0.wrapping_add(1))),
            Err(origin_err.clone())
        );
        let hole = [encs[0].as_slice(), &encs[2..].concat()].concat();
        assert_eq!(scan_log(&hole, origin), Err(seq_err.clone()));
        assert_eq!(
            scan_log(&encs[..count - 1].concat(), origin).unwrap().len(),
            count - 1
        );
        // Every cut: whole records scan, a cut inside a record is an error.
        let mut ends = vec![0];
        ends.extend(encs.iter().scan(0, |at, e| {
            *at += e.len();
            Some(*at)
        }));
        for cut in 0..=buf.len() {
            let got = scan_log(&buf[..cut], origin);
            match ends.iter().position(|e| *e == cut) {
                Some(n) => assert_eq!(
                    got.as_ref(),
                    Ok(&recs[..n].to_vec()),
                    "case {case} cut {cut}"
                ),
                None => assert!(got.is_err(), "case {case} cut {cut}"),
            }
        }
        for _ in 0..20 {
            let damaged = mutate(&mut r, &buf);
            if let Ok(list) = scan_log(&damaged, origin) {
                assert!(list.iter().all(|x| x.origin == origin));
            }
            let _ = scan_log_after(&damaged, origin, first - 1);
        }
    }
    let last = gen_log_record(&mut r, NodeId(1), u64::MAX)
        .encode()
        .unwrap();
    assert_eq!(scan_log_after(&last, NodeId(1), u64::MAX), Err(seq_err));
    assert_eq!(scan_log(&[], NodeId(1)), Ok(vec![]));
    assert_eq!(scan_log_after(&[], NodeId(1), 9), Ok(vec![]));
}
