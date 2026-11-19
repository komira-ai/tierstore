//! The body of a request, parsed per opcode.

use std::fmt;

use super::args::Args;
use super::wire::{
    BatchForgetIn, CreateIn, FlushIn, ForgetIn, ForgetOne, FsyncIn, GetattrIn, InitIn, InterruptIn,
    LinkIn, MkdirIn, MknodIn, OpenIn, ReadIn, ReleaseIn, Rename2In, RenameIn, SetattrIn, WriteIn,
};
use crate::fs::Request;
use crate::opcode::Opcode;
use crate::reply::Errno;

/// The bytes of `fuse_init_in` a kernel before 7.36 sends: `major`, `minor`, `max_readahead`,
/// `flags`.
pub const INIT_IN_COMPAT_SIZE: usize = 16;

/// The arguments of one request of an implemented opcode. Names are the bytes the kernel sent,
/// without their NUL; a struct is a copy, so the request's buffer can be reused at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation<'a> {
    /// LOOKUP of `name` in the request's node.
    Lookup {
        /// The name.
        name: &'a [u8],
    },
    /// FORGET of the request's node (no reply).
    Forget(ForgetIn),
    /// GETATTR.
    Getattr(GetattrIn),
    /// SETATTR.
    Setattr(SetattrIn),
    /// READLINK.
    Readlink,
    /// MKDIR of `name` in the request's node.
    Mkdir {
        /// Mode and umask.
        arg: MkdirIn,
        /// The name.
        name: &'a [u8],
    },
    /// UNLINK of `name` in the request's node.
    Unlink {
        /// The name.
        name: &'a [u8],
    },
    /// RENAME of `name` in the request's node to `newname` in `arg.newdir`.
    Rename {
        /// The new parent.
        arg: RenameIn,
        /// The old name.
        name: &'a [u8],
        /// The new name.
        newname: &'a [u8],
    },
    /// OPEN.
    Open(OpenIn),
    /// READ.
    Read(ReadIn),
    /// WRITE of `data` (exactly `arg.size` bytes).
    Write {
        /// Handle, offset, size and flags.
        arg: WriteIn,
        /// The bytes to write.
        data: &'a [u8],
    },
    /// STATFS.
    Statfs,
    /// RELEASE.
    Release(ReleaseIn),
    /// FSYNC.
    Fsync(FsyncIn),
    /// FLUSH.
    Flush(FlushIn),
    /// INIT ([`super::InitConfig::negotiate`]).
    Init(InitIn),
    /// OPENDIR.
    Opendir(OpenIn),
    /// READDIR.
    Readdir(ReadIn),
    /// RELEASEDIR.
    Releasedir(ReleaseIn),
    /// CREATE of `name` in the request's node.
    Create {
        /// Flags, mode and umask.
        arg: CreateIn,
        /// The name.
        name: &'a [u8],
    },
    /// INTERRUPT of request `unique`.
    Interrupt(InterruptIn),
    /// DESTROY.
    Destroy,
    /// BATCH_FORGET of `arg.count` nodes (no reply).
    BatchForget {
        /// The count.
        arg: BatchForgetIn,
        /// The nodes.
        nodes: Forgets<'a>,
    },
    /// READDIRPLUS.
    Readdirplus(ReadIn),
    /// SYMLINK: the link `name` in the request's node, pointing at `target`.
    Symlink {
        /// The name of the new link.
        name: &'a [u8],
        /// What the link points at.
        target: &'a [u8],
    },
    /// MKNOD of `name` in the request's node.
    Mknod {
        /// Mode, device number and umask.
        arg: MknodIn,
        /// The name.
        name: &'a [u8],
    },
    /// RMDIR of `name` in the request's node.
    Rmdir {
        /// The name.
        name: &'a [u8],
    },
    /// LINK of `arg.oldnodeid` as `newname` in the request's node.
    Link {
        /// The node linked.
        arg: LinkIn,
        /// The new name.
        newname: &'a [u8],
    },
    /// SETXATTR of the request's node. Its body is not parsed: its argument struct has one of
    /// two sizes, by whether INIT agreed `FUSE_SETXATTR_EXT` (which the library never asks for),
    /// and a file system that serves no extended attribute refuses every SETXATTR alike.
    Setxattr,
    /// REMOVEXATTR of the attribute `name` of the request's node.
    Removexattr {
        /// The attribute's name.
        name: &'a [u8],
    },
    /// RENAME2 of `name` in the request's node to `newname` in `arg.newdir`, with `arg.flags`.
    Rename2 {
        /// The new parent and the `RENAME_*` flags.
        arg: Rename2In,
        /// The old name.
        name: &'a [u8],
        /// The new name.
        newname: &'a [u8],
    },
}

/// Why a request body could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The opcode is not one the library implements (answered `ENOSYS`).
    #[error("FUSE opcode {0} is not implemented")]
    Unimplemented(u32),
    /// The body is shorter than the opcode's arguments.
    #[error("FUSE {opcode:?} request body has {have} bytes; its arguments need {need}")]
    Short {
        /// The opcode.
        opcode: Opcode,
        /// The bytes the arguments need.
        need: usize,
        /// The bytes the body has.
        have: usize,
    },
    /// A name is not NUL-terminated within the body.
    #[error("FUSE {opcode:?} request has a name with no NUL terminator")]
    MissingNul {
        /// The opcode.
        opcode: Opcode,
    },
}

impl ParseError {
    /// The errno the request is answered with: `ENOSYS` for an opcode the library does not
    /// implement, `EIO` for a body the kernel should never have sent.
    #[must_use]
    pub fn errno(&self) -> Errno {
        match self {
            Self::Unimplemented(_) => Errno::ENOSYS,
            Self::Short { .. } | Self::MissingNul { .. } => Errno::EIO,
        }
    }
}

