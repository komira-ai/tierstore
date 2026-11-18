//! [`MemfdCounters`]: the server's read-only mapping of one holder's counter memfd (5.1).
//!
//! A mount creates one counter memfd for its whole life, maps it read-write, allocates a region
//! of `u16` reader counters per open pack inside it, and grows it (`ftruncate`) as it opens more
//! packs. It sends the memfd at handshake; the server maps it `PROT_READ`. When an `open-pack`
//! names a region that ends past the server's mapping, [`HolderCounters::ensure_len`] remaps the
//! memfd at the larger length before the `open-pack` is acknowledged, so the evictor never reads
//! past its mapping.
//!
//! **The memfd must carry `F_SEAL_SHRINK`.** The server maps a file another process can resize;
//! a page of the mapping past the end of the file would fault (`SIGBUS`) when the evictor reads
//! it and take the whole server down. With `F_SEAL_SHRINK` the memfd can only grow, and the
//! server maps at most its length at the time of the remap, so a read inside the mapping is always
//! inside the file. [`MemfdCounters::new`] refuses a memfd without the seal. The mount therefore
//! creates its counter memfd with `MFD_ALLOW_SEALING` and adds `F_SEAL_SHRINK` (and not
//! `F_SEAL_GROW`) before the handshake; design 5.1 and the handshake's documentation say so.

use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU16, AtomicU32};
use std::sync::{Arc, PoisonError, RwLock};

use rustix::fs::SealFlags;
use tierstore_fetch_api::entry::{EntryError, HolderCounters};
use tierstore_fetch_api::page::PAGE_BYTES;

use crate::map::{Mapping, SEQ, atomic, map_shared};

/// Why a holder's counter memfd is refused at handshake.
#[derive(Debug, thiserror::Error)]
pub enum CountersError {
    /// The memfd can shrink, so a mapping of it could fault.
    #[error(
        "the counter memfd is not sealed against shrinking (F_SEAL_SHRINK); its seals are {seals:#x}"
    )]
    NotSealed {
        /// The seals `F_GET_SEALS` reported.
        seals: u32,
    },
    /// `F_GET_SEALS` failed: the descriptor is not a memfd that allows sealing.
    #[error("reading the seals of the counter memfd: {source}")]
    Seals {
        /// The error.
        #[source]
        source: io::Error,
    },
}

/// One holder's counter memfd, mapped read-only by the server.
pub struct MemfdCounters {
    file: Arc<File>,
    /// The mapping, replaced whole by a remap; `None` until the first [`HolderCounters::ensure_len`].
    map: RwLock<Option<Mapping>>,
    /// The mappings made.
    maps: AtomicU32,
}

impl MemfdCounters {
    /// The counters of the memfd `fd` a holder sent at handshake, not mapped yet (the first
    /// `open-pack` maps them). A memfd without `F_SEAL_SHRINK` is [`CountersError::NotSealed`];
    /// a descriptor that is not a sealable memfd is [`CountersError::Seals`].
    pub fn new(fd: OwnedFd) -> Result<Self, CountersError> {
        let seals = rustix::fs::fcntl_get_seals(&fd)
            .map_err(io::Error::from)
            .map_err(|source| CountersError::Seals { source })?;
        if !seals.contains(SealFlags::SHRINK) {
            return Err(CountersError::NotSealed {
                seals: seals.bits(),
            });
        }
        Ok(Self {
            file: Arc::new(File::from(fd)),
            map: RwLock::new(None),
            maps: AtomicU32::new(0),
        })
    }

    /// How many mappings were made: the first one and every remap.
    #[must_use]
    pub fn maps(&self) -> u32 {
        self.maps.load(SEQ)
    }
}

impl fmt::Debug for MemfdCounters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemfdCounters")
            .field("len", &self.len())
            .field("maps", &self.maps())
            .finish()
    }
}

impl HolderCounters for MemfdCounters {
    fn len(&self) -> u64 {
        let map = self.map.read().unwrap_or_else(PoisonError::into_inner);
        map.as_ref().map_or(0, |m| m.size() as u64)
    }

    /// Remaps at `len` rounded up to a page, but never past the memfd's length, so the mapping has
    /// no page past the end of the file. A memfd shorter than `len` is
    /// [`EntryError::CountersMap`] (the holder named a region it did not allocate).
    fn ensure_len(&self, len: u64) -> Result<(), EntryError> {
        let mut map = self.map.write().unwrap_or_else(PoisonError::into_inner);
        if len <= map.as_ref().map_or(0, |m| m.size() as u64) {
            return Ok(());
        }
        let failed = |source| EntryError::CountersMap { len, source };
        let size = rustix::fs::fstat(&*self.file)
            .map_err(io::Error::from)
            .map_err(failed)?;
        let size = u64::try_from(size.st_size).unwrap_or(0);
        if len > size {
            return Err(failed(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("the counter memfd has {size} bytes"),
            )));
        }
        // `len <= size <= i64::MAX`: the rounding cannot overflow.
        let want = len.next_multiple_of(PAGE_BYTES).min(size);
        *map = Some(map_shared(&self.file, want, libc::PROT_READ).map_err(failed)?);
        self.maps.fetch_add(1, SEQ);
        Ok(())
    }

    fn load(&self, offset: u64) -> Option<u16> {
        let map = self.map.read().unwrap_or_else(PoisonError::into_inner);
        atomic::<AtomicU16>(map.as_ref()?, offset).map(|c| c.load(SEQ))
    }
}
