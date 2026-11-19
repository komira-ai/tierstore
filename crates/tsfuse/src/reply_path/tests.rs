//! The reply path against memfds standing in for the data file and the device: a memfd takes a
//! splice from a pipe and a `writev` as `/dev/fuse` does (at its file position), so the bytes a
//! reply put on the "device" are read back and compared whole. The kernel's own answer to a
//! spliced reply is checked on a real mount by `tests/kernel_reply_integration.rs`.
#![allow(
    clippy::unwrap_used,
    reason = "a unit test: a failed unwrap is the failure it reports"
)]

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::Arc;

use rustix::fs::{
    FallocateFlags, MemfdFlags, OFlags, fallocate, fcntl_getfl, fcntl_setfl, memfd_create,
};

use super::*;
use crate::abi::{InitFlags, InitOut};
use crate::dev::DevFd;
use crate::reply::ReplyDevice;
use crate::tokens::Tokens;

const UNIQUE: u64 = 0x5eed;

fn page() -> usize {
    rustix::param::page_size()
}

/// The byte at `offset` of the data files.
fn byte(offset: usize) -> u8 {
    (offset % 251) as u8
}

fn content(offset: usize, len: usize) -> Vec<u8> {
    (offset..offset + len).map(byte).collect()
}

/// A memfd holding `len` bytes of [`content`].
fn data_file(len: usize) -> OwnedFd {
    let fd = memfd_create("tsfuse-reply-data", MemfdFlags::CLOEXEC).unwrap();
    assert_eq!(rustix::io::write(&fd, &content(0, len)).unwrap(), len);
    fd
}

/// Everything written to a memfd device.
fn written(dev: &impl AsFd) -> Vec<u8> {
    let len = rustix::fs::fstat(dev).unwrap().st_size as usize;
    let mut buf = vec![0u8; len];
    assert_eq!(rustix::io::pread(dev, &mut buf, 0).unwrap(), len);
    buf
}

/// The bytes of a success reply carrying `data`.
fn reply_of(data: &[u8]) -> Vec<u8> {
    let mut r = encode_out_header(UNIQUE, None, data.len()).to_vec();
    r.extend_from_slice(data);
    r
}

/// A device that takes spliced replies: a memfd `DevFd`.
fn device() -> Arc<DevFd> {
    let fd = memfd_create("tsfuse-reply-device", MemfdFlags::CLOEXEC).unwrap();
    Arc::new(DevFd::from_fd(fd))
}

/// A device that takes replies only by `send` (as an io_uring queue).
struct SendOnly(DevFd);

impl ReplyDevice for SendOnly {
    fn send(&self, parts: &[&[u8]]) -> io::Result<()> {
        self.0.send(parts)
    }
}

/// A device that refuses every write with `errno`.
struct Refuses(i32);

impl ReplyDevice for Refuses {
    fn send(&self, _parts: &[&[u8]]) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(self.0))
    }
}

fn config(max_data: usize) -> ReplyConfig {
    ReplyConfig {
        splice_min: Some(SPLICE_MIN),
        max_data,
        pool: 4,
        move_pages: false,
    }
}

fn path(config: ReplyConfig) -> ReplyPath {
    ReplyPath::new(config).unwrap()
}

fn idle_pipes(path: &ReplyPath) -> usize {
    path.pipes.lock().unwrap().len()
}

fn idle_buffers(path: &ReplyPath) -> usize {
    path.buffers.lock().unwrap().len()
}

/// Fills and commits a reply of `len` bytes at `offset` of `data` to a new memfd device; whether
/// it was spliced, and the device's bytes.
fn send(path: &ReplyPath, data: &OwnedFd, offset: u64, len: usize) -> (bool, Vec<u8>) {
    let (tokens, dev) = (Tokens::new(), device());
    let filled = path
        .fill(
            tokens.register(UNIQUE, dev.clone()),
            data.as_fd(),
            offset,
            len,
        )
        .unwrap();
    let spliced = filled.is_spliced();
    assert!(matches!(filled.commit(), Completion::Written));
    assert_eq!(
        tokens.registered(),
        0,
        "the commit took the request's entry"
    );
    (spliced, written(&*dev))
}

fn counts(splice_bytes: usize, pread_bytes: usize) -> ReplyCounts {
    ReplyCounts {
        splice_bytes: splice_bytes as u64,
        pread_bytes: pread_bytes as u64,
    }
}

