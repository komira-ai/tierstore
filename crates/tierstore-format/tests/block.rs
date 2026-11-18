//! Block geometry and the block builder: the boundary of one block, writing against reading, the
//! trailer as a function of the block's bytes (the errors of a misused builder are in
//! `block_errors.rs`).
//!
//! Catches:
//! - a geometry that accepts a zero or unaligned size, a reserve not below it, or one byte too small
//!   for the longest key, or that refuses the smallest reserve that fits;
//! - an object of `block_bytes - block_reserve` that does not fit one block alone (any key length), or
//!   one byte more that is not refused as a chain, for every key and kind;
//! - a block that grows past `block_bytes` with its trailer, or a `fits` that disagrees with `begin`;
//! - `append` and `begin`/`data`/`end` bytes that differ, or a trailer that differs between writing a
//!   block, reading its bytes back at any cut, and resuming at a record boundary;
//! - a trailer whose entries, cell table, length or blake3 differ from a scan of the bytes, whose
//!   entries are unsorted, or whose pack entries lack (or pack headers carry) the relocate flag;
//! - a pack record off 4 KiB or a segment record off 16 bytes, or a nonzero gap or padding accepted;
//! - a limit off by one: a block of exactly `block_bytes` with its trailer refused, or one more
//!   data byte accepted.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

#[path = "block/support.rs"]
#[allow(
    dead_code,
    unused_imports,
    reason = "the helpers are shared by two test files; each uses a part"
)]
mod support;

use support::*;

#[test]
fn the_default_geometry_is_one_gib_blocks_with_a_one_mib_reserve() {
    assert_eq!(BlockGeometry::DEFAULT, BlockGeometry::default());
    assert_eq!(DEFAULT_BLOCK_BYTES, 1 << 30);
    assert_eq!(DEFAULT_BLOCK_RESERVE, 1 << 20);
    let g = BlockGeometry::default();
    assert_eq!((g.block_bytes(), g.block_reserve()), (1 << 30, 1 << 20));
    assert_eq!(g.max_object(), (1 << 30) - (1 << 20));
    assert_eq!((PACK_ALIGN, TRAILER_ENTRY_FIXED_LEN), (4096, 48));
}

#[test]
fn a_block_size_that_is_not_a_positive_multiple_of_4_kib_or_a_reserve_not_below_it_is_refused() {
    let size = "block_bytes is not a positive multiple of 4 KiB";
    for (bytes, reserve) in [(0, 0), (0, 5), (4097, 0), (MIB + 1, MIN_RESERVE), (4095, 0)] {
        let got = BlockGeometry::new(bytes, reserve);
        assert_eq!(got, Err(invalid_geometry(size)));
    }
    let reserve = "block_reserve is not below block_bytes";
    for (bytes, reserve_bytes) in [(8192, 8192), (8192, 9000), (MIB, MIB), (MIB, u64::MAX)] {
        let got = BlockGeometry::new(bytes, reserve_bytes);
        assert_eq!(got, Err(invalid_geometry(reserve)));
    }
}

fn invalid_geometry(reason: &'static str) -> FormatError {
    FormatError::Invalid {
        what: "block",
        reason,
    }
}

#[test]
fn the_smallest_reserve_that_holds_the_longest_key_is_131295_for_a_1_mib_block() {
    // By hand, for block_bytes = 1 MiB and reserve r: the largest object is 1 MiB - r, and a
    // record with a 65535-byte key at offset 0 takes 64 + 65535 + (1 MiB - r), rounded up to 16,
    // plus one trailer entry (48 + 65535 rounded up to 8 = 65584), one cell crc (4) and the 96-byte
    // footer. That is at most 1 MiB when r >= 131283 + ((r + 1) mod 16), first true at 131295.
    let refused =
        invalid_geometry("block_reserve cannot hold a header, its padding and a one-entry trailer");
    assert_eq!(lone_need(65535, MIB - MIN_RESERVE), MIB - 12);
    assert_eq!(lone_need(65535, MIB - MIN_RESERVE + 1), MIB + 4);
    let g = BlockGeometry::new(MIB, MIN_RESERVE).unwrap();
    assert_eq!((g.block_reserve(), g.max_object()), (MIN_RESERVE, 917_281));
    for reserve in 0..MIN_RESERVE + 200 {
        let got = BlockGeometry::new(MIB, reserve);
        if reserve >= MIN_RESERVE {
            assert_eq!(got.unwrap().max_object(), MIB - reserve, "{reserve}");
        } else {
            assert_eq!(got, Err(refused.clone()), "{reserve}");
        }
    }
    assert_eq!(BlockGeometry::new(MIB, MIB - 1).unwrap().max_object(), 1);
    // A 4 KiB block cannot hold a header with any reserve below it.
    assert_eq!(BlockGeometry::new(4096, 4095), Err(refused));
}

