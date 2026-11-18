//! The hot device: the file operations the hot tier makes on its directory (10.3 layout), as a
//! small trait with the real directory ([`DirDev`]) behind it; the tests put the crash-point
//! disk of `tierstore-harness` behind it.
//!
//! Paths are relative to the hot directory, `/`-separated (`seg/<block_id>.seg`), as
//! [`tierstore_format::names::HotName::path`] makes them. Every call is blocking: [`crate::sched`]
//! runs them on its own threads, never on the async runtime.
//!
//! The durability model is the kernel's: [`HotDev::write_at`] and [`HotDev::set_len`] change what
//! reads see; [`HotDev::fdatasync`] makes one file's bytes durable; a new name, a rename or an
//! unlink is durable after [`HotDev::fsync_dir`] of its directory.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// The file operations of the hot tier.
pub trait HotDev: Send + Sync + 'static {
    /// Creates an empty file at `path` (its directory is made if missing); an error if it exists.
    fn create(&self, path: &str) -> io::Result<()>;
    /// Writes `bytes` at `offset` of the file at `path` (it may extend the file).
    fn write_at(&self, path: &str, offset: u64, bytes: &[u8]) -> io::Result<()>;
    /// Up to `len` bytes of the file at `path` from `offset` (fewer at its end).
    fn read_at(&self, path: &str, offset: u64, len: u64) -> io::Result<Vec<u8>>;
    /// The length of the file at `path`.
    fn len(&self, path: &str) -> io::Result<u64>;
    /// Cuts or zero-extends the file at `path` to `len` bytes.
    fn set_len(&self, path: &str, len: u64) -> io::Result<()>;
    /// Reserves `len` bytes for the file at `path` without changing its length (`fallocate` with
    /// `KEEP_SIZE`); a device that cannot does nothing.
    fn allocate(&self, path: &str, len: u64) -> io::Result<()>;
    /// Makes the bytes of the file at `path` durable.
    fn fdatasync(&self, path: &str) -> io::Result<()>;
    /// Makes the entries of directory `dir` durable (`""` is the root).
    fn fsync_dir(&self, dir: &str) -> io::Result<()>;
    /// Renames `from` to `to`; with `noreplace`, an error of kind `AlreadyExists` if `to`
    /// exists, and `from` is left in place.
    fn rename(&self, from: &str, to: &str, noreplace: bool) -> io::Result<()>;
    /// Removes the file at `path`.
    fn unlink(&self, path: &str) -> io::Result<()>;
    /// The names of the entries of directory `dir`, sorted; a missing directory has none.
    fn list(&self, dir: &str) -> io::Result<Vec<String>>;
}

/// The directory part of `path`: everything before its last `/`, `""` for the root.
#[must_use]
pub fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// [`HotDev`] on a directory of the real file system (the hot tier's xfs on the NVMe, or the
/// SSD partition of the NOW profile).
#[derive(Debug, Clone)]
pub struct DirDev {
    root: PathBuf,
}

impl DirDev {
    /// The device whose files are under `root`, which must exist.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    fn open_rw(&self, path: &str) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.at(path))
    }
}

fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(""))
}

impl HotDev for DirDev {
    fn create(&self, path: &str) -> io::Result<()> {
        let full = self.at(path);
        fs::create_dir_all(parent(&full))?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(full)
            .map(drop)
    }

    fn write_at(&self, path: &str, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.open_rw(path)?.write_all_at(bytes, offset)
    }

    fn read_at(&self, path: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let file = File::open(self.at(path))?;
        let size = file.metadata()?.len();
        let end = size.min(offset.saturating_add(len));
        let mut buf = vec![0u8; end.saturating_sub(offset) as usize];
        file.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }

    fn len(&self, path: &str) -> io::Result<u64> {
        Ok(fs::metadata(self.at(path))?.len())
    }

    fn set_len(&self, path: &str, len: u64) -> io::Result<()> {
        self.open_rw(path)?.set_len(len)
    }

    fn allocate(&self, path: &str, len: u64) -> io::Result<()> {
        let file = self.open_rw(path)?;
        allocate(&file, len)
    }

    fn fdatasync(&self, path: &str) -> io::Result<()> {
        self.open_rw(path)?.sync_data()
    }

    fn fsync_dir(&self, dir: &str) -> io::Result<()> {
        File::open(self.at(dir))?.sync_all()
    }

    fn rename(&self, from: &str, to: &str, noreplace: bool) -> io::Result<()> {
        let (src, dst) = (self.at(from), self.at(to));
        fs::create_dir_all(parent(&dst))?;
        if noreplace {
            // A hard link fails with `AlreadyExists` when the target exists and is atomic, which
            // is `renameat2(RENAME_NOREPLACE)` on every file system the hot tier runs on; the
            // source name is then removed. A crash between the two leaves the source as an
            // `.up` file, which the startup sweep removes.
            fs::hard_link(&src, &dst)?;
            fs::remove_file(src)
        } else {
            fs::rename(src, dst)
        }
    }

    fn unlink(&self, path: &str) -> io::Result<()> {
        fs::remove_file(self.at(path))
    }

    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        let entries = match fs::read_dir(self.at(dir)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut names = entries
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }
}

#[cfg(target_os = "linux")]
fn allocate(file: &File, len: u64) -> io::Result<()> {
    use rustix::fs::{FallocateFlags, fallocate};
    fallocate(file, FallocateFlags::KEEP_SIZE, 0, len).map_err(io::Error::from)
}

#[cfg(not(target_os = "linux"))]
fn allocate(_file: &File, _len: u64) -> io::Result<()> {
    Ok(())
}
