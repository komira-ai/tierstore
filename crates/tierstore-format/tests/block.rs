//! Block geometry and the block builder: the boundary of one block, writing against reading, the
//! trailer as a function of the block's bytes, and the errors of a misused builder.
//!
//! Catches:
//! - a geometry that accepts a zero or unaligned size, a reserve not below it, or one byte too small
//!   for the longest key, or that refuses the smallest reserve that fits;
//! - an object of `block_bytes - block_reserve` that does not fit one block alone (any key length), or
//!   one byte more that is not `Full` with the exact need and limit at the smallest reserve;
//! - a block that grows past `block_bytes` with its trailer, or a `fits` that disagrees with `begin`;
//! - `append` and `begin`/`data`/`end` bytes that differ, or a trailer that differs between writing a
//!   block, reading its bytes back at any cut, and resuming at a record boundary;
//! - a trailer whose entries, cell table, length or blake3 differ from a scan of the bytes, whose
//!   entries are unsorted, or whose pack entries lack (or pack headers carry) the relocate flag;
//! - a pack record off 4 KiB or a segment record off 16 bytes, or a nonzero gap or padding accepted;
//! - a flipped header, key or data byte, or a record that does not fit, accepted while reading;
//! - a call out of order, a wrong declared crc, a second chain-slice record, a long key or a flag of
//!   the wrong kind accepted, or a builder that goes on after bytes that are not a block.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use tierstore_format::FormatError;
use tierstore_format::block::{
    BlockBuilder, BlockError, BlockGeometry, DEFAULT_BLOCK_BYTES, DEFAULT_BLOCK_RESERVE,
    PACK_ALIGN, RecordStart, Sealed, TRAILER_ENTRY_FIXED_LEN, trailer_entry_len, trailer_len,
};
use tierstore_format::crc32c;
use tierstore_format::digest::blake3_256;
use tierstore_format::ids::Generation;
use tierstore_format::record::{
    RecordFlags, RecordHeader, RecordIn, decode_record, encode_record, record_len, scan,
};
use tierstore_format::stamp::Stamp;
use tierstore_format::trailer::{BlockKind, Trailer, TrailerEntry, cell_crcs};

const MIB: u64 = 1 << 20;
/// The smallest reserve of a 1 MiB block (worked out by hand in `the_smallest_reserve...`).
const MIN_RESERVE: u64 = 131_295;
const PENDING: &str = "a record starts while another is open or bytes are pending";
const NO_DATA: &str = "data beyond the declared length, or with no record started";
const NO_END: &str = "a record ends before its data, or with no record started";
const PADDING: &str = "a padding byte is not zero";
const INSIDE: &str = "the block ends inside a record or its padding";
const CHAIN_KIND: &str = "a chain slice record outside a chain slice block, or the reverse";
const OFFSET: &str = "an object offset on a record that is not a chain slice";
const ONE_SLICE: &str = "a chain slice block holds one record";
const LONG_KEY: &str = "key longer than 65535 bytes";
const FED_OPEN: &str = "bytes fed while a record started by begin is open";
const UNKNOWN_BIT: RecordFlags = RecordFlags(0x8000);

struct Rng(u64);

impl Rng {
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

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn nb(kind: BlockKind) -> BlockBuilder {
    BlockBuilder::new(BlockGeometry::DEFAULT, kind).unwrap()
}

fn min_geometry() -> BlockGeometry {
    BlockGeometry::new(MIB, MIN_RESERVE).unwrap()
}

/// An empty builder of `kind` in geometry `g`.
fn nb_in(g: BlockGeometry, kind: BlockKind) -> BlockBuilder {
    BlockBuilder::new(g, kind).unwrap()
}

fn mid_geometry() -> BlockGeometry {
    BlockGeometry::new(4 * MIB, MIB).unwrap()
}

fn inv(reason: &'static str) -> BlockError {
    BlockError::Format(FormatError::Invalid {
        what: "block",
        reason,
    })
}

fn earlier() -> BlockError {
    inv("an earlier error left the bytes counted not a block")
}

#[derive(Clone)]
struct Rec {
    flags: RecordFlags,
    key: Vec<u8>,
    data: Vec<u8>,
    generation: Generation,
    stamp: u64,
    object_offset: u64,
}

impl Rec {
    fn plain(key: &[u8], data: &[u8]) -> Self {
        Self {
            flags: RecordFlags::default(),
            key: key.to_vec(),
            data: data.to_vec(),
            generation: Generation([7; 16]),
            stamp: 1,
            object_offset: 0,
        }
    }

