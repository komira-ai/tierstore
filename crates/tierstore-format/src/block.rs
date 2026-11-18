//! Blocks (10.2): their geometry, the building of a block and its trailer, and chains.
//!
//! **Geometry.** A block is at most `cold.block_bytes` long (default 1 GiB), its trailer
//! included. `cold.block_reserve` (default 1 MiB) is the room kept for a record's header, its
//! alignment and a one-entry trailer, so an object of at most `block_bytes - block_reserve`
//! ([`BlockGeometry::max_object`]) always fits in one block alone, whatever its key; a larger
//! object is a **chain**. [`BlockGeometry::new`] refuses a reserve too small for that.
//!
//! **Building.** A [`BlockBuilder`] follows a block's bytes from offset 0, in one pass and without
//! holding them, and gives the block's trailer at the end. It takes the bytes in either of two
//! ways, which give the same trailer for the same bytes:
//! - **writing**: [`BlockBuilder::append`] encodes a whole record, or [`BlockBuilder::begin`],
//!   [`BlockBuilder::data`] and [`BlockBuilder::end`] frame a record whose data streams through
//!   (an object of hundreds of MiB is never buffered); each returns the bytes the caller writes,
//!   in order;
//! - **reading**: [`BlockBuilder::feed`] parses bytes already written (a segment's committed
//!   prefix after a restart, a copy that converges to a sealed length), in pieces of any size.
//!
//! The two can follow each other at a record boundary: a writer that restarts feeds the committed
//! prefix of its open segment and appends after it. The trailer is a deterministic function of
//! the block's bytes and its kind ([`BlockBuilder::finish`]): every entry is taken from the bytes,
//! the cell table and the blake3 are computed over them, and the entries are sorted, so two copies
//! of the same bytes give byte-identical trailers.
//!
//! The layout by kind:
//! - a **segment** ([`BlockKind::Segment`]): records one after the other, each padded to 16 bytes
//!   (the records of a hot segment in arrival order);
//! - a **pack block** ([`BlockKind::Pack`]): records whose headers start at multiples of 4 KiB,
//!   zero bytes between them; their trailer entries carry [`RecordFlags::RELOCATE`] (10.5: the
//!   assembly of a pack block repoints the index entries of its objects);
//! - a **chain slice** ([`BlockKind::ChainSlice`]): exactly one record, with
//!   [`RecordFlags::CHAIN_SLICE`], at offset 0.
//!
//! A record's data is at most [`BlockGeometry::max_object`] bytes (a larger object is a chain,
//! whatever its key, so the boundary is the same for every key), and it fits a block when the
//! block's records, this one included, and the trailer that would
//! then follow them are at most `block_bytes` ([`BlockBuilder::fits`]): the trailer grows by one
//! entry per record and one crc per MiB, so a segment of many small records seals before its
//! records reach `block_bytes - block_reserve`, and a block is never longer than `block_bytes`.
//!
//! Every byte between records and after a record's data up to its alignment must be zero; the
//! builder refuses anything else, so a flipped padding byte is an error rather than a trailer of
//! other bytes.
//!
//! **Chains.** An object larger than [`BlockGeometry::max_object`] is stored as consecutive
//! blocks (consecutive ids of one allocating node), slice `i` in block `first + i`, each slice
//! [`BlockGeometry::max_object`] bytes long except the last ([`Chain`]). A slice's record carries
//! the whole object's generation and the slice's offset in the object. A range read touches only
//! the slices it covers ([`Chain::covering`]).

use crate::digest::DataHasher;
use crate::ids::{BlockId, COUNTER_MAX, Generation};
use crate::record::{
    RECORD_ALIGN, RECORD_HEADER_LEN, RecordFlags, RecordHeader, RecordIn, record_len,
};
use crate::stamp::Stamp;
use crate::trailer::{BlockKind, TRAILER_FOOTER_LEN, Trailer, TrailerEntry};
use crate::{FormatError, cells_for, crc32c_append};

