//! Kernel checks of the read path of a FUSE file (`docs/design/tsfuse-kernel-findings.md`, K15
//! and K17 to K21): the page cache across opens under `NO_OPEN_SUPPORT` and with or without
//! `FOPEN_KEEP_CACHE`, the size of a direct READ under `max_pages`, and a READ answered by
//! `splice(2)` from a pipe.
//!
//! The tree's server here answers every request; `FUSE_ASYNC_READ` is not asked for, so every
//! READ of a buffered read is answered before the caller's `read(2)` returns, and the counts a
//! check compares are complete when its caller's call has returned.
//!
//! An integration target: it mounts through `mount(2)` as root (the farm) or `fusermount3`
//! (CI), and says on stderr why it does not run where neither can.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod kernel;

use std::io::{Read as _, Write as _};
use std::os::unix::fs::FileExt as _;

use kernel::{
    Aligned, Conn, FILE_SIZE, FOPEN_DIRECT_IO, FOPEN_KEEP_CACHE, Open, Server, Setup, caller,
    content, dup, errno, errno_of, flag, is_read_of, op, pread, reply, serve_until, skip,
};
use rustix::fs::{MemfdFlags, memfd_create};
use rustix::pipe::{SpliceFlags, fcntl_setpipe_size, pipe, splice};
use tsfuse::abi::OutHeader;

/// Reads the whole of `name` through the mount and checks its bytes.
fn read_whole(conn: &Conn, name: &str, ino: u64) {
    let mut bytes = Vec::new();
    std::fs::File::open(conn.path(name))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, content(ino, 0, FILE_SIZE));
}

/// K18. Under `NO_OPEN_SUPPORT` the first OPEN, answered `ENOSYS`, is the last: the second open
/// sends no OPEN. The pages read through that first handle stay cached: reading the file again
/// through the second open sends no READ. No RELEASE is sent for either handle. Every READ of
/// these buffered reads is at most the readahead window the kernel offered at INIT.
#[test]
fn no_open_support_stops_open_after_one_enosys_and_keeps_the_page_cache_for_every_later_open() {
    let setup = Setup {
        want: Setup::default().want | flag::NO_OPEN_SUPPORT,
        ..Setup::default()
    };
    let Some(conn) = Conn::mount("k18-no-open", &setup) else {
        return;
    };
    if conn.granted & flag::NO_OPEN_SUPPORT == 0 {
        skip("the kernel does not offer FUSE_NO_OPEN_SUPPORT");
        return;
    }
    let server = Server::spawn(dup(&conn.dev), Open::Enosys);

    read_whole(&conn, "f", 2);
    let first = server.seen();
    assert_eq!(first.count(op::OPEN), 1);
    assert!(first.count(op::READ) > 0);
    // K20 and K21: every READ of a buffered read is at most the readahead window the kernel
    // offered at INIT (128 KiB by default), whatever `max_pages` allows.
    let largest = first.reads.iter().map(|r| r.2).max().unwrap();
    assert!(
        largest <= conn.offered_readahead,
        "{largest} > {}",
        conn.offered_readahead
    );
    let _ = writeln!(
        std::io::stderr(),
        "measured: largest buffered READ {largest} bytes"
    );

    read_whole(&conn, "f", 2);
    let second = server.seen();
    assert_eq!(second.count(op::OPEN), 1, "no OPEN after the ENOSYS");
    assert_eq!(
        second.count(op::READ),
        first.count(op::READ),
        "served from the cache"
    );
    assert_eq!(second.count(op::RELEASE), 0);
    assert!(second.refused.is_empty(), "{:?}", second.refused);
}

