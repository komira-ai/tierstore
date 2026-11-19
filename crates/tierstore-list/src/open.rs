//! The checks of [`TreeListL2::open`](crate::TreeListL2::open) ahead of the walk of the entries:
//! the header, where each section is, the packs and their keys, the cells, the ranges and the
//! extension records.

use tierstore_fetch_api::list::{CELL_DIGEST_SHA256, ListVersionUnsupported, Refusal};

use crate::error::{ExtensionFault, HeaderFault, Malformed, OpenError, PackFault};
use crate::layout::{
    CELL_SHIFT, CRC_AT, EXTENSION_REQUIRED, HEADER_LEN, MAGIC, MAJOR, MAX_KEY_BYTES, PACK_LEN,
    RANGE_LEN, SECTIONS_AT, Section, V2_MAGIC, align8, cells_of, digest_at, header_crc, le_u16,
    le_u32, le_u64,
};

/// Where a section is in the object: its first byte, its length, and its count (records, or
/// bytes for a blob).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Place {
    pub(crate) start: usize,
    pub(crate) len: usize,
    pub(crate) count: u32,
}

impl Place {
    /// The section's bytes in `bytes`, which the open checked to hold it.
    pub(crate) fn of(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.start..self.start + self.len]
    }
}

/// What the header says: where each section is, and the tree the list is of.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    pub(crate) places: [Place; 8],
    pub(crate) tree_id: [u8; 32],
}

impl Header {
    /// Where `section` is.
    pub(crate) const fn place(&self, section: Section) -> Place {
        self.places[section as usize]
    }
}

/// The refusal of bytes that are not an L2 list: a version 1 list (`{`) or a version 2 list (its
/// magic) is a list this release does not read; anything else is not a list.
fn not_l2(bytes: &[u8]) -> OpenError {
    if bytes.starts_with(b"{") || bytes.starts_with(&V2_MAGIC) {
        OpenError::Unsupported(ListVersionUnsupported {
            reason: Refusal::V1List,
        })
    } else {
        OpenError::Malformed(Malformed::BadMagic)
    }
}

/// Checks the header of `bytes` and where it says each section is.
pub(crate) fn header(bytes: &[u8]) -> Result<Header, OpenError> {
    if !bytes.starts_with(&MAGIC) {
        return Err(not_l2(bytes));
    }
    let fault = |fault| OpenError::Malformed(Malformed::Header(fault));
    if bytes.len() < HEADER_LEN {
        return Err(fault(HeaderFault::Truncated));
    }
    let major = le_u16(bytes, 8);
    if major != MAJOR {
        return Err(OpenError::Unsupported(ListVersionUnsupported {
            reason: Refusal::UnknownVersion(u32::from(major)),
        }));
    }
    let header_len = le_u32(bytes, 16);
    let len = header_len as usize;
    if len < HEADER_LEN || !len.is_multiple_of(8) || len > bytes.len() {
        return Err(fault(HeaderFault::Length(header_len)));
    }
    let stored = le_u32(bytes, CRC_AT);
    let computed = header_crc(&bytes[..len]);
    if stored != computed {
        return Err(fault(HeaderFault::Crc { stored, computed }));
    }
    let flags = le_u32(bytes, 12);
    if flags != 0 {
        return Err(fault(HeaderFault::RequiredFlags(flags)));
    }
    if bytes[20] != CELL_DIGEST_SHA256 {
        return Err(OpenError::Unsupported(ListVersionUnsupported {
            reason: Refusal::UnknownCellDigest(bytes[20]),
        }));
    }
    if bytes[21] != CELL_SHIFT {
        return Err(fault(HeaderFault::CellShift(bytes[21])));
    }
    let mut places = [Place::default(); 8];
    let mut previous_end = len as u64;
    for (at, section) in Section::ALL.into_iter().enumerate() {
        let offset = le_u64(bytes, SECTIONS_AT + 16 * at);
        let count = le_u64(bytes, SECTIONS_AT + 16 * at + 8);
        if !offset.is_multiple_of(8) {
            return Err(fault(HeaderFault::Misaligned(section)));
        }
        if offset < previous_end {
            return Err(fault(HeaderFault::Overlap(section)));
        }
        let count = u32::try_from(count).map_err(|_| fault(HeaderFault::TooMany(section)))?;
        // A count is below 2^32 and a unit at most 48 bytes: the product fits in 64 bits.
        let end = offset.checked_add(u64::from(count) * section.unit());
        let end = end
            .filter(|end| *end <= bytes.len() as u64)
            .ok_or(fault(HeaderFault::PastEnd(section)))?;
        // Both within the object, so within `usize`.
        places[at] = Place {
            start: offset as usize,
            len: (end - offset) as usize,
            count,
        };
        previous_end = end;
    }
    let header = Header {
        places,
        tree_id: digest_at(bytes, 24),
    };
    let entries = header.place(Section::Entries).count;
    if entries == 0 {
        return Err(fault(HeaderFault::NoEntries));
    }
    let digests = header.place(Section::Digests).count;
    if digests != entries {
        return Err(fault(HeaderFault::DigestCount(u64::from(digests))));
    }
    let extensions = header.place(Section::Extensions).count;
    if !extensions.is_multiple_of(8) {
        return Err(fault(HeaderFault::ExtensionLength(u64::from(extensions))));
    }
    Ok(header)
}

