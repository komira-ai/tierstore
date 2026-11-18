//! Shared mappings of a memfd, and atomic references into them.
//!
//! The mapping is `vm_memory::MmapRegion`: a safe `mmap` (no `MAP_FIXED`) that unmaps on drop, and
//! whose `get_atomic_ref` hands out an `&AtomicU8`/`&AtomicU16`/`&AtomicU32` only for an offset
//! that is inside the mapping and aligned for the type. The crate needs no `unsafe` of its own.
//!
//! `mmap` does not check the file's length: a page past the end of the file faults (`SIGBUS`) when
//! touched. Every mapping here is of a memfd sealed against shrinking (`F_SEAL_SHRINK`), and its
//! length is at most the memfd's length when it is made, so it never has such a page.

use std::fs::File;
use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use vm_memory::{AtomicInteger, FileOffset, MmapRegion, VolatileMemory};

/// One shared mapping of a memfd.
pub(crate) type Mapping = MmapRegion<()>;

/// Maps the first `len` bytes of `file`, shared, with the protection `prot` (`PROT_READ`, or
/// `PROT_READ | PROT_WRITE`).
pub(crate) fn map_shared(file: &Arc<File>, len: u64, prot: i32) -> io::Result<Mapping> {
    // A memfd the store maps is at most a few MiB; a u64 length fits a 64-bit usize.
    let len = usize::try_from(len).map_err(io::Error::other)?;
    MmapRegion::build(
        Some(FileOffset::from_arc(file.clone(), 0)),
        len,
        prot,
        libc::MAP_SHARED,
    )
    .map_err(io::Error::other)
}

/// The atomic at byte `offset` of `map`, or `None` when it is not inside the mapping or not
/// aligned for `A`.
pub(crate) fn atomic<A: AtomicInteger>(map: &Mapping, offset: u64) -> Option<&A> {
    map.get_atomic_ref::<A>(usize::try_from(offset).ok()?).ok()
}

/// `SeqCst`, the one ordering of every access to a shared cell byte or counter (5.1: the reader
/// and the evictor each store to one location and then load the other's).
pub(crate) const SEQ: Ordering = Ordering::SeqCst;