/// The default `cold.block_bytes`: 1 GiB [I].
pub const DEFAULT_BLOCK_BYTES: u64 = 1 << 30;

/// The default `cold.block_reserve`: 1 MiB [I].
pub const DEFAULT_BLOCK_RESERVE: u64 = 1 << 20;

/// The alignment of a record header in a pack block, and the unit `block_bytes` is a multiple of.
pub const PACK_ALIGN: u64 = 4096;

/// The bytes of a trailer entry before its key (the key follows, then padding to 8 bytes).
pub const TRAILER_ENTRY_FIXED_LEN: u64 = 48;

const WHAT: &str = "block";

/// The bytes one trailer entry of a key of `key_len` bytes takes.
#[must_use]
pub fn trailer_entry_len(key_len: u16) -> u64 {
    (TRAILER_ENTRY_FIXED_LEN + u64::from(key_len)).next_multiple_of(8)
}

/// The bytes of a trailer whose entries take `entries_len` bytes, after records of `data_len`
/// bytes: the entries, one crc32c per cell, the footer.
#[must_use]
pub fn trailer_len(entries_len: u64, data_len: u64) -> u64 {
    entries_len + 4 * cells_for(data_len) + TRAILER_FOOTER_LEN as u64
}

/// The size limits of a block: `cold.block_bytes` and `cold.block_reserve` (10.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockGeometry {
    block_bytes: u64,
    block_reserve: u64,
}

impl Default for BlockGeometry {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl BlockGeometry {
    /// 1 GiB blocks with a 1 MiB reserve, the defaults of both deployment profiles (21).
    pub const DEFAULT: Self = Self {
        block_bytes: DEFAULT_BLOCK_BYTES,
        block_reserve: DEFAULT_BLOCK_RESERVE,
    };

    /// The geometry of `block_bytes` (a positive multiple of 4 KiB) and `block_reserve` (below
    /// `block_bytes`, and enough for the header of a record with the longest key, its padding
    /// and a one-entry trailer, so an object of `block_bytes - block_reserve` bytes fits alone).
    pub fn new(block_bytes: u64, block_reserve: u64) -> Result<Self, FormatError> {
        if block_bytes == 0 || !block_bytes.is_multiple_of(PACK_ALIGN) {
            return Err(invalid("block_bytes is not a positive multiple of 4 KiB"));
        }
        if block_reserve >= block_bytes {
            return Err(invalid("block_reserve is not below block_bytes"));
        }
        let geometry = Self {
            block_bytes,
            block_reserve,
        };
        if lone_record_len(u16::MAX, geometry.max_object()) > block_bytes {
            return Err(invalid(
                "block_reserve cannot hold a header, its padding and a one-entry trailer",
            ));
        }
        Ok(geometry)
    }

    /// `cold.block_bytes`: the most bytes a block takes, its trailer included.
    #[must_use]
    pub const fn block_bytes(self) -> u64 {
        self.block_bytes
    }

    /// `cold.block_reserve`.
    #[must_use]
    pub const fn block_reserve(self) -> u64 {
        self.block_reserve
    }

    /// The largest object one block holds alone, `block_bytes - block_reserve`; also the length of
    /// every slice of a chain but the last.
    #[must_use]
    pub const fn max_object(self) -> u64 {
        self.block_bytes - self.block_reserve
    }

    /// Whether an object of `size` bytes is stored as a chain (it is larger than
    /// [`BlockGeometry::max_object`]).
    #[must_use]
    pub const fn is_chain(self, size: u64) -> bool {
        size > self.max_object()
    }