    fn slice(key: &[u8], data: &[u8], object_offset: u64) -> Self {
        Self {
            flags: RecordFlags::CHAIN_SLICE,
            object_offset,
            ..Self::plain(key, data)
        }
    }

    fn input(&self) -> RecordIn<'_> {
        RecordIn {
            flags: self.flags,
            key: &self.key,
            data: &self.data,
            generation: self.generation,
            stamp: Stamp::from_raw(self.stamp),
            object_offset: self.object_offset,
        }
    }

    fn start(&self) -> RecordStart<'_> {
        RecordStart {
            flags: self.flags,
            key: &self.key,
            data_len: self.data.len() as u64,
            data_crc: crc32c(&self.data),
            generation: self.generation,
            stamp: Stamp::from_raw(self.stamp),
            object_offset: self.object_offset,
        }
    }

    fn encoded(&self) -> Vec<u8> {
        encode_record(&self.input()).unwrap()
    }
}

fn random_recs(rng: &mut Rng, kind: BlockKind, n: usize, max_data: u64) -> Vec<Rec> {
    let flag_choices = [
        RecordFlags::default(),
        RecordFlags::RELOCATE,
        UNKNOWN_BIT,
        RecordFlags::RELOCATE.with(UNKNOWN_BIT),
    ];
    (0..n)
        .map(|_| {
            let key_len = match rng.below(8) {
                0 => 0,
                1 => 40 + rng.below(260) as usize,
                _ => 1 + rng.below(12) as usize,
            };
            let key: Vec<u8> = (0..key_len).map(|_| b'a' + rng.below(3) as u8).collect();
            let data_len = if rng.below(5) == 0 {
                0
            } else {
                rng.below(max_data + 1) as usize
            };
            let mut rec = Rec::plain(&key, &rng.bytes(data_len));
            rec.generation = Generation(rng.bytes(16).try_into().unwrap());
            rec.stamp = rng.below(6);
            rec.flags = flag_choices[rng.below(4) as usize];
            if kind == BlockKind::ChainSlice {
                rec.flags = rec.flags.with(RecordFlags::CHAIN_SLICE);
                rec.object_offset = rng.below(1 << 40);
            }
            rec
        })
        .collect()
}

/// Appends every record; the block's bytes and the length of the block after each record.
fn build_append(
    geom: BlockGeometry,
    kind: BlockKind,
    recs: &[Rec],
) -> (Vec<u8>, Vec<usize>, BlockBuilder) {
    let mut b = BlockBuilder::new(geom, kind).unwrap();
    let mut bytes = Vec::new();
    let mut ends = Vec::new();
    for rec in recs {
        bytes.extend(b.append(&rec.input()).unwrap());
        ends.push(bytes.len());
        assert_eq!(b.len(), bytes.len() as u64);
    }
    (bytes, ends, b)
}

/// The same records through begin, data in random chunks, end.
fn build_streamed(rng: &mut Rng, geom: BlockGeometry, kind: BlockKind, recs: &[Rec]) -> Vec<u8> {
    let mut b = BlockBuilder::new(geom, kind).unwrap();
    let mut bytes = Vec::new();
    for rec in recs {
        bytes.extend(b.begin(&rec.start()).unwrap());
        let mut rest = &rec.data[..];
        while !rest.is_empty() {
            let n = (rng.below(700) as usize).min(rest.len());
            b.data(&rest[..n]).unwrap();
            bytes.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
        }
        bytes.extend(b.end().unwrap());
    }
    let appended = build_append(geom, kind, recs).2.finish().unwrap();
    assert_eq!(b.finish().unwrap(), appended);
    bytes
}

