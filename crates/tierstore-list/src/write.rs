//! The writer: the cell digests of a pack as its bytes stream ([`CellHasher`]), and the L2 sidecar
//! of a version 2 list ([`sidecar_from_v2`]), named by [`sidecar_key`].

use ring::digest::{Context, SHA256};
use tierstore_fetch_api::list::{CELL_DIGEST_SHA256, CellDigest, PACK_FULLY_COVERED};

use crate::error::{V2Fault, WriteError, hex};
use crate::layout::{
    CELL_BYTES, CELL_SHIFT, CRC_AT, HEADER_LEN, KIND_FILE, MAGIC, MAJOR, MAX_KEY_BYTES, PACK_LEN,
    RANGE_LEN, RECORD_LEN, Record, V2_HEADER_LEN, V2_MAGIC, align8, cells_of, header_crc, le_u16,
    le_u32, le_u64,
};

/// The bit of `pack_flags` the CAS service sets on a pack that at least `shared_min` trees name
/// (design 6.1), defined beside the other bits of the record in `tierstore-fetch-api`.
pub use tierstore_fetch_api::list::PACK_SHARED;

/// The prefix of the key of a tree's L2 sidecar: `cas/l2/<id>`, where `cas/l/<id>` is the key of
/// the version 2 list the tree is named by.
pub const SIDECAR_PREFIX: &str = "cas/l2/";

/// The key of the L2 sidecar of the tree whose version 2 list is `cas/l/<tree_id>`.
#[must_use]
pub fn sidecar_key(tree_id: &[u8; 32]) -> String {
    format!("{SIDECAR_PREFIX}{}", hex(tree_id))
}

/// The cells of a pack: its size and the SHA-256 of each 1 MiB of it, the last cell short.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackCells {
    /// The pack's size in bytes.
    pub size: u64,
    /// `digests[i]` is the SHA-256 of the pack's bytes `[i << 20, min((i + 1) << 20, size))`.
    pub digests: Vec<CellDigest>,
}

impl PackCells {
    /// The cells of a pack whose bytes are `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = CellHasher::new();
        hasher.update(bytes);
        hasher.finish()
    }
}

/// The cell digests of a pack, as its bytes stream past: one 1 MiB accumulator, hashed as each
/// cell fills (design 3.2). The bytes are never held: each piece given is hashed and dropped.
pub struct CellHasher {
    cell: Context,
    in_cell: u64,
    size: u64,
    digests: Vec<CellDigest>,
}

impl Default for CellHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CellHasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellHasher")
            .field("size", &self.size)
            .field("cells", &self.digests.len())
            .finish_non_exhaustive()
    }
}

impl CellHasher {
    /// A hasher of no bytes yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cell: Context::new(&SHA256),
            in_cell: 0,
            size: 0,
            digests: Vec::new(),
        }
    }

    /// Hashes the next `bytes` of the pack.
    pub fn update(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            // The room left in the cell is at most 1 MiB.
            let take = (CELL_BYTES - self.in_cell).min(bytes.len() as u64) as usize;
            self.cell.update(&bytes[..take]);
            self.in_cell += take as u64;
            self.size += take as u64;
            bytes = &bytes[take..];
            if self.in_cell == CELL_BYTES {
                self.close();
            }
        }
    }

    /// The bytes hashed so far.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    fn close(&mut self) {
        let cell = std::mem::replace(&mut self.cell, Context::new(&SHA256));
        let mut digest = [0; 32];
        digest.copy_from_slice(cell.finish().as_ref());
        self.digests.push(digest);
        self.in_cell = 0;
    }

    /// The cells of the bytes hashed: the last cell, if short, hashed as it is.
    #[must_use]
    pub fn finish(mut self) -> PackCells {
        if self.in_cell > 0 {
            self.close();
        }
        PackCells {
            size: self.size,
            digests: self.digests,
        }
    }
}

/// What the writer of a sidecar is told of a pack the list names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackInfo {
    /// Its size and cell digests.
    pub cells: PackCells,
    /// The part size it was uploaded with in parts, 0 for a pack written by one PUT (every pack
    /// written to the storage tier). No reader uses it (design 3.1).
    pub upload_part_size: u64,
    /// Whether at least `shared_min` trees name it ([`PACK_SHARED`], design 6.1).
    pub shared: bool,
}

/// A version 2 list's sections, as its writer laid them out.
struct V2<'a> {
    entries: &'a [u8],
    digests: &'a [u8],
    keys: Vec<&'a str>,
    key_blob: &'a [u8],
    names: &'a [u8],
    ranges: &'a [u8],
}

