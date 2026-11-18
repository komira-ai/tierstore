//! Chains (10.2): an object larger than one block, stored as consecutive blocks, and the
//! geometry that says when an object is one.
//!
//! Catches:
//! - a geometry that accepts a reserve too small for the longest key, or a block size that is not a
//!   positive multiple of 4 KiB, or a reserve not below the block size (each with its exact error);
//! - an object of exactly `block_bytes - block_reserve` that is called a chain, or one byte more that
//!   is not;
//! - a chain made for an object that is not one, or whose last block id would leave the node's
//!   56-bit counter space (checked at the last counter that fits and the first that does not, for
//!   two and for three blocks);
//! - a wrong block count (`max_object + 1`, `2 * max_object`, `2 * max_object + 1`, 2.5 GiB at the
//!   default geometry), slices that are not consecutive block ids of one node, that do not start at
//!   `index * max_object`, that are not `max_object` long but for the last, or that do not tile the
//!   object exactly;
//! - a range read that touches a block it does not cover, misses one it covers, reports the wrong
//!   offset inside a slice, takes an empty range for a read, or accepts a range past the end
//!   (including an `offset + len` that wraps around `u64`);
//! - a chain slice block that is not exactly one record at offset 0 with the chain flag, the
//!   whole-object generation and the slice's object offset, or whose data does not concatenate
//!   back to the object.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use tierstore_format::FormatError;
use tierstore_format::block::{
    BlockBuilder, BlockError, BlockGeometry, Chain, DEFAULT_BLOCK_BYTES, DEFAULT_BLOCK_RESERVE,
    Slice,
};
use tierstore_format::digest::blake3_256;
use tierstore_format::ids::{BlockId, COUNTER_MAX, Generation, NodeId};
use tierstore_format::record::{RecordFlags, RecordIn, decode_record};
use tierstore_format::stamp::Stamp;
use tierstore_format::trailer::BlockKind;

const GIB: u64 = 1 << 30;
const NODE: NodeId = NodeId(7);

/// Slices of 3840 bytes: the small geometry the streaming tests use.
fn small() -> BlockGeometry {
    BlockGeometry::new(135_168, 131_328).unwrap()
}

fn block_err(reason: &'static str) -> FormatError {
    FormatError::Invalid {
        what: "block",
        reason,
    }
}

fn id(counter: u64) -> BlockId {
    BlockId::new(NODE, counter).unwrap()
}

fn chain_of(geometry: BlockGeometry, counter: u64, size: u64) -> Chain {
    geometry.chain(id(counter), size).unwrap()
}

/// SplitMix64, seeded, so a failing sweep reproduces.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ---- geometry ----

#[test]
fn the_default_geometry_is_one_gib_blocks_with_a_one_mib_reserve() {
    let g = BlockGeometry::DEFAULT;
    assert_eq!(BlockGeometry::default(), g);
    assert_eq!(DEFAULT_BLOCK_BYTES, GIB);
    assert_eq!(DEFAULT_BLOCK_RESERVE, 1 << 20);
    assert_eq!(g.block_bytes(), GIB);
    assert_eq!(g.block_reserve(), 1 << 20);
    assert_eq!(g.max_object(), GIB - (1 << 20));
}

#[test]
fn a_geometry_needs_a_positive_multiple_of_4_kib_and_a_reserve_below_it() {
    let bytes = block_err("block_bytes is not a positive multiple of 4 KiB");
    assert_eq!(BlockGeometry::new(0, 0), Err(bytes.clone()));
    assert_eq!(BlockGeometry::new(135_168 + 1, 131_328), Err(bytes.clone()));
    assert_eq!(BlockGeometry::new(2048, 0), Err(bytes));
    let reserve = block_err("block_reserve is not below block_bytes");
    assert_eq!(BlockGeometry::new(135_168, 135_168), Err(reserve.clone()));
    assert_eq!(BlockGeometry::new(135_168, 1 << 40), Err(reserve));
}

