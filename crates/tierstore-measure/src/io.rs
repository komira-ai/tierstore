//! File I/O the arms share: 4 KiB-aligned buffers for `O_DIRECT` (in safe code, by slicing an
//! over-allocated vector at its first aligned byte), opens in each mode, dropping a file's pages,
//! and writing a data file of known content.

use std::fs::{File, OpenOptions};
use std::num::NonZeroU64;
use std::os::unix::fs::{FileExt as _, OpenOptionsExt as _};
use std::path::Path;

use rustix::fs::{Advice, OFlags};

use crate::Error;

/// The alignment `O_DIRECT` needs on these drives (4 KiB physical sectors).
pub const ALIGN: usize = 4096;

/// One MiB.
pub const MIB: u64 = 1 << 20;

/// A zeroed buffer of `len` bytes whose start is aligned to [`ALIGN`].
#[derive(Debug)]
pub struct AlignedBuf {
    v: Vec<u8>,
    off: usize,
    len: usize,
}

impl AlignedBuf {
    /// A buffer of `len` bytes.
    #[must_use]
    pub fn new(len: usize) -> Self {
        let v = vec![0u8; len + ALIGN];
        let off = v.as_ptr().align_offset(ALIGN);
        Self { v, off, len }
    }

    /// Its bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.v[self.off..self.off + self.len]
    }

    /// Its bytes, to write into.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.v[self.off..self.off + self.len]
    }
}

/// How a file is read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// `O_DIRECT`: device-sized requests, no page cache.
    Direct,
    /// Through the page cache (and its readahead).
    Buffered,
}

impl Mode {
    /// The name used in tables.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Buffered => "buffered",
        }
    }
}

fn direct_flag(mode: Mode) -> i32 {
    match mode {
        Mode::Direct => i32::try_from(OFlags::DIRECT.bits()).unwrap_or(0),
        Mode::Buffered => 0,
    }
}

/// Opens `path` to read.
///
/// # Errors
/// When it cannot be opened.
pub fn open_read(path: &Path, mode: Mode) -> Result<File, Error> {
    OpenOptions::new()
        .read(true)
        .custom_flags(direct_flag(mode))
        .open(path)
        .map_err(Error::io("open", path))
}

/// Creates `path` (it must not exist) to write.
///
/// # Errors
/// When it exists or cannot be created.
pub fn create(path: &Path, mode: Mode) -> Result<File, Error> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(direct_flag(mode))
        .open(path)
        .map_err(Error::io("create", path))
}

/// Drops the cached pages of `file` (`POSIX_FADV_DONTNEED` over `[off, off + len)`, or to the
/// end for `len` 0), so the next read of it goes to the device.
///
/// # Errors
/// When the advice is refused.
pub fn drop_pages(file: &File, off: u64, len: u64, path: &Path) -> Result<(), Error> {
    rustix::fs::fadvise(file, off, NonZeroU64::new(len), Advice::DontNeed)
        .map_err(|e| Error::io("fadvise", path)(e.into()))
}

/// Drops the cached pages of every file at `paths`.
///
/// # Errors
/// When one cannot be opened or advised.
pub fn drop_cache(paths: &[impl AsRef<Path>]) -> Result<(), Error> {
    for p in paths {
        let f = open_read(p.as_ref(), Mode::Buffered)?;
        drop_pages(&f, 0, 0, p.as_ref())?;
    }
    Ok(())
}

/// Fills `buf` with bytes that depend on `seed` and on the position, so no two chunks of a data
/// file are alike (no device or file system can shortcut them).
pub fn fill(buf: &mut [u8], seed: u64) {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for chunk in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let b = x.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
}

