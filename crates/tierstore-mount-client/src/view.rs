//! A pack's view: the state memfd mapped read-only and the data fd, as one `open-pack` passed
//! them (5.1).
//!
//! The server creates the state memfd, writes the header and the cell bytes, and seals it with
//! [`MOUNT_SEALS`] before it passes it, so the mount can map it `PROT_READ` only and can never mark
//! a cell `present`. The mount refuses a state memfd that lacks any of those seals, whose header is
//! not a state page of this version, whose pack size is not the one `open-pack` answered, or that
//! is shorter than its page. The data fd is the pack's file as the server opened it; the mount
//! never reopens a pack by path.
//!
//! Every load of a cell byte is `SeqCst` (5.1: the reader and the evictor each store to one
//! location and then load the other's).

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use rustix::fs::fcntl_get_seals;
use tierstore_fetch_api::cell::CellByte;
use tierstore_fetch_api::entry::StateView;
use tierstore_fetch_api::page::{
    MOUNT_SEALS, STATE_PAGE_HEADER_LEN, StatePageLayout, sealed_for_mounts,
};
use vm_memory::{FileOffset, MmapRegion, VolatileMemory};

/// Why the descriptors of a `PackOpened` are refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadPackFds {
    /// The state descriptor is not a memfd that allows sealing (`F_GET_SEALS` fails).
    #[error("the state descriptor is not a sealed memfd: {0}")]
    NotMemfd(String),
    /// The state memfd lacks a seal of [`MOUNT_SEALS`]: a mount could change it.
    #[error("the state memfd is not sealed for mounts (seals {seals:#x}, needs {MOUNT_SEALS:#x})")]
    Unsealed {
        /// The seals `F_GET_SEALS` reported.
        seals: u32,
    },
    /// The state memfd's first 64 bytes are not a state page header of this version for a pack of
    /// the size `open-pack` answered.
    #[error("the state memfd's header is not a state page of a {pack_size}-byte pack")]
    BadHeader {
        /// The pack size `open-pack` answered.
        pack_size: u64,
    },
    /// The state memfd is shorter than its page.
    #[error("the state memfd has {have} bytes, its page {need}")]
    Short {
        /// Its length.
        have: u64,
        /// The page's length.
        need: u64,
    },
    /// `PackOpened` came without its data fd and state memfd.
    #[error("open-pack came without its two descriptors")]
    Missing,
    /// The state memfd could not be read or mapped.
    #[error("the state memfd could not be mapped: {0}")]
    Map(String),
}

/// One `open-pack`'s view of a pack: its state page, mapped read-only, and its data fd.
#[derive(Debug)]
pub struct PackView {
    layout: StatePageLayout,
    map: MmapRegion<()>,
    /// The state memfd; the mapping keeps it open too.
    state: Arc<File>,
    data: OwnedFd,
}

impl PackView {
    /// The view of the descriptors of a `PackOpened`, in their order: the data fd, then the state
    /// memfd.
    pub(crate) fn from_fds(pack_size: u64, fds: Vec<OwnedFd>) -> Result<Self, BadPackFds> {
        let mut fds = fds.into_iter();
        match (fds.next(), fds.next()) {
            (Some(data), Some(state)) => Self::new(pack_size, data, state),
            _ => Err(BadPackFds::Missing),
        }
    }

    /// Checks the descriptors a `PackOpened` passed for a pack of `pack_size` bytes and maps the
    /// state page read-only.
    pub(crate) fn new(pack_size: u64, data: OwnedFd, state: OwnedFd) -> Result<Self, BadPackFds> {
        let seals = fcntl_get_seals(&state).map_err(|e| BadPackFds::NotMemfd(e.to_string()))?;
        if !sealed_for_mounts(seals.bits()) {
            return Err(BadPackFds::Unsealed {
                seals: seals.bits(),
            });
        }
        let mut header = [0u8; STATE_PAGE_HEADER_LEN as usize];
        let read = rustix::io::pread(&state, &mut header, 0).map_err(map_failed)?;
        let layout = StatePageLayout::from_header(&header[..read])
            .filter(|l| l.pack_size == pack_size)
            .ok_or(BadPackFds::BadHeader { pack_size })?;
        let have = rustix::fs::fstat(&state).map_err(map_failed)?;
        // A memfd's length is never negative.
        let have = u64::try_from(have.st_size).unwrap_or(0);
        let need = layout.len();
        if have < need {
            return Err(BadPackFds::Short { have, need });
        }
        let state = Arc::new(File::from(state));
        // `need` is a page multiple of a pack's cells: it fits a 64-bit usize.
        let len = usize::try_from(need).map_err(map_failed)?;
        let map = MmapRegion::build(
            Some(FileOffset::from_arc(state.clone(), 0)),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
        )
        .map_err(map_failed)?;
        Ok(Self {
            layout,
            map,
            state,
            data,
        })
    }

    /// The pack's data file.
    #[must_use]
    pub fn data(&self) -> BorrowedFd<'_> {
        self.data.as_fd()
    }

    /// The state memfd (mapped read-only here).
    #[must_use]
    pub fn state_fd(&self) -> BorrowedFd<'_> {
        self.state.as_fd()
    }

    /// The layout of the state page.
    #[must_use]
    pub fn layout(&self) -> StatePageLayout {
        self.layout
    }

    fn cell(&self, cell: u32) -> Option<&AtomicU8> {
        if cell >= self.layout.cell_count {
            return None;
        }
        let at = usize::try_from(self.layout.cells_offset() + u64::from(cell)).ok()?;
        self.map.get_atomic_ref::<AtomicU8>(at).ok()
    }
}

