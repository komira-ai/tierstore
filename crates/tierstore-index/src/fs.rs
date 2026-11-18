//! The file access recovery and the tomb log use: a small trait over paths relative to a root, a
//! real directory ([`DirFs`]) and an in-memory fake that crashes on demand ([`fake::FakeFs`]).
//!
//! A path is relative to the root, `/`-separated, with no empty, `.` or `..` segment and not
//! absolute; anything else is refused with [`io::ErrorKind::InvalidInput`]. The directory of
//! [`Fs::list`] may also be `""`, the root.

pub mod fake;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Files under a root, as far as the index needs them.
pub trait Fs: Send + Sync {
    /// The names of the entries of directory `dir` (`""` is the root), sorted. A missing
    /// directory is an empty list.
    fn list(&self, dir: &str) -> io::Result<Vec<String>>;
    /// The length of the file at `path`.
    fn len(&self, path: &str) -> io::Result<u64>;
    /// Up to `len` bytes of the file at `path` from `offset`: fewer at the end of the file.
    fn read_at(&self, path: &str, offset: u64, len: u64) -> io::Result<Vec<u8>>;
    /// Appends `bytes` to the file at `path`, creating it and its parent directories.
    fn append(&self, path: &str, bytes: &[u8]) -> io::Result<()>;
    /// Cuts or zero-extends the file at `path` to `len` bytes.
    fn truncate(&self, path: &str, len: u64) -> io::Result<()>;
    /// Makes the file's bytes and its directory entry durable.
    fn sync(&self, path: &str) -> io::Result<()>;
    /// Removes the file at `path`; the removal is durable when this returns.
    fn remove(&self, path: &str) -> io::Result<()>;
}

/// Refuses a path that is not a relative `/`-separated path of plain segments (`dir` also
/// accepts the empty path, the root).
pub(crate) fn check(path: &str, dir: bool) -> io::Result<()> {
    let plain = if path.is_empty() {
        dir
    } else {
        path.split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
    };
    if plain {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a relative path of plain segments: {path:?}"),
        ))
    }
}

/// [`Fs`] on a directory of the real file system.
#[derive(Debug, Clone)]
pub struct DirFs {
    root: PathBuf,
}

impl DirFs {
    /// Files under `root`, which must exist.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Syncs the directories holding `path`, from its own up to the root, so that the entries
    /// made for it are durable.
    fn sync_parents(&self, path: &str) -> io::Result<()> {
        let full = self.root.join(path);
        let depth = path.split('/').count();
        for dir in full.ancestors().skip(1).take(depth) {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}

impl Fs for DirFs {
    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        check(dir, true)?;
        let entries = match fs::read_dir(self.root.join(dir)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut names = Vec::new();
        for entry in entries {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    fn len(&self, path: &str) -> io::Result<u64> {
        check(path, false)?;
        Ok(fs::metadata(self.root.join(path))?.len())
    }

    fn read_at(&self, path: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        check(path, false)?;
        let mut file = File::open(self.root.join(path))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        file.take(len).read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn append(&self, path: &str, bytes: &[u8]) -> io::Result<()> {
        check(path, false)?;
        let full = self.root.join(path);
        fs::create_dir_all(full.parent().unwrap_or(Path::new("")))?;
        let mut file = OpenOptions::new().create(true).append(true).open(full)?;
        file.write_all(bytes)
    }

    fn truncate(&self, path: &str, len: u64) -> io::Result<()> {
        check(path, false)?;
        OpenOptions::new()
            .write(true)
            .open(self.root.join(path))?
            .set_len(len)
    }

    fn sync(&self, path: &str) -> io::Result<()> {
        check(path, false)?;
        File::open(self.root.join(path))?.sync_all()?;
        self.sync_parents(path)
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        check(path, false)?;
        fs::remove_file(self.root.join(path))?;
        self.sync_parents(path)
    }
}
