//! The state memfd of one entry (5.1): the page a mount maps read-only.
//!
//! The store creates the memfd with `MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL` (not executable and
//! sealed against becoming so, which every `vm.memfd_noexec` setting allows, K25), sizes it to the
//! page, writes the header, maps it read-write and only then applies [`MOUNT_SEALS`]. From then on
//! `F_SEAL_FUTURE_WRITE` refuses every new writable shared mapping and every `write(2)` of the
//! memfd, through any descriptor, while the store's mapping, made before the seal, stays writable;
//! `F_SEAL_SHRINK` and `F_SEAL_GROW` fix its length, and `F_SEAL_SEAL` the seals. A mount can
//! therefore only read the cell bytes and the touch epochs, which only the server writes: it can
//! never mark a cell `present` or change another mount's view.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32};

use rustix::fs::{MemfdFlags, SealFlags};
use tierstore_fetch_api::page::{MOUNT_SEALS, StatePageLayout};

use crate::map::{Mapping, atomic, map_shared};

/// The name of every state memfd (it shows in `/proc/<pid>/fd` and `/proc/<pid>/maps`).
const NAME: &str = "tierstore-state";

/// One entry's sealed state memfd and the store's read-write mapping of it.
#[derive(Debug)]
pub(crate) struct StatePage {
    file: Arc<File>,
    map: Mapping,
    layout: StatePageLayout,
}

impl StatePage {
    /// A new state page of `layout`: every cell `absent` (zero), every touch epoch 0, the header
    /// written, mapped read-write and sealed for mounts.
    pub(crate) fn create(layout: StatePageLayout) -> io::Result<Self> {
        let flags = MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING | MemfdFlags::NOEXEC_SEAL;
        let fd = rustix::fs::memfd_create(NAME, flags)?;
        rustix::fs::ftruncate(&fd, layout.len())?;
        let file = Arc::new(File::from(fd));
        file.write_all_at(&layout.header(), 0)?;
        let map = map_shared(&file, layout.len(), libc::PROT_READ | libc::PROT_WRITE)?;
        rustix::fs::fcntl_add_seals(&*file, SealFlags::from_bits_retain(MOUNT_SEALS))?;
        Ok(Self { file, map, layout })
    }

    /// The memfd, as `open-pack` passes it.
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// The byte of `cell`, or `None` outside the pack.
    pub(crate) fn cell(&self, cell: u32) -> Option<&AtomicU8> {
        if cell >= self.layout.cell_count {
            return None;
        }
        atomic(&self.map, self.layout.cells_offset() + u64::from(cell))
    }

    /// The touch epoch of `group`, or `None` outside the pack.
    pub(crate) fn epoch(&self, group: u32) -> Option<&AtomicU32> {
        if group >= self.layout.group_count() {
            return None;
        }
        atomic(
            &self.map,
            self.layout.epochs_offset() + 4 * u64::from(group),
        )
    }
}
