//! [`DirFiles`]: an entry's files in a directory of the cache partition (Linux and macOS).
//!
//! The two systems differ in two calls (5.6 (b), (c)):
//! - **Durability.** Linux: `fdatasync`. macOS: `fcntl(F_FULLFSYNC)` (through `rustix`), since a
//!   plain `fsync` there leaves the bytes in the drive's cache.
//! - **Punch.** Linux: `fallocate(PUNCH_HOLE | KEEP_SIZE)`. macOS punches with
//!   `fcntl(F_PUNCHHOLE)`, which neither `rustix` nor `std` wraps, and this workspace adds no
//!   `unsafe` (AGENTS.md R2). So on macOS a punch that reaches the end of the file is done by
//!   truncating to its start and extending back (the blocks are released and the range reads as
//!   zeros), and any other punch is [`std::io::ErrorKind::Unsupported`]: the in-process store
//!   then evicts whole entries only, the fallback 5.6 (b) names for a volume that cannot punch.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use rustix::fs::FallocateFlags;

use crate::files::{EntryFiles, FileKind};

/// The files `<dir>/<stem>.pack`, `.jnl`, `.meta` and `.meta.tmp` of one entry (section 2:
/// `packs-v2/aa/bb/<hash>.pack`). The data file is opened once, by [`DirFiles::open`], and
/// [`DirFiles::data`] lends it to the entry, which `pwrite`s, `flock`s and passes the same
/// descriptor; the others are opened per operation.
#[derive(Debug)]
pub struct DirFiles {
    dir: PathBuf,
    stem: String,
    data: File,
}

impl DirFiles {
    /// Opens the data file `<dir>/<stem>.pack` read-write, creating it (empty) if it does not
    /// exist; the store sets its length and makes its name durable before it opens the journal.
    pub fn open(dir: &Path, stem: &str) -> io::Result<Self> {
        let data = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!("{stem}{}", FileKind::Data.suffix())))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            stem: stem.to_owned(),
            data,
        })
    }

    /// The data file.
    #[must_use]
    pub fn data(&self) -> &File {
        &self.data
    }

    /// The path of `file`.
    #[must_use]
    pub fn path(&self, file: FileKind) -> PathBuf {
        self.dir.join(format!("{}{}", self.stem, file.suffix()))
    }

    /// `file` opened for writing (created if it does not exist), or the data file.
    fn with_writable<T>(
        &self,
        file: FileKind,
        f: impl FnOnce(&File) -> io::Result<T>,
    ) -> io::Result<T> {
        if file == FileKind::Data {
            return f(&self.data);
        }
        let opened = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path(file))?;
        f(&opened)
    }
}

impl EntryFiles for DirFiles {
    fn read(&self, file: FileKind) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(file)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write_at(&self, file: FileKind, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.with_writable(file, |f| f.write_all_at(bytes, offset))
    }

    fn set_len(&self, file: FileKind, len: u64) -> io::Result<()> {
        self.with_writable(file, |f| f.set_len(len))
    }

    fn sync_data(&self, file: FileKind) -> io::Result<()> {
        if file == FileKind::Data {
            return sync(&self.data);
        }
        sync(&File::open(self.path(file))?)
    }

    fn punch_data(&self, offset: u64, len: u64) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        punch(&self.data, offset, len)
    }

    fn replace_snapshot(&self) -> io::Result<()> {
        std::fs::rename(
            self.path(FileKind::SnapshotTemp),
            self.path(FileKind::Snapshot),
        )
    }

    fn sync_dir(&self) -> io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }
}

/// Makes the bytes and the length of `file` durable: `fdatasync`.
#[cfg(target_os = "linux")]
fn sync(file: &File) -> io::Result<()> {
    file.sync_data()
}

/// Makes the bytes and the length of `file` durable: `fcntl(F_FULLFSYNC)`, which also flushes
/// the drive's cache.
#[cfg(target_os = "macos")]
fn sync(file: &File) -> io::Result<()> {
    rustix::fs::fcntl_fullfsync(file).map_err(io::Error::from)
}

/// Releases `len` (more than 0) bytes of `file` from `offset`, keeping its size:
/// `fallocate(PUNCH_HOLE | KEEP_SIZE)`.
#[cfg(target_os = "linux")]
fn punch(file: &File, offset: u64, len: u64) -> io::Result<()> {
    rustix::fs::fallocate(
        file,
        FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE,
        offset,
        len,
    )
    .map_err(io::Error::from)
}

/// Releases `len` (more than 0) bytes of `file` from `offset`, keeping its size, when the range
/// reaches the end of the file: truncates to `offset` and extends back. Any other range is
/// [`io::ErrorKind::Unsupported`] (no safe `F_PUNCHHOLE`; see the module documentation), with
/// nothing changed.
#[cfg(target_os = "macos")]
fn punch(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let size = file.metadata()?.len();
    if offset.saturating_add(len) < size {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a punch inside the file needs F_PUNCHHOLE, which has no safe wrapper",
        ));
    }
    if offset >= size {
        return Ok(());
    }
    file.set_len(offset)?;
    file.set_len(size)
}
