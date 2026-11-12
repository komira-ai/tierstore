//! Why a call of the staged-file store is refused, and the errno a frontend answers for it.

use std::io;
use std::path::PathBuf;

use rustix::io::Errno;

use super::names::Sha256Digest;

/// Why a call on the put directory is refused.
#[derive(Debug, thiserror::Error)]
pub(in crate::server) enum PutError {
    /// A name or a directory that is not one component of 1 to 255 bytes, or is `.` or `..`.
    #[error("not a name of the put directory: {0}")]
    InvalidName(&'static str),
    /// A name of more than 255 bytes, or a key of more than 1024.
    #[error("a name of more than 255 bytes, or a key of more than 1024")]
    NameTooLong,
    /// A directory under none of the allowed prefixes.
    #[error("the directory is under none of the prefixes of the put directory")]
    NotUnderPrefix,
    /// A call the put directory does not take.
    #[error("not permitted in the put directory: {0}")]
    NotPermitted(&'static str),
    /// No staged or committed file has the name.
    #[error("no such file")]
    NotFound,
    /// The name is taken.
    #[error("the name exists")]
    Exists,
    /// A handle that is not open.
    #[error("the handle is not open")]
    BadHandle,
    /// The stage's volume has less free space than `min_free_bytes`.
    #[error("the stage has {free} bytes free, under the {min} it keeps")]
    NoSpace {
        /// The free bytes.
        free: u64,
        /// The bytes kept free.
        min: u64,
    },
    /// The directory holds as many names, or bytes, as it may.
    #[error("the directory is at its limit of {0}")]
    Quota(&'static str),
    /// The object would be over `max_object_bytes`, or the upstream refused it as too large.
    #[error("the object is over the largest one the put directory takes")]
    TooLarge,
    /// The file is in use: a handle is open, a commit is running, or another commit of the key is.
    #[error("busy: {0}")]
    Busy(&'static str),
    /// The bytes do not hash to the digest the name expects.
    #[error("the bytes hash to {actual}, not to the {expected} the name expects")]
    DigestMismatch {
        /// The digest of the name.
        expected: Sha256Digest,
        /// The digest of the bytes.
        actual: Sha256Digest,
    },
    /// The key exists upstream with other bytes.
    #[error("the key exists upstream with other bytes")]
    ExistsDifferent,
    /// The upstream refused the bytes for their checksum.
    #[error("the upstream refused the bytes for their checksum")]
    Checksum,
    /// The upload did not finish.
    #[error("the upload did not finish")]
    Upstream,
    /// The stage could not be read or written.
    #[error("the stage: {0}")]
    Io(#[from] io::Error),
}

impl PutError {
    /// The errno a frontend answers.
    pub(in crate::server) fn errno(&self) -> Errno {
        match self {
            Self::InvalidName(_) => Errno::INVAL,
            Self::NameTooLong => Errno::NAMETOOLONG,
            Self::NotUnderPrefix | Self::NotPermitted(_) => Errno::PERM,
            Self::NotFound => Errno::NOENT,
            Self::Exists | Self::ExistsDifferent => Errno::EXIST,
            Self::BadHandle => Errno::BADF,
            Self::NoSpace { .. } => Errno::NOSPC,
            Self::Quota(_) => Errno::DQUOT,
            Self::TooLarge => Errno::FBIG,
            Self::Busy(_) => Errno::BUSY,
            Self::DigestMismatch { .. } | Self::Checksum => Errno::BADMSG,
            Self::Upstream => Errno::IO,
            Self::Io(error) => Errno::from_io_error(error).unwrap_or(Errno::IO),
        }
    }
}

/// Why the store cannot be opened.
#[derive(Debug, thiserror::Error)]
pub(in crate::server) enum OpenError {
    /// A prefix that is not a directory followed by `/`.
    #[error("fs.put.prefixes: {0:?} is not a key prefix ending in `/`")]
    Prefix(String),
    /// The stage directory is on another volume than the cache.
    #[error("the stage directory {stage} is not on the volume of the cache {cache}")]
    OtherVolume {
        /// The stage directory.
        stage: PathBuf,
        /// The cache directory.
        cache: PathBuf,
    },
    /// The stage directory, or the journal in it, could not be made or read.
    #[error("the stage directory {0}: {1}")]
    Io(PathBuf, io::Error),
}