#[test]
fn the_smallest_reserve_is_the_one_where_the_longest_key_still_fits_alone() {
    // A record of a 65535-byte key and `max_object` data bytes, and its one-entry trailer, must
    // fit `block_bytes`: 131295 is the least reserve at 135168, 131294 leaves 4 bytes too few.
    let ok = BlockGeometry::new(135_168, 131_295).unwrap();
    assert_eq!(ok.max_object(), 3873);
    let small_reserve =
        block_err("block_reserve cannot hold a header, its padding and a one-entry trailer");
    assert_eq!(
        BlockGeometry::new(135_168, 131_294),
        Err(small_reserve.clone())
    );
    assert_eq!(BlockGeometry::new(135_168, 0), Err(small_reserve.clone()));
    assert_eq!(BlockGeometry::new(4096, 1), Err(small_reserve));
    // A reserve of one byte below the block size: max_object is one byte, which fits.
    assert_eq!(
        BlockGeometry::new(1 << 20, (1 << 20) - 1)
            .unwrap()
            .max_object(),
        1
    );
}

#[test]
fn an_object_of_exactly_max_object_fits_one_block_and_one_byte_more_is_a_chain() {
    for g in [BlockGeometry::DEFAULT, small()] {
        let max = g.max_object();
        assert!(!g.is_chain(0));
        assert!(!g.is_chain(1));
        assert!(!g.is_chain(max - 1));
        assert!(!g.is_chain(max));
        assert!(g.is_chain(max + 1));
        assert!(g.is_chain(u64::MAX));
        // The longest key still fits a block with the largest object, and a block's worth of
        // data does not.
        let b = BlockBuilder::new(g, BlockKind::ChainSlice).unwrap();
        assert!(b.fits(u16::MAX, max));
        assert!(!b.fits(u16::MAX, g.block_bytes()));
    }
}

// ---- Chain::chain ----

#[test]
fn a_chain_of_an_object_that_is_not_one_is_refused() {
    let want = block_err("an object of at most max_object bytes is not a chain");
    for g in [BlockGeometry::DEFAULT, small()] {
        for size in [0, 1, g.max_object() - 1, g.max_object()] {
            assert_eq!(g.chain(id(5), size), Err(want.clone()), "size {size}");
        }
    }
}

#[test]
fn a_chain_remembers_its_first_block_size_and_slice_length() {
    let g = small();
    let c = chain_of(g, 41, 10_000);
    assert_eq!(c.first(), id(41));
    assert_eq!(c.size(), 10_000);
    assert_eq!(c.slice_len(), 3840);
    assert_eq!(c.slice_len(), g.max_object());
    assert_eq!(c.blocks(), 3);
}

#[test]
fn a_two_block_chain_fits_up_to_the_second_to_last_counter() {
    let g = small();
    let size = g.max_object() + 1;
    let c = g.chain(id(COUNTER_MAX - 1), size).unwrap();
    assert_eq!(c.blocks(), 2);
    assert_eq!(c.slice(1).unwrap().block, id(COUNTER_MAX));
    assert_eq!(
        g.chain(id(COUNTER_MAX), size),
        Err(block_err(
            "the chain's blocks pass the node's last block id"
        ))
    );
    // Far below the limit it is fine, and the node does not matter.
    assert!(g.chain(id(0), size).is_ok());
    assert!(
        g.chain(BlockId::new(NodeId(255), COUNTER_MAX - 1).unwrap(), size)
            .is_ok()
    );
}

#[test]
fn a_three_block_chain_needs_two_counters_after_the_first() {
    let g = small();
    let size = 2 * g.max_object() + 1;
    let c = g.chain(id(COUNTER_MAX - 2), size).unwrap();
    assert_eq!(c.blocks(), 3);
    assert_eq!(c.slice(2).unwrap().block, id(COUNTER_MAX));
    let pass = block_err("the chain's blocks pass the node's last block id");
    assert_eq!(g.chain(id(COUNTER_MAX - 1), size), Err(pass.clone()));
    assert_eq!(g.chain(id(COUNTER_MAX), size), Err(pass));
}

