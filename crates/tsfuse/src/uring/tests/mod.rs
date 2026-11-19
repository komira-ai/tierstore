//! The io_uring transport against a fake kernel: a [`FakeRing`] per queue that does what
//! `fs/fuse/dev_uring.c` does with REGISTER and COMMIT_AND_FETCH (keeps the entry's buffers,
//! writes a request into a free entry, reads the reply back, makes the entry free again), and a
//! socket pair standing in for the `/dev/fuse` descriptor. Every wait is on an event (a commit
//! recorded, a request handled), never on a timer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a unit test: a failed unwrap or expect is the failure it reports"
)]

use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::num::NonZeroU16;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};

use super::ring::{COMMIT_ID, Cqe, EntryBufs, IN_OUT, OP_IN, PAYLOAD_SZ, entry_of, tag};
use super::*;
use crate::abi::{InHeader, InitConfig, InitIn};
use crate::session::AskUring;
use crate::tokens::MAX_HELD;
use crate::{
    Completion, Errno, Fs, Handled, Interrupted, ReaderEnd, Replier, ReplyDevice, Request, Session,
    SessionError, Tokens,
};

pub(super) const INIT: u32 = 26;
pub(super) const GETATTR: u32 = 3;
pub(super) const LOOKUP: u32 = 1;
pub(super) const FORGET: u32 = 2;
pub(super) const BATCH_FORGET: u32 = 42;
pub(super) const READ: u32 = 15;
pub(super) const WRITE: u32 = 16;
pub(super) const INTERRUPT: u32 = 36;
pub(super) const DESTROY: u32 = 38;
pub(super) const STATFS: u32 = 17;
pub(super) const FLUSH: u32 = 25;

/// The flags an INIT of a kernel with `fuse.enable_uring` on offers (with `INIT_EXT`).
pub(super) fn offered_uring() -> u64 {
    InitFlags::INIT_EXT
        .union(InitFlags::OVER_IO_URING)
        .union(InitFlags::MAX_PAGES)
        .union(InitFlags::ASYNC_READ)
        .0
}

pub(super) fn request(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
    let header = InHeader {
        len: (InHeader::SIZE + body.len()) as u32,
        opcode,
        unique,
        nodeid,
        ..InHeader::default()
    };
    [header.as_bytes(), body].concat()
}

pub(super) fn init_request(flags: u64) -> Vec<u8> {
    let init = InitIn {
        major: 7,
        minor: 45,
        max_readahead: 65536,
        flags: flags as u32,
        flags2: (flags >> 32) as u32,
        ..InitIn::default()
    };
    request(INIT, 1, 0, init.as_bytes())
}

/// A `/dev/fuse` stand-in: the device end and the kernel end of a sequenced-packet pair.
pub(super) fn fake_device() -> (crate::DevFd, UnixStream) {
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
    let (device, kernel) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    (crate::DevFd::from_fd(device), UnixStream::from(kernel))
}

/// A reply as the fake kernel saw it: an out header and a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Reply {
    pub(super) qid: u16,
    pub(super) entry: u32,
    pub(super) commit_id: u64,
    pub(super) unique: u64,
    pub(super) error: i32,
    pub(super) payload: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct KState {
    /// The buffers of every registered entry, by tag.
    pub(super) bufs: HashMap<u64, Arc<EntryBufs>>,
    /// The REGISTERs in the order the kernel took them.
    pub(super) registers: Vec<u64>,
    /// Free entries of each queue.
    pub(super) free: HashMap<u16, VecDeque<u64>>,
    /// Requests waiting for a free entry of their queue: (`op_in` length, the wire bytes).
    pub(super) waiting: HashMap<u16, VecDeque<(usize, Vec<u8>)>>,
    /// Completions to post, per queue.
    pub(super) cqes: HashMap<u16, VecDeque<Cqe>>,
    /// The commits, in order.
    pub(super) replies: Vec<Reply>,
    /// Queues whose REGISTER the kernel refuses, with the errno.
    pub(super) refuse: HashMap<u16, i32>,
    /// Results to give the next commits instead of taking them.
    pub(super) commit_fail: VecDeque<i32>,
    /// The errno every submit fails with, once set (an `io_uring_enter` that fails).
    pub(super) fail_submit: Option<i32>,
    /// The REGISTERs of a queue past the last: (the ring's queue, the queue asked for).
    pub(super) poisoned: Vec<(u16, u16)>,
    /// The writing end of each ring's readiness pair.
    pub(super) wake: HashMap<u16, UnixStream>,
}

