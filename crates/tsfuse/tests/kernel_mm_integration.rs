//! Kernel checks of the memory the cache design leans on (`docs/design/tsfuse-kernel-findings.md`,
//! K24 to K26): the seals of the state memfd the server passes to a mount, and
//! `posix_fadvise(POSIX_FADV_DONTNEED)` on a cache file's synced cells.
//!
//! Whether a range is in the page cache is read from the disk reads it costs: `read_bytes` of
//! `/proc/thread-self/io` counts the bytes the block layer reads on behalf of the calling
//! thread, so a `pread` served from the page cache adds nothing and one that misses adds at least
//! the bytes it fetched. (`preadv2(RWF_NOWAIT)` is no probe: it starts readahead of the page it
//! misses. `mincore` and `cachestat` would need `unsafe`, AGENTS.md R2.)
//!
//! The mapping half of K24 (a `PROT_WRITE` shared mapping of a sealed memfd is refused) needs
//! `mmap`, which is `unsafe` in Rust and so not called here (AGENTS.md R2); it runs on the store's
//! own state memfd in `tierstore-entry`
//! (`a_writable_shared_mapping_of_the_passed_state_memfd_fails_with_eperm`, through `vm-memory`),
//! item 11 of the conformance suite (`docs/design/tsfuse-conformance.md`).
//!
//! An integration target: it reads `/proc/self/fd`, `/proc/thread-self/io` and the page cache of
//! a file in the target directory.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::fs::File;
use std::io::{self, Write as _};
use std::num::NonZeroU64;
use std::os::fd::{AsFd, AsRawFd as _};
use std::os::unix::fs::FileExt as _;

use rustix::fs::{
    Advice, FallocateFlags, MemfdFlags, Mode, OFlags, SealFlags, fadvise, fallocate,
    fcntl_add_seals, fcntl_get_seals, ftruncate, memfd_create, open,
};
use rustix::io::{Errno, pread, pwrite, write};
use tierstore_testkit::dir::TestDir;

/// Says on stderr why a check does not run.
fn skip(why: &str) {
    let _ = writeln!(io::stderr(), "skipped: {why}");
}

/// The seals the server applies to the state memfd before it passes it (design 5.1).
fn state_seals() -> SealFlags {
    SealFlags::FUTURE_WRITE | SealFlags::SHRINK | SealFlags::GROW
}

/// A memfd of two pages of `0xa5` made as the server makes the state memfd (sealable, and
/// `MFD_NOEXEC_SEAL`, which the kernel adds by itself where `vm.memfd_noexec` is 1 and requires
/// where it is 2), with [`state_seals`] applied.
fn sealed_state() -> rustix::fd::OwnedFd {
    let fd = memfd_create(
        "tsfuse-state",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING | MemfdFlags::NOEXEC_SEAL,
    )
    .unwrap();
    assert_eq!(write(&fd, &[0xa5; 8192]).unwrap(), 8192);
    fcntl_add_seals(&fd, state_seals()).unwrap();
    fd
}