/// Splits `v2` into its sections, taking only a list whose bytes are exactly what the version 2
/// writer writes: major 1, minor 0, no flags, each section at the next multiple of 8 with zeros
/// between them, nothing after the last; at least the root, as many digests as entries; keys of 1
/// to 1024 bytes of UTF-8, sorted, each once. The entries are left to the L2 reader.
fn split(v2: &[u8]) -> Result<V2<'_>, V2Fault> {
    if v2.len() < V2_HEADER_LEN || !v2.starts_with(&V2_MAGIC) {
        return Err(V2Fault::NotV2);
    }
    if le_u16(v2, 8) != 1 || le_u16(v2, 10) != 0 || le_u32(v2, 12) != 0 {
        return Err(V2Fault::Version);
    }
    let mut slices: [&[u8]; 6] = [&[]; 6];
    let mut counts = [0usize; 6];
    let mut at = V2_HEADER_LEN;
    for index in 0..6 {
        let offset = le_u32(v2, 16 + 8 * index) as usize;
        let count = le_u32(v2, 20 + 8 * index) as usize;
        let len = match index {
            0 | 1 => count * RECORD_LEN,
            2 => (count + 1) * 4,
            5 => count * RANGE_LEN,
            _ => count,
        };
        let gap = v2.get(at..offset).ok_or(V2Fault::Layout)?;
        if offset != align8(at) || gap.iter().any(|byte| *byte != 0) {
            return Err(V2Fault::Layout);
        }
        slices[index] = v2.get(offset..offset + len).ok_or(V2Fault::Layout)?;
        counts[index] = count;
        at = offset + len;
    }
    if at != v2.len() {
        return Err(V2Fault::Layout);
    }
    if counts[0] == 0 || counts[1] != counts[0] {
        return Err(V2Fault::Counts);
    }
    let (offsets, key_blob) = (slices[2], slices[3]);
    let mut keys: Vec<&str> = Vec::with_capacity(counts[2]);
    let mut start = 0usize;
    if le_u32(offsets, 0) != 0 {
        return Err(V2Fault::Keys);
    }
    for index in 0..counts[2] {
        let end = le_u32(offsets, 4 * (index + 1)) as usize;
        let key = (start < end && end - start <= MAX_KEY_BYTES)
            .then(|| key_blob.get(start..end))
            .flatten()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .filter(|key| keys.last().is_none_or(|last| *last < *key))
            .ok_or(V2Fault::Keys)?;
        keys.push(key);
        start = end;
    }
    if start != key_blob.len() {
        return Err(V2Fault::Keys);
    }
    Ok(V2 {
        entries: slices[0],
        digests: slices[1],
        keys,
        key_blob,
        names: slices[4],
        ranges: slices[5],
    })
}

/// Whether `spans` (`[start, end)` of each file's bytes) cover `[0, size)` with no gap.
fn covers(mut spans: Vec<(u64, u64)>, size: u64) -> bool {
    spans.sort_unstable();
    let mut covered = 0u64;
    for (start, end) in spans {
        if start > covered {
            return false;
        }
        covered = covered.max(end);
    }
    covered == size
}

/// The L2 sidecar of the version 2 list `v2`, with the cells of each pack the list names as
/// `pack` gives them by key.
///
/// The sidecar holds the list's entries, digests, keys, names and ranges as they are, so its
/// version 2 projection is `v2` byte for byte and its tree id is the SHA-256 of `v2` (the digest of
/// the key `cas/l/<id>` the tree is named by); a reader checks both
/// ([`TreeListL2::open_sidecar`](crate::TreeListL2::open_sidecar)). It adds a PACKS record per key,
/// in the order of the keys, and their cells, pack by pack. A pack's `pack_flags` has bit 0 when the
/// list's files cover its bytes with no gap, bit 1 when [`PackInfo::shared`] says so. The bytes are
/// a function of `v2` and what `pack` gives: the same inputs give the same sidecar.
///
/// Refuses a list that is not exactly what the version 2 writer writes ([`V2Fault`]: its layout,
/// counts, keys, key numbers and empty ranges), a key `pack` has no cells of, a pack of size 0,
/// cells that are not those of the pack's size, and a file or range past the end of its pack. It
/// does not walk the entries (names, parents, the order of children, the digests of empty files):
/// a version 2 list its reader refuses makes a sidecar the L2 reader refuses for the same fault,
/// so the caller hands it a list it has read back, as the CAS service does before it stores one.
pub fn sidecar_from_v2(
    v2: &[u8],
    mut pack: impl FnMut(&str) -> Option<PackInfo>,
) -> Result<Vec<u8>, WriteError> {
    sidecar(v2, &mut pack)
}