#[derive(Default)]
pub(crate) struct FakeKernel {
    pub(super) state: Mutex<KState>,
    pub(super) changed: Condvar,
}

impl FakeKernel {
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, KState> {
        self.state.lock().unwrap()
    }

    /// Waits until `done` holds of the state.
    pub(super) fn wait(&self, done: impl Fn(&KState) -> bool) -> std::sync::MutexGuard<'_, KState> {
        self.changed.wait_while(self.lock(), |s| !done(s)).unwrap()
    }

    /// Posts `cqe` on queue `qid` and wakes its ring.
    pub(super) fn post(s: &mut KState, qid: u16, cqe: Cqe) {
        s.cqes.entry(qid).or_default().push_back(cqe);
        if let Some(w) = s.wake.get(&qid) {
            let _ = (&*w).write(b"x");
        }
    }

    /// Gives the waiting requests of `qid` to its free entries, as the kernel's dispatch does.
    pub(super) fn dispatch(s: &mut KState, qid: u16) {
        loop {
            let (Some(_), Some(_)) = (
                s.free.get(&qid).and_then(|f| f.front()),
                s.waiting.get(&qid).and_then(|w| w.front()),
            ) else {
                return;
            };
            let t = s.free.get_mut(&qid).unwrap().pop_front().unwrap();
            let (op_len, bytes) = s.waiting.get_mut(&qid).unwrap().pop_front().unwrap();
            let bufs = s.bufs[&t].clone();
            let unique = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
            assert!(bufs.header.write(IN_OUT, &bytes[..40]));
            assert!(bufs.header.write(OP_IN, &bytes[40..40 + op_len]));
            let payload = &bytes[40 + op_len..];
            assert!(bufs.payload.write(0, payload));
            assert!(bufs.header.write(COMMIT_ID, &unique.to_le_bytes()));
            assert!(
                bufs.header
                    .write(PAYLOAD_SZ, &(payload.len() as u32).to_le_bytes())
            );
            Self::post(
                s,
                qid,
                Cqe {
                    user_data: t,
                    res: 0,
                },
            );
        }
    }

    /// Sends request `bytes`, whose first argument is `op_len` bytes, to queue `qid`.
    pub(super) fn deliver(&self, qid: u16, op_len: usize, bytes: Vec<u8>) {
        let mut s = self.lock();
        s.waiting.entry(qid).or_default().push_back((op_len, bytes));
        Self::dispatch(&mut s, qid);
        self.changed.notify_all();
    }

    /// Posts a completion the kernel makes on its own (an entry torn down, a cancel).
    pub(super) fn complete(&self, qid: u16, res: i32) {
        let mut s = self.lock();
        let t = s.free.get_mut(&qid).and_then(VecDeque::pop_front).unwrap();
        Self::post(&mut s, qid, Cqe { user_data: t, res });
        self.changed.notify_all();
    }
}

pub(super) enum Cmd {
    Register(u64, Arc<EntryBufs>),
    Poison(u16, u64),
    Commit(u64, u64),
}

pub(crate) struct FakeRing {
    qid: u16,
    kernel: Arc<FakeKernel>,
    queued: Vec<Cmd>,
    readiness: UnixStream,
}

impl FakeRing {
    pub(super) fn new(qid: u16, kernel: &Arc<FakeKernel>) -> Self {
        let (r, w) = UnixStream::pair().unwrap();
        r.set_nonblocking(true).unwrap();
        kernel.lock().wake.insert(qid, w);
        Self {
            qid,
            kernel: kernel.clone(),
            queued: Vec::new(),
            readiness: r,
        }
    }
}

