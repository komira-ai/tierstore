//! The kernel's io_uring, as the FUSE transport uses it (5.2 item 4): a ring made with
//! `IORING_SETUP_SQE128` (FUSE's commands carry `struct fuse_uring_cmd_req` in the 80-byte command
//! area of a 128-byte submission entry), its three mappings, and `io_uring_enter`.
//!
//! This is the one module of the transport with `unsafe` code (AGENTS.md R2): `io_uring_setup`,
//! `mmap` and `munmap` of the rings, the views of the mapped rings as atomic words, and
//! `io_uring_enter`. Each site is the smallest function around the call, `expect`s the lint with
//! its reason and argues its soundness in a `SAFETY` comment. Everything the kernel and this
//! process both touch (the rings' heads and tails, the submission entries, the completions and
//! the entries' buffers) is accessed as atomic words, so nothing here hands out a reference the
//! kernel could write under.
//!
//! **The entries' buffers outlive the kernel's use of them.** A REGISTER gives the kernel the
//! addresses of an entry's two buffers, which it keeps until the ring is torn down. The kernel
//! writes into them only from the ring's own task (the task work of a request's dispatch, or a
//! COMMIT_AND_FETCH in `io_uring_enter`), and once the ring is closed that task work is cancelled
//! instead of run (`fuse_uring_send_in_task` with `tw.cancel`, `fs/fuse/dev_uring.c`). A
//! [`KernelRing`] holds a reference to every buffer it registered and drops it last, after the
//! rings are unmapped and the ring's descriptor is closed, so a buffer is never freed while the
//! kernel can write to it.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rustix::io_uring::{
    IORING_OFF_CQ_RING, IORING_OFF_SQ_RING, IORING_OFF_SQES, IoringEnterFlags, IoringOp,
    IoringSetupFlags, io_uring_params,
};
use rustix::mm::{MapFlags, ProtFlags};

use super::ring::{CMD_COMMIT_AND_FETCH, CMD_REGISTER, Cqe, EntryBufs, Ring};
use crate::dev::DevFd;

/// A submission entry of a ring made with `IORING_SETUP_SQE128`: 128 bytes, as 16 words.
pub(crate) type Sqe = [u64; 16];

/// The FUSE command `cmd_op` on the descriptor `fd`, with `struct fuse_uring_cmd_req`
/// (`commit_id`, `qid`) in the command area and `addr`/`len` (the REGISTER's iovec array).
pub(crate) fn fuse_cmd(
    fd: RawFd,
    cmd_op: u32,
    addr: u64,
    len: u32,
    user_data: u64,
    commit_id: u64,
    qid: u16,
) -> Sqe {
    let mut sqe = [0u64; 16];
    // Byte 0 `opcode`, byte 1 `flags`, bytes 2-3 `ioprio`, bytes 4-7 `fd`.
    sqe[0] = u64::from(IoringOp::UringCmd as u8) | u64::from(fd as u32) << 32;
    // Bytes 8-11 `cmd_op` (the union with `off`), 12-15 its padding.
    sqe[1] = u64::from(cmd_op);
    sqe[2] = addr;
    // Bytes 24-27 `len`, 28-31 `uring_cmd_flags`.
    sqe[3] = u64::from(len);
    sqe[4] = user_data;
    // From byte 48 the command area: `flags` (u64), `commit_id` (u64), `qid` (u16).
    sqe[7] = commit_id;
    sqe[8] = u64::from(qid);
    sqe
}

/// A `IORING_OP_NOP` that completes with `user_data`.
#[cfg(test)]
pub(crate) fn nop(user_data: u64) -> Sqe {
    let mut sqe = [0u64; 16];
    sqe[0] = u64::from(IoringOp::Nop as u8);
    sqe[4] = user_data;
    sqe
}

/// One mapping of a ring's memory. The address is kept as an integer (its provenance exposed),
/// so the type holds no raw pointer.
struct Mmap {
    addr: usize,
    len: usize,
}

