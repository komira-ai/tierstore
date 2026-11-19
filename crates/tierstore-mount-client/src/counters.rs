//! The mount's counter memfd and its regions (5.1).
//!
//! A mount creates **one counter memfd for its whole life** with `MFD_ALLOW_SEALING |
//! MFD_NOEXEC_SEAL` (not executable, which every `vm.memfd_noexec` setting allows, K25), seals it
//! `F_SEAL_SHRINK` (never `F_SEAL_GROW`) and sends it at every handshake; the server maps it
//! read-only and refuses one without that seal. Inside it the mount allocates a **region per open
//! pack**: one `u16` reader counter per 16-cell group ([`CounterRegion`]), at an offset that is a
//! multiple of 8. The memfd grows (`ftruncate`) as the mount opens more packs; the server remaps
//! before it acknowledges an `open-pack` whose region ends past its mapping.
//!
//! **Growth without remapping.** A reader thread holds references into the mount's own mapping
//! while it reads, so the mount never replaces a mapping. The memfd grows by **chunks**: each
//! chunk is a mapping of its own of a new part of the file, and a region never straddles two
//! chunks. A chunk lives as long as a pack whose region is in it.
//!
//! A region is never freed: a pack stays open (and its holder's hold stays) until the mount
//! releases or exits, so its counters are needed until then (7).

use std::fs::File;
use std::io;
#[cfg(test)]
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use rustix::fs::{MemfdFlags, SealFlags};
use tierstore_fetch_api::entry::ReaderCounters;
use tierstore_fetch_api::page::{CounterRegion, F_SEAL_SHRINK, PAGE_BYTES};
use vm_memory::{FileOffset, MmapRegion, VolatileMemory};

/// The name of the counter memfd (it shows in `/proc/<pid>/fd`).
const NAME: &str = "tierstore-counters";

/// The smallest chunk the memfd grows by: 64 KiB, the counters of 32768 groups, so a mount that
/// opens a hundred packs of a few GiB each grows its memfd once.
pub(crate) const CHUNK_BYTES: u64 = 64 << 10;

/// One shared read-write mapping of a part of the counter memfd.
type Mapping = MmapRegion<()>;

/// A chunk: where it starts in the memfd, and its mapping.
#[derive(Debug)]
struct Chunk {
    start: u64,
    map: Mapping,
}

/// The mount's counter memfd: created and sealed once, grown by chunks.
#[derive(Debug)]
pub(crate) struct CounterMemfd {
    file: Arc<File>,
    /// The chunk regions are allocated in now, and the bytes of it already allocated.
    current: Option<(Arc<Chunk>, u64)>,
    /// The memfd's length.
    len: u64,
}

impl CounterMemfd {
    /// A new, empty counter memfd, sealed against shrinking.
    pub(crate) fn create() -> io::Result<Self> {
        let flags = MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING | MemfdFlags::NOEXEC_SEAL;
        let fd = rustix::fs::memfd_create(NAME, flags)?;
        rustix::fs::fcntl_add_seals(&fd, SealFlags::from_bits_retain(F_SEAL_SHRINK))?;
        Ok(Self {
            file: Arc::new(File::from(fd)),
            current: None,
            len: 0,
        })
    }

    /// The memfd.
    #[cfg(test)]
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// The memfd, shared with the frames of the handshakes that send it.
    pub(crate) fn file(&self) -> Arc<File> {
        self.file.clone()
    }

    /// Takes the memfd to be `len` bytes long with no chunk to allocate in, so a test reaches the
    /// end of a `u64`.
    #[cfg(test)]
    pub(crate) fn set_len(&mut self, len: u64) {
        self.len = len;
        self.current = None;
    }

    /// A new region of `groups` counters, every counter 0. The memfd is grown first when the
    /// region does not fit the current chunk, so the region is inside the file before an
    /// `open-pack` names it.
    pub(crate) fn allocate(&mut self, groups: u32) -> io::Result<PackCounters> {
        let region_len = CounterRegion { offset: 0, groups }.len();
        let fits = |(chunk, used): &(Arc<Chunk>, u64)| {
            used.checked_add(region_len)
                .is_some_and(|end| end <= chunk.map.size() as u64)
        };
        let (chunk, used) = match self.current.take().filter(fits) {
            Some(current) => current,
            None => (self.grow(region_len)?, 0),
        };
        let region = CounterRegion {
            offset: chunk.start + used,
            groups,
        };
        self.current = Some((chunk.clone(), used + region_len));
        Ok(PackCounters {
            chunk,
            at: used,
            region,
        })
    }

    /// Grows the memfd by a chunk of at least `need` bytes and maps the chunk.
    fn grow(&mut self, need: u64) -> io::Result<Arc<Chunk>> {
        let size = need.max(CHUNK_BYTES).next_multiple_of(PAGE_BYTES);
        let start = self.len;
        let end = start
            .checked_add(size)
            .ok_or_else(|| io::Error::other("the counter memfd would pass u64::MAX"))?;
        rustix::fs::ftruncate(&*self.file, end)?;
        self.len = end;
        let map = map_chunk(&self.file, start, size)?;
        Ok(Arc::new(Chunk { start, map }))
    }
}

/// Maps `len` bytes of `file` from `start`, shared and read-write.
fn map_chunk(file: &Arc<File>, start: u64, len: u64) -> io::Result<Mapping> {
    let len = usize::try_from(len).map_err(io::Error::other)?;
    MmapRegion::build(
        Some(FileOffset::from_arc(file.clone(), start)),
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
    )
    .map_err(io::Error::other)
}

