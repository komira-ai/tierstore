//! INIT: the flags and limits a session negotiates (docs/design/fetch-cache-serve.md, 5.2,
//! "INIT flags to negotiate").
//!
//! The library asks for the flags of its mount kind and is granted those the kernel offered;
//! it never assumes one the kernel did not offer, and it reports the required flags the
//! kernel lacks so the session can say what runs without them. Both sides use the smaller
//! minor version.

use std::fmt;

use super::wire::{InitIn, InitOut};
use super::{KERNEL_MINOR_VERSION, KERNEL_VERSION, MIN_KERNEL_MINOR};
use crate::reply::Errno;

/// A set of `FUSE_*` INIT flags: the low 32 bits travel in `flags`, the high 32 in `flags2`
/// (with [`InitFlags::INIT_EXT`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct InitFlags(pub u64);

impl InitFlags {
    /// `FUSE_ASYNC_READ`: more than one READ of a file in flight (without it the kernel
    /// serialises reads per file).
    pub const ASYNC_READ: Self = Self(1 << 0);
    /// `FUSE_SPLICE_WRITE`: replies may be spliced into the device.
    pub const SPLICE_WRITE: Self = Self(1 << 7);
    /// `FUSE_SPLICE_READ`: requests may be spliced out of the device (put mount).
    pub const SPLICE_READ: Self = Self(1 << 9);
    /// `FUSE_DO_READDIRPLUS`: the kernel sends READDIRPLUS.
    pub const DO_READDIRPLUS: Self = Self(1 << 13);
    /// `FUSE_READDIRPLUS_AUTO`: the kernel chooses READDIRPLUS or READDIR adaptively.
    pub const READDIRPLUS_AUTO: Self = Self(1 << 14);
    /// `FUSE_NO_OPEN_SUPPORT`: an OPEN answered `ENOSYS` is not sent again (zero-message opens).
    pub const NO_OPEN_SUPPORT: Self = Self(1 << 17);
    /// `FUSE_PARALLEL_DIROPS`: LOOKUP and READDIR of one directory run in parallel.
    pub const PARALLEL_DIROPS: Self = Self(1 << 18);
    /// `FUSE_MAX_PAGES`: `max_pages` of the reply sets the largest request.
    pub const MAX_PAGES: Self = Self(1 << 22);
    /// `FUSE_CACHE_SYMLINKS`: READLINK replies are cached.
    pub const CACHE_SYMLINKS: Self = Self(1 << 23);
    /// `FUSE_NO_OPENDIR_SUPPORT`: an OPENDIR answered `ENOSYS` is not sent again.
    pub const NO_OPENDIR_SUPPORT: Self = Self(1 << 24);
    /// `FUSE_INIT_EXT`: the INIT messages carry `flags2`.
    pub const INIT_EXT: Self = Self(1 << 30);
    /// `FUSE_HAS_RESEND`: the kernel resends pending requests on `FUSE_NOTIFY_RESEND`, and
    /// marks them with [`super::UNIQUE_RESEND`].
    pub const HAS_RESEND: Self = Self(1 << 39);