    /// The chain of an object of `size` bytes whose first slice is in block `first`; an error if
    /// the object is not a chain or the last slice's block id would pass the node's counters.
    pub fn chain(self, first: BlockId, size: u64) -> Result<Chain, FormatError> {
        if !self.is_chain(size) {
            return Err(invalid(
                "an object of at most max_object bytes is not a chain",
            ));
        }
        let slice = self.max_object();
        let blocks = size.div_ceil(slice);
        // `blocks >= 2`, and the last block's counter must stay a counter. `first.counter()` is
        // at most `COUNTER_MAX`, so the subtraction does not underflow.
        if blocks - 1 > COUNTER_MAX - first.counter() {
            return Err(invalid("the chain's blocks pass the node's last block id"));
        }
        Ok(Chain { first, size, slice })
    }
}

/// The bytes of a block holding only a record of `key_len` key bytes and `data_len` data bytes at
/// offset 0, and its one-entry trailer.
fn lone_record_len(key_len: u16, data_len: u64) -> u64 {
    let end = record_len(key_len, data_len);
    end.saturating_add(trailer_len(trailer_entry_len(key_len), end))
}

/// The slices of a chain: an object larger than one block holds, in consecutive blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chain {
    first: BlockId,
    size: u64,
    slice: u64,
}

/// One slice of a chain, or the part of one that a range covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Slice {
    /// The slice's index in the chain.
    pub index: u64,
    /// The block that holds the slice.
    pub block: BlockId,
    /// Where the bytes start in the object.
    pub object_offset: u64,
    /// Where the bytes start in the slice's data.
    pub slice_offset: u64,
    /// The bytes.
    pub len: u64,
}

impl Chain {
    /// The block of the first slice.
    #[must_use]
    pub const fn first(self) -> BlockId {
        self.first
    }

    /// The object's size.
    #[must_use]
    pub const fn size(self) -> u64 {
        self.size
    }

    /// The length of every slice but the last.
    #[must_use]
    pub const fn slice_len(self) -> u64 {
        self.slice
    }

    /// The number of slices, and of blocks (at least 2).
    #[must_use]
    pub const fn blocks(self) -> u64 {
        self.size.div_ceil(self.slice)
    }

    /// Slice `index` whole, or `None` past the last.
    #[must_use]
    pub fn slice(self, index: u64) -> Option<Slice> {
        if index >= self.blocks() {
            return None;
        }
        let object_offset = index * self.slice;
        Some(Slice {
            index,
            // `Chain` is made only when the last block's counter is a counter.
            block: BlockId::from_raw(self.first.raw() + index),
            object_offset,
            slice_offset: 0,
            len: self.slice.min(self.size - object_offset),
        })
    }

    /// The parts of the slices that the range `[offset, offset + len)` of the object covers, in
    /// order; nothing for an empty range. An error for a range past the object's end.
    pub fn covering(self, offset: u64, len: u64) -> Result<Vec<Slice>, FormatError> {
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= self.size)
            .ok_or(invalid("a range past the end of the chain"))?;
        let mut out = Vec::new();
        let mut at = offset;
        while at < end {
            let index = at / self.slice;
            let start = index * self.slice;
            // The last slice of an object near `u64::MAX` bytes may end past `u64::MAX` on the grid.
            let slice_end = start.saturating_add(self.slice).min(end);
            out.push(Slice {
                index,
                block: BlockId::from_raw(self.first.raw() + index),
                object_offset: at,
                slice_offset: at - start,
                len: slice_end - at,
            });
            at = slice_end;
        }
        Ok(out)
    }
}

/// Why a record cannot be added to a block.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockError {
    /// The record and the trailer that would follow it do not fit the block: the writer seals the
    /// block (or places the record elsewhere) and nothing was added.
    #[error("block: the record needs a block of {need} bytes, the limit is {limit}")]
    Full {
        /// The block's length with the record and its trailer.
        need: u64,
        /// `block_bytes`.
        limit: u64,
    },
    /// The bytes or the calls are not a block of this kind. After such an error from
    /// [`BlockBuilder::data`], [`BlockBuilder::end`] or [`BlockBuilder::feed`], or from a call
    /// made out of order, the builder refuses every later call, since bytes it already counted
    /// are not a block; an error of [`BlockBuilder::begin`] about the record itself (its key, its
    /// flags, its offset) adds nothing and leaves the builder usable.
    #[error(transparent)]
    Format(#[from] FormatError),
}

