//! Block building by `feed`, and chains.

use super::*;

// Blocks

fn small_geo() -> BlockGeometry {
    BlockGeometry::new(256 * 1024, 135_168).unwrap()
}

struct Built {
    bytes: Vec<u8>,
    starts: Vec<u64>,
    ends: Vec<u64>,
    heads: Vec<RecordHeader>,
    keys: Vec<Vec<u8>>,
    datas: Vec<Vec<u8>>,
    sealed: Sealed,
}

fn build(r: &mut Rng, kind: BlockKind) -> Built {
    let mut b = BlockBuilder::new(small_geo(), kind).unwrap();
    let mut built = Built {
        bytes: vec![],
        starts: vec![],
        ends: vec![],
        heads: vec![],
        keys: vec![],
        datas: vec![],
        sealed: b.finish().unwrap(),
    };
    let chain = kind == BlockKind::ChainSlice;
    for _ in 0..if chain { 1 } else { r.range(1, 5) } {
        let key = gen_key(r);
        let n = r.len(3000);
        let data = r.bytes(n);
        let flags = if chain {
            RecordFlags(r.u16()).with(RecordFlags::CHAIN_SLICE)
        } else {
            RecordFlags(r.u16() & !RecordFlags::CHAIN_SLICE.0)
        };
        let rec = RecordIn {
            flags,
            key: &key,
            data: &data,
            generation: Generation(r.array()),
            stamp: Stamp::from_raw(r.u64()),
            object_offset: if chain { r.u64() } else { 0 },
        };
        if !b.fits(key.len() as u16, data.len() as u64) {
            break;
        }
        let out = b.append(&rec).unwrap();
        let gap = out.iter().take_while(|x| **x == 0).count();
        built.starts.push(built.bytes.len() as u64 + gap as u64);
        built.bytes.extend(&out);
        built.ends.push(built.bytes.len() as u64);
        built.heads.push(RecordHeader::decode(&out[gap..]).unwrap());
        built.keys.push(key);
        built.datas.push(data);
    }
    assert_eq!(
        (b.len(), b.records()),
        (built.bytes.len() as u64, built.starts.len())
    );
    built.sealed = b.finish().unwrap();
    built
}

fn feed_pieces(b: &mut BlockBuilder, r: &mut Rng, mut bytes: &[u8]) -> Result<(), BlockError> {
    while !bytes.is_empty() {
        let cap = *r.pick(&[7, 300, 5000]);
        let n = r.range(1, cap).min(bytes.len());
        b.feed(&bytes[..n])?;
        bytes = &bytes[n..];
    }
    Ok(())
}

struct Probe {
    fed: Result<(), BlockError>,
    sealed: Option<Sealed>,
}

/// Feeds `bytes` in random pieces and in one piece, and checks what must hold of any input.
fn probe(kind: BlockKind, bytes: &[u8], r: &mut Rng) -> Probe {
    let geo = small_geo();
    let mut split = BlockBuilder::new(geo, kind).unwrap();
    let fed = feed_pieces(&mut split, r, bytes);
    let mut one = BlockBuilder::new(geo, kind).unwrap();
    let whole = if bytes.is_empty() {
        Ok(())
    } else {
        one.feed(bytes)
    };
    assert_eq!(fed, whole, "the split of the input changed the outcome");
    let (a, b) = (split.finish(), one.finish());
    assert_eq!(a, b, "the split of the input changed the trailer");
    if fed.is_err() {
        let broken = invalid(
            "block",
            "an earlier error left the bytes counted not a block",
        );
        assert_eq!(a, Err(BlockError::Format(broken)));
        assert!(split.feed(&[0]).is_err());
        return Probe { fed, sealed: None };
    }
    let sealed = a.ok();
    if let Some(s) = &sealed {
        assert_eq!(s.trailer.blake3, blake3_256(bytes));
        assert_eq!(s.trailer.data_len, bytes.len() as u64);
        assert_eq!(s.trailer.cell_crcs, cell_crcs(bytes));
        assert_eq!(Trailer::decode(&s.bytes).as_ref(), Ok(&s.trailer));
        assert_eq!(s.trailer.entries.len(), one.records());
    }
    Probe { fed, sealed }
}

const KINDS: [BlockKind; 3] = [BlockKind::Segment, BlockKind::Pack, BlockKind::ChainSlice];