/// A state memfd that could not be read or mapped.
fn map_failed(e: impl std::fmt::Display) -> BadPackFds {
    BadPackFds::Map(e.to_string())
}

impl StateView for PackView {
    fn cell_count(&self) -> u32 {
        self.layout.cell_count
    }

    fn load(&self, cell: u32) -> Option<CellByte> {
        self.cell(cell).map(|b| CellByte(b.load(Ordering::SeqCst)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, memfd_create};
    use tierstore_fetch_api::cell::CellState;
    use tierstore_fetch_api::page::{F_SEAL_SEAL, F_SEAL_SHRINK};

    const SIZE: u64 = 3 * (1 << 20) + 5;

    fn memfd(len: u64) -> OwnedFd {
        let fd = memfd_create("t", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).unwrap();
        ftruncate(&fd, len).unwrap();
        fd
    }

    /// A state memfd of `len` bytes with the header of a pack of `header_size` bytes, cell 1
    /// present and cell 2 bad, sealed with `seals`.
    fn state(len: u64, header_size: u64, seals: u32) -> OwnedFd {
        let fd = memfd(len);
        let layout = StatePageLayout::for_pack(header_size).unwrap();
        rustix::io::pwrite(&fd, &layout.header(), 0).unwrap();
        let cells = [
            CellByte::new(CellState::Absent, false).0,
            CellByte::new(CellState::Present, true).0,
            CellByte::new(CellState::Bad, false).0,
        ];
        rustix::io::pwrite(&fd, &cells, layout.cells_offset()).unwrap();
        fcntl_add_seals(&fd, SealFlags::from_bits_retain(seals)).unwrap();
        fd
    }

    fn page_len() -> u64 {
        StatePageLayout::for_pack(SIZE).unwrap().len()
    }

    #[test]
    fn a_sealed_state_page_is_mapped_read_only_and_read_seqcst() {
        let view = PackView::from_fds(
            SIZE,
            vec![memfd(SIZE), state(page_len(), SIZE, MOUNT_SEALS)],
        )
        .unwrap();
        assert_eq!(view.cell_count(), 4);
        assert_eq!(view.layout().pack_size, SIZE);
        assert_eq!(
            view.load(0).and_then(CellByte::state),
            Some(CellState::Absent)
        );
        assert!(view.load(1).unwrap().servable());
        assert!(view.load(1).unwrap().durable());
        assert_eq!(view.load(2).and_then(CellByte::state), Some(CellState::Bad));
        assert_eq!(view.load(4), None);
        // The mount cannot map the state memfd for writing (section 14: "Mount's state mapping is
        // read-only").
        let file = Arc::new(File::from(view.state_fd().try_clone_to_owned().unwrap()));
        let writable = MmapRegion::<()>::build(
            Some(FileOffset::from_arc(file, 0)),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
        );
        assert!(writable.is_err());
        assert_eq!(rustix::fs::fstat(view.data()).unwrap().st_size as u64, SIZE);
    }

    #[test]
    fn descriptors_that_are_not_a_sealed_state_page_of_the_pack_are_refused() {
        let ok_len = page_len();
        let case = |state: OwnedFd| PackView::new(SIZE, memfd(1), state).unwrap_err();
        // Not a memfd: a socket.
        let (sock, _other) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        assert!(matches!(case(sock), BadPackFds::NotMemfd(_)));
        // Missing a seal.
        let some = F_SEAL_SHRINK | F_SEAL_SEAL;
        assert_eq!(
            case(state(ok_len, SIZE, some)),
            BadPackFds::Unsealed { seals: some }
        );
        // A header of another pack, and no header at all.
        assert_eq!(
            case(state(ok_len, SIZE + (1 << 20), MOUNT_SEALS)),
            BadPackFds::BadHeader { pack_size: SIZE }
        );
        let blank = memfd(ok_len);
        fcntl_add_seals(&blank, SealFlags::from_bits_retain(MOUNT_SEALS)).unwrap();
        assert_eq!(case(blank), BadPackFds::BadHeader { pack_size: SIZE });
        // An empty memfd: the header read is short.
        let empty = memfd(0);
        fcntl_add_seals(&empty, SealFlags::from_bits_retain(MOUNT_SEALS)).unwrap();
        assert_eq!(case(empty), BadPackFds::BadHeader { pack_size: SIZE });
        // Shorter than its page.
        assert_eq!(
            case(state(64 + 3, SIZE, MOUNT_SEALS)),
            BadPackFds::Short {
                have: 67,
                need: ok_len
            }
        );
        // One descriptor where two are passed.
        assert_eq!(
            PackView::from_fds(SIZE, vec![memfd(1)]).unwrap_err(),
            BadPackFds::Missing
        );
        assert!(BadPackFds::Missing.to_string().contains("two descriptors"));
        assert_eq!(map_failed("no memory"), BadPackFds::Map("no memory".into()));
    }
}