#[test]
fn a_chain_is_refused_exactly_when_its_last_block_would_pass_the_node_s_counters() {
    // The default geometry with the largest object: about 2^34 blocks, which still fit counter 0.
    let g = BlockGeometry::DEFAULT;
    let c = g.chain(id(0), u64::MAX).unwrap();
    assert_eq!(c.blocks(), u64::MAX.div_ceil(g.max_object()));
    // One byte per block would pass 2^56 counters: a geometry of 1-byte slices.
    let tiny = BlockGeometry::new(1 << 20, (1 << 20) - 1).unwrap();
    assert_eq!(tiny.max_object(), 1);
    let blocks = tiny.chain(id(0), COUNTER_MAX).unwrap().blocks();
    assert_eq!(blocks, COUNTER_MAX);
    assert_eq!(
        tiny.chain(id(2), COUNTER_MAX),
        Err(block_err(
            "the chain's blocks pass the node's last block id"
        ))
    );
    assert!(tiny.chain(id(1), COUNTER_MAX).is_ok());
    // More blocks than there are counters (2^64 - 1 one-byte slices): refused from any first
    // block, counter 0 included, never wrapped into an accepted chain.
    let past = block_err("the chain's blocks pass the node's last block id");
    for first in [0, 1, COUNTER_MAX] {
        assert_eq!(tiny.chain(id(first), u64::MAX), Err(past.clone()));
    }
    // The default geometry near the last counter: the largest object's chain fits exactly when
    // its last block is counter COUNTER_MAX.
    let last_first = COUNTER_MAX - (c.blocks() - 1);
    assert!(g.chain(id(last_first), u64::MAX).is_ok());
    assert_eq!(g.chain(id(last_first + 1), u64::MAX), Err(past.clone()));
    assert_eq!(g.chain(id(COUNTER_MAX), u64::MAX), Err(past));
}

#[test]
fn a_range_at_the_end_of_an_object_of_u64_max_bytes_is_covered_without_overflow() {
    // The slice grid passes u64::MAX in the last slice: `covering` must clip to the object.
    let g = BlockGeometry::DEFAULT;
    let c = g.chain(id(0), u64::MAX).unwrap();
    let last = c.blocks() - 1;
    let start = last * g.max_object();
    let got = c.covering(u64::MAX - 5, 5).unwrap();
    assert_eq!(
        got,
        vec![Slice {
            index: last,
            block: id(last),
            object_offset: u64::MAX - 5,
            slice_offset: u64::MAX - 5 - start,
            len: 5,
        }]
    );
    let whole_last = c.slice(last).unwrap();
    assert_eq!(whole_last.len, u64::MAX - start);
    assert_eq!(
        c.covering(start, u64::MAX - start).unwrap(),
        vec![whole_last]
    );
}

// ---- blocks and slices ----

#[test]
fn the_block_count_rounds_the_size_up_to_whole_slices() {
    let g = BlockGeometry::DEFAULT;
    let max = g.max_object();
    assert_eq!(chain_of(g, 0, max + 1).blocks(), 2);
    assert_eq!(chain_of(g, 0, 2 * max).blocks(), 2);
    assert_eq!(chain_of(g, 0, 2 * max + 1).blocks(), 3);
    assert_eq!(chain_of(g, 0, 3 * max).blocks(), 3);
    assert_eq!(chain_of(g, 0, 3 * max + 1).blocks(), 4);
}

#[test]
fn an_object_of_two_and_a_half_gib_is_three_blocks() {
    let g = BlockGeometry::DEFAULT;
    let size = 5 * GIB / 2;
    let c = chain_of(g, 100, size);
    assert_eq!(c.blocks(), 3);
    let max = g.max_object();
    let slices: Vec<Slice> = (0..3).map(|i| c.slice(i).unwrap()).collect();
    assert_eq!(slices[0].len, max);
    assert_eq!(slices[1].len, max);
    assert_eq!(slices[2].len, size - 2 * max);
    assert_eq!(slices[2].len, GIB / 2 + 2 * (1 << 20));
    assert_eq!(slices[2].object_offset, 2 * max);
    assert_eq!(slices[2].block, id(102));
    assert_eq!(c.slice(3), None);
}