impl Ring for FakeRing {
    fn register(&mut self, qid: u16, tag: u64, bufs: &Arc<EntryBufs>) -> io::Result<()> {
        match qid == self.qid {
            true => self.queued.push(Cmd::Register(tag, bufs.clone())),
            // A queue past the last: the kernel refuses it (`EINVAL`) and turns io_uring off.
            false => self.queued.push(Cmd::Poison(qid, tag)),
        }
        Ok(())
    }

    fn commit(&mut self, qid: u16, tag: u64, commit_id: u64) -> io::Result<()> {
        assert_eq!(qid, self.qid);
        self.queued.push(Cmd::Commit(tag, commit_id));
        Ok(())
    }

    fn submit(&mut self) -> io::Result<()> {
        let k = self.kernel.clone();
        let mut s = k.lock();
        if let Some(errno) = s.fail_submit {
            return Err(io::Error::from_raw_os_error(errno));
        }
        for cmd in self.queued.drain(..) {
            match cmd {
                Cmd::Register(t, bufs) => {
                    s.registers.push(t);
                    if let Some(&errno) = s.refuse.get(&self.qid) {
                        FakeKernel::post(
                            &mut s,
                            self.qid,
                            Cqe {
                                user_data: t,
                                res: -errno,
                            },
                        );
                        continue;
                    }
                    s.bufs.insert(t, bufs);
                    s.free.entry(self.qid).or_default().push_back(t);
                }
                Cmd::Poison(qid, t) => {
                    s.poisoned.push((self.qid, qid));
                    FakeKernel::post(
                        &mut s,
                        self.qid,
                        Cqe {
                            user_data: t,
                            res: -22,
                        },
                    );
                }
                Cmd::Commit(t, commit_id) => {
                    let bufs = s.bufs[&t].clone();
                    let mut h = [0u8; 16];
                    assert!(bufs.header.read(IN_OUT, &mut h));
                    let n = bufs.header.u32_at(PAYLOAD_SZ) as usize;
                    let mut payload = vec![0u8; n];
                    assert!(bufs.payload.read(0, &mut payload));
                    s.replies.push(Reply {
                        qid: self.qid,
                        entry: entry_of(t),
                        commit_id,
                        unique: u64::from_le_bytes(h[8..16].try_into().unwrap()),
                        error: i32::from_le_bytes(h[4..8].try_into().unwrap()),
                        payload,
                    });
                    if let Some(res) = s.commit_fail.pop_front() {
                        FakeKernel::post(&mut s, self.qid, Cqe { user_data: t, res });
                        continue;
                    }
                    s.free.entry(self.qid).or_default().push_back(t);
                }
            }
        }
        FakeKernel::dispatch(&mut s, self.qid);
        k.changed.notify_all();
        Ok(())
    }

    fn reap(&mut self, out: &mut Vec<Cqe>) {
        let mut sink = [0u8; 64];
        while (&self.readiness).read(&mut sink).is_ok_and(|n| n > 0) {}
        let mut s = self.kernel.lock();
        if let Some(q) = s.cqes.get_mut(&self.qid) {
            out.extend(q.drain(..));
        }
    }

    fn readiness(&self) -> RawFd {
        self.readiness.as_raw_fd()
    }
}

/// A request as the file system saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Seen {
    pub(super) opcode: u32,
    pub(super) unique: u64,
    pub(super) bytes: Vec<u8>,
}

/// Answers GETATTR and LOOKUP with `attr` bytes and STATFS with a payload larger than any entry
/// holds; parks READ; answers the rest `ENOSYS`.
#[derive(Default)]
pub(crate) struct TestFs {
    pub(super) seen: Mutex<Vec<Seen>>,
    pub(super) parked: Mutex<Vec<Replier>>,
    pub(super) inline: Mutex<Vec<Completion>>,
    pub(super) changed: Condvar,
}