#[test]
fn a_reply_of_exactly_the_threshold_is_spliced_and_its_header_names_the_bytes_spliced() {
    let (path, data) = (path(config(128 * 1024)), data_file(64 * 1024));
    let (spliced, bytes) = send(&path, &data, 0, SPLICE_MIN);
    assert!(spliced);
    assert_eq!(bytes, reply_of(&content(0, SPLICE_MIN)));
    assert_eq!(path.counts(), counts(SPLICE_MIN, 0));
    assert_eq!(
        idle_pipes(&path),
        1,
        "the emptied pipe went back to the pool"
    );
}

#[test]
fn a_reply_one_byte_under_the_threshold_goes_by_pread_and_writev() {
    let (path, data) = (path(config(128 * 1024)), data_file(64 * 1024));
    let (spliced, bytes) = send(&path, &data, 100, SPLICE_MIN - 1);
    assert!(!spliced);
    assert_eq!(bytes, reply_of(&content(100, SPLICE_MIN - 1)));
    assert_eq!(path.counts(), counts(0, SPLICE_MIN - 1));
    assert_eq!(idle_buffers(&path), 1, "the buffer went back to the pool");
}

#[test]
fn without_a_splice_minimum_every_reply_goes_by_pread() {
    let path = path(ReplyConfig {
        splice_min: None,
        ..config(128 * 1024)
    });
    let data = data_file(256 * 1024);
    let (spliced, bytes) = send(&path, &data, 0, 128 * 1024);
    assert!(!spliced);
    assert_eq!(bytes, reply_of(&content(0, 128 * 1024)));
    assert_eq!(path.counts(), counts(0, 128 * 1024));
}

#[test]
fn an_empty_read_is_a_bare_header_even_with_a_threshold_of_zero() {
    let path = path(ReplyConfig {
        splice_min: Some(0),
        ..config(128 * 1024)
    });
    let data = data_file(4096);
    let (spliced, bytes) = send(&path, &data, 0, 0);
    assert!(!spliced);
    assert_eq!(bytes, reply_of(&[]));
    assert_eq!(path.counts(), counts(0, 0));
}

#[test]
fn a_device_that_takes_no_splice_gets_a_pread_reply_above_the_threshold() {
    let (path, data, tokens) = (
        path(config(128 * 1024)),
        data_file(64 * 1024),
        Tokens::new(),
    );
    let memfd = memfd_create("tsfuse-reply-device", MemfdFlags::CLOEXEC).unwrap();
    let dev = Arc::new(SendOnly(DevFd::from_fd(memfd)));
    let filled = path
        .fill(
            tokens.register(UNIQUE, dev.clone()),
            data.as_fd(),
            0,
            32 * 1024,
        )
        .unwrap();
    assert!(!filled.is_spliced());
    assert!(matches!(filled.commit(), Completion::Written));
    assert_eq!(written(&dev.0), reply_of(&content(0, 32 * 1024)));
    assert_eq!(path.counts(), counts(0, 32 * 1024));
}

#[test]
fn a_spliced_read_that_reaches_eof_first_is_built_again_by_pread_with_the_bytes_there() {
    let (path, data) = (path(config(128 * 1024)), data_file(100 * 1024));
    // 36 KiB are left after 64 KiB: the splice ends short, the pipe is closed, and the header
    // of the pread reply names the 36 KiB.
    let (spliced, bytes) = send(&path, &data, 64 * 1024, 128 * 1024);
    assert!(!spliced);
    assert_eq!(bytes, reply_of(&content(64 * 1024, 36 * 1024)));
    assert_eq!(path.counts(), counts(0, 36 * 1024));
    assert_eq!(
        idle_pipes(&path),
        0,
        "the pipe holding a partial reply was closed"
    );
}

#[test]
fn a_read_at_or_past_eof_is_an_empty_reply() {
    let (path, data) = (path(config(128 * 1024)), data_file(100 * 1024));
    for offset in [100 * 1024, 200 * 1024] {
        let (spliced, bytes) = send(&path, &data, offset, 64 * 1024);
        assert!(!spliced);
        assert_eq!(bytes, reply_of(&[]), "offset {offset}");
    }
    assert_eq!(path.counts(), counts(0, 0));
}