/// Reads `buf.len()` bytes at `off`, fewer only at the end of the file; returns the count.
///
/// # Errors
/// When a read fails.
pub fn read_full_at(file: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        let n = file.read_at(&mut buf[done..], off + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

/// Writes all of `buf` at `off`.
///
/// # Errors
/// When a write fails or writes nothing.
pub fn write_full_at(file: &File, buf: &[u8], off: u64) -> std::io::Result<()> {
    file.write_all_at(buf, off)
}

/// Writes a new file of `size` bytes (a multiple of [`ALIGN`]) at `path` in `chunk`-byte
/// `O_DIRECT` writes of content seeded by `seed`, then `fsync`s it.
///
/// # Errors
/// When the file exists, a write fails, or `size` or `chunk` is not aligned.
pub fn write_data_file(path: &Path, size: u64, chunk: usize, seed: u64) -> Result<(), Error> {
    if !size.is_multiple_of(ALIGN as u64) || chunk == 0 || !chunk.is_multiple_of(ALIGN) {
        return Err(Error::Invalid(format!(
            "size {size} and chunk {chunk} must be multiples of {ALIGN}"
        )));
    }
    let f = create(path, Mode::Direct)?;
    let mut buf = AlignedBuf::new(chunk);
    let mut off = 0u64;
    while off < size {
        let n = usize::try_from((size - off).min(chunk as u64)).unwrap_or(chunk);
        fill(&mut buf.as_mut_slice()[..n], seed ^ off);
        write_full_at(&f, &buf.as_slice()[..n], off).map_err(Error::io("write", path))?;
        off += n as u64;
    }
    f.sync_all().map_err(Error::io("fsync", path))
}

/// The size and the free space (to an unprivileged writer) of the file system holding `path`,
/// in bytes.
///
/// # Errors
/// When it cannot be read.
pub fn fs_space(path: &Path) -> Result<(u64, u64), Error> {
    let v = rustix::fs::statvfs(path).map_err(|e| Error::io("statvfs", path)(e.into()))?;
    Ok((v.f_blocks * v.f_frsize, v.f_bavail * v.f_frsize))
}

/// The bytes the file system has allocated to `path` (`st_blocks` x 512).
///
/// # Errors
/// When it cannot be read.
pub fn allocated(path: &Path) -> Result<u64, Error> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(std::fs::metadata(path)
        .map_err(Error::io("stat", path))?
        .blocks()
        * 512)
}

/// Frees `[off, off + len)` of `file` and keeps its size (`FALLOC_FL_PUNCH_HOLE`).
///
/// # Errors
/// When the file system refuses.
pub fn punch(file: &File, off: u64, len: u64, path: &Path) -> Result<(), Error> {
    use rustix::fs::FallocateFlags;
    rustix::fs::fallocate(
        file,
        FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE,
        off,
        len,
    )
    .map_err(|e| Error::io("punch", path)(e.into()))
}

/// Allocates `[0, len)` of `file` (`fallocate` mode 0), so a sequential write fills allocated
/// space.
///
/// # Errors
/// When the file system refuses.
pub fn preallocate(file: &File, len: u64, path: &Path) -> Result<(), Error> {
    rustix::fs::fallocate(file, rustix::fs::FallocateFlags::empty(), 0, len)
        .map_err(|e| Error::io("fallocate", path)(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tierstore_testkit::dir::TestDir;

    #[test]
    fn aligned_buffer_starts_on_the_alignment() {
        let mut b = AlignedBuf::new(8192);
        assert_eq!(b.as_slice().len(), 8192);
        assert_eq!(b.as_slice().as_ptr().align_offset(ALIGN), 0);
        b.as_mut_slice()[0] = 7;
        assert_eq!(b.as_slice()[0], 7);
    }

    #[test]
    fn fill_depends_on_the_seed_and_covers_a_short_tail() {
        let mut a = [0u8; 13];
        let mut b = [0u8; 13];
        fill(&mut a, 1);
        fill(&mut b, 2);
        assert_ne!(a, b);
        assert_ne!(a[12], 0);
    }

    #[test]
    fn writes_a_data_file_and_reads_it_back_in_both_modes() {
        let d = TestDir::under(std::env::temp_dir(), "io");
        let p = d.path().join("data");
        write_data_file(&p, 3 * 4096, 8192, 5).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 3 * 4096);
        let mut want = vec![0u8; 8192];
        fill(&mut want, 5);
        for mode in [Mode::Buffered, Mode::Direct] {
            let f = open_read(&p, mode).unwrap();
            let mut buf = AlignedBuf::new(8192);
            assert_eq!(read_full_at(&f, buf.as_mut_slice(), 0).unwrap(), 8192);
            assert_eq!(buf.as_slice(), &want[..]);
            // The last chunk is short: the read stops at the end.
            assert_eq!(read_full_at(&f, buf.as_mut_slice(), 8192).unwrap(), 4096);
        }
        drop_cache(&[&p]).unwrap();
        assert!(write_data_file(&p, 4096, 4096, 0).is_err(), "exists");
        assert!(drop_cache(&[d.path().join("none")]).is_err());
        assert_eq!(Mode::Direct.name(), "direct");
        assert_eq!(Mode::Buffered.name(), "buffered");
    }

    #[test]
    fn refuses_unaligned_sizes_and_reports_failed_calls() {
        let d = TestDir::under(std::env::temp_dir(), "io");
        let p = d.path().join("x");
        for (size, chunk) in [(100, 4096), (4096, 0), (4096, 100)] {
            assert!(matches!(
                write_data_file(&p, size, chunk, 0),
                Err(Error::Invalid(_))
            ));
        }
        assert!(open_read(&p, Mode::Buffered).is_err());
        let f = std::fs::File::open(d.path()).unwrap();
        let mut buf = [0u8; 4];
        assert!(read_full_at(&f, &mut buf, 0).is_err());
        let ro = std::fs::File::open(d.path()).unwrap();
        assert!(write_full_at(&ro, &buf, 0).is_err());
        let bad = std::fs::File::open(d.path()).unwrap();
        // fadvise on a directory is accepted by Linux; on a pipe it is refused.
        let (r, _w) = rustix::pipe::pipe().unwrap();
        let pf = File::from(r);
        assert!(drop_pages(&pf, 0, 0, Path::new("pipe")).is_err());
        drop(bad);
    }

    #[test]
    fn space_allocation_punch_and_preallocate() {
        let d = TestDir::under(std::env::temp_dir(), "io");
        let (total, free) = fs_space(d.path()).unwrap();
        assert!(total >= free && total > 0);
        assert!(fs_space(&d.path().join("none")).is_err());
        let p = d.path().join("f");
        write_data_file(&p, 1 << 20, 1 << 16, 1).unwrap();
        let before = allocated(&p).unwrap();
        assert!(before >= 1 << 20);
        let f = OpenOptions::new().write(true).open(&p).unwrap();
        punch(&f, 0, 512 << 10, &p).unwrap();
        assert!(allocated(&p).unwrap() < before);
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 1 << 20);
        preallocate(&f, 2 << 20, &p).unwrap();
        assert!(allocated(&p).unwrap() >= 2 << 20);
        assert!(allocated(&d.path().join("none")).is_err());
        let ro = File::open(&p).unwrap();
        assert!(punch(&ro, 0, 4096, &p).is_err());
        assert!(preallocate(&ro, 4096, &p).is_err());
    }
}