/// K19. An OPEN answered with `FOPEN_KEEP_CACHE` keeps the pages of the earlier open: the
/// second whole read sends no READ. Answered with no flag, every open drops the file's pages
/// and the second whole read sends READs again.
#[test]
fn keep_cache_keeps_the_page_cache_across_opens_and_an_open_without_it_drops_it() {
    for (open_flags, cached) in [(FOPEN_KEEP_CACHE, true), (0, false)] {
        let name = format!("k19-keep-cache-{open_flags}");
        let Some(conn) = Conn::mount(&name, &Setup::default()) else {
            return;
        };
        let server = Server::spawn(dup(&conn.dev), Open::Flags(open_flags));
        read_whole(&conn, "f", 2);
        let first = server.seen().count(op::READ);
        read_whole(&conn, "f", 2);
        let second = server.seen();
        assert_eq!(second.count(op::OPEN), 2);
        match cached {
            true => assert_eq!(second.count(op::READ), first, "served from the cache"),
            false => assert!(second.count(op::READ) > first, "read again"),
        }
    }
}

/// The `fs.fuse.max_pages_limit` sysctl: the cap the kernel puts on `max_pages`.
fn max_pages_limit() -> u64 {
    std::fs::read_to_string("/proc/sys/fs/fuse/max_pages_limit")
        .map_or(256, |s| s.trim().parse().unwrap())
}

/// K21. A 4 MiB direct read (`FOPEN_DIRECT_IO`) into a page-aligned buffer is sent as READs of
/// `max_pages` pages each, in order: 256 pages make four 1 MiB READs, 64 pages sixteen of
/// 256 KiB, and an INIT asking for 1024 pages gets the `max_pages_limit` sysctl's cap (256 by
/// default, 1 MiB).
#[test]
fn a_direct_read_is_split_into_reads_of_max_pages_capped_by_the_sysctl() {
    for asked in [256u16, 64, 1024] {
        let setup = Setup {
            max_pages: asked,
            ..Setup::default()
        };
        let Some(conn) = Conn::mount(&format!("k21-max-pages-{asked}"), &setup) else {
            return;
        };
        assert_ne!(conn.granted & flag::MAX_PAGES, 0, "MAX_PAGES is offered");
        let server = Server::spawn(dup(&conn.dev), Open::Flags(FOPEN_DIRECT_IO));
        let mut buf = Aligned::new(FILE_SIZE as usize);
        let file = std::fs::File::open(conn.path("f")).unwrap();
        let n = file.read_at(buf.as_mut(), 0).unwrap();
        assert_eq!(n as u64, FILE_SIZE);
        assert_eq!(buf.as_mut().to_vec(), content(2, 0, FILE_SIZE));

        let piece = u64::from(asked).min(max_pages_limit()) * 4096;
        let want: Vec<(u64, u64, u32)> = (0..FILE_SIZE / piece)
            .map(|i| (2, i * piece, u32::try_from(piece).unwrap()))
            .collect();
        assert_eq!(server.seen().reads, want, "asked for {asked} pages");
    }
}

/// A memfd holding `bytes`, the data side of a spliced reply.
fn memfd_with(bytes: &[u8]) -> std::os::fd::OwnedFd {
    let fd = memfd_create("tsfuse-splice", MemfdFlags::CLOEXEC).unwrap();
    assert_eq!(rustix::io::write(&fd, bytes).unwrap(), bytes.len());
    fd
}

/// Splices up to `len` bytes of `from` at offset 0 into the pipe `to`; the bytes moved.
fn splice_in(from: &std::os::fd::OwnedFd, to: &std::os::fd::OwnedFd, len: usize) -> usize {
    let mut off = 0u64;
    let mut moved = 0;
    while moved < len {
        let n = splice(
            from,
            Some(&mut off),
            to,
            None,
            len - moved,
            SpliceFlags::NONBLOCK,
        );
        match n {
            Ok(0) => break,
            Ok(n) => moved += n,
            // The pipe is full.
            Err(e) if e == rustix::io::Errno::AGAIN => break,
            Err(e) => panic!("splice into the pipe: {e}"),
        }
    }
    moved
}