#[test]
fn an_object_is_a_chain_exactly_when_it_is_larger_than_max_object() {
    for g in [BlockGeometry::DEFAULT, min_geometry(), mid_geometry()] {
        let m = g.max_object();
        let chains = [0, m - 1, m, m + 1, u64::MAX].map(|n| g.is_chain(n));
        assert_eq!(chains, [false, false, false, true, true]);
    }
}

#[test]
fn the_default_geometry_fits_max_object_of_every_kind_with_any_key_and_begin_accepts_it() {
    let g = BlockGeometry::DEFAULT;
    for kind in [BlockKind::Segment, BlockKind::Pack, BlockKind::ChainSlice] {
        for key_len in [0usize, 65535] {
            let mut b = BlockBuilder::new(g, kind).unwrap();
            assert!(b.fits(key_len as u16, g.max_object()));
            let rec = chain_if(kind, Rec::plain(&vec![b'k'; key_len], &[]));
            let mut start = rec.start();
            start.data_len = g.max_object();
            assert_eq!(b.begin(&start).unwrap().len(), 64 + key_len);
            assert_eq!((b.len(), b.records()), (64 + key_len as u64, 1));
        }
    }
}

#[test]
fn a_whole_block_of_max_object_bytes_with_the_longest_key_is_the_smallest_geometry_s_limit() {
    let g = min_geometry();
    let data: Vec<u8> = (0..g.max_object()).map(|i| (i % 251) as u8).collect();
    for kind in [BlockKind::Segment, BlockKind::Pack, BlockKind::ChainSlice] {
        for key_len in [0usize, 65535] {
            let rec = chain_if(kind, Rec::plain(&vec![b'k'; key_len], &data));
            let mut b = BlockBuilder::new(g, kind).unwrap();
            let mut bytes = b.begin(&rec.start()).unwrap();
            for chunk in data.chunks(1 << 16) {
                b.data(chunk).unwrap();
                bytes.extend_from_slice(chunk);
            }
            bytes.extend(b.end().unwrap());
            let sealed = b.finish().unwrap();
            check_block(g, kind, &bytes, &sealed);
            let total = bytes.len() as u64 + sealed.bytes.len() as u64;
            assert_eq!(total, lone_need(key_len as u64, g.max_object()));
            assert_eq!(total == MIB - 12, key_len == 65535);
        }
    }
}

#[test]
fn one_byte_more_than_max_object_is_a_chain_for_every_key_and_kind_and_nothing_is_added() {
    for g in [min_geometry(), mid_geometry(), BlockGeometry::DEFAULT] {
        let m = g.max_object();
        for kind in [BlockKind::Segment, BlockKind::Pack, BlockKind::ChainSlice] {
            let mut b = BlockBuilder::new(g, kind).unwrap();
            for key_len in [0usize, 1, 65535] {
                assert!(b.fits(key_len as u16, m), "{kind:?} key {key_len}");
                assert!(!b.fits(key_len as u16, m + 1), "{kind:?} key {key_len}");
                let key = vec![b'k'; key_len];
                let rec = chain_if(kind, Rec::plain(&key, &[]));
                let mut start = rec.start();
                start.data_len = m + 1;
                assert_eq!(b.begin(&start), Err(chain_only()));
                assert!(g.is_chain(m + 1) && !g.is_chain(m));
            }
            // Refused before anything is counted: the builder stays usable.
            assert_eq!((b.len(), b.records(), b.is_empty()), (0, 0, true));
            b.append(&chain_if(kind, Rec::plain(b"s", b"x")).input())
                .unwrap();
            assert_eq!(b.records(), 1);
        }
    }
}

