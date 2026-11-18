//! The errors of the block builder: calls out of order, data that does not match its declared crc,
//! records a block cannot take, and bytes read back that are not a block; and which of them leave
//! the builder usable and which break it.
//!
//! Catches:
//! - a call out of order, a wrong declared crc, a second chain-slice record, a long key, an object
//!   offset or a flag of the wrong kind accepted;
//! - a flipped header, key or data byte, a nonzero padding or gap byte, zero bytes where a header
//!   starts, or a record that does not fit or is longer than `max_object`, accepted while reading;
//! - a builder that goes on after bytes that are not a block, or one that breaks on an error of
//!   `begin` that added nothing.

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