fn feed_cycle(geom: BlockGeometry, kind: BlockKind, bytes: &[u8], sizes: &[usize]) -> BlockBuilder {
    let mut b = BlockBuilder::new(geom, kind).unwrap();
    let mut rest = bytes;
    let mut i = 0;
    while !rest.is_empty() {
        let n = sizes[i % sizes.len()].min(rest.len());
        b.feed(&rest[..n]).unwrap();
        rest = &rest[n..];
        i += 1;
    }
    assert_eq!(b.len(), bytes.len() as u64);
    b
}

struct Walked {
    offset: u64,
    header: RecordHeader,
    key: Vec<u8>,
}

/// Decodes the records of a block of `align`-aligned records, checking the gaps are zero.
fn walk(bytes: &[u8], align: u64) -> Vec<Walked> {
    let mut out = Vec::new();
    let mut pos = 0u64;
    while (pos as usize) < bytes.len() {
        let start = pos.next_multiple_of(align);
        assert!(bytes[pos as usize..start as usize].iter().all(|b| *b == 0));
        let (rec, len) = decode_record(&bytes[start as usize..]).unwrap();
        out.push(Walked {
            offset: start,
            header: rec.header,
            key: rec.key.to_vec(),
        });
        pos = start + len;
    }
    assert_eq!(pos, bytes.len() as u64);
    out
}

/// What the trailer of `bytes` must say, computed without the builder.
fn check_block(geom: BlockGeometry, kind: BlockKind, bytes: &[u8], sealed: &Sealed) {
    assert_eq!(Trailer::decode(&sealed.bytes).unwrap(), sealed.trailer);
    let t = &sealed.trailer;
    assert_eq!(t.kind, kind);
    assert_eq!(t.data_len, bytes.len() as u64);
    assert_eq!(t.blake3, blake3_256(bytes));
    assert_eq!(t.cell_crcs, cell_crcs(bytes));
    let align = if kind == BlockKind::Segment {
        16
    } else {
        PACK_ALIGN
    };
    let walked = walk(bytes, align);
    let mut expected: Vec<TrailerEntry> = walked
        .iter()
        .map(|w| TrailerEntry {
            key: w.key.clone(),
            stamp: w.header.stamp,
            offset: w.offset,
            length: w.header.data_len,
            generation: w.header.generation,
            crc32c: w.header.data_crc,
            flags: if kind == BlockKind::Pack {
                w.header.flags.with(RecordFlags::RELOCATE)
            } else {
                w.header.flags
            },
        })
        .collect();
    expected.sort_by(|a, b| (&a.key, a.stamp, a.offset).cmp(&(&b.key, b.stamp, b.offset)));
    assert_eq!(t.entries, expected);
    if kind == BlockKind::Segment {
        let s = scan(bytes);
        assert_eq!(s.stop, None);
        assert_eq!(s.valid_len, bytes.len() as u64);
        let offsets: Vec<u64> = s.records.iter().map(|(o, _)| *o).collect();
        assert_eq!(offsets, walked.iter().map(|w| w.offset).collect::<Vec<_>>());
    } else {
        assert!(walked.iter().all(|w| w.offset % 4096 == 0));
    }
    let entries_len: u64 = walked
        .iter()
        .map(|w| trailer_entry_len(w.key.len() as u16))
        .sum();
    let (len, trailer) = (bytes.len() as u64, sealed.bytes.len() as u64);
    assert_eq!(trailer, trailer_len(entries_len, len));
    assert!(len + trailer <= geom.block_bytes());
}

/// The bytes of a block holding only one record, and its one-entry trailer, by hand.
fn lone_need(key: u64, data: u64) -> u64 {
    let end = (64 + key + data).next_multiple_of(16);
    end + (48 + key).next_multiple_of(8) + 4 * end.div_ceil(MIB) + 96
}

const PIECES: [usize; 12] = [1, 2, 3, 5, 8, 13, 16, 17, 63, 64, 65, 4097];