#[test]
fn a_record_of_max_object_after_another_is_full_with_the_exact_need_and_limit() {
    let g = min_geometry();
    let m = g.max_object();
    let mut b = nb_in(g, BlockKind::Segment);
    b.append(&Rec::plain(b"s", b"x").input()).unwrap();
    assert!(!b.fits(65535, m));
    let key = vec![b'k'; 65535];
    let rec = Rec::plain(&key, &[]);
    let mut start = rec.start();
    start.data_len = m;
    let full = BlockError::Full {
        need: need_after_one(65535, m),
        limit: MIB,
    };
    assert!(need_after_one(65535, m) > MIB);
    assert_eq!(b.begin(&start), Err(full));
    // Full adds nothing: the block seals with its one record.
    assert_eq!(b.records(), 1);
    assert_eq!(b.finish().unwrap().trailer.entries.len(), 1);
}

#[test]
fn segment_blocks_written_by_append_are_read_back_at_every_cut_with_the_same_trailer() {
    write_read_round_trip(BlockKind::Segment, 6, 30, 600);
}

#[test]
fn pack_blocks_are_4_kib_aligned_with_relocate_only_in_the_trailer() {
    write_read_round_trip(BlockKind::Pack, 4, 7, 9000);
    // A record whose header, key and data are exactly 4096 bytes leaves no gap.
    let recs = [
        Rec::plain(b"", &[3; 4032]),
        Rec::plain(b"x", &[4; 5]),
        Rec::plain(b"y", &[5; 4096 - 66]),
        Rec::plain(b"z", &[]),
    ];
    let (bytes, ends, b) = build_append(BlockGeometry::DEFAULT, BlockKind::Pack, &recs);
    assert_eq!(ends[0], 4096);
    let sealed = b.finish().unwrap();
    check_block(BlockGeometry::DEFAULT, BlockKind::Pack, &bytes, &sealed);
    let entry_flags = sealed.trailer.entries.iter().map(|e| e.flags);
    assert!(entry_flags.into_iter().all(|f| f == RecordFlags::RELOCATE));
    assert!(
        walk(&bytes, 4096)
            .iter()
            .all(|w| w.header.flags == RecordFlags::default())
    );
}

#[test]
fn chain_slice_blocks_hold_one_slice_at_offset_zero_with_its_flags_kept() {
    write_read_round_trip(BlockKind::ChainSlice, 6, 1, 20_000);
    let rec = Rec::slice(b"big", &[9; 777], 5 * MIB);
    let (bytes, _, b) = build_append(BlockGeometry::DEFAULT, BlockKind::ChainSlice, &[rec]);
    let sealed = b.finish().unwrap();
    assert_eq!(sealed.trailer.entries[0].flags, RecordFlags::CHAIN_SLICE);
    assert_eq!(walk(&bytes, 4096)[0].header.object_offset, 5 * MIB);
}

#[test]
fn a_block_of_several_cells_has_one_crc_per_mib_however_it_is_cut() {
    let g = mid_geometry();
    let mut rng = Rng(99);
    for kind in [BlockKind::Segment, BlockKind::Pack] {
        let recs = vec![
            Rec::plain(b"one", &rng.bytes(1_200_000)),
            Rec::plain(b"", &rng.bytes(70)),
            Rec::plain(b"two", &rng.bytes(1_100_000)),
        ];
        let (bytes, _, b) = build_append(g, kind, &recs);
        let sealed = b.finish().unwrap();
        check_block(g, kind, &bytes, &sealed);
        assert!(sealed.trailer.cell_crcs.len() >= 2);
        for sizes in [
            vec![1 << 20],
            vec![(1 << 20) - 1, 3],
            vec![77_777],
            vec![1 << 19, 13],
        ] {
            assert_eq!(
                feed_cycle(g, kind, &bytes, &sizes).finish().unwrap(),
                sealed
            );
        }
        assert_eq!(build_streamed(&mut rng, g, kind, &recs), bytes);
    }
}