    /// The flags of this type with their header names, in bit order.
    const NAMED: [(Self, &'static str); 12] = [
        (Self::ASYNC_READ, "ASYNC_READ"),
        (Self::SPLICE_WRITE, "SPLICE_WRITE"),
        (Self::SPLICE_READ, "SPLICE_READ"),
        (Self::DO_READDIRPLUS, "DO_READDIRPLUS"),
        (Self::READDIRPLUS_AUTO, "READDIRPLUS_AUTO"),
        (Self::NO_OPEN_SUPPORT, "NO_OPEN_SUPPORT"),
        (Self::PARALLEL_DIROPS, "PARALLEL_DIROPS"),
        (Self::MAX_PAGES, "MAX_PAGES"),
        (Self::CACHE_SYMLINKS, "CACHE_SYMLINKS"),
        (Self::NO_OPENDIR_SUPPORT, "NO_OPENDIR_SUPPORT"),
        (Self::INIT_EXT, "INIT_EXT"),
        (Self::HAS_RESEND, "HAS_RESEND"),
    ];

    /// No flag.
    pub const EMPTY: Self = Self(0);

    /// Whether every flag of `other` is in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The flags in `self` or `other`.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The flags in both.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// The flags of `self` not in `other`.
    #[must_use]
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether no flag is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The flags of an INIT request: `flags2` counts only with `FUSE_INIT_EXT` in `flags`.
    #[must_use]
    pub fn offered(init: &InitIn) -> Self {
        let low = Self(u64::from(init.flags));
        if low.contains(Self::INIT_EXT) {
            Self(low.0 | u64::from(init.flags2) << 32)
        } else {
            low
        }
    }
}

/// `InitFlags(ASYNC_READ | HAS_RESEND | 0x4)`: the named flags in bit order, then any other
/// bits in hex.
impl fmt::Debug for InitFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut rest = *self;
        let mut parts = Vec::new();
        for (flag, name) in Self::NAMED {
            if self.contains(flag) {
                parts.push(name.to_owned());
                rest = rest.difference(flag);
            }
        }
        if !rest.is_empty() || parts.is_empty() {
            parts.push(format!("{:#x}", rest.0));
        }
        write!(f, "InitFlags({})", parts.join(" | "))
    }
}

/// The kind of mount a session serves; it decides the flags asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MountKind {
    /// A read-only tree.
    Tree,
    /// The put directory (writes).
    Put,
}

impl MountKind {
    /// The flags 5.2 requires of this kind of mount: `ASYNC_READ`, `PARALLEL_DIROPS`,
    /// `MAX_PAGES`, `DO_READDIRPLUS`, `READDIRPLUS_AUTO`, `CACHE_SYMLINKS`, `SPLICE_WRITE`,
    /// `HAS_RESEND`, `NO_OPEN_SUPPORT`, `NO_OPENDIR_SUPPORT`, and for the put mount
    /// `SPLICE_READ`.
    #[must_use]
    pub const fn required(self) -> InitFlags {
        let tree = InitFlags::ASYNC_READ
            .union(InitFlags::PARALLEL_DIROPS)
            .union(InitFlags::MAX_PAGES)
            .union(InitFlags::DO_READDIRPLUS)
            .union(InitFlags::READDIRPLUS_AUTO)
            .union(InitFlags::CACHE_SYMLINKS)
            .union(InitFlags::SPLICE_WRITE)
            .union(InitFlags::HAS_RESEND)
            .union(InitFlags::NO_OPEN_SUPPORT)
            .union(InitFlags::NO_OPENDIR_SUPPORT);
        match self {
            Self::Tree => tree,
            Self::Put => tree.union(InitFlags::SPLICE_READ),
        }
    }
}

/// The limits a session asks for at INIT. The kernel lowers some of them (`max_readahead` to
/// what it offered, `max_pages` to its `max_pages_limit` sysctl); it keeps `max_write` (at least
/// 4096), so a reader's buffer must hold 80 bytes more than `max_write`, but sends WRITEs of at
/// most `max_pages` pages. The reply carries what was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitConfig {
    /// The kind of mount.
    pub kind: MountKind,
    /// The readahead limit, bytes (answered as the smaller of this and the kernel's offer).
    pub max_readahead: u32,
    /// The largest WRITE, bytes (the kernel uses at least 4096).
    pub max_write: u32,
    /// The largest request, pages (sent with `MAX_PAGES` only).
    pub max_pages: u16,
    /// The most background requests (readahead and async reads) in flight.
    pub max_background: u16,
    /// The background requests at which the kernel marks the connection congested (answered
    /// as at most `max_background`).
    pub congestion_threshold: u16,
    /// The time granularity of the attributes, nanoseconds.
    pub time_gran: u32,
}