/// One open pack's reader counters: its region of the counter memfd (5.1 reader steps 1 and 5).
#[derive(Debug, Clone)]
pub struct PackCounters {
    chunk: Arc<Chunk>,
    /// Where the region starts in its chunk.
    at: u64,
    region: CounterRegion,
}

impl PackCounters {
    /// The region, as `open-pack` names it.
    #[must_use]
    pub fn region(&self) -> CounterRegion {
        self.region
    }

    /// The counter of `group`, or `None` outside the region.
    fn counter(&self, group: u32) -> Option<&AtomicU16> {
        if group >= self.region.groups {
            return None;
        }
        let at = usize::try_from(self.at + 2 * u64::from(group)).ok()?;
        self.chunk.map.get_atomic_ref::<AtomicU16>(at).ok()
    }

    /// The counter of `group` now (`SeqCst`), or `None` outside the region.
    #[must_use]
    pub fn load(&self, group: u32) -> Option<u16> {
        self.counter(group).map(|c| c.load(Ordering::SeqCst))
    }
}

impl ReaderCounters for PackCounters {
    /// `fetch_add(1, SeqCst)`; a group outside the region is not counted (no read of the pack
    /// covers it).
    fn enter(&self, group: u32) {
        if let Some(c) = self.counter(group) {
            c.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// `fetch_sub(1, SeqCst)`; a group outside the region is not counted.
    fn leave(&self, group: u32) {
        if let Some(c) = self.counter(group) {
            c.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::{fcntl_get_seals, fstat};

    fn len(m: &CounterMemfd) -> u64 {
        fstat(m.fd()).unwrap().st_size as u64
    }

    #[test]
    fn the_memfd_is_sealed_against_shrinking_and_exec_only() {
        let m = CounterMemfd::create().unwrap();
        let seals = fcntl_get_seals(m.fd()).unwrap();
        // `MFD_NOEXEC_SEAL` (5.1, K25): not executable, and sealed against becoming so.
        assert_eq!(seals, SealFlags::SHRINK | SealFlags::EXEC);
        assert_eq!(fstat(m.fd()).unwrap().st_mode & 0o111, 0);
        assert_eq!(len(&m), 0);
        // It can grow, never shrink.
        rustix::fs::ftruncate(m.fd(), 4096).unwrap();
        assert!(rustix::fs::ftruncate(m.fd(), 0).is_err());
    }

    #[test]
    fn regions_are_aligned_inside_chunks_and_a_region_never_straddles_two() {
        let mut m = CounterMemfd::create().unwrap();
        let a = m.allocate(3).unwrap();
        assert_eq!(
            a.region(),
            CounterRegion {
                offset: 0,
                groups: 3
            }
        );
        assert_eq!(len(&m), CHUNK_BYTES);
        // 3 groups take 6 bytes, rounded to 8.
        let b = m.allocate(1).unwrap();
        assert_eq!(
            b.region(),
            CounterRegion {
                offset: 8,
                groups: 1
            }
        );
        // A pack of no cells has an empty region.
        let empty = m.allocate(0).unwrap();
        assert_eq!(
            empty.region(),
            CounterRegion {
                offset: 16,
                groups: 0
            }
        );
        // The rest of the chunk exactly.
        let rest = (CHUNK_BYTES - 16) / 2;
        let c = m.allocate(u32::try_from(rest).unwrap()).unwrap();
        assert_eq!(c.region().offset, 16);
        assert_eq!(c.region().end(), Some(CHUNK_BYTES));
        assert_eq!(len(&m), CHUNK_BYTES);
        // The next region starts a chunk.
        let d = m.allocate(1).unwrap();
        assert_eq!(d.region().offset, CHUNK_BYTES);
        assert_eq!(len(&m), 2 * CHUNK_BYTES);
        // A region larger than a chunk gets a chunk of its own, rounded to pages.
        let groups = u32::try_from(CHUNK_BYTES).unwrap();
        let e = m.allocate(groups).unwrap();
        assert_eq!(e.region().offset, 2 * CHUNK_BYTES);
        assert_eq!(len(&m), 4 * CHUNK_BYTES);
        let f = m.allocate(1).unwrap();
        assert_eq!(f.region().offset, 4 * CHUNK_BYTES);
        for r in [&a, &b, &c, &d, &e, &f] {
            assert!(r.region().is_valid());
        }
    }

    #[test]
    fn a_memfd_that_would_pass_u64_max_allocates_nothing() {
        let mut m = CounterMemfd::create().unwrap();
        m.set_len(u64::MAX - 100);
        let err = m.allocate(1).unwrap_err();
        assert_eq!(err.to_string(), "the counter memfd would pass u64::MAX");
    }

    #[test]
    fn counters_count_in_the_shared_memfd_and_ignore_groups_outside_the_region() {
        let mut m = CounterMemfd::create().unwrap();
        let _first = m.allocate(5).unwrap();
        let c = m.allocate(2).unwrap();
        c.enter(1);
        c.enter(1);
        c.enter(0);
        c.leave(1);
        c.enter(2);
        c.leave(2);
        assert_eq!(c.load(0), Some(1));
        assert_eq!(c.load(1), Some(1));
        assert_eq!(c.load(2), None);
        // What another mapping of the memfd (the server's) reads at the region's offsets.
        let mut raw = [0u8; 4];
        rustix::io::pread(m.fd(), &mut raw, c.region().offset).unwrap();
        assert_eq!(raw, [1, 0, 1, 0]);
        // The clone shares the counters.
        let twin = c.clone();
        twin.leave(0);
        assert_eq!(c.load(0), Some(0));
    }
}
