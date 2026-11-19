//! The read of a range of a file: the reader protocol of 5.1 around a caller's [`Stage`], and
//! what it can end in.

use std::os::fd::BorrowedFd;

use tierstore_fetch_api::ids::ByteRange;
use tierstore_mount_client::ClientError;

/// Where a read puts its bytes before they are served: a FUSE reply being built, a buffer.
///
/// [`fill`](Self::fill) reads `len` bytes at `offset` of a pack's data descriptor (by `pread` or
/// `splice`) without serving them; the [`Staged`] it returns is then either committed (served) or
/// discarded, by [`Tree::read`](crate::Tree::read) after its post-read recheck.
///
/// The trait keeps the protocol in one place and this crate free of the FUSE transport: the
/// daemon's tree filesystem implements it over a `tsfuse` reply, and this crate's tests over a
/// buffer.
pub trait Stage: Sized {
    /// The bytes read, not yet served.
    type Staged: Staged<Back = Self>;
    /// Why the bytes could not be read.
    type Error;

    /// Reads `len` bytes at `offset` of `data`; fewer only at the end of the pack.
    ///
    /// # Errors
    ///
    /// The read failed; the error carries what the caller answers with.
    fn fill(
        self,
        data: BorrowedFd<'_>,
        offset: u64,
        len: usize,
    ) -> Result<Self::Staged, Self::Error>;
}

/// Bytes a [`Stage`] read and holds unserved.
pub trait Staged {
    /// The stage, given back when the bytes are discarded.
    type Back;
    /// What serving the bytes returns.
    type Committed;

    /// Serves the bytes.
    fn commit(self) -> Self::Committed;

    /// Drops the bytes unserved and gives the stage back, to read again later.
    fn discard(self) -> Self::Back;
}

/// What a read must wait for: cells of a pack that were not all `present`. Waited on with
/// [`Tree::wait`](crate::Tree::wait).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wait {
    pub(crate) pack: u32,
    pub(crate) range: ByteRange,
    pub(crate) span_end: u64,
}

impl Wait {
    /// The number of the pack in the list.
    #[must_use]
    pub const fn pack(&self) -> u32 {
        self.pack
    }

    /// The bytes of the pack the read covers.
    #[must_use]
    pub const fn range(&self) -> ByteRange {
        self.range
    }
}

/// Why a read failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadFailure {
    /// The node is not an entry of the tree.
    NotFound,
    /// The node is not a file.
    NotAFile,
    /// A covered cell is `bad`: it failed verification twice, and reads covering it fail until it
    /// expires (4.6). EIO.
    Bad {
        /// The cell, in the pack.
        cell: u32,
    },
    /// The tree is lost (4.6): EIO until the tree is released.
    Lost,
    /// The pack cannot be read: the mount client ended, or refused the pack. EIO.
    Broken(ClientError),
}

/// How [`Tree::read`](crate::Tree::read) ended.
pub enum ReadStep<S: Stage> {
    /// The bytes were served: the commit's return.
    Served(<S::Staged as Staged>::Committed),
    /// There are no bytes to read (at or past the end of the file, or a read of 0 bytes); the
    /// stage was not used.
    Empty(S),
    /// Not every covered cell was `present`, or one changed while the bytes were read (they were
    /// discarded): the stage back, and what to wait for before reading again.
    Park(S, Wait),
    /// The read failed; the stage was not used.
    Failed(S, ReadFailure),
    /// The stage could not read the bytes.
    FillFailed(S::Error),
}

/// How [`Tree::wait`](crate::Tree::wait) ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waited {
    /// The cells were `present` when the server answered: read again from the start (they may
    /// have changed since).
    Retry,
    /// The fill of a covered cell failed (4.6): EIO for this read; the tree is not lost.
    Eio {
        /// The cell, in the pack.
        cell: u32,
    },
    /// The tree is lost: EIO for this read and every later one.
    Lost,
    /// The pack cannot be read: EIO.
    Broken(ClientError),
}