impl TestFs {
    pub(super) fn wait_seen(&self, n: usize) -> Vec<Seen> {
        self.changed
            .wait_while(self.seen.lock().unwrap(), |s| s.len() < n)
            .unwrap()
            .clone()
    }

    pub(super) fn take_parked(&self, n: usize) -> Vec<Replier> {
        let mut parked = self
            .changed
            .wait_while(self.parked.lock().unwrap(), |p| p.len() < n)
            .unwrap();
        parked.drain(..).collect()
    }
}

impl Fs for TestFs {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        let seen = Seen {
            opcode: req.header.opcode,
            unique: req.header.unique,
            bytes: req.body.to_vec(),
        };
        let handled = match req.header.opcode {
            READ => {
                self.parked.lock().unwrap().push(replier);
                Handled::Parked
            }
            GETATTR | LOOKUP => {
                let c = replier.ok(&[b"attr-", &req.header.unique.to_le_bytes()]);
                self.inline.lock().unwrap().push(c);
                Handled::Done
            }
            // Answered by a task of the reader's own runtime, after the handler returned.
            FLUSH => {
                drop(tokio::spawn(async move {
                    tokio::task::yield_now().await;
                    let _ = replier.ok(&[]);
                }));
                Handled::Parked
            }
            STATFS => {
                let c = replier.ok(&[&vec![7u8; 64 * 1024]]);
                self.inline.lock().unwrap().push(c);
                Handled::Done
            }
            _ => {
                let c = replier.error(Errno::ENOSYS);
                self.inline.lock().unwrap().push(c);
                Handled::Done
            }
        };
        self.seen.lock().unwrap().push(seen);
        self.changed.notify_all();
        handled
    }
}

/// A host with io_uring on and `cpus` possible CPUs.
pub(super) fn host(cpus: u32) -> Host {
    Host {
        enabled: true,
        cpus: Some(cpus),
        page: 4096,
    }
}

/// INIT terms with 8 KiB payloads (`max_pages` 2, `max_write` 4096).
pub(super) fn small() -> InitConfig {
    InitConfig {
        max_pages: 2,
        ..InitConfig::tree()
    }
}

/// A session INIT'ed on a fake device with fake rings, as `host` and `config` say.
pub(super) struct Rig {
    pub(super) kernel: Arc<FakeKernel>,
    pub(super) dev: UnixStream,
    pub(super) session: Session,
    pub(super) init_reply: Vec<u8>,
}

pub(super) fn rig(host: Host, config: UringConfig, offered: u64) -> Rig {
    let kernel = Arc::new(FakeKernel::default());
    let (device, dev) = fake_device();
    (&dev).write_all(&init_request(offered)).unwrap();
    let k = kernel.clone();
    let mut make = move |_dev: &Arc<crate::DevFd>, memory: UringMemory| {
        make_rings(memory, &mut |qid| {
            Ok(Box::new(FakeRing::new(qid, &k)) as Box<dyn Ring>)
        })
    };
    let ask = AskUring {
        config: &config,
        host,
        make: &mut make,
    };
    let session = Session::init_with(device, &small(), Some(ask)).unwrap();
    let mut buf = [0u8; 4096];
    let n = (&dev).read(&mut buf).unwrap();
    Rig {
        kernel,
        dev,
        session,
        init_reply: buf[..n].to_vec(),
    }
}

pub(super) fn depth(n: u16) -> UringConfig {
    UringConfig {
        depth: NonZeroU16::new(n).unwrap(),
        ..UringConfig::default()
    }
}

/// `flags2` of an INIT reply (`fuse_out_header`, then `fuse_init_out`: `flags2` at 32).
pub(super) fn reply_flags2(reply: &[u8]) -> u32 {
    u32::from_le_bytes(reply[16 + 32..16 + 36].try_into().unwrap())
}

pub(super) const OVER_IO_URING_HIGH: u32 = 1 << 9;

mod interrupt;
mod plan;
mod serve;