/// A record to frame when its data streams through [`BlockBuilder::data`]: everything the header
/// carries, the data's length and crc32c declared up front.
#[derive(Debug, Clone, Copy)]
pub struct RecordStart<'a> {
    /// The flags.
    pub flags: RecordFlags,
    /// The key bytes.
    pub key: &'a [u8],
    /// The length of the data that will follow.
    pub data_len: u64,
    /// The crc32c of that data; [`BlockBuilder::end`] checks the data against it.
    pub data_crc: u32,
    /// The object's generation.
    pub generation: Generation,
    /// The stamp.
    pub stamp: Stamp,
    /// The object offset of a chain slice, else 0.
    pub object_offset: u64,
}

/// A block's trailer as [`BlockBuilder::finish`] gives it: decoded and encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    /// The trailer, its entries sorted.
    pub trailer: Trailer,
    /// Its bytes, which follow the records in the block (and are the `.idx` files).
    pub bytes: Vec<u8>,
}

/// Where the builder is in the block's bytes.
#[derive(Debug, Clone)]
enum State {
    /// Between records (or at offset 0).
    Boundary,
    /// Reading: zero bytes before the next record header, `left` of them still to come.
    Gap { left: u64 },
    /// Reading: the header and key bytes of a record, gathered until they are whole.
    Head(Vec<u8>),
    /// A record's data, `left` bytes still to come, `crc` the crc32c of the data so far.
    Data {
        left: u64,
        crc: u32,
        declared: u32,
        /// Whether the record came from [`BlockBuilder::feed`] (else from `begin`).
        fed: bool,
        /// The zero bytes after the data that end the record.
        pad: u64,
    },
    /// Reading: the zero bytes after a record's data, `left` of them still to come.
    Pad { left: u64 },
    /// An earlier error: the bytes counted are not a block.
    Broken,
}

/// Follows a block's bytes and gives its trailer (see the module documentation).
#[derive(Debug, Clone)]
pub struct BlockBuilder {
    geometry: BlockGeometry,
    kind: BlockKind,
    align: u64,
    entries: Vec<TrailerEntry>,
    entries_len: u64,
    digest: DataHasher,
    state: State,
}

impl BlockBuilder {
    /// A builder of an empty block of `kind` ([`BlockKind::Other`] is refused: this release does
    /// not know its layout).
    pub fn new(geometry: BlockGeometry, kind: BlockKind) -> Result<Self, FormatError> {
        let align = match kind {
            BlockKind::Segment => RECORD_ALIGN as u64,
            BlockKind::Pack | BlockKind::ChainSlice => PACK_ALIGN,
            BlockKind::Other(_) => return Err(invalid("a block kind this release does not write")),
        };
        Ok(Self {
            geometry,
            kind,
            align,
            entries: Vec::new(),
            entries_len: 0,
            digest: DataHasher::new(),
            state: State::Boundary,
        })
    }

    /// The block's kind.
    #[must_use]
    pub fn kind(&self) -> BlockKind {
        self.kind
    }