impl Mmap {
    #[expect(
        unsafe_code,
        reason = "mmap of an io_uring region has no safe wrapper; the call is checked below"
    )]
    fn map(fd: BorrowedFd<'_>, len: usize, offset: u64) -> io::Result<Self> {
        // SAFETY: a new shared mapping at an address the kernel picks (null hint, no
        // `MAP_FIXED`), so no existing memory of this process is replaced. `fd` is the ring's
        // descriptor and `offset` one of the three regions io_uring maps (`IORING_OFF_*`), of
        // the length the kernel's offsets give; the kernel refuses anything else.
        let ptr = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED | MapFlags::POPULATE,
                fd,
                offset,
            )?
        };
        Ok(Self {
            addr: ptr.expose_provenance(),
            len,
        })
    }

    /// The mapping as `u32` words (every field of the rings io_uring shares is one, or two).
    #[expect(
        unsafe_code,
        reason = "a view of memory the kernel shares; the conditions are checked below"
    )]
    fn words32(&self) -> &[AtomicU32] {
        // SAFETY: `addr` is a live mapping of `len` bytes (unmapped only in `drop`, which the
        // borrow of `self` excludes), page-aligned, so aligned for `AtomicU32`. The kernel
        // changes this memory at any time; atomics are the types whose shared references allow
        // that, and every access through them is an atomic load or store.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::with_exposed_provenance::<AtomicU32>(self.addr),
                self.len / 4,
            )
        }
    }

    /// The mapping as `u64` words (the submission entries).
    #[expect(
        unsafe_code,
        reason = "a view of memory the kernel shares; the conditions are checked below"
    )]
    fn words64(&self) -> &[AtomicU64] {
        // SAFETY: as `words32`; a page-aligned mapping is aligned for `AtomicU64`.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::with_exposed_provenance::<AtomicU64>(self.addr),
                self.len / 8,
            )
        }
    }
}

impl Drop for Mmap {
    #[expect(
        unsafe_code,
        reason = "munmap has no safe wrapper; the call is checked below"
    )]
    fn drop(&mut self) {
        // SAFETY: the mapping `map` made, unmapped once; no view of it outlives `self` (each
        // borrows `self`).
        let _ = unsafe {
            rustix::mm::munmap(std::ptr::with_exposed_provenance_mut(self.addr), self.len)
        };
    }
}

/// The offsets (in `u32` words) of the fields of the submission ring.
#[derive(Debug, Clone, Copy)]
struct SqOff {
    head: usize,
    tail: usize,
    mask: usize,
    array: usize,
    entries: u32,
}

/// The offsets (in `u32` words) of the fields of the completion ring.
#[derive(Debug, Clone, Copy)]
struct CqOff {
    head: usize,
    tail: usize,
    mask: usize,
    cqes: usize,
}

/// A ring of the kernel, for the FUSE commands of one queue on one `/dev/fuse` descriptor.
///
/// The fields drop in order: the mappings and the ring's descriptor first, the buffers the
/// kernel was given last (see the module documentation).
pub(crate) struct KernelRing {
    sq: Mmap,
    cq: Mmap,
    sqes: Mmap,
    fd: OwnedFd,
    /// The connection's descriptor the commands name, shared by every ring of the session.
    dev: Arc<DevFd>,
    sq_off: SqOff,
    cq_off: CqOff,
    /// The submission tail as this process has advanced it.
    tail: u32,
    /// Entries queued and not yet submitted.
    queued: u32,
    /// Every entry registered: its buffers, and the iovec array its REGISTER points at.
    bufs: Vec<Arc<EntryBufs>>,
}