#[test]
fn a_fed_block_gives_the_trailer_of_its_bytes_whatever_the_pieces() {
    let mut r = Rng(29);
    for case in 0..90 {
        let kind = KINDS[case % 3];
        let built = build(&mut r, kind);
        let t = &built.sealed.trailer;
        let mut sorted: Vec<&TrailerEntry> = t.entries.iter().collect();
        sorted.sort();
        assert_eq!(t.entries.iter().collect::<Vec<_>>(), sorted);
        assert_eq!((t.kind, t.entries.len()), (kind, built.starts.len()));
        for i in 0..built.starts.len() {
            let e = t
                .entries
                .iter()
                .find(|e| e.offset == built.starts[i])
                .unwrap();
            let h = built.heads[i];
            let relocate = if kind == BlockKind::Pack {
                RecordFlags::RELOCATE
            } else {
                RecordFlags(0)
            };
            assert_eq!(e.key, built.keys[i]);
            assert_eq!(
                (e.stamp, e.generation, e.flags),
                (h.stamp, h.generation, h.flags.with(relocate))
            );
            assert_eq!(
                (e.length, e.crc32c),
                (built.datas[i].len() as u64, crc32c(&built.datas[i]))
            );
            assert_eq!(built.ends[i] - built.starts[i], h.record_len());
        }
        if kind == BlockKind::Segment {
            let s = scan(&built.bytes);
            assert_eq!(s.valid_len, built.bytes.len() as u64);
            let starts: Vec<u64> = s.records.iter().map(|(p, _)| *p).collect();
            assert_eq!(starts, built.starts);
        }
        for _ in 0..3 {
            let p = probe(kind, &built.bytes, &mut r);
            assert_eq!(
                (p.fed, p.sealed),
                (Ok(()), Some(built.sealed.clone())),
                "case {case}"
            );
        }
    }
}

#[test]
fn a_flipped_bit_anywhere_in_a_fed_block_is_refused_by_feed() {
    let mut r = Rng(30);
    for case in 0..90 {
        let kind = KINDS[case % 3];
        let built = build(&mut r, kind);
        if built.bytes.is_empty() {
            continue;
        }
        for _ in 0..25 {
            let bit = r.below(built.bytes.len() as u64 * 8) as usize;
            let p = probe(kind, &flipped(&built.bytes, bit), &mut r);
            assert!(p.fed.is_err(), "case {case} bit {bit} kind {kind:?}");
        }
    }
}

#[test]
fn a_truncated_block_finishes_only_at_a_record_boundary() {
    let mut r = Rng(31);
    let inside = invalid("block", "the block ends inside a record or its padding");
    for case in 0..90 {
        let kind = KINDS[case % 3];
        let built = build(&mut r, kind);
        let mut cuts: Vec<u64> = vec![0, built.bytes.len() as u64];
        for e in &built.ends {
            cuts.extend([e.saturating_sub(1), *e, e + 1]);
        }
        cuts.extend((0..20).map(|_| r.below(built.bytes.len() as u64 + 1)));
        for cut in cuts.into_iter().filter(|c| *c <= built.bytes.len() as u64) {
            let mut b = BlockBuilder::new(small_geo(), kind).unwrap();
            assert_eq!(
                feed_pieces(&mut b, &mut r, &built.bytes[..cut as usize]),
                Ok(())
            );
            let whole = built.ends.iter().filter(|e| **e <= cut).count();
            if cut == 0 || built.ends.contains(&cut) {
                let s = b.finish().unwrap();
                assert_eq!(
                    (s.trailer.entries.len(), s.trailer.data_len),
                    (whole, cut),
                    "case {case}"
                );
            } else {
                assert_eq!(
                    b.finish(),
                    Err(BlockError::Format(inside.clone())),
                    "case {case} cut {cut}"
                );
            }
        }
    }
}

#[test]
fn damaged_and_random_blocks_never_panic_in_any_kind_of_builder() {
    let mut r = Rng(32);
    let mut accepted = 0;
    for case in 0..45 {
        let built = build(&mut r, KINDS[case % 3]);
        for _ in 0..12 {
            let damaged = mutate(&mut r, &built.bytes);
            for kind in KINDS {
                probe(kind, &damaged, &mut r);
            }
        }
        // A cut at a record boundary is the one damage that still is a block.
        let cut = *r.pick(&built.ends) as usize;
        accepted += usize::from(
            probe(KINDS[case % 3], &built.bytes[..cut], &mut r)
                .sealed
                .is_some(),
        );
        // The first record's header with a byte changed and its crc repaired.
        for _ in 0..12 {
            let mut v = built.bytes.clone();
            if v.len() < 64 {
                continue;
            }
            v[r.range(0, 59)] = r.u8();
            let crc = crc32c(&v[..60]);
            v[60..64].copy_from_slice(&crc.to_le_bytes());
            for kind in KINDS {
                probe(kind, &v, &mut r);
            }
        }
    }
    assert_eq!(accepted, 45);
    for _ in 0..300 {
        let n = r.range(0, 2000);
        let junk = r.bytes(n);
        let zeros = vec![0u8; n];
        for kind in KINDS {
            probe(kind, &junk, &mut r);
            probe(kind, &zeros, &mut r);
        }
    }
}