impl<'a> Operation<'a> {
    /// The arguments of `req`, by its opcode. Bytes after the arguments (padding, or
    /// extensions the session did not negotiate) are ignored.
    ///
    /// # Errors
    ///
    /// [`ParseError::Unimplemented`] for an opcode the library answers `ENOSYS`;
    /// [`ParseError::Short`] or [`ParseError::MissingNul`] for a body too short for the
    /// opcode's arguments.
    pub fn parse(req: &Request<'a>) -> Result<Self, ParseError> {
        let code = req.header.opcode;
        let opcode = Opcode::from_code(code).ok_or(ParseError::Unimplemented(code))?;
        let body = req.body;
        let mut args = Args::new(body);
        let short = |need: usize| ParseError::Short {
            opcode,
            need,
            have: body.len(),
        };
        macro_rules! fetch {
            ($t:ty) => {
                args.fetch::<$t>().ok_or_else(|| short(size_of::<$t>()))?
            };
        }
        macro_rules! name {
            () => {
                args.fetch_str().ok_or(ParseError::MissingNul { opcode })?
            };
        }
        Ok(match opcode {
            Opcode::Lookup => Self::Lookup { name: name!() },
            Opcode::Forget => Self::Forget(fetch!(ForgetIn)),
            Opcode::Getattr => Self::Getattr(fetch!(GetattrIn)),
            Opcode::Setattr => Self::Setattr(fetch!(SetattrIn)),
            Opcode::Readlink => Self::Readlink,
            Opcode::Mkdir => {
                let arg = fetch!(MkdirIn);
                Self::Mkdir { arg, name: name!() }
            }
            Opcode::Unlink => Self::Unlink { name: name!() },
            Opcode::Rename => {
                let arg = fetch!(RenameIn);
                let name = name!();
                Self::Rename {
                    arg,
                    name,
                    newname: name!(),
                }
            }
            Opcode::Open => Self::Open(fetch!(OpenIn)),
            Opcode::Read => Self::Read(fetch!(ReadIn)),
            Opcode::Write => {
                let arg = fetch!(WriteIn);
                let size = arg.size as usize;
                let data = args
                    .fetch_bytes(size)
                    .ok_or_else(|| short(WriteIn::SIZE + size))?;
                Self::Write { arg, data }
            }
            Opcode::Statfs => Self::Statfs,
            Opcode::Release => Self::Release(fetch!(ReleaseIn)),
            Opcode::Fsync => Self::Fsync(fetch!(FsyncIn)),
            Opcode::Flush => Self::Flush(fetch!(FlushIn)),
            Opcode::Init => Self::Init(init_in(body).ok_or_else(|| short(INIT_IN_COMPAT_SIZE))?),
            Opcode::Opendir => Self::Opendir(fetch!(OpenIn)),
            Opcode::Readdir => Self::Readdir(fetch!(ReadIn)),
            Opcode::Releasedir => Self::Releasedir(fetch!(ReleaseIn)),
            Opcode::Create => {
                let arg = fetch!(CreateIn);
                Self::Create { arg, name: name!() }
            }
            Opcode::Interrupt => Self::Interrupt(fetch!(InterruptIn)),
            Opcode::Destroy => Self::Destroy,
            Opcode::BatchForget => {
                let arg = fetch!(BatchForgetIn);
                let need = (arg.count as usize).saturating_mul(ForgetOne::SIZE);
                let bytes = args
                    .fetch_bytes(need)
                    .ok_or_else(|| short(BatchForgetIn::SIZE.saturating_add(need)))?;
                Self::BatchForget {
                    arg,
                    nodes: Forgets { bytes },
                }
            }
            Opcode::Readdirplus => Self::Readdirplus(fetch!(ReadIn)),
            Opcode::Symlink => {
                let name = name!();
                Self::Symlink {
                    name,
                    target: name!(),
                }
            }
            Opcode::Mknod => {
                let arg = fetch!(MknodIn);
                Self::Mknod { arg, name: name!() }
            }
            Opcode::Rmdir => Self::Rmdir { name: name!() },
            Opcode::Link => {
                let arg = fetch!(LinkIn);
                Self::Link {
                    arg,
                    newname: name!(),
                }
            }
            Opcode::Setxattr => Self::Setxattr,
            Opcode::Removexattr => Self::Removexattr { name: name!() },
            Opcode::Rename2 => {
                let arg = fetch!(Rename2In);
                let name = name!();
                Self::Rename2 {
                    arg,
                    name,
                    newname: name!(),
                }
            }
        })
    }
}

/// `fuse_init_in` from a body of at least the 16 bytes every kernel sends; the fields a kernel
/// before 7.36 does not send (`flags2`, `unused`) read as zeros.
fn init_in(body: &[u8]) -> Option<InitIn> {
    if body.len() < INIT_IN_COMPAT_SIZE {
        return None;
    }
    let mut full = [0u8; InitIn::SIZE];
    let n = body.len().min(InitIn::SIZE);
    full[..n].copy_from_slice(&body[..n]);
    InitIn::read_prefix(&full).map(|(init, _)| init)
}

/// The nodes of a BATCH_FORGET, read one at a time from the request's bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Forgets<'a> {
    bytes: &'a [u8],
}

impl fmt::Debug for Forgets<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(*self).finish()
    }
}

impl Iterator for Forgets<'_> {
    type Item = ForgetOne;

    fn next(&mut self) -> Option<ForgetOne> {
        let (one, rest) = ForgetOne::read_prefix(self.bytes)?;
        self.bytes = rest;
        Some(one)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.bytes.len() / ForgetOne::SIZE;
        (n, Some(n))
    }
}

impl ExactSizeIterator for Forgets<'_> {}