/// `io_uring_setup`.
#[expect(
    unsafe_code,
    reason = "io_uring_setup has no safe wrapper; the call is checked below"
)]
fn setup(entries: u32, params: &mut io_uring_params) -> io::Result<OwnedFd> {
    // SAFETY: `params` asks only for `IORING_SETUP_SQE128` (and `CQSIZE`): no SQ polling thread,
    // no attached work queue, no user-provided ring memory, so the kernel reads no address from
    // it; it writes the ring's offsets back into `params`, which is borrowed for the call.
    Ok(unsafe { rustix::io_uring::io_uring_setup(entries, params)? })
}

/// `io_uring_enter`, submitting `to_submit` and waiting for nothing.
#[expect(
    unsafe_code,
    reason = "io_uring_enter has no safe wrapper; the call is checked below"
)]
fn enter(fd: BorrowedFd<'_>, to_submit: u32) -> io::Result<u32> {
    // SAFETY: no extended argument (no signal mask, no wait region), so the kernel reads no
    // memory through this call's arguments. What it reads through the submitted entries is
    // memory this process keeps alive for the ring's life: the iovec arrays and the entries'
    // buffers, held by the `KernelRing` until after the ring is closed.
    Ok(unsafe { rustix::io_uring::io_uring_enter(fd, to_submit, 0, IoringEnterFlags::GETEVENTS)? })
}

/// `f`, again as long as it fails with `EINTR` (a signal arrived during the call).
fn retry_interrupted<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match f() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            done => return done,
        }
    }
}

fn words(bytes: u32) -> usize {
    bytes as usize / 4
}

impl KernelRing {
    /// A ring with room for `depth` queued commands, whose FUSE commands name `dev` (a
    /// descriptor of the connection).
    ///
    /// # Errors
    ///
    /// The error of `io_uring_setup` (for example `EPERM` where `kernel.io_uring_disabled` or a
    /// seccomp profile refuses it, `ENOSYS` without io_uring) or of a mapping.
    pub(crate) fn new(dev: Arc<DevFd>, depth: u32) -> io::Result<Self> {
        let entries = depth.max(1);
        let mut params = io_uring_params::default();
        params.flags = IoringSetupFlags::SQE128 | IoringSetupFlags::CQSIZE;
        // Every entry has at most one command in flight, so at most `entries` completions wait
        // at once; twice that is room to spare.
        params.cq_entries = entries.saturating_mul(2);
        let fd = setup(entries, &mut params)?;
        let (s, c) = (params.sq_off, params.cq_off);
        let sq_len = s.array as usize + params.sq_entries as usize * 4;
        let cq_len = c.cqes as usize + params.cq_entries as usize * 16;
        let sq = Mmap::map(fd.as_fd(), sq_len, IORING_OFF_SQ_RING)?;
        let cq = Mmap::map(fd.as_fd(), cq_len, IORING_OFF_CQ_RING)?;
        let sqes = Mmap::map(
            fd.as_fd(),
            params.sq_entries as usize * 128,
            IORING_OFF_SQES,
        )?;
        let sq_off = SqOff {
            head: words(s.head),
            tail: words(s.tail),
            mask: words(s.ring_mask),
            array: words(s.array),
            entries: params.sq_entries,
        };
        let cq_off = CqOff {
            head: words(c.head),
            tail: words(c.tail),
            mask: words(c.ring_mask),
            cqes: words(c.cqes),
        };
        let tail = sq.words32()[sq_off.tail].load(Ordering::Acquire);
        Ok(Self {
            sq,
            cq,
            sqes,
            fd,
            dev,
            sq_off,
            cq_off,
            tail,
            queued: 0,
            bufs: Vec::new(),
        })
    }

