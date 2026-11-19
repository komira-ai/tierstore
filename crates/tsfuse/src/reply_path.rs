//! The reply path of a READ (`docs/design/fetch-cache-serve.md`, 5.3 item 2, row T-F4): by
//! `splice(2)` through a pooled pipe at and above a threshold, by `pread` into a pooled buffer and
//! one `writev` below it, and the `posix_fadvise(DONTNEED)` helper the server uses on `durable`
//! cells (4.8).
//!
//! **Two steps, so the post-read recheck sits between them (5.1).** [`ReplyPath::fill`] reads the
//! bytes (into a pipe, behind the reply's header, or into a buffer) and hands back a [`Filled`];
//! the caller reloads the state of every covered cell, then either [`Filled::commit`]s the reply
//! or [`Filled::discard`]s the bytes and gets its [`Replier`] back to park the read again. The
//! caller holds its group reader counters until `commit` returns (5.1, step 5), so the cells it
//! read cannot be punched in between.
//!
//! **Splice.** `fill` writes the 16-byte out header into an empty pipe, then splices the data in
//! until the full length is in the pipe; `commit` splices the pipe into the request's own device
//! in one call. The header names exactly the bytes in the pipe: a splice that ends short (the data
//! file ends first; only a read at EOF may be short) or that fails gives up on the pipe, which is
//! closed, never reused, and the reply is built by `pread`, whose header is written after the read
//! from the bytes actually read. The kernel refuses a spliced reply whose length is not the bytes
//! in the pipe with `EINVAL` and leaves the request answerable (K15); should it refuse one, the
//! pipe is drained and the drained reply written again with `writev`.
//!
//! **Pipe size.** A pipe holds a number of buffers (`F_GETPIPE_SZ` / page size), not bytes: the
//! header takes one, and the data one per page of the file it touches, so a reply of `len` bytes
//! at `offset` needs `1 + ceil((offset % page + len) / page)` of them. Every pipe is sized with
//! `F_SETPIPE_SZ` for the largest reply of the session at the worst offset before it is used (a
//! splice into a full pipe moves a short count and the next would block, K17). A server without
//! `CAP_SYS_RESOURCE` is held to `fs.pipe-max-size` (1 MiB on the build nodes) and to a per-user
//! total: where the kernel refuses the size, the next smaller one is asked for, down to the pipe's
//! default, and a reply that does not fit the pipe it got goes by `pread`. Every splice into a pipe
//! is `SPLICE_F_NONBLOCK`, so a miscount is a short count (and a `pread` reply), never a blocked
//! reader thread.
//!
//! **Threshold.** A reply of at least [`ReplyConfig::splice_min`] data bytes (starting value
//! [`SPLICE_MIN`], 16 KiB) is spliced when its device takes spliced replies
//! ([`crate::ReplyDevice::splice_target`]: a `/dev/fuse` descriptor does, an io_uring queue does
//! not); a smaller one, an empty one, and every reply with `splice_min` unset go by `pread`.
//!
//! **Counts.** [`ReplyCounts`] holds the bytes of the replies written each way, the
//! `splice_bytes_total` and `pread_bytes_total` of section 12; [`Dontneed::asked_bytes`] is
//! `fadvise_dontneed_bytes_total` (bytes asked for, not dropped).

use std::io;
use std::num::NonZeroU64;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use rustix::fs::Advice;
use rustix::io::Errno as Sys;
use rustix::pipe::{
    PipeFlags, SpliceFlags, fcntl_getpipe_size, fcntl_setpipe_size, pipe_with, splice,
};

use crate::abi::Negotiated;
use crate::reply::{Completion, Errno, OUT_HEADER_LEN, Replier, encode_out_header};

/// The starting splice threshold of 5.3 item 2: replies of fewer data bytes go by `pread`.
pub const SPLICE_MIN: usize = 16 * 1024;

/// The `max_pages` of a connection whose INIT reply did not carry `FUSE_MAX_PAGES` (the kernel's
/// `FUSE_DEFAULT_MAX_PAGES_PER_REQ`, K21).
const DEFAULT_MAX_PAGES: usize = 32;