#[test]
fn a_read_at_an_unaligned_offset_is_spliced_whole() {
    let (path, data) = (path(config(128 * 1024)), data_file(512 * 1024));
    let offset = page() - 1;
    let (spliced, bytes) = send(&path, &data, offset as u64, 128 * 1024);
    assert!(
        spliced,
        "a reply of max_data at the worst offset fits the sized pipe"
    );
    assert_eq!(bytes, reply_of(&content(offset, 128 * 1024)));
    assert_eq!(path.counts(), counts(128 * 1024, 0));
}

#[test]
fn a_hole_is_spliced_as_zeros_and_the_header_has_the_full_length() {
    let (path, data) = (path(config(128 * 1024)), data_file(256 * 1024));
    let hole = FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE;
    fallocate(&data, hole, 64 * 1024, 64 * 1024).unwrap();
    let (spliced, bytes) = send(&path, &data, 32 * 1024, 128 * 1024);
    assert!(spliced);
    let mut want = content(32 * 1024, 128 * 1024);
    want[32 * 1024..96 * 1024].fill(0);
    assert_eq!(bytes, reply_of(&want));
}

#[test]
fn the_pipe_is_sized_before_use_for_the_largest_reply_at_the_worst_offset() {
    for max_data in [64 * 1024, 128 * 1024, 256 * 1024] {
        let path = path(config(max_data));
        // One buffer for the header, one more page than the data fills; the kernel rounds the
        // size up to a power of two pages.
        let pages = 2 + max_data.div_ceil(page());
        assert_eq!(
            path.pipe_size(),
            pages.next_power_of_two() * page(),
            "max_data {max_data}"
        );
    }
}

#[test]
fn a_refused_pipe_size_is_halved_while_it_is_above_the_default() {
    let mut asked = Vec::new();
    // As `fs.pipe-max-size` of 1 MiB refuses an unprivileged server.
    let got = grant(2 << 20, 64 << 10, &mut |n| {
        asked.push(n);
        match n <= 1 << 20 {
            true => Ok(n.next_power_of_two()),
            false => Err(Sys::PERM),
        }
    });
    assert_eq!((got, asked), (1 << 20, vec![2 << 20, 1 << 20]));

    let mut asked = Vec::new();
    let got = grant(256 << 10, 64 << 10, &mut |n| {
        asked.push(n);
        Err(Sys::PERM)
    });
    assert_eq!(
        (got, asked),
        (64 << 10, vec![256 << 10, 128 << 10]),
        "every size refused: the default"
    );

    let mut asked = Vec::new();
    let got = grant(64 << 10, 64 << 10, &mut |n| {
        asked.push(n);
        Ok(n)
    });
    assert_eq!(
        (got, asked),
        (64 << 10, vec![]),
        "a pipe of the default size is not resized"
    );
}

#[test]
fn a_reply_needs_a_pipe_buffer_for_the_header_and_one_per_page_it_touches() {
    let p = page();
    let cases = [
        (0, 0, 1),
        (5, 0, 1),
        (0, 1, 2),
        (0, p, 2),
        (0, p + 1, 3),
        (p as u64 - 1, 2, 3),
        (p as u64, p, 2),
        (3 * p as u64 + 1, 2 * p, 4),
    ];
    for (offset, len, slots) in cases {
        assert_eq!(
            slots_for(offset, len, p),
            slots,
            "offset {offset}, len {len}"
        );
    }
}

#[test]
fn a_reply_that_does_not_fit_the_pipe_it_got_goes_by_pread() {
    // A path sized for small replies keeps the default pipe of 16 buffers: a reply of 15 pages
    // at offset 0 fills it exactly, one byte more needs a 17th.
    let path = path(config(SPLICE_MIN));
    let slots = path.pipe_size() / page();
    let data = data_file(256 * 1024);
    let fits = (slots - 1) * page();
    let (spliced, bytes) = send(&path, &data, 0, fits);
    assert!(spliced);
    assert_eq!(bytes, reply_of(&content(0, fits)));
    let (spliced, bytes) = send(&path, &data, 0, fits + 1);
    assert!(!spliced);
    assert_eq!(bytes, reply_of(&content(0, fits + 1)));
    assert_eq!(path.counts(), counts(fits, fits + 1));
    assert_eq!(
        idle_pipes(&path),
        1,
        "the pipe too small for the reply went back unused"
    );
}

