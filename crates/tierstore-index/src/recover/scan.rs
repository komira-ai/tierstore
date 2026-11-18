//! Reading files for recovery: whole files, the tail of a region, the trailer and the footer found
//! from the end, and the windowed scan of an open segment.

use std::io;

use tierstore_format::FormatError;
use tierstore_format::footer::{FOOTER_MAGIC, ShardFooter};
use tierstore_format::record::{RECORD_HEADER_LEN, RecordHeader, decode_record};
use tierstore_format::trailer::{TRAILER_FOOTER_LEN, TRAILER_MAGIC, Trailer};

use super::RecoverError;
use crate::fs::Fs;

/// The bytes read from the tail of a region at first, for a trailer: a larger one is read again.
const TRAILER_GUESS: u64 = 64 * 1024;

/// The bytes read from the tail of a shard file at first, for a footer (its least length).
const FOOTER_GUESS: u64 = 4096;

pub(super) fn io_err<T>(path: &str, r: io::Result<T>) -> Result<T, RecoverError> {
    r.map_err(|source| RecoverError::Io {
        path: path.to_owned(),
        source,
    })
}

pub(super) fn fmt_err<T>(path: &str, r: Result<T, FormatError>) -> Result<T, RecoverError> {
    r.map_err(|source| RecoverError::Format {
        path: path.to_owned(),
        source,
    })
}

/// Every byte of the file at `path`.
pub(super) fn read_all(fs: &dyn Fs, path: &str) -> Result<Vec<u8>, RecoverError> {
    let len = io_err(path, fs.len(path))?;
    io_err(path, fs.read_at(path, 0, len))
}

/// The little-endian `u64` at `at`; the caller has checked that the bytes are there.
fn u64_at(buf: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&buf[at..at + 8]);
    u64::from_le_bytes(a)
}

/// The little-endian `u32` at `at`; the caller has checked that the bytes are there.
fn u32_at(buf: &[u8], at: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&buf[at..at + 4]);
    u32::from_le_bytes(a)
}

/// The length of the trailer that ends `tail`, if its last 96 bytes are a trailer footer.
fn trailer_total(tail: &[u8]) -> Option<u64> {
    let at = tail.len().checked_sub(TRAILER_FOOTER_LEN)?;
    if u32_at(tail, at) != TRAILER_MAGIC {
        return None;
    }
    let entries = u64_at(tail, at + 16);
    let cells = u64_at(tail, at + 24);
    Some(
        entries
            .saturating_add(cells.saturating_mul(4))
            .saturating_add(TRAILER_FOOTER_LEN as u64),
    )
}

/// The length of the footer that ends `tail`, if its last 16 bytes locate one.
fn footer_total(tail: &[u8]) -> Option<u64> {
    let at = tail.len().checked_sub(16)?;
    if u32_at(tail, at + 8) != FOOTER_MAGIC {
        return None;
    }
    Some(u64_at(tail, at))
}

/// The last `total` bytes of the region `[0, end)` of the file, where `total_of` says from the
/// last bytes read how long the structure at the end is: one read of up to `first` bytes, and a
/// second when the structure is longer. `None` when the end is not shaped like the structure.
fn read_tail(
    fs: &dyn Fs,
    path: &str,
    end: u64,
    first: u64,
    total_of: fn(&[u8]) -> Option<u64>,
) -> Result<Option<Vec<u8>>, RecoverError> {
    let start = end.saturating_sub(first);
    let mut buf = io_err(path, fs.read_at(path, start, end - start))?;
    let Some(total) = total_of(&buf) else {
        return Ok(None);
    };
    if total > end {
        return Ok(None);
    }
    let have = buf.len() as u64;
    if total > have {
        return Ok(Some(io_err(path, fs.read_at(path, end - total, total))?));
    }
    Ok(Some(buf.split_off((have - total) as usize)))
}

/// What the end of a region holds.
pub(super) enum Tail {
    /// Not shaped like a trailer.
    Absent,
    /// Shaped like a trailer and not one.
    Bad(FormatError),
    /// The trailer that covers the records of `[0, end)`.
    Found(Trailer),
}