/// How a [`ReplyPath`] sends READ replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyConfig {
    /// Replies of at least this many data bytes are spliced; `None` sends every reply by `pread`
    /// and `writev` (the benchmark's V3 arm, 17.1).
    pub splice_min: Option<usize>,
    /// The data bytes of the largest READ reply: every pipe is sized for it.
    pub max_data: usize,
    /// How many idle pipes, and how many idle buffers, are kept for reuse (one per reader thread
    /// and completing task that replies at once is enough; more are made when needed and closed
    /// or freed when returned to a full pool).
    pub pool: usize,
    /// `SPLICE_F_MOVE` on the splice into the device: the kernel then moves the pages of a READ
    /// sent by readahead instead of copying them, which takes them out of the data file's page
    /// cache (K16). Off unless the benchmark (V4) says otherwise.
    pub move_pages: bool,
}

impl ReplyConfig {
    /// The configuration for replies to `negotiated`'s connection: the starting threshold, pipes
    /// sized for a READ of the connection's `max_pages` pages (a READ is never larger, K21), and
    /// `pool` idle pipes and buffers.
    #[must_use]
    pub fn for_session(negotiated: &Negotiated, pool: usize) -> Self {
        let max_pages = match usize::from(negotiated.reply.max_pages) {
            0 => DEFAULT_MAX_PAGES,
            pages => pages,
        };
        Self {
            splice_min: Some(SPLICE_MIN),
            max_data: max_pages * rustix::param::page_size(),
            pool,
            move_pages: false,
        }
    }
}

/// Why a [`ReplyPath`] could not be made.
#[derive(Debug, thiserror::Error)]
pub enum ReplyPathError {
    /// The first pipe could not be made or sized (`pipe2` or `fcntl`, for example `EMFILE`).
    #[error("making the first pipe of the splice reply path failed: {0}")]
    Pipe(#[source] io::Error),
}

/// The bytes of the READ replies a [`ReplyPath`] wrote, by the way they went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplyCounts {
    /// Data bytes of replies spliced into their device (`splice_bytes_total`).
    pub splice_bytes: u64,
    /// Data bytes of replies written with `writev` from a buffer (`pread_bytes_total`): read by
    /// `pread`, or drained from a pipe whose splice the device refused.
    pub pread_bytes: u64,
}

/// A pipe of the pool and the buffers it holds.
#[derive(Debug)]
struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
    /// `F_GETPIPE_SZ / page`: how many pipe buffers it holds.
    slots: usize,
}

impl Pipe {
    /// A non-blocking, close-on-exec pipe sized for `want` bytes, or for the largest size below
    /// that the kernel grants, or left at its default size.
    fn new(want: usize, page: usize) -> io::Result<Self> {
        let (read, write) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)?;
        let default = fcntl_getpipe_size(&write)?;
        let size = grant(want, default, &mut |n| fcntl_setpipe_size(&write, n));
        Ok(Self {
            read,
            write,
            slots: size / page,
        })
    }

    /// Whether a reply of `len` data bytes read from `offset` fits.
    fn fits(&self, offset: u64, len: usize, page: usize) -> bool {
        slots_for(offset, len, page) <= self.slots
    }
}

/// The size a pipe of `default` bytes gets when `want` is asked for: `want`, or where the kernel
/// refuses it (`EPERM` past `fs.pipe-max-size` or the per-user total without `CAP_SYS_RESOURCE`)
/// half of it, and so on while it is above `default`; `default` when every size is refused.
fn grant(
    mut want: usize,
    default: usize,
    set: &mut dyn FnMut(usize) -> rustix::io::Result<usize>,
) -> usize {
    while want > default {
        if let Ok(got) = set(want) {
            return got;
        }
        want /= 2;
    }
    default
}

/// The pipe buffers a reply of `len` data bytes read from `offset` takes: one for the header and
/// one for each page of the data file the data touches.
fn slots_for(offset: u64, len: usize, page: usize) -> usize {
    match len {
        0 => 1,
        _ => {
            // `page` is a power of two below `u64::MAX`, so the remainder fits a `usize`.
            let head = (offset % page as u64) as usize;
            1 + (head + len).div_ceil(page)
        }
    }
}

