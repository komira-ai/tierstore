//! The helpers the block tests share: records to write, a seeded generator, builders driven both
//! ways, and the trailer of a block computed without the builder.

pub use tierstore_format::FormatError;
pub use tierstore_format::block::{
    BlockBuilder, BlockError, BlockGeometry, DEFAULT_BLOCK_BYTES, DEFAULT_BLOCK_RESERVE,
    PACK_ALIGN, RecordStart, Sealed, TRAILER_ENTRY_FIXED_LEN, trailer_entry_len, trailer_len,
};
pub use tierstore_format::crc32c;
pub use tierstore_format::digest::blake3_256;
pub use tierstore_format::ids::Generation;
pub use tierstore_format::record::{
    RecordFlags, RecordHeader, RecordIn, decode_record, encode_record, record_len, scan,
};
pub use tierstore_format::stamp::Stamp;
pub use tierstore_format::trailer::{BlockKind, Trailer, TrailerEntry, cell_crcs};

pub const MIB: u64 = 1 << 20;
/// The smallest reserve of a 1 MiB block (worked out by hand in `the_smallest_reserve...`).
pub const MIN_RESERVE: u64 = 131_295;
pub const PENDING: &str = "a record starts while another is open or bytes are pending";
pub const NO_DATA: &str = "data beyond the declared length, or with no record started";
pub const NO_END: &str = "a record ends before its data, or with no record started";
pub const PADDING: &str = "a padding byte is not zero";
pub const INSIDE: &str = "the block ends inside a record or its padding";
pub const CHAIN_KIND: &str = "a chain slice record outside a chain slice block, or the reverse";
pub const OFFSET: &str = "an object offset on a record that is not a chain slice";
pub const ONE_SLICE: &str = "a chain slice block holds one record";
pub const LONG_KEY: &str = "key longer than 65535 bytes";
pub const FED_OPEN: &str = "bytes fed while a record started by begin is open";
pub const UNKNOWN_BIT: RecordFlags = RecordFlags(0x8000);

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

pub fn nb(kind: BlockKind) -> BlockBuilder {
    BlockBuilder::new(BlockGeometry::DEFAULT, kind).unwrap()
}

pub fn min_geometry() -> BlockGeometry {
    BlockGeometry::new(MIB, MIN_RESERVE).unwrap()
}

/// An empty builder of `kind` in geometry `g`.
pub fn nb_in(g: BlockGeometry, kind: BlockKind) -> BlockBuilder {
    BlockBuilder::new(g, kind).unwrap()
}

pub fn mid_geometry() -> BlockGeometry {
    BlockGeometry::new(4 * MIB, MIB).unwrap()
}

pub fn inv(reason: &'static str) -> BlockError {
    BlockError::Format(FormatError::Invalid {
        what: "block",
        reason,
    })
}

pub fn earlier() -> BlockError {
    inv("an earlier error left the bytes counted not a block")
}

#[derive(Clone)]
pub struct Rec {
    pub flags: RecordFlags,
    pub key: Vec<u8>,
    pub data: Vec<u8>,
    pub generation: Generation,
    pub stamp: u64,
    pub object_offset: u64,
}

impl Rec {
    pub fn plain(key: &[u8], data: &[u8]) -> Self {
        Self {
            flags: RecordFlags::default(),
            key: key.to_vec(),
            data: data.to_vec(),
            generation: Generation([7; 16]),
            stamp: 1,
            object_offset: 0,
        }
    }

    pub fn slice(key: &[u8], data: &[u8], object_offset: u64) -> Self {
        Self {
            flags: RecordFlags::CHAIN_SLICE,
            object_offset,
            ..Self::plain(key, data)
        }
    }

    pub fn input(&self) -> RecordIn<'_> {
        RecordIn {
            flags: self.flags,
            key: &self.key,
            data: &self.data,
            generation: self.generation,
            stamp: Stamp::from_raw(self.stamp),
            object_offset: self.object_offset,
        }
    }

    pub fn start(&self) -> RecordStart<'_> {
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

    pub fn encoded(&self) -> Vec<u8> {
        encode_record(&self.input()).unwrap()
    }
}

pub fn random_recs(rng: &mut Rng, kind: BlockKind, n: usize, max_data: u64) -> Vec<Rec> {
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
pub fn build_append(
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
pub fn build_streamed(
    rng: &mut Rng,
    geom: BlockGeometry,
    kind: BlockKind,
    recs: &[Rec],
) -> Vec<u8> {
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

pub fn feed_cycle(
    geom: BlockGeometry,
    kind: BlockKind,
    bytes: &[u8],
    sizes: &[usize],
) -> BlockBuilder {
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

pub struct Walked {
    pub offset: u64,
    pub header: RecordHeader,
    pub key: Vec<u8>,
}

/// Decodes the records of a block of `align`-aligned records, checking the gaps are zero.
pub fn walk(bytes: &[u8], align: u64) -> Vec<Walked> {
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
pub fn check_block(geom: BlockGeometry, kind: BlockKind, bytes: &[u8], sealed: &Sealed) {
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
pub fn lone_need(key: u64, data: u64) -> u64 {
    let end = (64 + key + data).next_multiple_of(16);
    end + (48 + key).next_multiple_of(8) + 4 * end.div_ceil(MIB) + 96
}

pub const PIECES: [usize; 12] = [1, 2, 3, 5, 8, 13, 16, 17, 63, 64, 65, 4097];

/// Writing a block of random records, reading its bytes back at many cuts, and streaming the
/// records give one block.
pub fn write_read_round_trip(kind: BlockKind, seeds: u64, n: usize, max_data: u64) {
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

/// `rec` as a chain slice when the block is one.
pub fn chain_if(kind: BlockKind, mut rec: Rec) -> Rec {
    if kind == BlockKind::ChainSlice {
        rec.flags = RecordFlags::CHAIN_SLICE;
    }
    rec
}

/// The error of a record whose data is longer than `max_object`.
pub fn chain_only() -> BlockError {
    inv("an object larger than max_object is a chain, never one record")
}

/// The block length `begin` reports when a record of `key_len` and `data_len` follows one record
/// `s`/`x` in a segment of `g` (computed from the layout, record by record).
pub fn need_after_one(key_len: u16, data_len: u64) -> u64 {
    let start = record_len(1, 1);
    let end = start + record_len(key_len, data_len);
    end + trailer_len(trailer_entry_len(1) + trailer_entry_len(key_len), end)
}

/// Every call of a broken builder, answered the same.
pub fn assert_broken(b: &mut BlockBuilder) {
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

pub fn feed_by(b: &mut BlockBuilder, bytes: &[u8], size: usize) -> Result<(), BlockError> {
    bytes.chunks(size).try_for_each(|c| b.feed(c))
}

/// Feeding `bytes` to a new builder fails with `want` at any piece size and breaks the builder.
pub fn refused(g: BlockGeometry, kind: BlockKind, bytes: &[u8], want: BlockError) {
    for size in [1, 7, 64, bytes.len()] {
        let mut b = BlockBuilder::new(g, kind).unwrap();
        let got = feed_by(&mut b, bytes, size);
        assert_eq!(got, Err(want.clone()), "piece {size}");
        assert_broken(&mut b);
    }
}

pub fn flipped(bytes: &[u8], at: usize) -> Vec<u8> {
    let mut v = bytes.to_vec();
    v[at] ^= 0x01;
    v
}