    /// The bytes of the block so far.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.digest.len()
    }

    /// Whether the block has no byte yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.digest.is_empty()
    }

    /// The number of records so far (one being written or read included).
    #[must_use]
    pub fn records(&self) -> usize {
        self.entries.len()
    }

    /// Whether a record of `key_len` key bytes and `data_len` data bytes fits after the records so
    /// far ([`BlockBuilder::begin`] would not answer [`BlockError::Full`]). A chain slice block
    /// fits one record only, and no block fits data longer than [`BlockGeometry::max_object`]
    /// (such an object is a chain, whatever its key).
    #[must_use]
    pub fn fits(&self, key_len: u16, data_len: u64) -> bool {
        matches!(self.state, State::Boundary)
            && (self.kind != BlockKind::ChainSlice || self.entries.is_empty())
            && data_len <= self.geometry.max_object()
            && self.need(self.len(), key_len, data_len).1 <= self.geometry.block_bytes
    }

    /// For a record after the first `at` bytes of the block: the offset its header starts at, and
    /// the block's length with that record and the trailer.
    fn need(&self, at: u64, key_len: u16, data_len: u64) -> (u64, u64) {
        let start = at.next_multiple_of(self.align);
        let end = start.saturating_add(record_len(key_len, data_len));
        let entries = self.entries_len + trailer_entry_len(key_len);
        (start, end.saturating_add(trailer_len(entries, end)))
    }

    /// Adds a whole record and returns the bytes to write: the zero bytes that align it, the
    /// header, the key, the data and the padding.
    pub fn append(&mut self, rec: &RecordIn<'_>) -> Result<Vec<u8>, BlockError> {
        let start = RecordStart {
            flags: rec.flags,
            key: rec.key,
            data_len: rec.data.len() as u64,
            data_crc: crate::crc32c(rec.data),
            generation: rec.generation,
            stamp: rec.stamp,
            object_offset: rec.object_offset,
        };
        let mut out = self.begin(&start)?;
        self.data(rec.data)?;
        out.extend_from_slice(rec.data);
        out.extend(self.end()?);
        Ok(out)
    }

    /// Starts a record whose data the caller streams through [`BlockBuilder::data`], and returns
    /// the bytes to write before the data: the zero bytes that align it, the header and the key.
    /// [`BlockError::Full`] when it does not fit (nothing is added; the block stays usable).
    pub fn begin(&mut self, rec: &RecordStart<'_>) -> Result<Vec<u8>, BlockError> {
        if !matches!(self.state, State::Boundary) {
            return Err(self.misuse("a record starts while another is open or bytes are pending"));
        }
        let key_len =
            u16::try_from(rec.key.len()).map_err(|_| invalid("key longer than 65535 bytes"))?;
        let header = RecordHeader {
            flags: rec.flags,
            key_len,
            data_len: rec.data_len,
            generation: rec.generation,
            stamp: rec.stamp,
            object_offset: rec.object_offset,
            data_crc: rec.data_crc,
            key_crc: crate::crc32c(rec.key),
        };
        let start = self.admit(self.len(), &header)?;
        let mut out = vec![0u8; (start - self.len()) as usize];
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(rec.key);
        self.digest.update(&out);
        self.open(header, rec.key, start, false);
        Ok(out)
    }

    /// The next bytes of the data of the record that [`BlockBuilder::begin`] started; the caller
    /// writes them itself. An error for more bytes than declared.
    pub fn data(&mut self, bytes: &[u8]) -> Result<(), BlockError> {
        match &mut self.state {
            State::Data {
                left,
                crc,
                fed: false,
                ..
            } if bytes.len() as u64 <= *left => {
                *left -= bytes.len() as u64;
                *crc = crc32c_append(*crc, bytes);
                self.digest.update(bytes);
                Ok(())
            }
            _ => Err(self.misuse("data beyond the declared length, or with no record started")),
        }
    }

    /// Ends the record that [`BlockBuilder::begin`] started once all its data went through
    /// [`BlockBuilder::data`], checks the data's crc32c against the declared one, and returns the
    /// padding to write after the data.
    pub fn end(&mut self) -> Result<Vec<u8>, BlockError> {
        match self.state {
            State::Data {
                left: 0,
                crc,
                declared,
                fed: false,
                pad,
            } => {
                if crc != declared {
                    self.state = State::Broken;
                    return Err(FormatError::BadCrc {
                        what: WHAT,
                        stored: declared,
                        computed: crc,
                    }
                    .into());
                }
                let out = vec![0u8; pad as usize];
                self.digest.update(&out);
                self.state = State::Boundary;
                Ok(out)
            }
            _ => Err(self.misuse("a record ends before its data, or with no record started")),
        }
    }

    /// Reads bytes of the block that are already written, from where the builder is: records of
    /// this kind and their padding, in pieces of any size. Every record's header, key and data
    /// crc32c is checked; an error leaves the builder refusing every later call.
    pub fn feed(&mut self, mut bytes: &[u8]) -> Result<(), BlockError> {
        while !bytes.is_empty() {
            let used = self.feed_some(bytes).inspect_err(|_| {
                self.state = State::Broken;
            })?;
            bytes = &bytes[used..];
        }
        Ok(())
    }

    /// Takes some of `bytes` in the current state and returns how many: none when it only moves
    /// from a record boundary to the gap or header that follows it.
    fn feed_some(&mut self, bytes: &[u8]) -> Result<usize, BlockError> {
        match &mut self.state {
            State::Boundary => {
                let gap = self.len().next_multiple_of(self.align) - self.len();
                self.state = if gap > 0 {
                    State::Gap { left: gap }
                } else {
                    State::Head(Vec::new())
                };
                Ok(0)
            }
            State::Gap { left } | State::Pad { left } => {
                // `left` is below the alignment (4 KiB), so it fits a `usize`.
                let n = bytes.len().min(*left as usize);
                if bytes[..n].iter().any(|b| *b != 0) {
                    return Err(invalid("a padding byte is not zero").into());
                }
                *left -= n as u64;
                let done = *left == 0;
                let gap = matches!(self.state, State::Gap { .. });
                self.digest.update(&bytes[..n]);
                if done {
                    self.state = if gap {
                        State::Head(Vec::new())
                    } else {
                        State::Boundary
                    };
                }
                Ok(n)
            }
            State::Head(head) => {
                let want = if head.len() < RECORD_HEADER_LEN {
                    RECORD_HEADER_LEN
                } else {
                    // The header is whole and decoded; its key length is valid.
                    RECORD_HEADER_LEN + usize::from(RecordHeader::decode(head)?.key_len)
                };
                let n = bytes.len().min(want - head.len());
                head.extend_from_slice(&bytes[..n]);
                let head = std::mem::take(head);
                self.digest.update(&bytes[..n]);
                self.read_head(head)?;
                Ok(n)
            }
            State::Data {
                left,
                crc,
                fed: true,
                ..
            } => {
                let n = bytes
                    .len()
                    .min(usize::try_from(*left).unwrap_or(usize::MAX));
                *left -= n as u64;
                *crc = crc32c_append(*crc, &bytes[..n]);
                self.digest.update(&bytes[..n]);
                self.data_done()?;
                Ok(n)
            }
            State::Data { fed: false, .. } => {
                Err(invalid("bytes fed while a record started by begin is open").into())
            }
            State::Broken => Err(broken().into()),
        }
    }

    /// The header and key bytes gathered so far: wait for more, or open the record.
    fn read_head(&mut self, head: Vec<u8>) -> Result<(), BlockError> {
        if head.len() < RECORD_HEADER_LEN {
            self.state = State::Head(head);
            return Ok(());
        }
        let header = RecordHeader::decode(&head)?;
        let key_end = RECORD_HEADER_LEN + usize::from(header.key_len);
        if head.len() < key_end {
            self.state = State::Head(head);
            return Ok(());
        }
        let key = &head[RECORD_HEADER_LEN..key_end];
        crate::codec::expect_crc(WHAT, key, header.key_crc)?;
        // The header starts where the gap before it ended, at an aligned offset, so `admit` places
        // it there.
        let start = self.admit(self.len() - key_end as u64, &header)?;
        self.open(header, key, start, true);
        self.data_done()
    }

    /// Ends a fed record whose data is complete: checks its crc and moves to its padding.
    fn data_done(&mut self) -> Result<(), BlockError> {
        if let State::Data {
            left: 0,
            crc,
            declared,
            fed: true,
            pad,
        } = self.state
        {
            if crc != declared {
                return Err(FormatError::BadCrc {
                    what: WHAT,
                    stored: declared,
                    computed: crc,
                }
                .into());
            }
            self.state = if pad > 0 {
                State::Pad { left: pad }
            } else {
                State::Boundary
            };
        }
        Ok(())
    }

    /// Checks that a record of `header` may follow the first `at` bytes of the block and returns
    /// its header's offset.
    fn admit(&self, at: u64, header: &RecordHeader) -> Result<u64, BlockError> {
        let chain = header.flags.contains(RecordFlags::CHAIN_SLICE);
        if chain != (self.kind == BlockKind::ChainSlice) {
            return Err(invalid(
                "a chain slice record outside a chain slice block, or the reverse",
            )
            .into());
        }
        if !chain && header.object_offset != 0 {
            return Err(invalid("an object offset on a record that is not a chain slice").into());
        }
        if header.data_len > self.geometry.max_object() {
            return Err(
                invalid("an object larger than max_object is a chain, never one record").into(),
            );
        }
        if self.kind == BlockKind::ChainSlice && !self.entries.is_empty() {
            return Err(invalid("a chain slice block holds one record").into());
        }
        let (start, need) = self.need(at, header.key_len, header.data_len);
        if need > self.geometry.block_bytes {
            return Err(BlockError::Full {
                need,
                limit: self.geometry.block_bytes,
            });
        }
        Ok(start)
    }

    /// Lists a record whose header starts at `start` and moves to its data.
    fn open(&mut self, header: RecordHeader, key: &[u8], start: u64, fed: bool) {
        let mut flags = header.flags;
        if self.kind == BlockKind::Pack {
            flags = flags.with(RecordFlags::RELOCATE);
        }
        let total = record_len(header.key_len, header.data_len);
        let unpadded = RECORD_HEADER_LEN as u64 + u64::from(header.key_len) + header.data_len;
        self.entries_len += trailer_entry_len(header.key_len);
        self.entries.push(TrailerEntry {
            key: key.to_vec(),
            stamp: header.stamp,
            offset: start,
            length: header.data_len,
            generation: header.generation,
            crc32c: header.data_crc,
            flags,
        });
        self.state = State::Data {
            left: header.data_len,
            crc: 0,
            declared: header.data_crc,
            fed,
            pad: total - unpadded,
        };
    }

    /// The block's trailer: the entries sorted, the cell table and the blake3 of the records'
    /// bytes. An error if a record is open, or the bytes fed end inside padding or a record.
    pub fn finish(&self) -> Result<Sealed, BlockError> {
        if !matches!(self.state, State::Boundary) {
            return Err(match self.state {
                State::Broken => broken(),
                _ => invalid("the block ends inside a record or its padding"),
            }
            .into());
        }
        let digest = self.digest.finish();
        let mut entries = self.entries.clone();
        entries.sort_by(|a, b| (&a.key, a.stamp, a.offset).cmp(&(&b.key, b.stamp, b.offset)));
        let trailer = Trailer {
            kind: self.kind,
            entries,
            cell_crcs: digest.cell_crcs,
            data_len: digest.len,
            blake3: digest.blake3,
        };
        let bytes = trailer.encode()?;
        Ok(Sealed { trailer, bytes })
    }

    /// The error of a call made in the wrong state; a builder that was broken stays broken and
    /// says so.
    fn misuse(&mut self, reason: &'static str) -> BlockError {
        let err = match self.state {
            State::Broken => broken(),
            _ => invalid(reason),
        };
        self.state = State::Broken;
        err.into()
    }
}

fn invalid(reason: &'static str) -> FormatError {
    FormatError::Invalid { what: WHAT, reason }
}

fn broken() -> FormatError {
    invalid("an earlier error left the bytes counted not a block")
}