/// The reply path of READ replies: a pool of sized pipes, a pool of buffers, and the counts.
#[derive(Debug)]
pub struct ReplyPath {
    config: ReplyConfig,
    page: usize,
    /// The bytes every pipe is asked to hold.
    want: usize,
    /// The size the first pipe got.
    pipe_size: usize,
    pipes: Mutex<Vec<Pipe>>,
    buffers: Mutex<Vec<Vec<u8>>>,
    splice_bytes: AtomicU64,
    pread_bytes: AtomicU64,
    /// Makes a pipe when the pool has none ([`Pipe::new`]; a unit test's stand-in fails).
    make_pipe: fn(usize, usize) -> io::Result<Pipe>,
}

impl ReplyPath {
    /// A reply path for `config`. It makes and sizes one pipe at once, so a server learns at
    /// start the size the kernel grants ([`Self::pipe_size`]) and fails here, not on its first
    /// READ, when it cannot make pipes at all.
    ///
    /// # Errors
    ///
    /// [`ReplyPathError::Pipe`] when the first pipe cannot be made.
    pub fn new(config: ReplyConfig) -> Result<Self, ReplyPathError> {
        Self::with_pipes(config, Pipe::new)
    }

    /// [`Self::new`] with the pipes made by `make_pipe`.
    fn with_pipes(
        config: ReplyConfig,
        make_pipe: fn(usize, usize) -> io::Result<Pipe>,
    ) -> Result<Self, ReplyPathError> {
        let page = rustix::param::page_size();
        // The largest reply at the worst offset: one buffer for the header, and its data
        // straddling one page more than it fills.
        let want = (2 + config.max_data.div_ceil(page)) * page;
        let first = make_pipe(want, page).map_err(ReplyPathError::Pipe)?;
        let pipe_size = first.slots * page;
        let path = Self {
            config,
            page,
            want,
            pipe_size,
            pipes: Mutex::new(Vec::new()),
            buffers: Mutex::new(Vec::new()),
            splice_bytes: AtomicU64::new(0),
            pread_bytes: AtomicU64::new(0),
            make_pipe,
        };
        path.put_pipe(first);
        Ok(path)
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &ReplyConfig {
        &self.config
    }

    /// The bytes the first pipe holds (`F_GETPIPE_SZ`): at least the largest reply plus its
    /// header at any offset, unless the kernel refused that size.
    #[must_use]
    pub fn pipe_size(&self) -> usize {
        self.pipe_size
    }

    /// The bytes of the replies written so far, by the way they went.
    #[must_use]
    pub fn counts(&self) -> ReplyCounts {
        ReplyCounts {
            splice_bytes: self.splice_bytes.load(Ordering::Relaxed),
            pread_bytes: self.pread_bytes.load(Ordering::Relaxed),
        }
    }

    /// Reads up to `len` bytes of `data` at `offset` for the reply `replier` will send: into a
    /// pipe behind the reply's header when the reply is spliced, otherwise into a buffer. Fewer
    /// bytes are read only where `data` ends first. Nothing is written to the device yet: the
    /// caller rechecks its cells, then commits or discards.
    ///
    /// # Errors
    ///
    /// [`FillFailed`], with the replier, when `pread` fails (the caller answers it, `EIO`).
    pub fn fill(
        &self,
        replier: Replier,
        data: BorrowedFd<'_>,
        offset: u64,
        len: usize,
    ) -> Result<Filled<'_>, FillFailed> {
        if self.splices(&replier, len)
            && let Some(pipe) = self.take_pipe()
        {
            match pipe.fits(offset, len, self.page) {
                true => {
                    if splice_in(&pipe, replier.unique, data, offset, len) {
                        return Ok(self.filled(replier, Bytes::Pipe { pipe, len }));
                    }
                    // Short or failed: the pipe holds a partial reply and is closed.
                }
                false => self.put_pipe(pipe),
            }
        }
        match self.pread_in(data, offset, len) {
            Ok(buf) => Ok(self.filled(replier, Bytes::Buffer(buf))),
            Err(error) => Err(FillFailed { replier, error }),
        }
    }