/// [`sidecar_from_v2`], made once for every caller's `pack`.
fn sidecar(
    v2: &[u8],
    pack: &mut dyn FnMut(&str) -> Option<PackInfo>,
) -> Result<Vec<u8>, WriteError> {
    let list = split(v2).map_err(WriteError::V2)?;
    let mut infos = Vec::with_capacity(list.keys.len());
    for key in &list.keys {
        let info = pack(key).ok_or_else(|| WriteError::UnknownPack((*key).to_owned()))?;
        if info.cells.size == 0 {
            return Err(WriteError::EmptyPack((*key).to_owned()));
        }
        let expected = cells_of(info.cells.size);
        if info.cells.digests.len() as u64 != expected {
            return Err(WriteError::CellCount {
                key: (*key).to_owned(),
                expected,
                found: info.cells.digests.len() as u64,
            });
        }
        infos.push(info);
    }
    let within = |key: u32, start: u64, len: u64| -> Result<(u64, u64), WriteError> {
        let info = infos
            .get(key as usize)
            .ok_or(WriteError::V2(V2Fault::KeyNumber))?;
        start
            .checked_add(len)
            .filter(|end| *end <= info.cells.size)
            .map(|end| (start, end))
            .ok_or_else(|| WriteError::OutsidePack(list.keys[key as usize].to_owned()))
    };
    let mut spans: Vec<Vec<(u64, u64)>> = vec![Vec::new(); infos.len()];
    for record in list.entries.chunks_exact(RECORD_LEN) {
        let record = Record::of(record);
        if record.kind == KIND_FILE && record.b > 0 {
            // `within` refuses a key number past the keys, so the index below is in bounds.
            let span = within(record.x, record.a, record.b)?;
            spans[record.x as usize].push(span);
        }
    }
    for range in list.ranges.chunks_exact(RANGE_LEN) {
        let len = le_u64(range, 16);
        if len == 0 {
            return Err(WriteError::V2(V2Fault::EmptyRange));
        }
        within(le_u32(range, 0), le_u64(range, 8), len)?;
    }
    let mut packs = Vec::with_capacity(infos.len() * PACK_LEN);
    let mut cells = Vec::new();
    let (mut key_off, mut cell_first) = (0u32, 0u64);
    for ((key, info), spans) in list.keys.iter().zip(&infos).zip(spans) {
        let mut flags = 0u8;
        if covers(spans, info.cells.size) {
            flags |= PACK_FULLY_COVERED;
        }
        if info.shared {
            flags |= PACK_SHARED;
        }
        let count = info.cells.digests.len() as u64;
        packs.extend_from_slice(&key_off.to_le_bytes());
        packs.extend_from_slice(&(key.len() as u32).to_le_bytes());
        packs.extend_from_slice(&info.cells.size.to_le_bytes());
        packs.extend_from_slice(&cell_first.to_le_bytes());
        // At most 2^44 cells of a pack below 2^64 bytes; a count past 32 bits is refused by
        // every reader, as the size is.
        packs.extend_from_slice(&(count as u32).to_le_bytes());
        packs.extend_from_slice(&[CELL_SHIFT, flags, 0, 0]);
        packs.extend_from_slice(&info.upload_part_size.to_le_bytes());
        packs.extend_from_slice(&0u64.to_le_bytes());
        for digest in &info.cells.digests {
            cells.extend_from_slice(digest);
        }
        key_off += key.len() as u32;
        cell_first += count;
    }
    let sections: [(&[u8], u64); 8] = [
        (list.entries, (list.entries.len() / RECORD_LEN) as u64),
        (list.digests, (list.digests.len() / RECORD_LEN) as u64),
        (&packs, infos.len() as u64),
        (&cells, cell_first),
        (list.key_blob, list.key_blob.len() as u64),
        (list.names, list.names.len() as u64),
        (list.ranges, (list.ranges.len() / RANGE_LEN) as u64),
        (&[], 0),
    ];
    let mut header = vec![0u8; HEADER_LEN];
    header[..8].copy_from_slice(&MAGIC);
    header[8..10].copy_from_slice(&MAJOR.to_le_bytes());
    header[16..20].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    header[20] = CELL_DIGEST_SHA256;
    header[21] = CELL_SHIFT;
    header[24..56].copy_from_slice(&tree_id_of(v2));
    let mut at = HEADER_LEN;
    let mut places = [0usize; 8];
    for (index, (bytes, count)) in sections.iter().enumerate() {
        places[index] = align8(at);
        at = places[index] + bytes.len();
        let entry = 56 + 16 * index;
        header[entry..entry + 8].copy_from_slice(&(places[index] as u64).to_le_bytes());
        header[entry + 8..entry + 16].copy_from_slice(&count.to_le_bytes());
    }
    let crc = header_crc(&header);
    header[CRC_AT..CRC_AT + 4].copy_from_slice(&crc.to_le_bytes());
    let mut out = header;
    out.reserve(at - HEADER_LEN);
    for (place, (bytes, _)) in places.iter().zip(&sections) {
        out.resize(*place, 0);
        out.extend_from_slice(bytes);
    }
    Ok(out)
}

/// The SHA-256 of `bytes`.
fn tree_id_of(bytes: &[u8]) -> [u8; 32] {
    let mut id = [0; 32];
    id.copy_from_slice(ring::digest::digest(&SHA256, bytes).as_ref());
    id
}