#[test]
fn feeding_a_prefix_that_ends_at_a_record_boundary_then_appending_equals_appending_all() {
    for kind in [BlockKind::Segment, BlockKind::Pack] {
        let mut rng = Rng(5);
        let recs = random_recs(&mut rng, kind, 9, 5000);
        let (bytes, ends, whole) = build_append(BlockGeometry::DEFAULT, kind, &recs);
        let want = whole.finish().unwrap();
        for k in 0..recs.len() {
            let prefix = if k == 0 { 0 } else { ends[k - 1] };
            let mut b = feed_cycle(BlockGeometry::DEFAULT, kind, &bytes[..prefix], &[977]);
            assert_eq!(b.records(), k);
            let mut out = bytes[..prefix].to_vec();
            for rec in &recs[k..] {
                out.extend(b.append(&rec.input()).unwrap());
            }
            assert_eq!(out, bytes, "{kind:?} resume at record {k}");
            assert_eq!(b.finish().unwrap(), want);
        }
    }
}

#[test]
fn the_same_records_in_another_order_make_another_block_and_each_trailer_follows_its_bytes() {
    let g = BlockGeometry::DEFAULT;
    let recs = random_recs(&mut Rng(31), BlockKind::Segment, 12, 300);
    let mut reversed = recs.clone();
    reversed.reverse();
    let (a, _, ba) = build_append(g, BlockKind::Segment, &recs);
    let (r, _, br) = build_append(g, BlockKind::Segment, &reversed);
    assert_ne!(a, r);
    let (sa, sr) = (ba.finish().unwrap(), br.finish().unwrap());
    assert_ne!(sa.bytes, sr.bytes);
    check_block(g, BlockKind::Segment, &a, &sa);
    check_block(g, BlockKind::Segment, &r, &sr);
    // Two builders on the same bytes give byte-identical trailers, and finish is repeatable.
    for bytes in [&a, &r] {
        let one = feed_cycle(g, BlockKind::Segment, bytes, &[11]);
        let two = feed_cycle(g, BlockKind::Segment, bytes, &[4096]);
        assert_eq!(one.finish().unwrap().bytes, two.finish().unwrap().bytes);
        assert_eq!(one.finish().unwrap(), one.finish().unwrap());
    }
}

/// Fills a 1 MiB-block builder with records of 8-byte keys until `begin` says Full.
fn fill_until_full(kind: BlockKind, data_len: usize) {
    let g = min_geometry();
    let mut b = BlockBuilder::new(g, kind).unwrap();
    let (mut recs, mut bytes) = (Vec::new(), Vec::new());
    for i in 0u64.. {
        let rec = Rec {
            stamp: i,
            ..Rec::plain(&i.to_le_bytes(), &vec![(i % 251) as u8; data_len])
        };
        let fits = b.fits(8, data_len as u64);
        let Err(err) = b.append(&rec.input()).map(|out| bytes.extend(out)) else {
            assert!(fits);
            recs.push(rec);
            continue;
        };
        assert!(!fits);
        let BlockError::Full { need, limit } = err else {
            panic!("{err}")
        };
        assert_eq!((limit, need > MIB), (MIB, true));
        // The accepted block and its trailer are within the limit.
        let sealed = b.finish().unwrap();
        check_block(g, kind, &bytes, &sealed);
        // The refused record would have made exactly `need` bytes: build it in a block with room.
        recs.push(rec);
        let (big, _, bb) = build_append(mid_geometry(), kind, &recs);
        assert_eq!(
            big.len() as u64 + bb.finish().unwrap().bytes.len() as u64,
            need
        );
        assert_eq!(b.records(), recs.len() - 1);
        return;
    }
}

#[test]
fn a_segment_of_small_records_seals_when_the_record_and_its_trailer_pass_block_bytes() {
    fill_until_full(BlockKind::Segment, 100);
    fill_until_full(BlockKind::Segment, 0);
}