    /// Whether a reply of `len` data bytes to `replier` is spliced.
    fn splices(&self, replier: &Replier, len: usize) -> bool {
        len > 0
            && self.config.splice_min.is_some_and(|min| len >= min)
            && replier.device.splice_target().is_some()
    }

    fn filled(&self, replier: Replier, bytes: Bytes) -> Filled<'_> {
        Filled {
            path: self,
            replier,
            bytes,
        }
    }

    /// An idle pipe, or a new one; `None` when none can be made (the reply then goes by
    /// `pread`).
    fn take_pipe(&self) -> Option<Pipe> {
        let idle = self
            .pipes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        idle.or_else(|| (self.make_pipe)(self.want, self.page).ok())
    }

    /// Returns an empty pipe to the pool, or closes it when the pool is full.
    fn put_pipe(&self, pipe: Pipe) {
        let mut pipes = self.pipes.lock().unwrap_or_else(PoisonError::into_inner);
        if pipes.len() < self.config.pool {
            pipes.push(pipe);
        }
    }

    fn take_buffer(&self) -> Vec<u8> {
        let idle = self
            .buffers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        idle.unwrap_or_default()
    }

    fn put_buffer(&self, mut buf: Vec<u8>) {
        buf.clear();
        let mut buffers = self.buffers.lock().unwrap_or_else(PoisonError::into_inner);
        if buffers.len() < self.config.pool {
            buffers.push(buf);
        }
    }

    /// `pread`s up to `len` bytes of `data` at `offset` into a pooled buffer, stopping at EOF.
    fn pread_in(&self, data: BorrowedFd<'_>, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = self.take_buffer();
        buf.resize(len, 0);
        let mut got = 0;
        while got < len {
            match rustix::io::pread(data, &mut buf[got..], offset.saturating_add(got as u64)) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) => {
                    self.put_buffer(buf);
                    return Err(e.into());
                }
            }
        }
        buf.truncate(got);
        Ok(buf)
    }

    /// Drains a pipe whose splice into the device was refused (`EINVAL`: the kernel takes
    /// nothing from the pipe then, K15) into a pooled buffer: the reply, if all `total` bytes of
    /// it are still there.
    fn drain(&self, pipe: &Pipe, total: usize) -> Option<Vec<u8>> {
        let mut buf = self.take_buffer();
        buf.resize(total, 0);
        let mut got = 0;
        while got < total {
            match rustix::io::read(&pipe.read, &mut buf[got..]) {
                // Empty (`EAGAIN`; never 0, the write end is open) or failed: the reply is not
                // all there.
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
        }
        match got == total {
            true => Some(buf),
            false => {
                self.put_buffer(buf);
                None
            }
        }
    }
}

/// Writes the header of a reply of `len` data bytes to `unique` into the empty `pipe`, then
/// splices `len` bytes of `data` at `offset` behind it; whether all of them are in the pipe.
fn splice_in(pipe: &Pipe, unique: u64, data: BorrowedFd<'_>, offset: u64, len: usize) -> bool {
    let header = encode_out_header(unique, None, len);
    if !matches!(rustix::io::write(&pipe.write, &header), Ok(OUT_HEADER_LEN)) {
        return false;
    }
    let mut off = offset;
    let mut got = 0;
    while got < len {
        match splice(
            data,
            Some(&mut off),
            &pipe.write,
            None,
            len - got,
            SpliceFlags::NONBLOCK,
        ) {
            // EOF first (0), the pipe full (`EAGAIN`: a miscount), or a file that cannot be
            // spliced: the reply goes by `pread`.
            Ok(0) | Err(_) => return false,
            Ok(n) => got += n,
        }
    }
    true
}

/// The flags of the splice of a pipe into the device: non-blocking, and `SPLICE_F_MOVE` when
/// [`ReplyConfig::move_pages`] asks for it.
fn device_flags(move_pages: bool) -> SpliceFlags {
    match move_pages {
        true => SpliceFlags::NONBLOCK | SpliceFlags::MOVE,
        false => SpliceFlags::NONBLOCK,
    }
}