#[test]
fn a_header_with_a_repaired_crc_is_refused_for_the_rule_it_breaks() {
    let mut r = Rng(33);
    for _ in 0..30 {
        let seg = build(&mut r, BlockKind::Segment);
        let chain = build(&mut r, BlockKind::ChainSlice);
        let repaired = |bytes: &[u8], at: usize, with: &[u8]| {
            let mut v = bytes.to_vec();
            v[at..at + with.len()].copy_from_slice(with);
            let crc = crc32c(&v[..60]);
            v[60..64].copy_from_slice(&crc.to_le_bytes());
            v
        };
        let flags = seg.heads[0].flags.0 | 2;
        let kind_err = BlockError::Format(invalid(
            "block",
            "a chain slice record outside a chain slice block, or the reverse",
        ));
        let v = repaired(&seg.bytes, 4, &flags.to_le_bytes());
        assert_eq!(
            probe(BlockKind::Segment, &v, &mut r).fed,
            Err(kind_err.clone())
        );
        let flags = chain.heads[0].flags.0 & !2;
        let v = repaired(&chain.bytes, 4, &flags.to_le_bytes());
        assert_eq!(probe(BlockKind::ChainSlice, &v, &mut r).fed, Err(kind_err));
        let v = repaired(&seg.bytes, 40, &5u64.to_le_bytes());
        let want = BlockError::Format(invalid(
            "block",
            "an object offset on a record that is not a chain slice",
        ));
        assert_eq!(probe(BlockKind::Segment, &v, &mut r).fed, Err(want));
        // A data length past max_object is a chain, refused when the header is read.
        let v = repaired(&seg.bytes, 8, &(1u64 << 40).to_le_bytes());
        let want = BlockError::Format(invalid(
            "block",
            "an object larger than max_object is a chain, never one record",
        ));
        assert_eq!(probe(BlockKind::Segment, &v, &mut r).fed, Err(want));
        let mut two = chain.bytes.clone();
        two.resize(two.len().next_multiple_of(4096), 0);
        two.extend(&chain.bytes);
        let want = BlockError::Format(invalid("block", "a chain slice block holds one record"));
        assert_eq!(probe(BlockKind::ChainSlice, &two, &mut r).fed, Err(want));
    }
}

#[test]
fn chain_slices_tile_every_requested_range_exactly() {
    let mut r = Rng(34);
    let geo = small_geo();
    let max = geo.max_object();
    let past = invalid("block", "a range past the end of the chain");
    for case in 0..500 {
        let size = max + 1 + r.below(8 * max);
        let first = BlockId::new(NodeId(r.u8()), r.below(1 << 40)).unwrap();
        let chain = geo.chain(first, size).unwrap();
        let blocks = chain.blocks();
        assert_eq!(blocks, size.div_ceil(max), "case {case}");
        assert_eq!(chain.slice(blocks), None);
        let last = chain.slice(blocks - 1).unwrap();
        assert_eq!(
            (last.len, last.block.raw()),
            (size - (blocks - 1) * max, first.raw() + blocks - 1)
        );
        let offset = r.below(size + 1);
        let len = r.below(size - offset + 1);
        let cover = chain.covering(offset, len).unwrap();
        assert_eq!(cover.is_empty(), len == 0);
        let mut at = offset;
        for s in &cover {
            assert_eq!(
                (s.object_offset, s.index, s.slice_offset),
                (at, at / max, at % max)
            );
            assert_eq!(s.block.raw(), first.raw() + s.index);
            assert!(s.len > 0 && s.slice_offset + s.len <= max);
            at += s.len;
        }
        assert_eq!(at, offset + len);
        assert_eq!(
            chain.covering(offset, size - offset + 1 + r.below(5)),
            Err(past.clone())
        );
        assert_eq!(chain.covering(u64::MAX - r.below(3), 5), Err(past.clone()));
    }
}