/// Writing a block of random records, reading its bytes back at many cuts, and streaming the
/// records give one block.
fn write_read_round_trip(kind: BlockKind, seeds: u64, n: usize, max_data: u64) {
    let geom = BlockGeometry::DEFAULT;
    for seed in 0..seeds {
        let mut rng = Rng(seed * 7919 + n as u64);
        let recs = random_recs(&mut rng, kind, n, max_data);
        let (bytes, _, b) = build_append(geom, kind, &recs);
        let sealed = b.finish().unwrap();
        check_block(geom, kind, &bytes, &sealed);
        assert_eq!(b.records(), recs.len());
        assert_eq!(build_streamed(&mut rng, geom, kind, &recs), bytes);
        for size in PIECES.into_iter().chain([bytes.len().max(1)]) {
            let fed = feed_cycle(geom, kind, &bytes, &[size]);
            assert_eq!(fed.records(), recs.len());
            assert_eq!(fed.finish().unwrap(), sealed);
        }
        let cuts: Vec<usize> = (0..64).map(|_| 1 + rng.below(300) as usize).collect();
        let cut_fed = feed_cycle(geom, kind, &bytes, &cuts).finish().unwrap();
        assert_eq!(cut_fed, sealed);
    }
}

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

/// `rec` as a chain slice when the block is one.
fn chain_if(kind: BlockKind, mut rec: Rec) -> Rec {
    if kind == BlockKind::ChainSlice {
        rec.flags = RecordFlags::CHAIN_SLICE;
    }
    rec
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

/// The error of a record whose data is longer than `max_object`.
fn chain_only() -> BlockError {
    inv("an object larger than max_object is a chain, never one record")
}

/// The block length `begin` reports when a record of `key_len` and `data_len` follows one record
/// `s`/`x` in a segment of `g` (computed from the layout, record by record).
fn need_after_one(key_len: u16, data_len: u64) -> u64 {
    let start = record_len(1, 1);
    let end = start + record_len(key_len, data_len);
    end + trailer_len(trailer_entry_len(1) + trailer_entry_len(key_len), end)
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
fn a_builder_of_the_kind_other_is_refused() {
    for code in [0u8, 4, 255] {
        let err = BlockBuilder::new(BlockGeometry::DEFAULT, BlockKind::Other(code)).unwrap_err();
        assert_eq!(
            err,
            FormatError::Invalid {
                what: "block",
                reason: "a block kind this release does not write",
            }
        );
    }
}

#[test]
fn a_record_the_block_cannot_take_is_refused_by_begin_and_append_and_the_builder_stays_usable() {
    let mut offset = Rec::plain(b"k", b"d");
    offset.object_offset = 1;
    let cases = [
        (
            BlockKind::Segment,
            Rec::plain(&[b'k'; 65536], b"d"),
            LONG_KEY,
        ),
        (BlockKind::Segment, Rec::slice(b"k", b"d", 0), CHAIN_KIND),
        (BlockKind::Pack, Rec::slice(b"k", b"d", 0), CHAIN_KIND),
        (BlockKind::ChainSlice, Rec::plain(b"k", b"d"), CHAIN_KIND),
        (BlockKind::Segment, offset.clone(), OFFSET),
        (BlockKind::Pack, offset, OFFSET),
    ];
    for (kind, rec, reason) in cases {
        let mut b = nb(kind);
        assert_eq!(b.begin(&rec.start()), Err(inv(reason)));
        assert_eq!(b.append(&rec.input()), Err(inv(reason)));
        assert_eq!((b.len(), b.records()), (0, 0));
        let ok = if kind == BlockKind::ChainSlice {
            Rec::slice(b"k", b"d", u64::MAX)
        } else {
            Rec::plain(&[b'k'; 65535], b"d")
        };
        b.append(&ok.input()).unwrap();
        assert_eq!(b.records(), 1);
    }
}

#[test]
fn a_second_record_in_a_chain_slice_block_is_refused_and_fits_says_no() {
    let mut b = nb(BlockKind::ChainSlice);
    assert!(b.fits(1, 1));
    let bytes = b.append(&Rec::slice(b"k", b"data", 4096).input()).unwrap();
    assert!(!b.fits(0, 0));
    assert_eq!(
        b.append(&Rec::slice(b"j", b"x", 8192).input()),
        Err(inv(ONE_SLICE))
    );
    assert_eq!((b.len(), b.records()), (bytes.len() as u64, 1));
    check_block(
        BlockGeometry::DEFAULT,
        BlockKind::ChainSlice,
        &bytes,
        &b.finish().unwrap(),
    );
}

/// Every call of a broken builder, answered the same.
fn assert_broken(b: &mut BlockBuilder) {
    let rec = Rec::plain(b"k", b"d");
    assert_eq!(b.begin(&rec.start()), Err(earlier()));
    assert_eq!(b.append(&rec.input()), Err(earlier()));
    assert_eq!(b.data(b"x"), Err(earlier()));
    assert_eq!(b.data(b""), Err(earlier()));
    assert_eq!(b.end(), Err(earlier()));
    assert_eq!(b.feed(b"x"), Err(earlier()));
    assert_eq!(b.finish(), Err(earlier()));
    assert!(!b.fits(0, 0));
    assert_eq!(b.feed(b""), Ok(()));
}

fn feed_by(b: &mut BlockBuilder, bytes: &[u8], size: usize) -> Result<(), BlockError> {
    bytes.chunks(size).try_for_each(|c| b.feed(c))
}

/// Feeding `bytes` to a new builder fails with `want` at any piece size and breaks the builder.
fn refused(g: BlockGeometry, kind: BlockKind, bytes: &[u8], want: BlockError) {
    for size in [1, 7, 64, bytes.len()] {
        let mut b = BlockBuilder::new(g, kind).unwrap();
        let got = feed_by(&mut b, bytes, size);
        assert_eq!(got, Err(want.clone()), "piece {size}");
        assert_broken(&mut b);
    }
}

#[test]
fn begin_data_and_end_called_out_of_order_break_the_builder() {
    let rec = Rec::plain(b"k", b"abc");
    let mut b = nb(BlockKind::Segment);
    b.begin(&rec.start()).unwrap();
    assert!(!b.fits(0, 0));
    assert_eq!(b.begin(&rec.start()), Err(inv(PENDING)));
    assert_broken(&mut b);

    let mut b = nb(BlockKind::Segment);
    assert_eq!(b.data(b"x"), Err(inv(NO_DATA)));
    assert_broken(&mut b);
    let mut b = nb(BlockKind::Segment);
    b.begin(&rec.start()).unwrap();
    b.data(b"ab").unwrap();
    assert_eq!(b.data(b"cd"), Err(inv(NO_DATA)));
    assert_broken(&mut b);
    // An empty piece is fine inside a record, even one with no data; after its end it is not.
    let mut b = nb(BlockKind::Segment);
    b.begin(&Rec::plain(b"k", b"").start()).unwrap();
    b.data(b"").unwrap();
    b.end().unwrap();
    assert_eq!(b.data(b""), Err(inv(NO_DATA)));

    let mut b = nb(BlockKind::Segment);
    assert_eq!(b.end(), Err(inv(NO_END)));
    assert_broken(&mut b);
    let mut b = nb(BlockKind::Segment);
    b.begin(&rec.start()).unwrap();
    b.data(b"ab").unwrap();
    assert_eq!(b.end(), Err(inv(NO_END)));
    assert_broken(&mut b);
}

#[test]
fn end_with_data_that_does_not_match_the_declared_crc_is_a_bad_crc_and_breaks_the_builder() {
    let mut b = nb(BlockKind::Pack);
    b.begin(&Rec::plain(b"k", b"abc").start()).unwrap();
    b.data(b"abd").unwrap();
    let bad = FormatError::BadCrc {
        what: "block",
        stored: crc32c(b"abc"),
        computed: crc32c(b"abd"),
    };
    assert_eq!(b.end(), Err(BlockError::Format(bad)));
    assert_broken(&mut b);
}

#[test]
fn the_calls_of_one_mode_do_not_mix_with_the_other_inside_a_record() {
    let rec = Rec::plain(b"key", &[7; 100]);
    let bytes = rec.encoded();
    let mut b = nb(BlockKind::Segment);
    b.begin(&rec.start()).unwrap();
    assert_eq!(b.feed(&bytes), Err(inv(FED_OPEN)));
    assert_broken(&mut b);
    // A fed record whose data is still to come: begin, data and end are out of order.
    let cut = &bytes[..64 + 3 + 10];
    for call in 0..3 {
        let mut b = nb(BlockKind::Segment);
        b.feed(cut).unwrap();
        assert_eq!((b.len(), b.records()), (cut.len() as u64, 1));
        assert!(!b.fits(0, 0));
        let (got, reason) = match call {
            0 => (b.begin(&rec.start()).map(drop), PENDING),
            1 => (b.data(b"x"), NO_DATA),
            _ => (b.end().map(drop), NO_END),
        };
        assert_eq!(got, Err(inv(reason)));
        assert_broken(&mut b);
    }
    let mut b = nb(BlockKind::Pack);
    b.feed(&bytes).unwrap();
    b.feed(&[0]).unwrap();
    assert_eq!(b.begin(&rec.start()), Err(inv(PENDING)));
}

fn flipped(bytes: &[u8], at: usize) -> Vec<u8> {
    let mut v = bytes.to_vec();
    v[at] ^= 0x01;
    v
}

#[test]
fn a_flipped_byte_in_a_header_a_key_or_data_is_refused_at_any_piece_size() {
    let bytes = Rec::plain(b"some-key", &[0x42; 70]).encoded();
    let header = RecordHeader::decode(&bytes).unwrap();
    let header_crc = u32::from_le_bytes(bytes[60..64].try_into().unwrap());
    let g = BlockGeometry::DEFAULT;
    let found = u32::from_le_bytes(flipped(&bytes, 0)[..4].try_into().unwrap());
    let magic = FormatError::BadMagic {
        what: "record",
        found,
    };
    refused(
        g,
        BlockKind::Segment,
        &flipped(&bytes, 0),
        BlockError::Format(magic),
    );
    // A header byte (the generation, a reserved byte); the crc is the one stored.
    for at in [20, 58] {
        let bad = flipped(&bytes, at);
        let computed = crc32c(&bad[..60]);
        let want = FormatError::BadCrc {
            what: "record",
            stored: header_crc,
            computed,
        };
        refused(g, BlockKind::Segment, &bad, BlockError::Format(want));
    }
    let (key, data) = (flipped(&bytes, 66), flipped(&bytes, 77));
    let want = FormatError::BadCrc {
        what: "block",
        stored: header.key_crc,
        computed: crc32c(&key[64..72]),
    };
    refused(g, BlockKind::Segment, &key, BlockError::Format(want));
    let want = FormatError::BadCrc {
        what: "block",
        stored: header.data_crc,
        computed: crc32c(&data[72..142]),
    };
    refused(g, BlockKind::Segment, &data, BlockError::Format(want));
}

#[test]
fn a_nonzero_padding_byte_after_data_or_in_a_pack_gap_is_refused() {
    let g = BlockGeometry::DEFAULT;
    // 64 + 2 + 1 = 67 bytes, 13 of padding.
    let bytes = Rec::plain(b"kk", b"d").encoded();
    assert_eq!(bytes.len(), 80);
    for at in [67, 70, 79] {
        refused(g, BlockKind::Segment, &flipped(&bytes, at), inv(PADDING));
    }
    let recs = [Rec::plain(b"a", b"0123456789"), Rec::plain(b"b", b"x")];
    let (bytes, _, _) = build_append(g, BlockKind::Pack, &recs);
    assert_eq!(bytes.len(), 4096 + 80);
    for at in [80, 100, 4095] {
        refused(g, BlockKind::Pack, &flipped(&bytes, at), inv(PADDING));
    }
    nb(BlockKind::Pack).feed(&bytes).unwrap();
}

#[test]
fn zero_bytes_where_a_record_header_should_start_are_a_bad_magic() {
    let mut b = nb(BlockKind::Segment);
    b.feed(&[0]).unwrap();
    assert_eq!(b.finish(), Err(inv(INSIDE)));
    let magic = FormatError::BadMagic {
        what: "record",
        found: 0,
    };
    assert_eq!(b.feed(&[0; 63]), Err(BlockError::Format(magic)));
    assert_broken(&mut b);
}

#[test]
fn a_fed_record_that_does_not_fit_is_full_and_breaks_the_builder_unlike_begin() {
    let g = min_geometry();
    let m = g.max_object();
    let key = vec![b'k'; 65535];
    let rec = Rec::plain(&key, &[]);
    let mut start = rec.start();
    start.data_len = m;
    // A first record, then only the header and key of a second one that does not fit after it:
    // the record is refused when its header is read. The header is taken from a block of the
    // default geometry, where it fits.
    let mut bytes = nb_in(g, BlockKind::Segment)
        .append(&Rec::plain(b"s", b"x").input())
        .unwrap();
    let mut wide = nb(BlockKind::Segment);
    wide.append(&Rec::plain(b"s", b"x").input()).unwrap();
    bytes.extend(wide.begin(&start).unwrap());
    assert_eq!(bytes.len() as u64, record_len(1, 1) + 64 + 65535);
    let full = BlockError::Full {
        need: need_after_one(65535, m),
        limit: MIB,
    };
    refused(g, BlockKind::Segment, &bytes, full);
}

#[test]
fn a_fed_record_longer_than_max_object_is_refused_as_a_chain_and_breaks_the_builder() {
    let g = min_geometry();
    let rec = Rec::plain(b"k", &[]);
    let mut start = rec.start();
    start.data_len = g.max_object() + 1;
    let bytes = nb(BlockKind::Segment).begin(&start).unwrap();
    refused(g, BlockKind::Segment, &bytes, chain_only());
}

#[test]
fn a_fed_record_of_the_wrong_kind_or_a_second_chain_slice_is_refused_and_breaks_the_builder() {
    let g = BlockGeometry::DEFAULT;
    let slice = Rec::slice(b"k", b"d", 0).encoded();
    let plain = Rec::plain(b"k", b"d").encoded();
    let mut with_offset = Rec::plain(b"k", b"d");
    with_offset.object_offset = 8;
    refused(g, BlockKind::Segment, &slice, inv(CHAIN_KIND));
    refused(g, BlockKind::Pack, &slice, inv(CHAIN_KIND));
    refused(g, BlockKind::ChainSlice, &plain, inv(CHAIN_KIND));
    refused(g, BlockKind::Segment, &with_offset.encoded(), inv(OFFSET));
    let mut two = Rec::slice(b"k", b"data", 0).encoded();
    two.resize(4096, 0);
    two.extend(Rec::slice(b"j", b"data", 4).encoded());
    refused(g, BlockKind::ChainSlice, &two, inv(ONE_SLICE));
}

#[test]
fn finish_inside_a_record_or_inside_padding_is_refused_but_does_not_break_the_builder() {
    let rec = Rec::plain(b"kk", &[5; 20]);
    let bytes = rec.encoded();
    assert_eq!(bytes.len(), 96);
    // Inside the header, the key, the data and the padding (86..96).
    for cut in [1, 63, 64, 65, 66, 67, 85, 86, 87, 95] {
        let mut b = nb(BlockKind::Segment);
        b.feed(&bytes[..cut]).unwrap();
        assert_eq!(b.finish(), Err(inv(INSIDE)), "cut {cut}");
        // The rest of the record completes the block.
        b.feed(&bytes[cut..]).unwrap();
        assert_eq!(b.finish().unwrap().trailer.entries.len(), 1);
    }
    let mut b = nb(BlockKind::Pack);
    b.feed(&bytes).unwrap();
    assert!(b.finish().is_ok());
    b.feed(&[0; 5]).unwrap();
    assert_eq!(b.finish(), Err(inv(INSIDE)));
    // Inside a record that begin started.
    let mut b = nb(BlockKind::Segment);
    b.begin(&rec.start()).unwrap();
    b.data(&rec.data).unwrap();
    assert_eq!(b.finish(), Err(inv(INSIDE)));
    b.end().unwrap();
    assert!(b.finish().is_ok());
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