#[test]
fn slices_are_consecutive_blocks_of_one_node_tiling_the_object() {
    let g = small();
    let max = g.max_object();
    for size in [
        max + 1,
        2 * max,
        2 * max + 1,
        5 * max,
        5 * max + 17,
        7 * max - 1,
    ] {
        let c = chain_of(g, 1000, size);
        let n = c.blocks();
        assert_eq!(n, size.div_ceil(max));
        let mut total = 0;
        for i in 0..n {
            let s = c.slice(i).unwrap();
            assert_eq!(s.index, i);
            assert_eq!(s.block, id(1000 + i));
            assert_eq!(s.block.node(), NODE);
            assert_eq!(s.block.counter(), 1000 + i);
            assert_eq!(s.object_offset, i * max);
            assert_eq!(s.object_offset, total);
            assert_eq!(s.slice_offset, 0);
            let want = if i + 1 == n {
                size - (n - 1) * max
            } else {
                max
            };
            assert_eq!(s.len, want, "size {size} slice {i}");
            total += s.len;
        }
        assert_eq!(total, size);
        assert_eq!(c.slice(n), None);
        assert_eq!(c.slice(n + 1), None);
        assert_eq!(c.slice(u64::MAX), None);
    }
}

#[test]
fn a_last_slice_of_exactly_one_slice_length_is_whole() {
    let g = small();
    let c = chain_of(g, 3, 2 * g.max_object());
    assert_eq!(c.slice(1).unwrap().len, g.max_object());
    let c = chain_of(g, 3, 2 * g.max_object() + 1);
    assert_eq!(c.slice(2).unwrap().len, 1);
}

// ---- covering ----

#[test]
fn an_empty_range_covers_no_block_even_at_the_end() {
    let g = small();
    let c = chain_of(g, 9, 3 * g.max_object() + 5);
    assert_eq!(c.covering(0, 0), Ok(vec![]));
    assert_eq!(c.covering(g.max_object(), 0), Ok(vec![]));
    assert_eq!(c.covering(c.size(), 0), Ok(vec![]));
}

#[test]
fn the_whole_object_covers_every_slice_whole() {
    let g = small();
    let c = chain_of(g, 9, 3 * g.max_object() + 5);
    let all = c.covering(0, c.size()).unwrap();
    assert_eq!(all.len(), 4);
    for (i, s) in all.iter().enumerate() {
        assert_eq!(Some(*s), c.slice(i as u64));
    }
}

#[test]
fn a_range_inside_one_slice_touches_one_block_at_the_right_offset() {
    let g = small();
    let max = g.max_object();
    let c = chain_of(g, 9, 3 * max + 5);
    let got = c.covering(max + 100, 50).unwrap();
    assert_eq!(
        got,
        vec![Slice {
            index: 1,
            block: id(10),
            object_offset: max + 100,
            slice_offset: 100,
            len: 50,
        }]
    );
    // The first byte, the last byte of the first slice, and the short last slice.
    assert_eq!(c.covering(0, 1).unwrap().len(), 1);
    assert_eq!(c.covering(max - 1, 1).unwrap()[0].slice_offset, max - 1);
    let last = c.covering(3 * max + 2, 3).unwrap();
    assert_eq!(
        last,
        vec![Slice {
            index: 3,
            block: id(12),
            object_offset: 3 * max + 2,
            slice_offset: 2,
            len: 3,
        }]
    );
}

#[test]
fn a_range_across_one_boundary_touches_exactly_two_blocks() {
    let g = small();
    let max = g.max_object();
    let c = chain_of(g, 9, 3 * max + 5);
    let got = c.covering(max - 10, 30).unwrap();
    assert_eq!(
        got,
        vec![
            Slice {
                index: 0,
                block: id(9),
                object_offset: max - 10,
                slice_offset: max - 10,
                len: 10,
            },
            Slice {
                index: 1,
                block: id(10),
                object_offset: max,
                slice_offset: 0,
                len: 20,
            },
        ]
    );
    // A range over a whole middle slice and a byte on each side touches three.
    let three = c.covering(max - 1, max + 2).unwrap();
    assert_eq!(three.iter().map(|s| s.index).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(three.iter().map(|s| s.len).collect::<Vec<_>>(), [1, max, 1]);
}

#[test]
fn a_range_ending_exactly_at_a_boundary_does_not_touch_the_next_block() {
    let g = small();
    let max = g.max_object();
    let c = chain_of(g, 9, 3 * max + 5);
    let got = c.covering(max - 7, 7).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].index, 0);
    assert_eq!(got[0].len, 7);
    let got = c.covering(0, 2 * max).unwrap();
    assert_eq!(got.iter().map(|s| s.index).collect::<Vec<_>>(), [0, 1]);
    // And one starting exactly at a boundary does not touch the previous block.
    let got = c.covering(max, 7).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].index, 1);
    assert_eq!(got[0].slice_offset, 0);
}