impl InitConfig {
    /// A read-only tree: a tiny `max_write` (the tree is read-only, so the reader's buffers stay
    /// 8 KiB), requests of up to 256 pages (1 MiB, one cell), readahead 128 KiB, and 1024
    /// background requests (the value the mount runs with today; the kernel default of 12
    /// wedges a busy build node).
    #[must_use]
    pub const fn tree() -> Self {
        Self {
            kind: MountKind::Tree,
            max_readahead: 128 * 1024,
            max_write: 4096,
            max_pages: 256,
            max_background: 1024,
            congestion_threshold: 768,
            time_gran: 1,
        }
    }

    /// The put directory: as [`Self::tree`] with a `max_write` of 1 MiB (256 pages).
    #[must_use]
    pub const fn put() -> Self {
        Self {
            kind: MountKind::Put,
            max_write: 1024 * 1024,
            ..Self::tree()
        }
    }

    /// The reply to `init`.
    ///
    /// # Errors
    ///
    /// [`InitError::Unsupported`] for a kernel of major version below 7, or of version 7 below
    /// [`MIN_KERNEL_MINOR`]; the session answers `EPROTO`.
    pub fn negotiate(&self, init: &InitIn) -> Result<InitOutcome, InitError> {
        if init.major > KERNEL_VERSION {
            // The kernel speaks a newer major version: answer with ours and wait for an INIT
            // of it (uapi `fuse.h`, "Version negotiation").
            return Ok(InitOutcome::MajorAhead(InitOut {
                major: KERNEL_VERSION,
                minor: KERNEL_MINOR_VERSION,
                ..InitOut::default()
            }));
        }
        if init.major < KERNEL_VERSION || init.minor < MIN_KERNEL_MINOR {
            return Err(InitError::Unsupported {
                major: init.major,
                minor: init.minor,
            });
        }
        let offered = InitFlags::offered(init);
        let required = self.kind.required();
        let mut granted = required.intersection(offered);
        if offered.contains(InitFlags::INIT_EXT) {
            granted = granted.union(InitFlags::INIT_EXT);
        }
        let max_pages = if granted.contains(InitFlags::MAX_PAGES) {
            self.max_pages
        } else {
            0
        };
        let reply = InitOut {
            major: KERNEL_VERSION,
            minor: init.minor.min(KERNEL_MINOR_VERSION),
            max_readahead: self.max_readahead.min(init.max_readahead),
            flags: low_bits(granted),
            max_background: self.max_background,
            congestion_threshold: self.congestion_threshold.min(self.max_background),
            max_write: self.max_write,
            time_gran: self.time_gran,
            max_pages,
            flags2: high_bits(granted),
            ..InitOut::default()
        };
        Ok(InitOutcome::Accepted(Negotiated {
            minor: reply.minor,
            offered,
            granted,
            missing: required.difference(offered),
            reply,
        }))
    }
}

fn low_bits(flags: InitFlags) -> u32 {
    (flags.0 & u64::from(u32::MAX)) as u32
}

fn high_bits(flags: InitFlags) -> u32 {
    (flags.0 >> 32) as u32
}

/// What INIT came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    /// The session is initialised with these terms.
    Accepted(Negotiated),
    /// The kernel's major version is newer: this reply (our version only) asks it for an INIT
    /// of ours, and the session is not initialised yet.
    MajorAhead(InitOut),
}

/// The terms a session was initialised with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    /// The minor version both sides use.
    pub minor: u32,
    /// The flags the kernel offered.
    pub offered: InitFlags,
    /// The flags granted: those asked for and offered, and `INIT_EXT` if offered.
    pub granted: InitFlags,
    /// The required flags the kernel did not offer: the session runs without them.
    pub missing: InitFlags,
    /// The reply.
    pub reply: InitOut,
}

/// Why INIT failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InitError {
    /// The kernel's version is older than the library supports.
    #[error(
        "FUSE kernel protocol {major}.{minor} is older than the {KERNEL_VERSION}.{MIN_KERNEL_MINOR} tsfuse needs"
    )]
    Unsupported {
        /// The kernel's major version.
        major: u32,
        /// The kernel's minor version.
        minor: u32,
    },
}

impl InitError {
    /// The errno INIT is answered with: `EPROTO`.
    #[must_use]
    pub fn errno(&self) -> Errno {
        Errno::EPROTO
    }
}