/// The bytes of a filled reply.
#[derive(Debug)]
enum Bytes {
    /// The header and `len` data bytes, in a pipe.
    Pipe { pipe: Pipe, len: usize },
    /// The data bytes (the header is made when they are written).
    Buffer(Vec<u8>),
}

/// A READ reply whose bytes are read and not yet sent: [`Self::commit`] sends it,
/// [`Self::discard`] drops the bytes and gives the replier back. Dropped instead, it closes its
/// pipe or frees its buffer and its replier answers `EIO`.
#[derive(Debug)]
#[must_use = "a filled reply is committed or discarded; dropping it answers EIO"]
pub struct Filled<'a> {
    path: &'a ReplyPath,
    replier: Replier,
    bytes: Bytes,
}

impl Filled<'_> {
    /// The data bytes of the reply: those asked for, or fewer where the data file ended.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.bytes {
            Bytes::Pipe { len, .. } => *len,
            Bytes::Buffer(buf) => buf.len(),
        }
    }

    /// Whether the reply carries no data (a read at or past EOF, or of zero bytes).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the reply goes by splice (its bytes are in a pipe).
    #[must_use]
    pub fn is_spliced(&self) -> bool {
        matches!(self.bytes, Bytes::Pipe { .. })
    }

    /// Sends the reply on the request's own device, once (an INTERRUPT that answered the request
    /// first makes it [`Completion::AlreadyAnswered`], and the bytes are dropped). A spliced reply
    /// goes in one splice of the pipe; should the device refuse it with `EINVAL` (the request
    /// stays answerable, K15), the pipe is drained and the same bytes written with `writev`, or,
    /// where they cannot be drained whole, the request is answered `EIO` and the refusal returned.
    pub fn commit(self) -> Completion {
        let Self {
            path,
            replier,
            bytes,
        } = self;
        if !replier.slot.take() {
            if let Bytes::Buffer(buf) = bytes {
                path.put_buffer(buf);
            }
            return Completion::AlreadyAnswered;
        }
        match bytes {
            Bytes::Buffer(buf) => {
                let written = replier.write(None, &[&buf]);
                path.count_pread(&written, buf.len());
                path.put_buffer(buf);
                written
            }
            Bytes::Pipe { pipe, len } => {
                let total = OUT_HEADER_LEN + len;
                let flags = device_flags(path.config.move_pages);
                let spliced = replier
                    .device
                    .splice_target()
                    .map_or(Err(Sys::INVAL), |dev| {
                        splice(&pipe.read, None, dev, None, total, flags)
                    });
                match spliced.map(|n| n == total) {
                    Ok(true) => {
                        path.splice_bytes.fetch_add(len as u64, Ordering::Relaxed);
                        path.put_pipe(pipe);
                        Completion::Written
                    }
                    // Spliced short (the device took part of it) or refused as a whole.
                    Ok(false) | Err(Sys::INVAL) => path.rewrite(&replier, &pipe, len),
                    Err(e) => Completion::DeviceError(e.into()),
                }
            }
        }
    }

    /// Drops the bytes (the post-read recheck found a cell no longer `present`: the pipe is
    /// closed, the buffer returned) and gives back the replier, unanswered, to park the read.
    pub fn discard(self) -> Replier {
        if let Bytes::Buffer(buf) = self.bytes {
            self.path.put_buffer(buf);
        }
        self.replier
    }
}

impl ReplyPath {
    fn count_pread(&self, written: &Completion, len: usize) {
        if matches!(written, Completion::Written) {
            self.pread_bytes.fetch_add(len as u64, Ordering::Relaxed);
        }
    }