    /// Queues `sqe`, submitting first if the submission ring is full.
    pub(crate) fn push(&mut self, sqe: &Sqe) -> io::Result<()> {
        let ring = self.sq.words32();
        let head = ring[self.sq_off.head].load(Ordering::Acquire);
        if self.tail.wrapping_sub(head) >= self.sq_off.entries {
            // `submit` returns once the kernel has consumed every queued entry (moved the head
            // past them), so the ring has room after it.
            self.submit()?;
        }
        let ring = self.sq.words32();
        let index = self.tail & ring[self.sq_off.mask].load(Ordering::Relaxed);
        let slot = &self.sqes.words64()[index as usize * 16..][..16];
        for (cell, word) in slot.iter().zip(sqe) {
            cell.store(*word, Ordering::Relaxed);
        }
        ring[self.sq_off.array + index as usize].store(index, Ordering::Relaxed);
        self.tail = self.tail.wrapping_add(1);
        // The release orders the entry's words before the tail that publishes them.
        ring[self.sq_off.tail].store(self.tail, Ordering::Release);
        self.queued += 1;
        Ok(())
    }

    fn dev(&self) -> RawFd {
        self.dev.as_raw_fd()
    }
}

impl Ring for KernelRing {
    fn register(&mut self, qid: u16, tag: u64, bufs: &Arc<EntryBufs>) -> io::Result<()> {
        let addr = bufs.iov_addr();
        self.bufs.push(bufs.clone());
        let sqe = fuse_cmd(self.dev(), CMD_REGISTER, addr, 2, tag, 0, qid);
        self.push(&sqe)
    }

    fn commit(&mut self, qid: u16, tag: u64, commit_id: u64) -> io::Result<()> {
        let sqe = fuse_cmd(self.dev(), CMD_COMMIT_AND_FETCH, 0, 0, tag, commit_id, qid);
        self.push(&sqe)
    }

    fn submit(&mut self) -> io::Result<()> {
        while self.queued > 0 {
            let n = retry_interrupted(|| enter(self.fd.as_fd(), self.queued))?;
            self.queued -= n.min(self.queued);
        }
        Ok(())
    }

    fn reap(&mut self, out: &mut Vec<Cqe>) {
        let ring = self.cq.words32();
        let mut head = ring[self.cq_off.head].load(Ordering::Relaxed);
        let tail = ring[self.cq_off.tail].load(Ordering::Acquire);
        let mask = ring[self.cq_off.mask].load(Ordering::Relaxed);
        while head != tail {
            let at = self.cq_off.cqes + (head & mask) as usize * 4;
            let lo = ring[at].load(Ordering::Relaxed);
            let hi = ring[at + 1].load(Ordering::Relaxed);
            let res = ring[at + 2].load(Ordering::Relaxed) as i32;
            out.push(Cqe {
                user_data: u64::from(lo) | u64::from(hi) << 32,
                res,
            });
            head = head.wrapping_add(1);
        }
        // The release orders the reads of the completions before the head that frees them.
        ring[self.cq_off.head].store(head, Ordering::Release);
    }

