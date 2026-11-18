//! The blake3 and crc32c helpers: generation, key hash, the rendezvous hash, the data digest and
//! crc32c continuation.
//!
//! Catches:
//! - a generation or key hash that is not the first 16 bytes of the blake3 of the input, or a
//!   streaming generation that differs from the one-shot value at some cut (including empty pieces),
//!   or a `finish` that consumes or disturbs the state;
//! - a `hash64` that is not the first 8 bytes of the blake3 little-endian, that depends on how the
//!   parts are cut, or that ignores a part;
//! - a data digest whose length, cell crcs or blake3 depend on how the input was cut, whose cell
//!   boundary is off by one (at 0, 1, CELL_BYTES - 1, CELL_BYTES, CELL_BYTES + 1 and two cells), or
//!   that differs from `trailer::cell_crcs` and a bitwise crc32c reference;
//! - a `finish` that adds a phantom cell at an exact cell boundary, loses the short last cell, or
//!   stops the hasher from taking more bytes afterwards;
//! - a `crc32c_append` that is not the crc32c of the concatenation at every cut.

use tierstore_format::digest::{DataHasher, GenerationHasher, blake3_256, hash64};
use tierstore_format::ids::{Generation, KeyHash};
use tierstore_format::trailer::cell_crcs;
use tierstore_format::{CELL_BYTES, cells_for, crc32c, crc32c_append};

const EMPTY_BLAKE3: [u8; 32] = [
    0xaf, 0x13, 0x49, 0xb9, 0xf5, 0xf9, 0xa1, 0xa6, 0xa0, 0x40, 0x4d, 0xea, 0x36, 0xdc, 0xc9, 0x49,
    0x9b, 0xcb, 0x25, 0xc9, 0xad, 0xc1, 0x12, 0xb7, 0xcc, 0x9a, 0x93, 0xca, 0xe4, 0x1f, 0x32, 0x62,
];

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = SplitMix64(seed);
    (0..len).map(|_| rng.next() as u8).collect()
}