/// K24 and K25. After `F_SEAL_FUTURE_WRITE | F_SEAL_SHRINK | F_SEAL_GROW`: `write`, `pwrite`
/// in place, a hole punch, shrinking and growing (`ftruncate`, `fallocate` past the end) are
/// all refused with `EPERM`; `ftruncate` to the same size and `fallocate` inside the size, which
/// change no byte, are allowed; the bytes still read back; the same holds through a descriptor
/// opened again read-write from `/proc/self/fd` (the seals are the inode's). Through one opened
/// read-only a seal cannot be added (`EPERM`). After `F_SEAL_SEAL` no seal can be added (`EPERM`).
#[test]
fn a_future_write_sealed_memfd_refuses_every_write_and_resize_and_keeps_its_bytes() {
    let fd = sealed_state();
    let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
    let reopened = open(path.as_str(), OFlags::RDWR | OFlags::CLOEXEC, Mode::empty()).unwrap();
    let read_only = open(
        path.as_str(),
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    // MFD_NOEXEC_SEAL sets F_SEAL_EXEC at creation.
    let seals = state_seals() | SealFlags::EXEC;

    for target in [fd.as_fd(), reopened.as_fd()] {
        assert_eq!(write(target, b"x"), Err(Errno::PERM), "write");
        assert_eq!(pwrite(target, b"x", 0), Err(Errno::PERM), "pwrite in place");
        let punch = FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE;
        assert_eq!(fallocate(target, punch, 0, 4096), Err(Errno::PERM), "punch");
        assert_eq!(ftruncate(target, 4096), Err(Errno::PERM), "shrink");
        assert_eq!(ftruncate(target, 16384), Err(Errno::PERM), "grow");
        let grow = FallocateFlags::empty();
        assert_eq!(
            fallocate(target, grow, 8192, 4096),
            Err(Errno::PERM),
            "fallocate past the end"
        );
        assert_eq!(ftruncate(target, 8192), Ok(()), "the same size");
        assert_eq!(fallocate(target, grow, 0, 8192), Ok(()), "inside the size");
        let mut back = [0u8; 8192];
        assert_eq!(pread(target, &mut back, 0).unwrap(), 8192);
        assert_eq!(back, [0xa5; 8192]);
        assert_eq!(fcntl_get_seals(target).unwrap(), seals);
    }
    assert_eq!(
        fcntl_add_seals(&read_only, SealFlags::SEAL),
        Err(Errno::PERM)
    );

    fcntl_add_seals(&fd, SealFlags::SEAL).unwrap();
    assert_eq!(fcntl_add_seals(&fd, SealFlags::WRITE), Err(Errno::PERM));
    assert_eq!(fcntl_get_seals(&fd).unwrap(), seals | SealFlags::SEAL);
}

/// K25. A memfd made without `MFD_ALLOW_SEALING` (and with `MFD_EXEC`, so that the kernel does
/// not add `MFD_NOEXEC_SEAL`, which also allows sealing) starts with `F_SEAL_SEAL`: no seal can
/// be added (`EPERM`). Where `vm.memfd_noexec` is 2 such a memfd cannot be made at all.
#[test]
fn a_memfd_made_without_allow_sealing_takes_no_seal() {
    let made = memfd_create("tsfuse-unsealable", MemfdFlags::CLOEXEC | MemfdFlags::EXEC);
    let fd = match made {
        Ok(fd) => fd,
        Err(Errno::ACCESS) => {
            skip("vm.memfd_noexec is 2: an executable memfd cannot be made");
            return;
        }
        Err(e) => panic!("memfd_create: {e}"),
    };
    assert_eq!(fcntl_get_seals(&fd).unwrap(), SealFlags::SEAL);
    assert_eq!(fcntl_add_seals(&fd, state_seals()), Err(Errno::PERM));
}

/// The cell size of the cache (design 2).
const CELL: u64 = 1 << 20;
/// Cells in the cache file of the checks.
const CELLS: u64 = 4;

/// The bytes the block layer has read for this thread (`read_bytes` of `/proc/thread-self/io`);
/// `None` without task I/O accounting.
fn read_bytes() -> Option<u64> {
    let io = std::fs::read_to_string("/proc/thread-self/io").ok()?;
    io.lines()
        .find_map(|l| l.strip_prefix("read_bytes: "))
        .map(|v| v.trim().parse().unwrap())
}

/// The content of cell `i` (a different byte per cell and position).
fn cell(i: u64) -> Vec<u8> {
    (0..CELL).map(|o| ((o % 251) as u8) ^ (i as u8)).collect()
}

/// Reads cells `first..first + count` of `file`, checks their bytes, and returns the bytes the
/// block layer read to serve them: 0 when every page was in the page cache.
fn disk_bytes_to_read(file: &File, first: u64, count: u64) -> u64 {
    let before = read_bytes().unwrap();
    for i in first..first + count {
        let mut buf = vec![0u8; CELL as usize];
        file.read_exact_at(&mut buf, i * CELL).unwrap();
        assert_eq!(buf, cell(i), "cell {i}");
    }
    read_bytes().unwrap() - before
}

/// `TMPFS_MAGIC`: on tmpfs the page cache is the file's only copy, DONTNEED drops nothing and no
/// read reaches a disk.
const TMPFS_MAGIC: i64 = 0x0102_1994;

/// A cache file of [`CELLS`] cells, each written by its own aligned 1 MiB `pwrite` (as the
/// engine writes cells), in a scratch directory; `None` (said on stderr) where the checks cannot
/// observe the page cache.
fn cache_file(name: &str) -> Option<(TestDir, File)> {
    if read_bytes().is_none() {
        skip("no /proc/thread-self/io (task I/O accounting is off)");
        return None;
    }
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dir.path().join("cells"))
        .unwrap();
    if rustix::fs::fstatfs(&file).unwrap().f_type == TMPFS_MAGIC {
        skip("the target directory is on tmpfs");
        return None;
    }
    for i in 0..CELLS {
        file.write_all_at(&cell(i), i * CELL).unwrap();
    }
    Some((dir, file))
}

/// K26. Cells written and `fdatasync`ed (the design's `durable` cells) are in the page cache:
/// reading them back reads no disk byte. `DONTNEED` on cell 1 alone drops cell 1 and no other:
/// cells 0, 2 and 3 still read no disk byte, cell 1 reads at least a cell from disk. `DONTNEED`
/// on the whole file then drops every cell.
#[test]
fn dontneed_drops_the_synced_cells_of_its_range_and_no_other() {
    let Some((_dir, file)) = cache_file("k26-dontneed") else {
        return;
    };
    file.sync_data().unwrap();
    assert_eq!(disk_bytes_to_read(&file, 0, CELLS), 0, "all cached");

    fadvise(&file, CELL, NonZeroU64::new(CELL), Advice::DontNeed).unwrap();
    for i in [0, 2, 3] {
        assert_eq!(disk_bytes_to_read(&file, i, 1), 0, "cell {i} is kept");
    }
    assert!(
        disk_bytes_to_read(&file, 1, 1) >= CELL,
        "cell 1 was dropped"
    );

    fadvise(&file, 0, None, Advice::DontNeed).unwrap();
    assert!(
        disk_bytes_to_read(&file, 0, CELLS) >= CELLS * CELL,
        "every cell was dropped"
    );
}

/// K26. `DONTNEED` on cells just written (dirty, not synced) succeeds and loses nothing: after a
/// later `fdatasync` every byte reads back. How many of them it dropped (it starts their
/// writeback and keeps the dirty and under-writeback pages) is a race with the writeback it
/// started, so it is measured and printed, not asserted.
#[test]
fn dontneed_on_dirty_cells_succeeds_and_loses_nothing() {
    let Some((_dir, file)) = cache_file("k26-dirty") else {
        return;
    };
    fadvise(&file, 0, None, Advice::DontNeed).unwrap();
    file.sync_data().unwrap();
    let dropped = disk_bytes_to_read(&file, 0, CELLS);
    let _ = writeln!(
        io::stderr(),
        "measured: DONTNEED right after the writes dropped {dropped} of {} bytes",
        CELLS * CELL
    );
}