    /// The reply of `len` data bytes whose splice the device refused, written again from the
    /// drained pipe with `writev`; `EIO` when the pipe no longer holds all of it.
    fn rewrite(&self, replier: &Replier, pipe: &Pipe, len: usize) -> Completion {
        match self.drain(pipe, OUT_HEADER_LEN + len) {
            Some(reply) => {
                // The drained bytes are the header `fill` wrote and the data after it.
                let written = replier.write(None, &[&reply[OUT_HEADER_LEN..]]);
                self.count_pread(&written, len);
                self.put_buffer(reply);
                written
            }
            None => {
                let refused = replier.write(Some(Errno::EIO), &[]);
                match refused {
                    Completion::Written => Completion::DeviceError(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the device refused a spliced reply and its pipe could not be drained \
                         whole; the request was answered EIO",
                    )),
                    other => other,
                }
            }
        }
    }
}

/// A [`ReplyPath::fill`] whose `pread` failed: the replier, unanswered, and the error.
#[derive(Debug)]
pub struct FillFailed {
    /// The request's replier: the caller answers it (`EIO`).
    pub replier: Replier,
    /// The `pread` error.
    pub error: io::Error,
}

/// `posix_fadvise(DONTNEED)` on whole cells of a pack file, counting the bytes asked for (4.8).
///
/// Only the server calls it, and only on `durable` cells: on a dirty page DONTNEED starts its
/// writeback (unbatched, and no durability) and keeps it; it drops only the clean pages of the
/// range that nothing maps, pipes or reads, so a splice in flight keeps its pages (K26). The call
/// can block while it starts writeback: call it from a thread that may block. The range is given
/// in cells, so it is cell-aligned by construction.
#[derive(Debug)]
pub struct Dontneed {
    cell: NonZeroU64,
    asked: AtomicU64,
}

/// Why a [`Dontneed::cells`] call did nothing.
#[derive(Debug, thiserror::Error)]
pub enum DontneedError {
    /// The cells lie past the largest offset of a file (`i64::MAX`).
    #[error("cells {first}..{first}+{cells} of {cell} bytes lie past the largest file offset")]
    Range {
        /// The first cell.
        first: u64,
        /// The number of cells.
        cells: u64,
        /// The cell size, bytes.
        cell: u64,
    },
    /// `posix_fadvise` failed (`ESPIPE` on a pipe, `EBADF`).
    #[error("posix_fadvise(DONTNEED) of cells {first}..{first}+{cells} failed: {source}")]
    Advise {
        /// The first cell.
        first: u64,
        /// The number of cells.
        cells: u64,
        /// The error.
        #[source]
        source: io::Error,
    },
}

impl Dontneed {
    /// The helper for cells of `cell` bytes.
    #[must_use]
    pub fn new(cell: NonZeroU64) -> Self {
        Self {
            cell,
            asked: AtomicU64::new(0),
        }
    }

    /// Advises the kernel to drop the pages of `cells` cells of `file` from cell `first` on;
    /// the bytes asked for.
    ///
    /// # Errors
    ///
    /// [`DontneedError::Range`] past the largest file offset, [`DontneedError::Advise`] when the
    /// call fails; neither counts.
    pub fn cells(
        &self,
        file: BorrowedFd<'_>,
        first: u64,
        cells: NonZeroU64,
    ) -> Result<u64, DontneedError> {
        let range = DontneedError::Range {
            first,
            cells: cells.get(),
            cell: self.cell.get(),
        };
        let offset = first.checked_mul(self.cell.get());
        let len = cells.checked_mul(self.cell);
        let (offset, len) = match (offset, len) {
            (Some(offset), Some(len))
                if offset
                    .checked_add(len.get())
                    .is_some_and(|end| end <= i64::MAX as u64) =>
            {
                (offset, len)
            }
            _ => return Err(range),
        };
        rustix::fs::fadvise(file, offset, Some(len), Advice::DontNeed).map_err(|e| {
            DontneedError::Advise {
                first,
                cells: cells.get(),
                source: e.into(),
            }
        })?;
        self.asked.fetch_add(len.get(), Ordering::Relaxed);
        Ok(len.get())
    }

    /// The bytes asked for so far (`fadvise_dontneed_bytes_total`).
    #[must_use]
    pub fn asked_bytes(&self) -> u64 {
        self.asked.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
#[path = "reply_path/tests.rs"]
mod tests;