#[test]
fn a_fill_discarded_after_the_recheck_writes_nothing_and_gives_the_replier_back() {
    let (path, data) = (path(config(128 * 1024)), data_file(128 * 1024));
    for len in [64 * 1024, 4 * 1024] {
        let (tokens, dev) = (Tokens::new(), device());
        let filled = path
            .fill(tokens.register(UNIQUE, dev.clone()), data.as_fd(), 0, len)
            .unwrap();
        assert_eq!((filled.len(), filled.is_empty()), (len, false));
        let replier = filled.discard();
        assert!(written(&*dev).is_empty(), "nothing reached the device");
        assert_eq!(tokens.registered(), 1, "the request is still registered");
        assert!(matches!(replier.error(Errno::EIO), Completion::Written));
        assert_eq!(
            written(&*dev),
            encode_out_header(UNIQUE, Some(Errno::EIO), 0)
        );
    }
    assert_eq!(path.counts(), counts(0, 0));
    assert_eq!(
        idle_pipes(&path),
        0,
        "the pipe of the discarded reply was closed"
    );
    assert_eq!(
        idle_buffers(&path),
        1,
        "the buffer of the discarded reply went back"
    );
    // The next spliced reply gets a new pipe.
    let (spliced, bytes) = send(&path, &data, 0, 64 * 1024);
    assert!(spliced);
    assert_eq!(bytes, reply_of(&content(0, 64 * 1024)));
}

#[test]
fn a_commit_after_an_interrupt_answered_the_request_writes_nothing_more() {
    let (path, data) = (path(config(128 * 1024)), data_file(128 * 1024));
    for len in [64 * 1024, 4 * 1024] {
        let (tokens, dev) = (Tokens::new(), device());
        let filled = path
            .fill(tokens.register(UNIQUE, dev.clone()), data.as_fd(), 0, len)
            .unwrap();
        assert!(matches!(
            tokens.interrupt(UNIQUE, UNIQUE + 1, &*dev),
            crate::Interrupted::Cancelled
        ));
        assert!(matches!(filled.commit(), Completion::AlreadyAnswered));
        assert_eq!(
            written(&*dev),
            encode_out_header(UNIQUE, Some(Errno::EINTR), 0)
        );
    }
    assert_eq!(path.counts(), counts(0, 0));
}

#[test]
fn a_dropped_fill_answers_eio() {
    let (path, data) = (path(config(128 * 1024)), data_file(128 * 1024));
    let (tokens, dev) = (Tokens::new(), device());
    let filled = path
        .fill(
            tokens.register(UNIQUE, dev.clone()),
            data.as_fd(),
            0,
            64 * 1024,
        )
        .unwrap();
    drop(filled);
    assert_eq!(
        written(&*dev),
        encode_out_header(UNIQUE, Some(Errno::EIO), 0)
    );
    assert_eq!(tokens.registered(), 0);
}

#[test]
fn a_splice_the_device_refuses_with_einval_is_drained_and_written_again_by_writev() {
    // A file opened for appending refuses a splice with `EINVAL` before it takes anything
    // from the pipe, as `/dev/fuse` refuses a reply whose pipe holds less than its length.
    let (path, data, tokens, dev) = (
        path(config(128 * 1024)),
        data_file(128 * 1024),
        Tokens::new(),
        device(),
    );
    fcntl_setfl(&*dev, fcntl_getfl(&*dev).unwrap() | OFlags::APPEND).unwrap();
    let filled = path
        .fill(
            tokens.register(UNIQUE, dev.clone()),
            data.as_fd(),
            0,
            64 * 1024,
        )
        .unwrap();
    assert!(filled.is_spliced());
    assert!(matches!(filled.commit(), Completion::Written));
    assert_eq!(written(&*dev), reply_of(&content(0, 64 * 1024)));
    assert_eq!(path.counts(), counts(0, 64 * 1024));
}

#[test]
fn a_refused_reply_whose_pipe_cannot_be_drained_whole_is_answered_eio() {
    let path = path(config(128 * 1024));
    let (tokens, dev) = (Tokens::new(), device());
    let replier = tokens.register(UNIQUE, dev.clone());
    assert!(replier.slot.take());
    let empty = Pipe::new(0, page()).unwrap();
    let Completion::DeviceError(e) = path.rewrite(&replier, &empty, 64 * 1024) else {
        panic!("a reply that cannot be drained is an error");
    };
    assert_eq!(
        e.to_string(),
        "the device refused a spliced reply and its pipe could not be drained whole; the \
         request was answered EIO"
    );
    assert_eq!(
        written(&*dev),
        encode_out_header(UNIQUE, Some(Errno::EIO), 0)
    );

    // And when the EIO cannot be written either, that refusal is what is returned.
    let replier = tokens.register(UNIQUE + 1, Arc::new(Refuses(19)));
    assert!(replier.slot.take());
    let Completion::DeviceError(e) = path.rewrite(&replier, &empty, 64 * 1024) else {
        panic!("a refused EIO is an error");
    };
    assert_eq!(e.raw_os_error(), Some(19));
    assert_eq!(path.counts(), counts(0, 0));
}