#[test]
fn a_pack_block_of_4_kib_records_seals_when_the_record_and_its_trailer_pass_block_bytes() {
    fill_until_full(BlockKind::Pack, 4000);
}

#[test]
fn an_empty_builder_of_each_kind_finishes_to_an_empty_trailer_of_its_kind() {
    let empty = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    for kind in [BlockKind::Segment, BlockKind::Pack, BlockKind::ChainSlice] {
        let mut b = nb(kind);
        assert_eq!((b.kind(), b.len(), b.records()), (kind, 0, 0));
        assert!(b.is_empty());
        b.feed(&[]).unwrap();
        let sealed = b.finish().unwrap();
        let t = &sealed.trailer;
        assert_eq!((t.kind, t.entries.len(), t.cell_crcs.len()), (kind, 0, 0));
        assert_eq!(t.data_len, 0);
        let hex: String = t.blake3.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, empty);
        assert_eq!(sealed.bytes.len() as u64, trailer_len(0, 0));
        assert_eq!(sealed.bytes.len(), 96);
        assert_eq!(Trailer::decode(&sealed.bytes).unwrap(), sealed.trailer);
    }
}

#[test]
fn trailer_entry_len_and_trailer_len_equal_the_encoded_sizes() {
    for key_len in [0usize, 1, 8, 9, 17, 65535] {
        let entry = TrailerEntry {
            key: vec![b'k'; key_len],
            stamp: Stamp::from_raw(1),
            offset: 0,
            length: 10,
            generation: Generation([1; 16]),
            crc32c: 5,
            flags: RecordFlags::default(),
        };
        let entry_len = trailer_entry_len(key_len as u16);
        assert_eq!(entry_len, (48 + key_len as u64).div_ceil(8) * 8);
        for data_len in [0u64, 1, MIB, MIB + 1] {
            let cell_crcs = vec![0; data_len.div_ceil(MIB) as usize];
            let kind = BlockKind::Segment;
            let t = Trailer {
                kind,
                entries: vec![entry.clone()],
                cell_crcs,
                data_len,
                blake3: [0; 32],
            };
            let encoded = t.encode().unwrap().len() as u64;
            assert_eq!(trailer_len(entry_len, data_len), encoded);
        }
    }
    assert_eq!(record_len(0, 0), 64);
}

#[test]
fn a_record_that_brings_the_block_to_exactly_block_bytes_fits_and_one_more_byte_is_full() {
    // 2 MiB blocks. Worked out by hand: a record of an 8-byte key and 1 MiB - 72 data bytes takes
    // 64 + 8 + (1 MiB - 72) = 1 MiB; then a record of an empty key and 1 MiB - 272 data bytes
    // takes 1 MiB - 208 and ends the records at 2 MiB - 208. The trailer is two entries (48 + 8
    // and 48 bytes), two cell crcs (8 bytes) and the 96-byte footer: 208 bytes. So the block is
    // exactly 2 MiB.
    let g = BlockGeometry::new(2 * MIB, MIB).unwrap();
    let first = vec![1u8; (MIB - 72) as usize];
    let second = vec![2u8; (MIB - 272) as usize];
    let mut b = nb_in(g, BlockKind::Segment);
    b.append(&Rec::plain(b"eightkey", &first).input()).unwrap();
    assert_eq!(b.len(), MIB);
    let at_limit = b.clone();
    assert!(b.fits(0, MIB - 272));
    b.append(&Rec::plain(b"", &second).input()).unwrap();
    let sealed = b.finish().unwrap();
    assert_eq!(b.len(), 2 * MIB - 208);
    assert_eq!(sealed.bytes.len(), 208);
    assert_eq!(b.len() + sealed.bytes.len() as u64, g.block_bytes());
    // One data byte more pads the record by 16 bytes: the block would be 2 MiB + 16.
    let mut b = at_limit;
    assert!(!b.fits(0, MIB - 271));
    let longer = Rec::plain(b"", &second);
    let mut start = longer.start();
    start.data_len = MIB - 271;
    assert_eq!(
        b.begin(&start),
        Err(BlockError::Full {
            need: 2 * MIB + 16,
            limit: 2 * MIB,
        })
    );
}