/// The header of a READ reply of `data` bytes to `unique`.
fn read_header(unique: u64, data: usize) -> Vec<u8> {
    OutHeader {
        len: u32::try_from(OutHeader::SIZE + data).unwrap(),
        error: 0,
        unique,
    }
    .as_bytes()
    .to_vec()
}

/// K15. A 256 KiB READ answered by `splice(2)`: the header written into a pipe sized with
/// `F_SETPIPE_SZ` to hold the whole reply, the data spliced into it from a memfd, and the pipe
/// spliced into the device in one call. The kernel takes it whether or not `FUSE_SPLICE_WRITE`
/// was answered at INIT, and the caller gets the bytes.
#[test]
fn a_read_answered_by_splice_from_a_pipe_reaches_the_caller() {
    for want in [
        Setup::default().want | flag::SPLICE_WRITE,
        Setup::default().want,
    ] {
        let setup = Setup {
            want,
            ..Setup::default()
        };
        let Some(conn) = Conn::mount(&format!("k15-splice-{want:x}"), &setup) else {
            return;
        };
        let open = Open::Flags(FOPEN_DIRECT_IO);
        let len = 256 * 1024;
        let path = conn.path("f");
        let f = caller(move || pread(&path, 0, len));
        let rf = serve_until(&conn.dev, open, |r| is_read_of(r, "f"));
        let size = rf.read_in().size as usize;
        assert_eq!(size, len, "a direct READ of the whole pread");

        let (r, w) = pipe().unwrap();
        assert!(fcntl_setpipe_size(&w, OutHeader::SIZE + size).unwrap() >= OutHeader::SIZE + size);
        rustix::io::write(&w, &read_header(rf.h.unique, size)).unwrap();
        let data = memfd_with(&content(2, 0, size as u64));
        assert_eq!(splice_in(&data, &w, size), size);
        let total = OutHeader::SIZE + size;
        assert_eq!(
            splice(&r, None, &conn.dev, None, total, SpliceFlags::empty()).unwrap(),
            total
        );

        let _server = Server::spawn(dup(&conn.dev), open);
        assert_eq!(f.join().unwrap().unwrap(), content(2, 0, size as u64));
    }
}

/// K15 and K17. In a pipe of the default size the 256 KiB data splice moves a short count, not
/// an error (the pipe is full); a splice into the device asking for the whole reply, more than
/// the pipe holds, is refused with `EINVAL` before the kernel looks at the header, and the
/// request stays answerable: the caller gets the correct reply that follows.
#[test]
fn a_reply_spliced_from_a_pipe_too_small_for_it_is_refused_einval_and_the_request_stays_answerable()
{
    let Some(conn) = Conn::mount("k17-small-pipe", &Setup::default()) else {
        return;
    };
    let open = Open::Flags(FOPEN_DIRECT_IO);
    let path = conn.path("f");
    let f = caller(move || pread(&path, 0, 256 * 1024));
    let rf = serve_until(&conn.dev, open, |r| is_read_of(r, "f"));
    let size = rf.read_in().size as usize;
    assert!(size > 64 * 1024, "a READ larger than the default pipe");

    let (r, w) = pipe().unwrap();
    rustix::io::write(&w, &read_header(rf.h.unique, size)).unwrap();
    let data = memfd_with(&content(2, 0, size as u64));
    let moved = splice_in(&data, &w, size);
    assert!(
        0 < moved && moved < size,
        "a short count: {moved} of {size}"
    );

    let total = OutHeader::SIZE + size;
    let refused = splice(&r, None, &conn.dev, None, total, SpliceFlags::empty()).unwrap_err();
    assert_eq!(
        errno_of(&std::io::Error::from(refused)),
        Some(errno::EINVAL)
    );
    assert!(
        !f.is_finished(),
        "a refused splice does not end the request"
    );

    reply(&conn.dev, rf.h.unique, 0, &[&content(2, 0, size as u64)]).unwrap();
    let _server = Server::spawn(dup(&conn.dev), open);
    assert_eq!(f.join().unwrap().unwrap(), content(2, 0, size as u64));
}
