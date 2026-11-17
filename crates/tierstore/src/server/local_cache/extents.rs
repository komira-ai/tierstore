//! Cache entries by extents: a pack of which only some ranges are in the cache.
//!
//! A tree made by the create-tree call names files that are spans of larger objects (packs); the
//! cache holds what the tree uses of a pack and not the whole pack. An entry by extents is a
//! sparse file of the pack's full size, with a sidecar that lists the ranges that are filled and
//! were checked against the SHA-256 the caller gave (module `files` has the layout of the
//! directory `extents-v1`). It is no object of the cache's index: `Index::get` never answers for
//! it, so a read of the pack's key by the ordinary paths finds no entry and cannot be given a
//! hole.
//!
//! - [`LocalCache::fill_range`] fills `[offset, offset + len)` of a pack from the upstream with
//!   one streaming pass that hashes the bytes against `sha256`; the range is recorded as filled
//!   only if it hashes to it. A range that is held already (by an entry by extents, or by the
//!   whole object in `obj/`) asks the upstream nothing. The fills of one range are one flight:
//!   a second request for the same `(pack, range, sha256)` joins it, and a request for a range
//!   that overlaps a running flight of the pack waits for it and looks again, so that no two
//!   fills write the same bytes of a file. A fill takes a permit of the fill socket's pool
//!   (`fill_socket_concurrency`, waited for up to `fill_socket_wait`) and never one of
//!   `max_concurrent_fills`.
//! - [`LocalCache::fill_parts`] fills several parts of a pack, each with its own digest, in one
//!   such pass from the first part's start to the last one's end: each part is hashed as its
//!   bytes pass and recorded as filled if it hashes to its digest, and the bytes between the
//!   parts are read and not recorded. It is one flight as a fill of one range is, of the whole
//!   span it reads: a fill of the same parts joins it, and a fill that overlaps it waits.
//! - [`LocalCache::span_of`] says which file holds a range and where: the pack's own file, in
//!   `obj/` if the pack is whole and in `extents-v1/` if not. Nothing is copied: the span is a
//!   range of the pack's file.
//! - [`LocalCache::pin_packs`] pins packs for a tree (the pins of module `pins`, which are per
//!   object and count the object's bytes once however many trees pin it). An entry by extents
//!   is pinned for the bytes it has filled, and the pin grows when the entry does.
//! - An ordinary ranged read of the pack's key is served from the entry if the range is held
//!   (and `revalidate_after` is not set), and is otherwise a miss, filled or passed through as it
//!   is today; the entry is never read for a range it does not hold.
//! - An entry that is filled whole is moved into `obj/` under the same name, as an ordinary
//!   object: the file is hashed once, hard-linked and its name in `extents-v1/` removed. A
//!   pin of the entry stays.
//! - The entries count, by the bytes they have filled, in the cache's bytes: in the budget, in
//!   the critical mark of a fill and in the eviction pass, which removes the least recently used
//!   of whole objects and entries together, and skips a pinned entry and one a fill writes to. An
//!   entry that a write or a delete through the cache says is gone is removed.
//! - At start the directory is scanned: an entry whose file has not the size its sidecar says,
//!   has no range, or is whole in `obj/` too is removed, and the rest is read back with its
//!   ranges. A read-only cache reads none: the holder of the lock owns the directory.
//!
//! What a power cut can leave: the cache does not sync, so a sidecar may list a range whose
//! bytes did not reach the disk. The data is written before the sidecar, so a crash of the
//! process cannot. A range that failed its hash leaves its bytes in the file, outside any range,
//! until the entry goes; an entry that has no range at the end of its last fill is removed.

mod counts;
mod error;
mod files;
mod fill;
mod map;
mod read;
mod table;

#[cfg(test)]
mod files_tests;
#[cfg(test)]
mod fill_parts_tests;
#[cfg(test)]
mod fill_tests;
#[cfg(test)]
mod map_tests;
#[cfg(test)]
mod read_tests;
#[cfg(test)]
mod table_tests;

pub use error::RangeError;
pub(super) use fill::ExtentState;
pub use fill::{PartsWant, RangePart, RangeWant};
pub use read::Span;
pub(super) use read::{build_index, scan};
pub(super) use table::ExtentTable;