#[test]
fn a_range_past_the_end_is_an_error_and_so_is_one_that_wraps() {
    let g = small();
    let c = chain_of(g, 9, 3 * g.max_object() + 5);
    let size = c.size();
    let past = block_err("a range past the end of the chain");
    assert_eq!(c.covering(0, size + 1), Err(past.clone()));
    assert_eq!(c.covering(size - 1, 2), Err(past.clone()));
    assert_eq!(c.covering(size, 1), Err(past.clone()));
    assert_eq!(c.covering(size + 1, 0), Err(past.clone()));
    assert_eq!(c.covering(u64::MAX, 1), Err(past.clone()));
    assert_eq!(c.covering(1, u64::MAX), Err(past.clone()));
    assert_eq!(c.covering(u64::MAX, u64::MAX), Err(past));
    // The last byte is fine.
    assert_eq!(c.covering(size - 1, 1).unwrap()[0].len, 1);
}

#[test]
fn random_ranges_cover_exactly_the_slices_that_hold_their_bytes() {
    let g = small();
    let size = 5 * g.max_object() + 1234;
    let c = chain_of(g, 77, size);
    // Which slice holds each byte, from the slices themselves.
    let mut owner = vec![0u64; size as usize];
    for i in 0..c.blocks() {
        let s = c.slice(i).unwrap();
        for p in s.object_offset..s.object_offset + s.len {
            owner[p as usize] = i;
        }
    }
    let mut rng = SplitMix64(0x5eed_c0de_0001);
    for round in 0..600 {
        let offset = rng.below(size + 1);
        // Mix short ranges (inside a slice) and long ones (across several).
        let cap = if round % 2 == 0 { 64 } else { size - offset };
        let len = rng.below(cap.min(size - offset) + 1);
        let got = c.covering(offset, len).unwrap();

        let mut want: Vec<Slice> = Vec::new();
        for p in offset..offset + len {
            let i = owner[p as usize];
            let base = c.slice(i).unwrap().object_offset;
            match want.last_mut() {
                Some(last) if last.index == i => last.len += 1,
                _ => want.push(Slice {
                    index: i,
                    block: id(77 + i),
                    object_offset: p,
                    slice_offset: p - base,
                    len: 1,
                }),
            }
        }
        assert_eq!(got, want, "range {offset}+{len}");
        assert_eq!(got.iter().map(|s| s.len).sum::<u64>(), len);
    }
}

// ---- chain slices built as blocks ----

fn object_bytes(len: u64) -> Vec<u8> {
    let mut rng = SplitMix64(0xfeed_0042);
    (0..len).map(|_| rng.next() as u8).collect()
}

