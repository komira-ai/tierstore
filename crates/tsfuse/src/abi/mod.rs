//! The FUSE kernel ABI of `tsfuse` (docs/design/fetch-cache-serve.md, 5.2, "ABI module";
//! section 18, row T-F1): the `#[repr(C)]` structs of the opcodes the library implements, the
//! parse of a request body per opcode ([`Operation::parse`]), the reply payloads, and INIT
//! negotiation ([`InitConfig::negotiate`]) with the flag list of 5.2.
//!
//! **Pinned header and layout probe.** `crates/tsfuse/abi/fuse.h` is the uapi header of the
//! farm kernel (Linux 7.0, protocol 7.45), byte for byte. `crates/tsfuse/abi/probe.c`, run on a
//! farm node, prints the protocol version and the size, alignment and field offsets of every
//! struct here as the C compiler lays them out; its output is `crates/tsfuse/abi/layout.txt`.
//! `tests/abi_layout.rs` compares each Rust struct with it, and the opcode and flag values with
//! the header. A new farm kernel is a new pinned header and a new probe run, in one change.
//!
//! **Byte order.** Structs are read and written in the host's byte order, as the kernel uses;
//! the three release platforms are little-endian.

mod args;
mod encode;
mod init;
mod op;
mod time;
mod wire;

pub use encode::{DirPushError, DirentBuf, DirentPlusBuf, EntryKind, NAME_MAX};
pub use init::{InitConfig, InitError, InitFlags, InitOutcome, MountKind, Negotiated};
pub use op::{Forgets, INIT_IN_COMPAT_SIZE, Operation, ParseError};
pub use time::Timestamp;
pub use wire::{
    Attr, AttrOut, BatchForgetIn, CreateIn, Dirent, DirentPlus, EntryOut, FlushIn, ForgetIn,
    ForgetOne, FsyncIn, GetattrIn, InHeader, InitIn, InitOut, InterruptIn, Kstatfs, MkdirIn,
    OpenIn, OpenOut, OutHeader, ReadIn, ReleaseIn, RenameIn, SetattrIn, StatfsOut, WriteIn,
    WriteOut,
};

/// The major version of the protocol (`FUSE_KERNEL_VERSION`).
pub const KERNEL_VERSION: u32 = 7;

/// The minor version of the pinned header (`FUSE_KERNEL_MINOR_VERSION`): the newest the library
/// speaks. A newer kernel is answered with this one.
pub const KERNEL_MINOR_VERSION: u32 = 45;

/// The oldest minor version the library speaks: from 7.23 every struct it writes has its full
/// size (`fuse_init_out` grew last, to 64 bytes, in 7.23), so it writes no compat sizes.
pub const MIN_KERNEL_MINOR: u32 = 23;

/// The node id of the root (`FUSE_ROOT_ID`).
pub const ROOT_ID: u64 = 1;

/// The bit of a request's `unique` that marks a request the kernel resent after
/// `FUSE_NOTIFY_RESEND` (`FUSE_UNIQUE_RESEND`); the reply names the `unique` as read.
pub const UNIQUE_RESEND: u64 = 1 << 63;

/// The smallest buffer a reader may read a request into (`FUSE_MIN_READ_BUFFER`).
pub const MIN_READ_BUFFER: usize = 8192;

/// `FOPEN_*`: the flags of an OPEN reply ([`OpenOut::open_flags`]).
pub mod fopen {
    /// `FOPEN_KEEP_CACHE`: keep the page cache of the file across this open.
    pub const KEEP_CACHE: u32 = 1 << 1;
}

/// `FATTR_*`: the fields a SETATTR sets ([`SetattrIn::valid`]); the put directory sets the size
/// and the times.
pub mod fattr {
    /// `FATTR_SIZE`.
    pub const SIZE: u32 = 1 << 3;
    /// `FATTR_ATIME`.
    pub const ATIME: u32 = 1 << 4;
    /// `FATTR_MTIME`.
    pub const MTIME: u32 = 1 << 5;
    /// `FATTR_FH`.
    pub const FH: u32 = 1 << 6;
    /// `FATTR_ATIME_NOW`.
    pub const ATIME_NOW: u32 = 1 << 7;
    /// `FATTR_MTIME_NOW`.
    pub const MTIME_NOW: u32 = 1 << 8;
    /// `FATTR_CTIME`.
    pub const CTIME: u32 = 1 << 10;
}
