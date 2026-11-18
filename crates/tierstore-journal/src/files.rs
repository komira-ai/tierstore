//! The file operations of one entry that the journal orders (section 8).

use std::io;

/// One of an entry's files (section 2: `packs-v2/aa/bb/<hash>.pack`, `.jnl`, `.meta`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileKind {
    /// The sparse data file `<hash>.pack`.
    Data,
    /// The append-only journal `<hash>.jnl`.
    Journal,
    /// The snapshot `<hash>.meta`.
    Snapshot,
    /// The snapshot being written, `<hash>.meta.tmp`, renamed over the snapshot once synced.
    SnapshotTemp,
}

impl FileKind {
    /// The four kinds.
    pub const ALL: [Self; 4] = [
        Self::Data,
        Self::Journal,
        Self::Snapshot,
        Self::SnapshotTemp,
    ];

    /// The suffix of the file's name after the entry's hash.
    #[must_use]
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Data => ".pack",
            Self::Journal => ".jnl",
            Self::Snapshot => ".meta",
            Self::SnapshotTemp => ".meta.tmp",
        }
    }
}

/// The operations on an entry's files whose order the journal decides: what reaches the disk
/// before what. Implementations here: [`crate::DirFiles`] (a directory of the cache partition,
/// Linux) and [`crate::fake::MemFs`], which keeps the durable and the volatile bytes apart and
/// can crash. The macOS store is to supply its own, with `F_FULLFSYNC` and `F_PUNCHHOLE` (5.6).
///
/// Every method does blocking I/O on a real file system.
pub trait EntryFiles: Send + Sync {
    /// The whole content of `file`, or `None` when it does not exist. The journal reads the
    /// journal and the snapshot with it, never the data file.
    fn read(&self, file: FileKind) -> io::Result<Option<Vec<u8>>>;

    /// Writes `bytes` at `offset` of `file`, creating it if it does not exist (`pwrite`).
    fn write_at(&self, file: FileKind, offset: u64, bytes: &[u8]) -> io::Result<()>;

    /// Sets the length of `file`, creating it if it does not exist (`ftruncate`).
    fn set_len(&self, file: FileKind, len: u64) -> io::Result<()>;

    /// Makes the bytes and the length of `file` durable (`fdatasync`; a macOS implementation uses `F_FULLFSYNC`).
    /// A file that does not exist is an error of kind `NotFound`.
    fn sync_data(&self, file: FileKind) -> io::Result<()>;

    /// Releases `len` bytes of the data file from `offset`, keeping its size
    /// (`fallocate(PUNCH_HOLE | KEEP_SIZE)`; a macOS implementation uses `F_PUNCHHOLE`). The range reads as zeros.
    fn punch_data(&self, offset: u64, len: u64) -> io::Result<()>;

    /// Renames [`FileKind::SnapshotTemp`] over [`FileKind::Snapshot`], atomically (`rename`).
    fn replace_snapshot(&self) -> io::Result<()>;

    /// Makes the directory's entries durable: the files created and the rename (`fsync` of the
    /// directory).
    fn sync_dir(&self) -> io::Result<()>;
}
