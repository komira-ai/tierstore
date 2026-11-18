//! The helpers of the crate root: the cell count of a length, the crc32c, the cell table of a
//! buffer, and the text of an error.
//!
//! Catches:
//! - a cell count that is off by one at an exact multiple of 1 MiB or one byte past it, or that
//!   counts an empty region as a cell;
//! - a crc32c that is not the Castagnoli one (its check value of "123456789" is 0xe3069283);
//! - a cell table that does not hold one crc per 1 MiB of the bytes, the last over the short tail;
//! - an error whose text does not name the format and the numbers it failed on.

use tierstore_format::trailer::cell_crcs;
use tierstore_format::{CELL_BYTES, CELL_SHIFT, FormatError, cells_for, crc32c};

#[test]
fn the_cell_is_one_mebibyte() {
    assert_eq!(CELL_SHIFT, 20);
    assert_eq!(CELL_BYTES, 1 << 20);
    assert_eq!(CELL_BYTES, 1_048_576);
}

#[test]
fn cells_for_rounds_up_at_every_boundary() {
    assert_eq!(cells_for(0), 0);
    assert_eq!(cells_for(1), 1);
    assert_eq!(cells_for(CELL_BYTES - 1), 1);
    assert_eq!(cells_for(CELL_BYTES), 1);
    assert_eq!(cells_for(CELL_BYTES + 1), 2);
    assert_eq!(cells_for(2 * CELL_BYTES), 2);
    assert_eq!(cells_for(2 * CELL_BYTES + 1), 3);
    assert_eq!(cells_for(u64::MAX), 1 << 44);
}

#[test]
fn crc32c_is_the_castagnoli_checksum() {
    assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    assert_eq!(crc32c(b""), 0);
    assert_ne!(crc32c(b"a"), crc32c(b"b"));
}

#[test]
fn the_cell_table_has_one_crc_per_mebibyte_and_a_short_last_cell() {
    assert_eq!(cell_crcs(b""), Vec::<u32>::new());
    assert_eq!(cell_crcs(b"abc"), vec![crc32c(b"abc")]);
    let mut bytes = vec![7u8; (CELL_BYTES as usize) * 2 + 5];
    bytes[CELL_BYTES as usize] = 9;
    let table = cell_crcs(&bytes);
    assert_eq!(table.len(), 3);
    assert_eq!(table[0], crc32c(&bytes[..CELL_BYTES as usize]));
    assert_eq!(
        table[1],
        crc32c(&bytes[CELL_BYTES as usize..2 * CELL_BYTES as usize])
    );
    assert_eq!(table[2], crc32c(&bytes[2 * CELL_BYTES as usize..]));
    assert_ne!(table[0], table[1]);
    let exact = cell_crcs(&bytes[..2 * CELL_BYTES as usize]);
    assert_eq!(exact, table[..2].to_vec());
    assert_eq!(cells_for(bytes.len() as u64), table.len() as u64);
}

#[test]
fn every_error_names_its_format_and_the_numbers() {
    let e = FormatError::Short {
        what: "x",
        need: 10,
        have: 3,
    };
    assert_eq!(e.to_string(), "x: 10 bytes needed, 3 present");
    let e = FormatError::BadMagic {
        what: "x",
        found: 0x1234,
    };
    assert_eq!(e.to_string(), "x: bad magic 0x00001234");
    let e = FormatError::BadCrc {
        what: "x",
        stored: 1,
        computed: 2,
    };
    assert_eq!(
        e.to_string(),
        "x: crc32c 0x00000001 stored, 0x00000002 computed"
    );
    let e = FormatError::UnsupportedVersion {
        what: "x",
        version: 9,
    };
    assert_eq!(e.to_string(), "x: version 9 is not supported");
    let e = FormatError::Invalid {
        what: "x",
        reason: "why",
    };
    assert_eq!(e.to_string(), "x: why");
}
