//! READs of a real mount answered through `tsfuse::ReplyPath` (row T-F4; conformance item 3 of
//! `docs/design/fetch-cache-serve.md` 5.2: "the `out.len` of a spliced reply equals the bytes
//! actually spliced"): the kernel takes the spliced reply of a whole READ and of one at an
//! unaligned offset, the reply rebuilt by `pread` when the data file ends before the READ does,
//! and the `writev` reply below the threshold, and each caller gets exactly the file's bytes.
//!
//! The data file is a memfd holding the bytes of the harness's file `f`. Every READ is a direct
//! one (`FOPEN_DIRECT_IO`), so the size of each READ is the caller's `pread`, which returns only
//! once its reply is in.
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

use std::os::fd::{AsFd as _, OwnedFd};
use std::sync::Arc;

use kernel::{
    Conn, FILE_SIZE, FOPEN_DIRECT_IO, Open, Server, Setup, caller, content, dup, ino_of,
    is_read_of, pread, serve_until,
};
use rustix::fs::{MemfdFlags, memfd_create};
use tsfuse::{Completion, ReplyConfig, ReplyCounts, ReplyPath, SPLICE_MIN, Tokens};

/// A memfd holding the first `len` bytes of the file `f`.
fn data_of_f(len: u64) -> OwnedFd {
    let fd = memfd_create("tsfuse-reply-data", MemfdFlags::CLOEXEC).unwrap();
    let bytes = content(ino_of("f").unwrap(), 0, len);
    assert_eq!(rustix::io::write(&fd, &bytes).unwrap(), bytes.len());
    fd
}

/// What one READ of `f` came to.
struct Answered {
    /// Whether its reply went by splice.
    spliced: bool,
    /// The bytes the caller's `pread` got.
    got: Vec<u8>,
    /// The bytes the reply path counted.
    counts: ReplyCounts,
}

/// Mounts the tree, lets a caller `pread` `len` bytes of `f` at `offset`, and answers its READ
/// through a reply path from `data`; `None` where no mount can be made.
fn answer_read(name: &str, data: &OwnedFd, offset: u64, len: usize) -> Option<Answered> {
    let conn = Conn::mount(name, &Setup::default())?;
    let path = ReplyPath::new(ReplyConfig {
        splice_min: Some(SPLICE_MIN),
        max_data: 256 * 1024,
        pool: 1,
        move_pages: false,
    })
    .unwrap();
    let open = Open::Flags(FOPEN_DIRECT_IO);
    let file = conn.path("f");
    let f = caller(move || pread(&file, offset, len));
    let read = serve_until(&conn.dev, open, |r| is_read_of(r, "f"));
    let asked = read.read_in();
    assert_eq!(
        (asked.offset, asked.size as usize),
        (offset, len),
        "one direct READ of the whole pread"
    );

    let tokens = Tokens::new();
    let device = Arc::new(dup(&conn.dev));
    let replier = tokens.register(read.h.unique, device);
    let filled = path
        .fill(replier, data.as_fd(), asked.offset, asked.size as usize)
        .unwrap();
    let spliced = filled.is_spliced();
    let committed = filled.commit();
    assert!(
        matches!(committed, Completion::Written),
        "the kernel took the reply: {committed:?}"
    );

    let _server = Server::spawn(dup(&conn.dev), open);
    let got = f.join().unwrap().unwrap();
    Some(Answered {
        spliced,
        got,
        counts: path.counts(),
    })
}

fn counts(splice_bytes: usize, pread_bytes: usize) -> ReplyCounts {
    ReplyCounts {
        splice_bytes: splice_bytes as u64,
        pread_bytes: pread_bytes as u64,
    }
}

#[test]
fn a_read_answered_by_splice_through_the_sized_pipe_reaches_the_caller() {
    let data = data_of_f(FILE_SIZE);
    let len = 256 * 1024;
    let Some(read) = answer_read("t-f4-splice", &data, 0, len) else {
        return;
    };
    assert!(read.spliced);
    assert_eq!(read.got, content(ino_of("f").unwrap(), 0, len as u64));
    assert_eq!(read.counts, counts(len, 0));
}

#[test]
fn a_read_at_an_unaligned_offset_answered_by_splice_reaches_the_caller() {
    let data = data_of_f(FILE_SIZE);
    let (offset, len) = (4095, 128 * 1024);
    let Some(read) = answer_read("t-f4-unaligned", &data, offset, len) else {
        return;
    };
    assert!(read.spliced);
    assert_eq!(read.got, content(ino_of("f").unwrap(), offset, len as u64));
    assert_eq!(read.counts, counts(len, 0));
}

#[test]
fn a_read_past_the_end_of_the_data_file_is_answered_short_with_the_bytes_there() {
    // The data file holds 100 KiB: the splice ends short, and the reply rebuilt by `pread`
    // carries the 100 KiB, which the kernel takes and the caller gets.
    let have = 100 * 1024;
    let data = data_of_f(have);
    let Some(read) = answer_read("t-f4-short", &data, 0, 256 * 1024) else {
        return;
    };
    assert!(!read.spliced);
    assert_eq!(read.got, content(ino_of("f").unwrap(), 0, have));
    assert_eq!(read.counts, counts(0, have as usize));
}

#[test]
fn a_read_below_the_threshold_is_answered_by_writev() {
    let data = data_of_f(FILE_SIZE);
    let len = SPLICE_MIN - 1;
    let Some(read) = answer_read("t-f4-small", &data, 8192, len) else {
        return;
    };
    assert!(!read.spliced);
    assert_eq!(read.got, content(ino_of("f").unwrap(), 8192, len as u64));
    assert_eq!(read.counts, counts(0, len));
}