/// A bitwise crc32c (Castagnoli, reflected polynomial 0x82F63B78), independent of the crate.
fn crc_ref(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn digest_in_pieces(data: &[u8], piece: usize) -> tierstore_format::digest::DataDigest {
    let mut h = DataHasher::new();
    for chunk in data.chunks(piece) {
        h.update(chunk);
    }
    h.finish()
}

#[test]
fn blake3_of_the_empty_input_is_the_official_vector() {
    assert_eq!(blake3_256(b""), EMPTY_BLAKE3);
}

#[test]
fn generation_and_key_hash_are_the_first_16_bytes_of_the_blake3() {
    let data = bytes(1, 1000);
    let full = blake3_256(&data);
    assert_eq!(Generation::of(&data).0[..], full[..16]);
    assert_eq!(KeyHash::of(&data).0[..], full[..16]);
    assert_eq!(Generation::of(b"").0[..], EMPTY_BLAKE3[..16]);
    assert_eq!(KeyHash::of(b"").0[..], EMPTY_BLAKE3[..16]);
    assert_ne!(Generation::of(b"a").0, Generation::of(b"b").0);
    assert_ne!(KeyHash::of(b"a").0, KeyHash::of(b"b").0);
}

#[test]
fn generation_hasher_equals_one_shot_at_every_cut() {
    let data = bytes(2, 300);
    let want = Generation::of(&data);
    for cut in 0..=data.len() {
        let mut h = GenerationHasher::new();
        h.update(&data[..cut]);
        h.update(&[]);
        h.update(&data[cut..]);
        assert_eq!(h.finish(), want, "cut {cut}");
    }
}

#[test]
fn generation_hasher_of_nothing_is_the_generation_of_empty() {
    assert_eq!(GenerationHasher::new().finish(), Generation::of(b""));
    let mut h = GenerationHasher::default();
    h.update(&[]);
    assert_eq!(h.finish(), Generation::of(b""));
}

#[test]
fn generation_hasher_finish_twice_agrees_and_leaves_it_usable() {
    let mut h = GenerationHasher::new();
    h.update(b"hello ");
    let first = h.finish();
    assert_eq!(h.finish(), first);
    assert_eq!(first, Generation::of(b"hello "));
    h.update(b"world");
    assert_eq!(h.finish(), Generation::of(b"hello world"));
    let copy = h.clone();
    assert_eq!(copy.finish(), h.finish());
}

#[test]
fn hash64_is_the_first_8_bytes_of_the_blake3_little_endian() {
    let data = bytes(3, 77);
    let full = blake3_256(&data);
    let want = u64::from_le_bytes(full[..8].try_into().unwrap());
    assert_eq!(hash64(&[&data]), want);
    assert_eq!(
        hash64(&[]),
        u64::from_le_bytes(EMPTY_BLAKE3[..8].try_into().unwrap())
    );
    assert_eq!(hash64(&[b"", b""]), hash64(&[]));
}

#[test]
fn hash64_does_not_depend_on_where_the_parts_are_cut() {
    let abc = hash64(&[b"abc"]);
    assert_eq!(hash64(&[b"ab", b"c"]), abc);
    assert_eq!(hash64(&[b"a", b"bc"]), abc);
    assert_eq!(hash64(&[b"a", b"", b"b", b"c"]), abc);
    assert_ne!(hash64(&[b"abc"]), hash64(&[b"abd"]));
}

#[test]
fn hash64_differs_for_a_different_node_byte_and_part_order() {
    let key = b"some-key";
    let mut seen = std::collections::BTreeSet::new();
    for node in 0u8..=255 {
        assert!(seen.insert(hash64(&[key, &[node]])), "node {node}");
    }
    assert_ne!(hash64(&[b"ab", b"c"]), hash64(&[b"c", b"ab"]));
    assert_ne!(hash64(&[key, &[1]]), hash64(&[&[1], key]));
}

#[test]
fn data_hasher_of_nothing_is_empty_with_no_cells() {
    let h = DataHasher::new();
    assert_eq!(h.len(), 0);
    assert!(h.is_empty());
    let d = h.finish();
    assert_eq!(d.len, 0);
    assert!(d.cell_crcs.is_empty());
    assert_eq!(d.blake3, EMPTY_BLAKE3);

    let mut h = DataHasher::default();
    h.update(&[]);
    assert!(h.is_empty());
    assert_eq!(h.finish(), d);
}

#[test]
fn data_hasher_digest_is_exact_at_the_cell_boundaries() {
    let cell = CELL_BYTES as usize;
    let lens = [1, cell - 1, cell, cell + 1, 2 * cell];
    for len in lens {
        let data = bytes(len as u64, len);
        let mut h = DataHasher::new();
        h.update(&data);
        assert_eq!(h.len(), len as u64, "len {len}");
        assert!(!h.is_empty());
        let d = h.finish();
        assert_eq!(d.len, len as u64);
        assert_eq!(d.cell_crcs.len() as u64, cells_for(len as u64), "len {len}");
        assert_eq!(d.cell_crcs, cell_crcs(&data), "len {len}");
        let reference: Vec<u32> = data.chunks(cell).map(crc_ref).collect();
        assert_eq!(d.cell_crcs, reference, "len {len}");
        assert_eq!(d.blake3, blake3_256(&data), "len {len}");
    }
}

#[test]
fn data_hasher_cell_count_steps_from_one_to_two_only_past_a_full_cell() {
    let cell = CELL_BYTES as usize;
    let count = |len: usize| {
        digest_in_pieces(&vec![7u8; len], len.max(1))
            .cell_crcs
            .len()
    };
    assert_eq!(count(cell - 1), 1);
    assert_eq!(count(cell), 1);
    assert_eq!(count(cell + 1), 2);
    assert_eq!(count(2 * cell), 2);
}

#[test]
fn data_hasher_does_not_depend_on_how_the_input_is_cut() {
    let cell = CELL_BYTES as usize;
    let data = bytes(9, 2 * cell + 12_345);
    let whole = digest_in_pieces(&data, data.len());
    assert_eq!(whole.cell_crcs, cell_crcs(&data));
    assert_eq!(whole.blake3, blake3_256(&data));
    for piece in [1 << 18, cell - 1, cell, cell + 1, 3 * cell] {
        assert_eq!(digest_in_pieces(&data, piece), whole, "piece {piece}");
    }
    // Cuts straddling each cell boundary, with empty pieces between.
    for boundary in [cell, 2 * cell] {
        for delta in [-2i64, -1, 0, 1, 2] {
            let cut = (boundary as i64 + delta) as usize;
            let mut h = DataHasher::new();
            h.update(&data[..cut]);
            h.update(&[]);
            h.update(&data[cut..]);
            assert_eq!(h.finish(), whole, "cut {cut}");
        }
    }
}

#[test]
fn data_hasher_small_pieces_of_random_sizes_match_the_whole() {
    let data = bytes(11, 5000);
    let whole = digest_in_pieces(&data, data.len());
    let mut rng = SplitMix64(12);
    for _ in 0..50 {
        let mut h = DataHasher::new();
        let mut at = 0;
        while at < data.len() {
            let n = (rng.next() % 700) as usize;
            let end = (at + n).min(data.len());
            h.update(&data[at..end]);
            assert_eq!(h.len(), end as u64);
            at = end;
        }
        assert_eq!(h.finish(), whole);
    }
    assert_eq!(whole.cell_crcs, vec![crc_ref(&data)]);
}

#[test]
fn data_hasher_finish_twice_agrees_and_accepts_more_bytes() {
    let cell = CELL_BYTES as usize;
    let data = bytes(13, cell + 10);
    let mut h = DataHasher::new();
    h.update(&data[..cell]);
    let at_boundary = h.finish();
    assert_eq!(h.finish(), at_boundary);
    assert_eq!(at_boundary.cell_crcs, vec![crc_ref(&data[..cell])]);

    h.update(&data[cell..cell + 4]);
    let mid = h.finish();
    assert_eq!(mid.len, cell as u64 + 4);
    assert_eq!(mid.cell_crcs.len(), 2);
    assert_eq!(h.finish(), mid);

    h.update(&data[cell + 4..]);
    let full = h.finish();
    assert_eq!(full.len, data.len() as u64);
    assert_eq!(full.cell_crcs, cell_crcs(&data));
    assert_eq!(full.blake3, blake3_256(&data));
    assert_eq!(h.clone().finish(), full);
}

#[test]
fn crc32c_matches_the_bitwise_reference_and_the_known_check_value() {
    assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    assert_eq!(crc_ref(b"123456789"), 0xe306_9283);
    assert_eq!(crc32c(b""), 0);
    for len in [1usize, 7, 64, 1000] {
        let data = bytes(len as u64, len);
        assert_eq!(crc32c(&data), crc_ref(&data), "len {len}");
    }
}

#[test]
fn crc32c_append_of_pieces_equals_the_crc_of_the_whole_at_every_cut() {
    let data = bytes(21, 200);
    let want = crc32c(&data);
    for cut in 0..=data.len() {
        let first = crc32c(&data[..cut]);
        assert_eq!(crc32c_append(first, &data[cut..]), want, "cut {cut}");
    }
    let mut running = 0;
    for chunk in data.chunks(13) {
        running = crc32c_append(running, chunk);
    }
    assert_eq!(running, want);
}

#[test]
fn crc32c_append_from_zero_is_the_plain_crc_and_empty_append_is_identity() {
    for len in [0usize, 1, 31, 4096] {
        let data = bytes(30 + len as u64, len);
        assert_eq!(crc32c_append(0, &data), crc32c(&data), "len {len}");
        let c = crc32c(&data);
        assert_eq!(crc32c_append(c, &[]), c);
    }
}
