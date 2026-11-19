//! The `#[repr(C)]` structs of the uapi `fuse.h` that the implemented opcodes read and write,
//! field for field (a field the header calls `type` is `typ` here). Each derives zerocopy's
//! `FromBytes` and `IntoBytes`, so the compiler refuses a struct with implicit padding, and is
//! read from and written as bytes in the host's byte order, as the kernel does.
//! `tests/abi_layout.rs` compares the size, alignment and every field offset with the layout
//! probe's output for the pinned header.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Defines a wire struct, its `SIZE`, and its byte conversions.
macro_rules! wire {
    ($(
        $(#[$meta:meta])*
        pub struct $name:ident {
            $( $(#[$fmeta:meta])* pub $field:ident: $ty:ty, )*
        }
    )*) => {$(
        $(#[$meta])*
        #[repr(C)]
        #[derive(
            Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
        )]
        pub struct $name {
            $( $(#[$fmeta])* pub $field: $ty, )*
        }

        impl $name {
            /// The struct's size in bytes, as the kernel reads and writes it.
            pub const SIZE: usize = size_of::<Self>();

            /// The value at the start of `bytes` (copied: `bytes` needs no alignment) and the
            /// bytes after it; `None` if `bytes` is shorter than [`Self::SIZE`].
            #[must_use]
            pub fn read_prefix(bytes: &[u8]) -> Option<(Self, &[u8])> {
                <Self as FromBytes>::read_from_prefix(bytes).ok()
            }

            /// The value's bytes, as written to the device.
            #[must_use]
            pub fn as_bytes(&self) -> &[u8] {
                IntoBytes::as_bytes(self)
            }
        }
    )*};
}

wire! {
    /// `fuse_in_header`: the header of every request.
    pub struct InHeader {
        /// The request's length, header included.
        pub len: u32,
        /// The opcode.
        pub opcode: u32,
        /// The request id; bit 63 marks a resent request ([`super::UNIQUE_RESEND`]).
        pub unique: u64,
        /// The node the request is about.
        pub nodeid: u64,
        /// The caller's uid.
        pub uid: u32,
        /// The caller's gid.
        pub gid: u32,
        /// The caller's pid.
        pub pid: u32,
        /// The length of the extensions after the arguments, in 8-byte units.
        pub total_extlen: u16,
        /// Padding.
        pub padding: u16,
    }

    /// `fuse_out_header`: the header of every reply.
    pub struct OutHeader {
        /// The reply's length, header included.
        pub len: u32,
        /// 0, or a negated errno.
        pub error: i32,
        /// The id of the request answered.
        pub unique: u64,
    }

    /// `fuse_attr`: the attributes of a node. Times are seconds since the epoch (a negative
    /// time as its two's complement, [`super::Timestamp::wire_secs`]) and nanoseconds.
    pub struct Attr {
        /// The inode number.
        pub ino: u64,
        /// The size in bytes.
        pub size: u64,
        /// The number of 512-byte blocks.
        pub blocks: u64,
        /// The access time, seconds.
        pub atime: u64,
        /// The modification time, seconds.
        pub mtime: u64,
        /// The status change time, seconds.
        pub ctime: u64,
        /// The access time, nanoseconds.
        pub atimensec: u32,
        /// The modification time, nanoseconds.
        pub mtimensec: u32,
        /// The status change time, nanoseconds.
        pub ctimensec: u32,
        /// The file type and mode bits (`st_mode`).
        pub mode: u32,
        /// The link count.
        pub nlink: u32,
        /// The owner.
        pub uid: u32,
        /// The group.
        pub gid: u32,
        /// The device number of a device node.
        pub rdev: u32,
        /// The preferred I/O size.
        pub blksize: u32,
        /// `FUSE_ATTR_*` flags.
        pub flags: u32,
    }

    /// `fuse_kstatfs`: the statistics of the file system.
    pub struct Kstatfs {
        /// Total blocks.
        pub blocks: u64,
        /// Free blocks.
        pub bfree: u64,
        /// Free blocks available to an unprivileged user.
        pub bavail: u64,
        /// Total inodes.
        pub files: u64,
        /// Free inodes.
        pub ffree: u64,
        /// The block size.
        pub bsize: u32,
        /// The longest name.
        pub namelen: u32,
        /// The fragment size.
        pub frsize: u32,
        /// Padding.
        pub padding: u32,
        /// Spare.
        pub spare: [u32; 6],
    }

    /// `fuse_entry_out`: the reply to LOOKUP (and the entry of CREATE and READDIRPLUS).
    pub struct EntryOut {
        /// The node id; 0 in READDIRPLUS means "no entry, only the name".
        pub nodeid: u64,
        /// The generation: `nodeid:generation` is unique for the life of the file system.
        pub generation: u64,
        /// How long the name stays valid, seconds.
        pub entry_valid: u64,
        /// How long the attributes stay valid, seconds.
        pub attr_valid: u64,
        /// How long the name stays valid, nanoseconds.
        pub entry_valid_nsec: u32,
        /// How long the attributes stay valid, nanoseconds.
        pub attr_valid_nsec: u32,
        /// The attributes.
        pub attr: Attr,
    }

    /// `fuse_forget_in`: the argument of FORGET.
    pub struct ForgetIn {
        /// The number of lookups to forget.
        pub nlookup: u64,
    }

    /// `fuse_forget_one`: one node of BATCH_FORGET.
    pub struct ForgetOne {
        /// The node.
        pub nodeid: u64,
        /// The number of lookups to forget.
        pub nlookup: u64,
    }

    /// `fuse_batch_forget_in`: the argument of BATCH_FORGET, followed by `count`
    /// [`ForgetOne`].
    pub struct BatchForgetIn {
        /// The number of [`ForgetOne`] that follow.
        pub count: u32,
        /// Unused.
        pub dummy: u32,
    }

    /// `fuse_getattr_in`: the argument of GETATTR.
    pub struct GetattrIn {
        /// `FUSE_GETATTR_*` flags.
        pub getattr_flags: u32,
        /// Unused.
        pub dummy: u32,
        /// The file handle, if `FUSE_GETATTR_FH` is set.
        pub fh: u64,
    }

    /// `fuse_attr_out`: the reply to GETATTR and SETATTR.
    pub struct AttrOut {
        /// How long the attributes stay valid, seconds.
        pub attr_valid: u64,
        /// How long the attributes stay valid, nanoseconds.
        pub attr_valid_nsec: u32,
        /// Unused.
        pub dummy: u32,
        /// The attributes.
        pub attr: Attr,
    }

    /// `fuse_mkdir_in`: the argument of MKDIR, followed by the name.
    pub struct MkdirIn {
        /// The mode of the new directory.
        pub mode: u32,
        /// The caller's umask.
        pub umask: u32,
    }

    /// `fuse_rename_in`: the argument of RENAME, followed by the old and the new name.
    pub struct RenameIn {
        /// The new parent directory.
        pub newdir: u64,
    }

    /// `fuse_rename2_in`: the argument of RENAME2 (a rename with `RENAME_*` flags), followed by
    /// the old and the new name.
    pub struct Rename2In {
        /// The new parent directory.
        pub newdir: u64,
        /// The `RENAME_*` flags of `renameat2(2)`.
        pub flags: u32,
        /// Padding.
        pub padding: u32,
    }

    /// `fuse_mknod_in`: the argument of MKNOD, followed by the name.
    pub struct MknodIn {
        /// The mode of the new node (its type and permission bits).
        pub mode: u32,
        /// The device number of a device node.
        pub rdev: u32,
        /// The caller's umask.
        pub umask: u32,
        /// Padding.
        pub padding: u32,
    }

    /// `fuse_link_in`: the argument of LINK, followed by the new name; the request's node is the
    /// new parent.
    pub struct LinkIn {
        /// The node linked.
        pub oldnodeid: u64,
    }

    /// `fuse_setattr_in`: the argument of SETATTR; `valid` says which fields are set
    /// ([`super::fattr`]).
    pub struct SetattrIn {
        /// The `FATTR_*` bits of the fields that are set.
        pub valid: u32,
        /// Padding.
        pub padding: u32,
        /// The file handle (`FATTR_FH`).
        pub fh: u64,
        /// The new size (`FATTR_SIZE`).
        pub size: u64,
        /// The lock owner (`FATTR_LOCKOWNER`).
        pub lock_owner: u64,
        /// The access time, seconds (`FATTR_ATIME`).
        pub atime: u64,
        /// The modification time, seconds (`FATTR_MTIME`).
        pub mtime: u64,
        /// The status change time, seconds (`FATTR_CTIME`).
        pub ctime: u64,
        /// The access time, nanoseconds.
        pub atimensec: u32,
        /// The modification time, nanoseconds.
        pub mtimensec: u32,
        /// The status change time, nanoseconds.
        pub ctimensec: u32,
        /// The new mode (`FATTR_MODE`).
        pub mode: u32,
        /// Unused.
        pub unused4: u32,
        /// The new owner (`FATTR_UID`).
        pub uid: u32,
        /// The new group (`FATTR_GID`).
        pub gid: u32,
        /// Unused.
        pub unused5: u32,
    }

    /// `fuse_open_in`: the argument of OPEN and OPENDIR.
    pub struct OpenIn {
        /// The `open(2)` flags.
        pub flags: u32,
        /// `FUSE_OPEN_*` flags.
        pub open_flags: u32,
    }

    /// `fuse_create_in`: the argument of CREATE, followed by the name.
    pub struct CreateIn {
        /// The `open(2)` flags.
        pub flags: u32,
        /// The mode of the new file.
        pub mode: u32,
        /// The caller's umask.
        pub umask: u32,
        /// `FUSE_OPEN_*` flags.
        pub open_flags: u32,
    }

    /// `fuse_open_out`: the reply to OPEN and OPENDIR (and the handle of CREATE).
    pub struct OpenOut {
        /// The file handle.
        pub fh: u64,
        /// `FOPEN_*` flags ([`super::fopen`]).
        pub open_flags: u32,
        /// The passthrough backing id; always 0 (passthrough is not implemented).
        pub backing_id: i32,
    }

    /// `fuse_release_in`: the argument of RELEASE and RELEASEDIR.
    pub struct ReleaseIn {
        /// The file handle.
        pub fh: u64,
        /// The `open(2)` flags.
        pub flags: u32,
        /// `FUSE_RELEASE_*` flags.
        pub release_flags: u32,
        /// The lock owner.
        pub lock_owner: u64,
    }

    /// `fuse_flush_in`: the argument of FLUSH.
    pub struct FlushIn {
        /// The file handle.
        pub fh: u64,
        /// Unused.
        pub unused: u32,
        /// Padding.
        pub padding: u32,
        /// The lock owner.
        pub lock_owner: u64,
    }

    /// `fuse_read_in`: the argument of READ, READDIR and READDIRPLUS.
    pub struct ReadIn {
        /// The file handle.
        pub fh: u64,
        /// The offset (READDIR: the offset of the last entry returned, 0 at the start).
        pub offset: u64,
        /// The most bytes the reply may hold.
        pub size: u32,
        /// `FUSE_READ_*` flags.
        pub read_flags: u32,
        /// The lock owner.
        pub lock_owner: u64,
        /// The `open(2)` flags.
        pub flags: u32,
        /// Padding.
        pub padding: u32,
    }

    /// `fuse_write_in`: the argument of WRITE, followed by `size` bytes of data.
    pub struct WriteIn {
        /// The file handle.
        pub fh: u64,
        /// The offset.
        pub offset: u64,
        /// The number of data bytes that follow.
        pub size: u32,
        /// `FUSE_WRITE_*` flags.
        pub write_flags: u32,
        /// The lock owner.
        pub lock_owner: u64,
        /// The `open(2)` flags.
        pub flags: u32,
        /// Padding.
        pub padding: u32,
    }

    /// `fuse_write_out`: the reply to WRITE.
    pub struct WriteOut {
        /// The number of bytes written.
        pub size: u32,
        /// Padding.
        pub padding: u32,
    }

    /// `fuse_statfs_out`: the reply to STATFS.
    pub struct StatfsOut {
        /// The statistics.
        pub st: Kstatfs,
    }

    /// `fuse_fsync_in`: the argument of FSYNC.
    pub struct FsyncIn {
        /// The file handle.
        pub fh: u64,
        /// `FUSE_FSYNC_*` flags.
        pub fsync_flags: u32,
        /// Padding.
        pub padding: u32,
    }

    /// `fuse_init_in`: the argument of INIT. A kernel before 7.36 sends the first 16 bytes
    /// only; [`super::Operation::parse`] reads the rest as zeros.
    pub struct InitIn {
        /// The kernel's major version.
        pub major: u32,
        /// The kernel's minor version.
        pub minor: u32,
        /// The kernel's readahead limit, bytes.
        pub max_readahead: u32,
        /// The low 32 bits of the flags the kernel offers.
        pub flags: u32,
        /// The high 32 bits, if `flags` has `FUSE_INIT_EXT`.
        pub flags2: u32,
        /// Unused.
        pub unused: [u32; 11],
    }

    /// `fuse_init_out`: the reply to INIT ([`super::InitConfig::negotiate`]).
    pub struct InitOut {
        /// The major version.
        pub major: u32,
        /// The minor version both sides use.
        pub minor: u32,
        /// The readahead limit, bytes.
        pub max_readahead: u32,
        /// The low 32 bits of the flags granted.
        pub flags: u32,
        /// The most background requests.
        pub max_background: u16,
        /// The background requests at which the kernel marks the connection congested.
        pub congestion_threshold: u16,
        /// The largest WRITE, bytes.
        pub max_write: u32,
        /// The time granularity, nanoseconds.
        pub time_gran: u32,
        /// The largest request, pages (with `FUSE_MAX_PAGES`).
        pub max_pages: u16,
        /// DAX mapping alignment (unused here).
        pub map_alignment: u16,
        /// The high 32 bits of the flags granted (with `FUSE_INIT_EXT`).
        pub flags2: u32,
        /// The passthrough stacking depth (unused here).
        pub max_stack_depth: u32,
        /// The request timeout, seconds; 0 for none.
        pub request_timeout: u16,
        /// Unused.
        pub unused: [u16; 11],
    }

    /// `fuse_interrupt_in`: the argument of INTERRUPT.
    pub struct InterruptIn {
        /// The id of the request to interrupt.
        pub unique: u64,
    }

    /// `fuse_dirent` without its name: one READDIR entry, followed by `namelen` bytes of name
    /// and zero padding to 8 bytes.
    pub struct Dirent {
        /// The inode number.
        pub ino: u64,
        /// The offset of the next entry (the READDIR offset that resumes after this one).
        pub off: u64,
        /// The length of the name.
        pub namelen: u32,
        /// The file type (`DT_*`).
        pub typ: u32,
    }

    /// `fuse_direntplus` without its name: one READDIRPLUS entry.
    pub struct DirentPlus {
        /// The entry, as LOOKUP would answer it.
        pub entry_out: EntryOut,
        /// The directory entry.
        pub dirent: Dirent,
    }
}