    fn readiness(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uring::ring::tag;

    fn bytes(sqe: &Sqe) -> Vec<u8> {
        sqe.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn a_fuse_command_puts_each_field_at_its_uapi_offset() {
        let sqe = fuse_cmd(9, CMD_REGISTER, 0x1122_3344_5566_7788, 2, 0xabcd, 77, 87);
        let b = bytes(&sqe);
        assert_eq!(b.len(), 128);
        assert_eq!(b[0], 46, "IORING_OP_URING_CMD");
        assert_eq!(b[1], 0, "no SQE flags");
        assert_eq!(i32::from_le_bytes(b[4..8].try_into().unwrap()), 9, "fd");
        assert_eq!(
            u32::from_le_bytes(b[8..12].try_into().unwrap()),
            1,
            "cmd_op"
        );
        assert_eq!(
            u64::from_le_bytes(b[16..24].try_into().unwrap()),
            0x1122_3344_5566_7788,
            "addr"
        );
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 2, "len");
        assert_eq!(
            u64::from_le_bytes(b[32..40].try_into().unwrap()),
            0xabcd,
            "user_data"
        );
        // struct fuse_uring_cmd_req at byte 48: flags, commit_id, qid.
        assert_eq!(u64::from_le_bytes(b[48..56].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(b[56..64].try_into().unwrap()), 77);
        assert_eq!(u16::from_le_bytes(b[64..66].try_into().unwrap()), 87);
        assert!(b[66..].iter().all(|&x| x == 0));
    }

    #[test]
    fn a_nop_carries_only_its_opcode_and_user_data() {
        let b = bytes(&nop(5));
        assert_eq!(b[0], 0);
        assert_eq!(u64::from_le_bytes(b[32..40].try_into().unwrap()), 5);
        assert_eq!(b.iter().filter(|&&x| x != 0).count(), 1);
    }

    /// A ring of the running kernel, or why there is none (a sandbox may refuse io_uring).
    fn ring(depth: u32) -> Option<KernelRing> {
        // A pipe stands in for the device: it has no `uring_cmd`, so the kernel refuses every
        // FUSE command on it.
        let (dev, _) = rustix::pipe::pipe().unwrap();
        let say = |e: &io::Error| eprint_skip(&format!("io_uring_setup failed here: {e}"));
        KernelRing::new(Arc::new(DevFd::from_fd(dev)), depth)
            .inspect_err(say)
            .ok()
    }

    #[test]
    fn nops_pushed_past_the_ring_size_are_all_submitted_and_reaped_in_order() {
        // Two submission entries and four completion entries: the third push submits the
        // first two to make room.
        // `map`, not `let else`: where io_uring is refused, `ring` said so and there is nothing
        // to check.
        let _ = ring(2).map(|mut r| {
            for i in 0..3u64 {
                r.push(&nop(100 + i)).unwrap();
            }
            r.submit().unwrap();
            let mut out = Vec::new();
            r.reap(&mut out);
            let got: Vec<(u64, i32)> = out.iter().map(|c| (c.user_data, c.res)).collect();
            assert_eq!(got, (100..103).map(|u| (u, 0)).collect::<Vec<_>>());
            out.clear();
            r.reap(&mut out);
            assert!(out.is_empty(), "a completion is reaped once");
            r.submit().unwrap();
        });
    }

    #[test]
    fn a_fuse_command_on_a_descriptor_that_is_not_a_fuse_device_completes_with_an_error() {
        let _ = ring(4).map(|mut r| {
            let bufs = EntryBufs::new(8192);
            r.register(3, tag(3, 1), &bufs).unwrap();
            r.commit(3, tag(3, 2), 9).unwrap();
            r.submit().unwrap();
            let mut out = Vec::new();
            r.reap(&mut out);
            assert_eq!(out.len(), 2, "{out:?}");
            assert_eq!(out[0].user_data, tag(3, 1));
            assert_eq!(out[1].user_data, tag(3, 2));
            // A pipe has no `uring_cmd`: the kernel refuses the command (`EOPNOTSUPP`).
            assert!(out.iter().all(|c| c.res == -95), "{out:?}");
            assert!(r.readiness() >= 0);
        });
    }

    /// Says on stderr why a check does not run.
    fn eprint_skip(why: &str) {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("skipped: {why}\n").as_bytes(),
        );
    }

    #[test]
    fn an_interrupted_call_is_made_again_and_another_error_is_returned() {
        let mut calls = 0;
        let got = retry_interrupted(|| {
            calls += 1;
            match calls {
                1 | 2 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                _ => Ok(calls),
            }
        });
        assert_eq!(got.unwrap(), 3);
        let err = retry_interrupted(|| Err::<(), _>(io::Error::from_raw_os_error(9))).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(9));
    }

    #[test]
    fn a_ring_of_no_entries_is_refused_by_the_kernel() {
        // `io_uring_setup(0)` is `EINVAL`; `KernelRing::new` asks for at least one.
        let mut params = io_uring_params::default();
        let err = setup(0, &mut params).unwrap_err();
        assert!(
            matches!(err.raw_os_error(), Some(22 | 1 | 38)),
            "EINVAL, or EPERM/ENOSYS where io_uring is refused: {err}"
        );
    }
}