#[test]
fn a_device_error_is_returned_and_counts_nothing() {
    let (path, data) = (path(config(128 * 1024)), data_file(128 * 1024));
    for len in [64 * 1024, 4 * 1024] {
        // A pipe whose reader is gone: `EPIPE` for a splice and for a `writev`.
        let (read, write) = rustix::pipe::pipe().unwrap();
        drop(read);
        let tokens = Tokens::new();
        let dev = Arc::new(DevFd::from_fd(write));
        let filled = path
            .fill(tokens.register(UNIQUE, dev), data.as_fd(), 0, len)
            .unwrap();
        let Completion::DeviceError(e) = filled.commit() else {
            panic!("a device whose reader is gone refuses the reply");
        };
        assert_eq!(
            e.raw_os_error(),
            Some(Sys::PIPE.raw_os_error()),
            "len {len}"
        );
    }
    assert_eq!(path.counts(), counts(0, 0));
}

#[test]
fn a_data_file_that_cannot_be_read_fails_the_fill_and_gives_the_replier_back() {
    let path = path(config(128 * 1024));
    // The write end of a pipe can be neither spliced from nor `pread`.
    let (_read, write) = rustix::pipe::pipe().unwrap();
    for len in [64 * 1024, 4 * 1024] {
        let (tokens, dev) = (Tokens::new(), device());
        let failed = path
            .fill(tokens.register(UNIQUE, dev.clone()), write.as_fd(), 0, len)
            .unwrap_err();
        assert_eq!(failed.error.raw_os_error(), Some(Sys::SPIPE.raw_os_error()));
        assert!(written(&*dev).is_empty());
        assert!(matches!(
            failed.replier.error(Errno::EIO),
            Completion::Written
        ));
    }
    assert_eq!(
        idle_buffers(&path),
        1,
        "the buffer of the failed read went back"
    );
}

#[test]
fn when_no_pipe_can_be_made_a_reply_goes_by_pread() {
    // A pool of 0 closes the first pipe; the stand-in maker fails as `pipe2` does at `EMFILE`.
    let mut path = path(ReplyConfig {
        pool: 0,
        ..config(128 * 1024)
    });
    assert_eq!(idle_pipes(&path), 0);
    path.make_pipe = |_, _| Err(io::Error::from_raw_os_error(Sys::MFILE.raw_os_error()));
    let data = data_file(128 * 1024);
    let (spliced, bytes) = send(&path, &data, 0, 64 * 1024);
    assert!(!spliced);
    assert_eq!(bytes, reply_of(&content(0, 64 * 1024)));
    assert_eq!(idle_buffers(&path), 0, "a pool of 0 keeps no buffer either");
}

#[test]
fn a_path_whose_first_pipe_cannot_be_made_is_an_error() {
    let err = ReplyPath::with_pipes(config(128 * 1024), |_, _| {
        Err(io::Error::from_raw_os_error(Sys::MFILE.raw_os_error()))
    })
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("making the first pipe of the splice reply path failed: "),
        "{err}"
    );
}

#[test]
fn the_pool_keeps_at_most_its_size_of_idle_pipes_and_buffers_and_reuses_them() {
    let path = path(ReplyConfig {
        pool: 1,
        ..config(128 * 1024)
    });
    let pooled = path.pipes.lock().unwrap()[0].read.as_raw_fd();
    let data = data_file(128 * 1024);
    let tokens = Tokens::new();
    let (d1, d2) = (device(), device());
    // Two replies at once, each way: two pipes and two buffers in use.
    let a = path
        .fill(tokens.register(1, d1.clone()), data.as_fd(), 0, 64 * 1024)
        .unwrap();
    let b = path
        .fill(tokens.register(2, d2.clone()), data.as_fd(), 0, 64 * 1024)
        .unwrap();
    let c = path
        .fill(tokens.register(3, d1), data.as_fd(), 0, 1024)
        .unwrap();
    let d = path
        .fill(tokens.register(4, d2), data.as_fd(), 0, 1024)
        .unwrap();
    for filled in [a, b, c, d] {
        assert!(matches!(filled.commit(), Completion::Written));
    }
    assert_eq!((idle_pipes(&path), idle_buffers(&path)), (1, 1));
    assert_eq!(
        path.pipes.lock().unwrap()[0].read.as_raw_fd(),
        pooled,
        "the first pipe, used first, went back first and the second was closed"
    );
}