/// The size of pack `index`, which the caller has bounded, from the PACKS bytes.
pub(crate) fn pack_size(packs: &[u8], index: u32) -> u64 {
    le_u64(packs, index as usize * PACK_LEN + 8)
}

/// Checks every PACKS record: its key (UTF-8, 1 to 1024 bytes, beginning where the one before it
/// ends and sorting after it), its size (above 0), its cell shift (20), its cell count (that of its
/// size) and its first cell (where the cells of the packs before it end); then that the keys end
/// with the last pack's and that the CELLS table holds exactly the packs' cells.
pub(crate) fn packs(bytes: &[u8], header: &Header) -> Result<(), Malformed> {
    let packs = header.place(Section::Packs);
    let keys = header.place(Section::Keys).of(bytes);
    let records = packs.of(bytes);
    let mut key_end = 0usize;
    let mut previous: &[u8] = &[];
    let mut cell_next = 0u64;
    for index in 0..packs.count {
        let record = &records[index as usize * PACK_LEN..(index as usize + 1) * PACK_LEN];
        let fault = |fault| Malformed::Pack { index, fault };
        let key_off = le_u32(record, 0) as usize;
        let key_len = le_u32(record, 4);
        if key_off != key_end {
            return Err(fault(PackFault::KeyStart));
        }
        if key_len == 0 || key_len as usize > MAX_KEY_BYTES {
            return Err(fault(PackFault::KeyLength(key_len)));
        }
        let key = keys
            .get(key_off..key_off + key_len as usize)
            .ok_or(fault(PackFault::KeyBounds))?;
        if std::str::from_utf8(key).is_err() {
            return Err(fault(PackFault::KeyNotUtf8));
        }
        if index > 0 && key <= previous {
            return Err(fault(PackFault::KeyNotAscending));
        }
        let size = le_u64(record, 8);
        if size == 0 {
            return Err(fault(PackFault::Empty));
        }
        if record[28] != CELL_SHIFT {
            return Err(fault(PackFault::CellShift(record[28])));
        }
        let count = le_u32(record, 24);
        let expected = cells_of(size);
        if u64::from(count) != expected {
            return Err(fault(PackFault::CellCount {
                expected,
                found: count,
            }));
        }
        let first = le_u64(record, 16);
        if first != cell_next {
            return Err(fault(PackFault::CellFirst {
                expected: cell_next,
                found: first,
            }));
        }
        // At most 2^32 packs of at most 2^32 cells each: the sum fits in 64 bits.
        cell_next += u64::from(count);
        key_end = key_off + key_len as usize;
        previous = key;
    }
    if key_end != keys.len() {
        return Err(Malformed::KeysLength);
    }
    let cells = u64::from(header.place(Section::Cells).count);
    if cells != cell_next {
        return Err(Malformed::CellTotal {
            expected: cell_next,
            found: cells,
        });
    }
    Ok(())
}

/// Checks that every range names a pack, is not empty, and ends within its pack.
pub(crate) fn ranges(bytes: &[u8], header: &Header) -> Result<(), Malformed> {
    let packs = header.place(Section::Packs);
    let ranges = header.place(Section::Ranges);
    let (pack_bytes, range_bytes) = (packs.of(bytes), ranges.of(bytes));
    for index in 0..ranges.count {
        let at = index as usize * RANGE_LEN;
        let pack = le_u32(range_bytes, at);
        let offset = le_u64(range_bytes, at + 8);
        let len = le_u64(range_bytes, at + 16);
        let valid = pack < packs.count
            && len > 0
            && offset
                .checked_add(len)
                .is_some_and(|end| end <= pack_size(pack_bytes, pack));
        if !valid {
            return Err(Malformed::Range(index));
        }
    }
    Ok(())
}

/// Checks the extension records: each a 4-byte tag, a 4-byte length and that many bytes, padded
/// with zeros to a multiple of 8, within the section. This release knows no tag: it skips every
/// record, and refuses one whose tag has [`EXTENSION_REQUIRED`] set.
pub(crate) fn extensions(bytes: &[u8], header: &Header) -> Result<(), Malformed> {
    let section = header.place(Section::Extensions).of(bytes);
    let mut at = 0usize;
    while at < section.len() {
        let fault = |fault| Malformed::Extension {
            at: at as u64,
            fault,
        };
        // The section's length is a multiple of 8, and so is `at`: a record header fits.
        let tag = le_u32(section, at);
        let len = le_u32(section, at + 4) as usize;
        let end = at + 8 + align8(len);
        if end > section.len() {
            return Err(fault(ExtensionFault::Truncated));
        }
        if tag & EXTENSION_REQUIRED != 0 {
            return Err(fault(ExtensionFault::Required(tag)));
        }
        at = end;
    }
    Ok(())
}
