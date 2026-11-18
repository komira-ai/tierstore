//! The opcodes `tsfuse` implements (5.2), with their values in the Linux uapi `fuse.h`. Every
//! other opcode is answered `ENOSYS`.

/// An opcode the library implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Opcode {
    /// `FUSE_LOOKUP`.
    Lookup,
    /// `FUSE_FORGET` (a no-op: inode numbers are record indexes).
    Forget,
    /// `FUSE_GETATTR`.
    Getattr,
    /// `FUSE_SETATTR` (put directory: size, times).
    Setattr,
    /// `FUSE_READLINK`.
    Readlink,
    /// `FUSE_MKDIR` (put directory).
    Mkdir,
    /// `FUSE_UNLINK` (put directory).
    Unlink,
    /// `FUSE_RENAME` (put directory).
    Rename,
    /// `FUSE_OPEN`.
    Open,
    /// `FUSE_READ`.
    Read,
    /// `FUSE_WRITE` (put directory).
    Write,
    /// `FUSE_STATFS`.
    Statfs,
    /// `FUSE_RELEASE`.
    Release,
    /// `FUSE_FSYNC` (put directory).
    Fsync,
    /// `FUSE_FLUSH` (put directory).
    Flush,
    /// `FUSE_INIT`.
    Init,
    /// `FUSE_OPENDIR`.
    Opendir,
    /// `FUSE_READDIR`.
    Readdir,
    /// `FUSE_RELEASEDIR`.
    Releasedir,
    /// `FUSE_CREATE` (put directory).
    Create,
    /// `FUSE_INTERRUPT`.
    Interrupt,
    /// `FUSE_DESTROY`.
    Destroy,
    /// `FUSE_BATCH_FORGET` (a no-op).
    BatchForget,
    /// `FUSE_READDIRPLUS`.
    Readdirplus,
}

impl Opcode {
    /// The opcode's uapi value.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            Self::Lookup => 1,
            Self::Forget => 2,
            Self::Getattr => 3,
            Self::Setattr => 4,
            Self::Readlink => 5,
            Self::Mkdir => 9,
            Self::Unlink => 10,
            Self::Rename => 12,
            Self::Open => 14,
            Self::Read => 15,
            Self::Write => 16,
            Self::Statfs => 17,
            Self::Release => 18,
            Self::Fsync => 20,
            Self::Flush => 25,
            Self::Init => 26,
            Self::Opendir => 27,
            Self::Readdir => 28,
            Self::Releasedir => 29,
            Self::Create => 35,
            Self::Interrupt => 36,
            Self::Destroy => 38,
            Self::BatchForget => 42,
            Self::Readdirplus => 44,
        }
    }

    /// The implemented opcode of uapi value `code`; `None` for an opcode the library answers
    /// `ENOSYS`.
    #[must_use]
    pub fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            1 => Self::Lookup,
            2 => Self::Forget,
            3 => Self::Getattr,
            4 => Self::Setattr,
            5 => Self::Readlink,
            9 => Self::Mkdir,
            10 => Self::Unlink,
            12 => Self::Rename,
            14 => Self::Open,
            15 => Self::Read,
            16 => Self::Write,
            17 => Self::Statfs,
            18 => Self::Release,
            20 => Self::Fsync,
            25 => Self::Flush,
            26 => Self::Init,
            27 => Self::Opendir,
            28 => Self::Readdir,
            29 => Self::Releasedir,
            35 => Self::Create,
            36 => Self::Interrupt,
            38 => Self::Destroy,
            42 => Self::BatchForget,
            44 => Self::Readdirplus,
            _ => return None,
        })
    }

    /// Whether the kernel expects no reply to the opcode (`FORGET`, `BATCH_FORGET`, and an
    /// `INTERRUPT` unless it is answered `EAGAIN`).
    #[must_use]
    pub fn no_reply(self) -> bool {
        matches!(self, Self::Forget | Self::BatchForget | Self::Interrupt)
    }
}