/// The trailer at the end of the region `[0, end)` of the file at `path`.
pub(super) fn trailer_at(fs: &dyn Fs, path: &str, end: u64) -> Result<Tail, RecoverError> {
    let Some(bytes) = read_tail(fs, path, end, TRAILER_GUESS, trailer_total)? else {
        return Ok(Tail::Absent);
    };
    Ok(match Trailer::decode(&bytes) {
        Ok(t) if t.data_len.saturating_add(bytes.len() as u64) == end => Tail::Found(t),
        Ok(_) => Tail::Bad(FormatError::Invalid {
            what: "block trailer",
            reason: "the trailer does not start where its records end",
        }),
        Err(e) => Tail::Bad(e),
    })
}

/// The footer of the shard or copy file at `path`, `len` bytes long.
pub(super) fn footer_at(fs: &dyn Fs, path: &str, len: u64) -> Result<ShardFooter, RecoverError> {
    let Some(bytes) = read_tail(fs, path, len, FOOTER_GUESS, footer_total)? else {
        return fmt_err(
            path,
            Err(FormatError::Invalid {
                what: "shard footer",
                reason: "no footer at the end of the file",
            }),
        );
    };
    fmt_err(path, ShardFooter::decode(&bytes))
}

/// A valid record of an open segment: its key, header and offset.
pub(super) struct Scanned {
    pub key: Vec<u8>,
    pub header: RecordHeader,
    pub offset: u64,
}

/// What the scan of an open segment found.
pub(super) struct OpenScan {
    pub records: Vec<Scanned>,
    pub valid_len: u64,
    pub torn: bool,
}

/// The bytes of a file read in windows.
struct Window<'a> {
    fs: &'a dyn Fs,
    path: &'a str,
    len: u64,
    window: u64,
    base: u64,
    buf: Vec<u8>,
}

impl Window<'_> {
    /// The bytes at `pos`, at least `need` of them unless the file ends first.
    fn get(&mut self, pos: u64, need: u64) -> Result<&[u8], RecoverError> {
        let have_end = self.base + self.buf.len() as u64;
        let wanted_end = pos.saturating_add(need).min(self.len);
        if have_end < wanted_end {
            let want = need.max(self.window).min(self.len - pos);
            self.buf = io_err(self.path, self.fs.read_at(self.path, pos, want))?;
            self.base = pos;
        }
        let off = ((pos - self.base) as usize).min(self.buf.len());
        Ok(&self.buf[off..])
    }
}

/// Whether every byte of `[from, len)` is zero.
fn rest_is_zero(
    fs: &dyn Fs,
    path: &str,
    from: u64,
    len: u64,
    window: u64,
) -> Result<bool, RecoverError> {
    let mut at = from;
    while at < len {
        let bytes = io_err(path, fs.read_at(path, at, window.max(1).min(len - at)))?;
        if bytes.iter().any(|b| *b != 0) {
            return Ok(false);
        }
        at += (bytes.len() as u64).max(1);
    }
    Ok(true)
}

/// Scans the records of the open segment at `path` (`len` bytes) from `from` to the last record
/// whose checksums verify, reading `window` bytes at a time. Bytes after it that are not all zero
/// are cut off the file (truncated and synced). A scan that starts past the end finds nothing.
pub(super) fn scan_open(
    fs: &dyn Fs,
    path: &str,
    len: u64,
    from: u64,
    window: u64,
) -> Result<OpenScan, RecoverError> {
    let mut pos = from.min(len);
    let mut w = Window {
        fs,
        path,
        len,
        window,
        base: 0,
        buf: Vec::new(),
    };
    let mut records = Vec::new();
    let torn = loop {
        if pos >= len {
            break false;
        }
        let Ok(header) = RecordHeader::decode(w.get(pos, RECORD_HEADER_LEN as u64)?) else {
            break !rest_is_zero(fs, path, pos, len, window)?;
        };
        let need = header.record_len();
        if need > len - pos {
            break true;
        }
        match decode_record(w.get(pos, need)?) {
            Ok((rec, used)) => {
                records.push(Scanned {
                    key: rec.key.to_vec(),
                    header: rec.header,
                    offset: pos,
                });
                pos += used;
            }
            Err(_) => break true,
        }
    };
    if torn {
        io_err(path, fs.truncate(path, pos))?;
        io_err(path, fs.sync(path))?;
    }
    Ok(OpenScan {
        records,
        valid_len: pos,
        torn,
    })
}
