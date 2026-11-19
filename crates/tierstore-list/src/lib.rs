//! The L2 tree list of the tierstore read path (`docs/design/fetch-cache-serve.md`, section 3): a
//! tree's version 2 list (`TSTREE`) with a PACKS table and one SHA-256 per 1 MiB cell of every
//! pack, so a reader verifies any cell of a pack on arrival, in any order, and serves a file once
//! the cells that cover it are present.
//!
//! The byte layout is in `docs/tree-list-l2.md` of this crate. In short: a 192-byte header (the
//! magic `TSL2TREE`, the version, the header's length, the digest function, the cell shift, the
//! tree id, the offset and count of eight sections, a CRC-32C), then the sections, each at a
//! multiple of 8: the entries, their SHA-256s, PACKS, CELLS, the keys, the names, the ranges and
//! the extension records. The entries, digests, keys, names and ranges are those of the version 2
//! list, byte for byte, so the version 2 list is the L2 list's projection and the tree keeps its
//! id: the SHA-256 of the version 2 list, the digest of the key `cas/l/<id>` a job names.
//!
//! - [`TreeListL2`] reads one: [`open`](TreeListL2::open) checks it whole, once, and every read
//!   after indexes into the bytes; [`open_sidecar`](TreeListL2::open_sidecar) also refuses a
//!   missing sidecar, a version 1 or 2 list ([`ListVersionUnsupported`](tierstore_fetch_api::list::ListVersionUnsupported))
//!   and a list whose projection is not the tree the id names. It implements
//!   [`ListL2`](tierstore_fetch_api::list::ListL2).
//! - [`CellHasher`] computes a pack's cell digests as its bytes stream; [`sidecar_from_v2`] makes
//!   the L2 sidecar of a version 2 list from its packs' cells, stored at [`sidecar_key`].
//! - [`EntryRecord`] decodes one entry record from its own bytes, for a reader that copies records
//!   out of a mapping at [`TreeListL2::section_range`] instead of reading the list as one slice.
//!
//! Pure: no I/O, no clock, no async.

mod error;
mod layout;
mod open;
mod read;
mod record;
mod walk;
mod write;

pub use error::{
    EntryFault, ExtensionFault, HeaderFault, Malformed, OpenError, PackFault, V2Fault, WriteError,
};
pub use layout::{
    CELL_BYTES, CELL_SHIFT, EXTENSION_REQUIRED, HEADER_LEN, MAGIC, MAJOR, MAX_KEY_BYTES,
    MAX_PATH_BYTES, MAX_TARGET_BYTES, Section,
};
pub use read::{Node, NodeKind, PackRange, TreeListL2, pack_root};
pub use record::{ENTRY_LEN, EntryRecord, RecordKind};
pub use walk::Shape;
pub use write::{
    CellHasher, PACK_SHARED, PackCells, PackInfo, SIDECAR_PREFIX, sidecar_from_v2, sidecar_key,
};