#[test]
fn moving_pages_still_sends_the_whole_reply() {
    let path = path(ReplyConfig {
        move_pages: true,
        ..config(128 * 1024)
    });
    let data = data_file(128 * 1024);
    let (spliced, bytes) = send(&path, &data, 0, 64 * 1024);
    assert!(spliced);
    assert_eq!(bytes, reply_of(&content(0, 64 * 1024)));
    assert!(path.config().move_pages);
}

#[test]
fn a_session_config_sizes_for_its_max_pages_or_the_kernels_default_of_32() {
    let negotiated = |max_pages| Negotiated {
        minor: 45,
        offered: InitFlags(0),
        granted: InitFlags(0),
        missing: InitFlags(0),
        reply: InitOut {
            max_pages,
            ..InitOut::default()
        },
    };
    for (max_pages, pages) in [(256, 256), (0, 32), (1, 1)] {
        let config = ReplyConfig::for_session(&negotiated(max_pages), 8);
        assert_eq!(
            config,
            ReplyConfig {
                splice_min: Some(SPLICE_MIN),
                max_data: pages * page(),
                pool: 8,
                move_pages: false,
            }
        );
    }
}

#[test]
fn the_header_is_never_spliced_behind_a_pipe_that_refused_it() {
    // A pipe whose reader is gone refuses the header (`EPIPE`): no data goes in.
    let pipe = Pipe::new(0, page()).unwrap();
    let Pipe { read, write, slots } = pipe;
    drop(read);
    let (spare, _) = rustix::pipe::pipe().unwrap();
    let broken = Pipe {
        read: spare,
        write,
        slots,
    };
    let data = data_file(64 * 1024);
    assert!(!splice_in(&broken, UNIQUE, data.as_fd(), 0, 32 * 1024));
}

#[test]
fn dontneed_asks_for_whole_cells_and_counts_the_bytes_asked_for() {
    let cell = NonZeroU64::new(1 << 20).unwrap();
    let helper = Dontneed::new(cell);
    let file = data_file(4 << 20);
    assert_eq!(
        helper
            .cells(file.as_fd(), 1, NonZeroU64::new(2).unwrap())
            .unwrap(),
        2 << 20
    );
    assert_eq!(
        helper
            .cells(file.as_fd(), 3, NonZeroU64::new(1).unwrap())
            .unwrap(),
        1 << 20
    );
    assert_eq!(helper.asked_bytes(), 3 << 20);
}

#[test]
fn dontneed_refuses_cells_past_the_largest_file_offset_and_counts_nothing() {
    let cell = NonZeroU64::new(1 << 20).unwrap();
    let helper = Dontneed::new(cell);
    let file = data_file(4096);
    let last = i64::MAX as u64 >> 20;
    let cases = [
        // The first offset overflows.
        (u64::MAX, 1),
        // The length overflows.
        (0, u64::MAX),
        // Both fit, the end is past `i64::MAX`.
        (last, 2),
    ];
    for (first, cells) in cases {
        let err = helper
            .cells(file.as_fd(), first, NonZeroU64::new(cells).unwrap())
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "cells {first}..{first}+{cells} of 1048576 bytes lie past the largest file offset"
            )
        );
    }
    // The last whole cell below the limit is accepted.
    assert_eq!(
        helper
            .cells(file.as_fd(), last - 1, NonZeroU64::new(1).unwrap())
            .unwrap(),
        1 << 20
    );
    assert_eq!(helper.asked_bytes(), 1 << 20);
}

#[test]
fn dontneed_on_a_pipe_fails_and_counts_nothing() {
    let helper = Dontneed::new(NonZeroU64::new(1 << 20).unwrap());
    let (read, _write) = rustix::pipe::pipe().unwrap();
    let err = helper
        .cells(read.as_fd(), 0, NonZeroU64::new(1).unwrap())
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "posix_fadvise(DONTNEED) of cells 0..0+1 failed: {}",
            io::Error::from_raw_os_error(Sys::SPIPE.raw_os_error())
        )
    );
    assert_eq!(helper.asked_bytes(), 0);
}