#[test]
fn a_chain_splits_into_chain_slice_blocks_that_read_back_as_the_object() {
    let g = small();
    let max = g.max_object();
    let object = object_bytes(2 * max + 500);
    let generation = Generation::of(&object);
    let key = b"big/object".as_slice();
    let stamp = Stamp::from_raw(0x0123_4567);
    let c = chain_of(g, 20, object.len() as u64);
    assert_eq!(c.blocks(), 3);

    let mut rebuilt = Vec::new();
    for i in 0..c.blocks() {
        let s = c.slice(i).unwrap();
        let data = &object[s.object_offset as usize..(s.object_offset + s.len) as usize];
        let mut b = BlockBuilder::new(g, BlockKind::ChainSlice).unwrap();
        assert!(b.fits(key.len() as u16, s.len));
        let bytes = b
            .append(&RecordIn {
                flags: RecordFlags::CHAIN_SLICE,
                key,
                data,
                generation,
                stamp,
                object_offset: s.object_offset,
            })
            .unwrap();
        assert!(
            !b.fits(key.len() as u16, 1),
            "a slice block holds one record"
        );
        assert_eq!(b.records(), 1);
        assert_eq!(b.len(), bytes.len() as u64);

        let sealed = b.finish().unwrap();
        assert_eq!(sealed.trailer.kind, BlockKind::ChainSlice);
        assert_eq!(sealed.trailer.entries.len(), 1);
        let e = &sealed.trailer.entries[0];
        assert_eq!(e.key, key);
        assert_eq!(e.offset, 0);
        assert_eq!(e.length, s.len);
        assert_eq!(e.generation, generation);
        assert_eq!(e.stamp, stamp);
        assert!(e.flags.contains(RecordFlags::CHAIN_SLICE));
        assert!(!e.flags.contains(RecordFlags::RELOCATE));
        assert_eq!(sealed.trailer.data_len, bytes.len() as u64);
        assert_eq!(sealed.trailer.blake3, blake3_256(&bytes));
        // The whole block, trailer included, is within block_bytes.
        assert!(bytes.len() as u64 + sealed.bytes.len() as u64 <= g.block_bytes());

        let (rec, used) = decode_record(&bytes).unwrap();
        assert_eq!(used, bytes.len() as u64);
        assert_eq!(rec.header.flags, RecordFlags::CHAIN_SLICE);
        assert_eq!(rec.header.generation, generation);
        assert_eq!(rec.header.object_offset, s.object_offset);
        assert_eq!(rec.data.len() as u64, s.len);
        rebuilt.extend_from_slice(rec.data);
    }
    assert_eq!(rebuilt, object);
    assert_eq!(Generation::of(&rebuilt), generation);
}

#[test]
fn a_full_slice_with_the_longest_key_still_fits_its_block() {
    let g = small();
    let key = vec![b'k'; usize::from(u16::MAX)];
    let data = object_bytes(g.max_object());
    let mut b = BlockBuilder::new(g, BlockKind::ChainSlice).unwrap();
    assert!(b.fits(u16::MAX, g.max_object()));
    let bytes = b
        .append(&RecordIn {
            flags: RecordFlags::CHAIN_SLICE,
            key: &key,
            data: &data,
            generation: Generation::of(&data),
            stamp: Stamp::from_raw(1),
            object_offset: 0,
        })
        .unwrap();
    let sealed = b.finish().unwrap();
    assert!(bytes.len() as u64 + sealed.bytes.len() as u64 <= g.block_bytes());
}

#[test]
fn a_chain_slice_block_refuses_a_second_record_and_a_record_without_the_chain_flag() {
    let g = small();
    let data = [7u8; 100];
    let rec = |flags| RecordIn {
        flags,
        key: b"k",
        data: &data,
        generation: Generation::of(&data),
        stamp: Stamp::from_raw(1),
        object_offset: 0,
    };
    let mut b = BlockBuilder::new(g, BlockKind::ChainSlice).unwrap();
    assert_eq!(
        b.append(&rec(RecordFlags::default())),
        Err(BlockError::Format(block_err(
            "a chain slice record outside a chain slice block, or the reverse"
        )))
    );
    let mut b = BlockBuilder::new(g, BlockKind::ChainSlice).unwrap();
    b.append(&rec(RecordFlags::CHAIN_SLICE)).unwrap();
    assert_eq!(
        b.append(&rec(RecordFlags::CHAIN_SLICE)),
        Err(BlockError::Format(block_err(
            "a chain slice block holds one record"
        )))
    );
}

#[test]
fn a_slice_longer_than_max_object_by_a_block_does_not_fit_a_chain_slice_block() {
    let g = small();
    let b = BlockBuilder::new(g, BlockKind::ChainSlice).unwrap();
    // The record and its trailer need more than block_bytes once the data is a block long.
    assert!(!b.fits(1, g.block_bytes()));
    assert!(b.fits(1, g.max_object()));
}
