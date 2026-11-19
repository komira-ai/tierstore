//! One queue of a FUSE connection on io_uring (5.2 item 4): its entries, their REGISTER, the
//! requests they carry, and the COMMIT_AND_FETCH that answers each and takes the next.
//!
//! [`UringReader`] is the io_uring [`Transport`]: it delivers each request an entry carries to the
//! shared reader loop, rebuilt as the bytes `/dev/fuse` would have read (the in header, the
//! first argument the kernel put in `op_in`, and the rest from the payload buffer), and names
//! the entry as the device its reply goes to. [`EntryReply`] is that device: it copies the reply
//! into the entry's buffers and hands the entry to the queue's thread, the only one that submits
//! to the ring, which commits it. A reply may come from any thread; the entry is the request's
//! until its reply, so exactly one writer ever touches its buffers (the request's single
//! completion, [`crate::Replier`]).
//!
//! A request that takes no reply (a FORGET the kernel forces from `readdirplus`) is committed at
//! once and not delivered: until it is, the entry stays out of the queue (K11). A queue ends when
//! the kernel tears its entries down (`ENOTCONN` at unmount or abort) or ends one with
//! `ECANCELED`; a request whose reply is sent after that is refused, as `/dev/fuse` refuses a
//! reply to a connection that is gone.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::{Either, select};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;

use super::ring::{
    COMMIT_ID, Cqe, EntryBufs, IN_OUT, OP_IN, OP_IN_LEN, PAYLOAD_SZ, Ring, entry_of, tag,
};
use crate::fs::IN_HEADER_LEN;
use crate::opcode::Opcode;
use crate::reply::{Errno, OUT_HEADER_LEN, ReplyDevice, encode_out_header};
use crate::transport::{ReaderEnd, Received, Transport};

/// What the queue's thread has to do that other threads ask for.
#[derive(Default)]
struct Pending {
    /// Entries whose reply is in their buffers, to commit.
    commits: Vec<u32>,
    /// Set once the queue has ended: a reply then is refused.
    ended: bool,
}

/// The part of a queue other threads reach: its pending commits and the wake of its thread.
pub(crate) struct Shared {
    qid: u16,
    pending: Mutex<Pending>,
    wake: Notify,
    /// Entries delivered to the file system and not committed yet (`uring_entries_parked`).
    held: AtomicU64,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("qid", &self.qid)
            .field("held", &self.held())
            .finish_non_exhaustive()
    }
}

impl Shared {
    pub(crate) fn new(qid: u16) -> Arc<Self> {
        Arc::new(Self {
            qid,
            pending: Mutex::new(Pending::default()),
            wake: Notify::new(),
            held: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The queue's number.
    pub(crate) fn qid(&self) -> u16 {
        self.qid
    }

    /// The entries of this queue holding a request the file system has not answered.
    pub(crate) fn held(&self) -> u64 {
        self.held.load(Ordering::Relaxed)
    }

    /// Queues `entry`'s commit, waking the queue's thread if another thread asks.
    fn commit(&self, entry: u32) -> io::Result<()> {
        {
            let mut pending = self.lock();
            if pending.ended {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "the io_uring queue has ended",
                ));
            }
            pending.commits.push(entry);
        }
        // Always, the queue's own thread included: a task of the queue's runtime runs on that
        // thread while the reader waits, and its reply must wake it. A permit is stored if the
        // reader is not waiting yet, so the wake is never lost; at worst the reader loops once
        // for nothing.
        self.wake.notify_one();
        Ok(())
    }
}

/// The reply device of one entry: replies go into its buffers and are committed by the queue's
/// thread.
pub(crate) struct EntryReply {
    entry: u32,
    bufs: Arc<EntryBufs>,
    shared: Arc<Shared>,
    /// The unique of the request the entry holds.
    unique: AtomicU64,
    /// Whether the request the entry holds is answered: a second reply is refused, as the kernel
    /// refuses one on `/dev/fuse` (`ENOENT`).
    answered: AtomicBool,
}

impl ReplyDevice for EntryReply {
    /// Copies the reply (its out header, then its payload) into the entry's buffers and has it
    /// committed. A reply whose payload does not fit the payload buffer, or with no whole out
    /// header, is answered `EIO` instead, so the entry comes back, and is refused
    /// (`InvalidInput`). A second reply to the request is refused with `ENOENT`, as `/dev/fuse`
    /// refuses it.
    fn send(&self, parts: &[&[u8]]) -> io::Result<()> {
        if self.answered.swap(true, Ordering::AcqRel) {
            return Err(io::Error::from_raw_os_error(2));
        }
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let mut header = [0u8; OUT_HEADER_LEN];
        let mut fits = total >= OUT_HEADER_LEN;
        let mut offset = 0usize;
        for part in parts {
            let head = OUT_HEADER_LEN.saturating_sub(offset).min(part.len());
            if head > 0 {
                header[offset..offset + head].copy_from_slice(&part[..head]);
            }
            let rest = &part[head..];
            if fits && !rest.is_empty() {
                fits = self
                    .bufs
                    .payload
                    .write(offset + head - OUT_HEADER_LEN, rest);
            }
            offset += part.len();
        }
        let payload_len = match fits {
            true => total - OUT_HEADER_LEN,
            false => {
                let unique = self.unique.load(Ordering::Acquire);
                header = encode_out_header(unique, Some(Errno::EIO), 0);
                0
            }
        };
        self.bufs.header.write(IN_OUT, &header);
        let payload_sz = u32::try_from(payload_len).unwrap_or(u32::MAX);
        self.bufs
            .header
            .write(PAYLOAD_SZ, &payload_sz.to_le_bytes());
        self.shared.commit(self.entry)?;
        match fits {
            true => Ok(()),
            false => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a FUSE reply of {total} bytes does not fit an io_uring entry (a 16-byte \
                     header and a payload of at most {} bytes); answered EIO",
                    self.bufs.payload.len()
                ),
            )),
        }
    }
}

/// The ring's descriptor, as `AsyncFd` registers it.
struct Readiness(Box<dyn Ring>);

impl AsRawFd for Readiness {
    fn as_raw_fd(&self) -> RawFd {
        self.0.readiness()
    }
}

/// The io_uring transport of one queue.
pub(crate) struct UringReader {
    ring: AsyncFd<Readiness>,
    shared: Arc<Shared>,
    entries: Vec<Arc<EntryReply>>,
    /// Entries carrying a request not delivered yet, in the order they completed.
    ready: VecDeque<u32>,
    /// The entry of the request delivered last: the device of its reply.
    current: u32,
    /// Why the queue ended, once it has.
    end: Option<ReaderEnd>,
    /// The error that ended it, as the kernel gave it (an errno).
    refused: Option<i32>,
    /// Completions reaped and not yet looked at.
    cqes: Vec<Cqe>,
}

impl UringReader {
    /// The transport of queue `shared.qid` on `ring`, with `depth` entries of `payload` bytes,
    /// none registered yet. Must be called inside a tokio runtime with I/O enabled.
    ///
    /// # Errors
    ///
    /// The error of registering the ring's descriptor with the runtime.
    #[expect(
        deprecated,
        reason = "tokio deprecates the safe registration because a value that does not own its \
                  descriptor could close it while registered; `Readiness` owns the ring, which \
                  owns the descriptor, for as long as the `AsyncFd` holds it, which is the \
                  condition of the `unsafe` replacement, so no `unsafe` is added here"
    )]
    pub(crate) fn new(
        ring: Box<dyn Ring>,
        shared: Arc<Shared>,
        depth: u16,
        payload: usize,
    ) -> io::Result<Self> {
        let entries = (0..u32::from(depth))
            .map(|entry| {
                Arc::new(EntryReply {
                    entry,
                    bufs: EntryBufs::new(payload),
                    shared: shared.clone(),
                    unique: AtomicU64::new(0),
                    answered: AtomicBool::new(true),
                })
            })
            .collect();
        let ring = AsyncFd::with_interest(Readiness(ring), Interest::READABLE)?;
        Ok(Self {
            ring,
            shared,
            entries,
            ready: VecDeque::new(),
            current: 0,
            end: None,
            refused: None,
            cqes: Vec::new(),
        })
    }

    fn ring(&mut self) -> &mut dyn Ring {
        &mut *self.ring.get_mut().0
    }

    /// Registers every entry and submits, then reports the first REGISTER the kernel refused,
    /// if one was: a refused REGISTER completes at once, a registered entry only when it carries
    /// a request. Requests that complete meanwhile are kept for [`Transport::receive`].
    ///
    /// # Errors
    ///
    /// The ring's error, or the refusal (`errno`) of a REGISTER.
    pub(crate) fn register(&mut self) -> io::Result<()> {
        let qid = self.shared.qid;
        let all: Vec<Arc<EntryReply>> = self.entries.clone();
        for entry in &all {
            self.ring()
                .register(qid, tag(qid, entry.entry), &entry.bufs)?;
        }
        self.ring().submit()?;
        self.collect();
        match self.end {
            Some(_) => Err(self.refusal()),
            None => Ok(()),
        }
    }

    /// Has the kernel turn io_uring off for the connection: a REGISTER for queue `bad`, past the
    /// last, which the kernel refuses (`EINVAL`); any refused REGISTER sets `fc->io_uring = 0`
    /// and lets the requests the kernel holds for the ring flow on `/dev/fuse` (K10). Used when
    /// a queue could not register, so the ring is never ready, instead of leaving the mount's
    /// requests waiting for it.
    ///
    /// # Errors
    ///
    /// The ring's error; or `Other` if the kernel took the REGISTER.
    pub(crate) fn poison(&mut self, bad: u16) -> io::Result<()> {
        let bufs = self.entries[0].bufs.clone();
        self.ring().register(bad, tag(bad, 0), &bufs)?;
        self.ring().submit()?;
        let refused_before = self.refused;
        self.refused = None;
        self.collect();
        let refused = self.refused.take();
        self.refused = refused_before.or(refused);
        match refused {
            Some(_) => Ok(()),
            None => Err(io::Error::other(
                "the kernel took a REGISTER of a queue past the last",
            )),
        }
    }

    /// The errno of the refusal that ended the queue.
    fn refusal(&self) -> io::Error {
        io::Error::from_raw_os_error(self.refused.unwrap_or(5))
    }

    /// Reaps the completions posted so far and looks at them.
    fn collect(&mut self) {
        let mut cqes = std::mem::take(&mut self.cqes);
        self.ring().reap(&mut cqes);
        self.cqes = cqes;
        self.process();
    }

    /// Looks at the completions reaped: an entry carrying a request joins `ready`; an error
    /// ends the queue, except `ENOENT` (which a REGISTER never gets).
    fn process(&mut self) {
        let mut cqes = std::mem::take(&mut self.cqes);
        for cqe in cqes.drain(..) {
            let entry = entry_of(cqe.user_data);
            match cqe.res {
                0 => self.ready.push_back(entry),
                // `ENOENT`: a commit of a request the kernel no longer holds (it was ended by
                // the kernel). That entry does not come back; the queue goes on with the rest.
                -2 => {}
                res => {
                    self.refused = Some(res.checked_neg().unwrap_or(5));
                    self.finish(match res {
                        -103 => ReaderEnd::Aborted,
                        _ => ReaderEnd::Unmounted,
                    });
                }
            }
        }
        self.cqes = cqes;
    }

    /// Ends the queue: a later reply is refused.
    fn finish(&mut self, end: ReaderEnd) {
        self.shared.lock().ended = true;
        self.end.get_or_insert(end);
    }

    /// Submits the commits other threads (or this one) queued.
    fn flush(&mut self) -> io::Result<()> {
        let commits = std::mem::take(&mut self.shared.lock().commits);
        if commits.is_empty() {
            return Ok(());
        }
        let qid = self.shared.qid;
        for entry in commits {
            let unique = self.entries[entry as usize].unique.load(Ordering::Acquire);
            self.shared.held.fetch_sub(1, Ordering::Relaxed);
            self.ring().commit(qid, tag(qid, entry), unique)?;
        }
        self.ring().submit()
    }

    /// Takes the next ready entry's request into `buf`: `Some(len)` to deliver; `None` when it
    /// was committed at once (a request that takes no reply, or one too large for `buf`).
    fn take(&mut self, entry: u32, buf: &mut [u8]) -> Option<usize> {
        // Every tag this queue submits names one of its entries.
        let reply = self.entries.get(entry as usize)?.clone();
        let bufs = &reply.bufs;
        let mut header = [0u8; IN_HEADER_LEN];
        bufs.header.read(IN_OUT, &mut header);
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let opcode = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let unique = bufs.header.u64_at(COMMIT_ID);
        let payload = bufs.header.u32_at(PAYLOAD_SZ) as usize;
        reply.unique.store(unique, Ordering::Release);
        reply.answered.store(false, Ordering::Release);
        self.shared.held.fetch_add(1, Ordering::Relaxed);
        let op_len = len.checked_sub(IN_HEADER_LEN + payload);
        let fits = op_len.is_some_and(|op| op <= OP_IN_LEN && len <= buf.len())
            && payload <= bufs.payload.len();
        if !fits {
            let _ = reply.send(&[&encode_out_header(unique, Some(Errno::EIO), 0)]);
            return None;
        }
        if matches!(
            Opcode::from_code(opcode),
            Some(Opcode::Forget | Opcode::BatchForget)
        ) {
            let _ = reply.send(&[&encode_out_header(unique, None, 0)]);
            return None;
        }
        let op_len = op_len.unwrap_or(0);
        buf[..IN_HEADER_LEN].copy_from_slice(&header);
        bufs.header
            .read(OP_IN, &mut buf[IN_HEADER_LEN..IN_HEADER_LEN + op_len]);
        bufs.payload.read(0, &mut buf[IN_HEADER_LEN + op_len..len]);
        Some(len)
    }
}

impl Transport for UringReader {
    async fn receive(&mut self, buf: &mut [u8]) -> io::Result<Received> {
        loop {
            self.flush()?;
            if let Some(entry) = self.ready.pop_front() {
                if let Some(len) = self.take(entry, buf) {
                    self.current = entry;
                    return Ok(Received::Request(len));
                }
                continue;
            }
            if let Some(end) = self.end {
                return Ok(Received::Ended(end));
            }
            {
                let shared = self.shared.clone();
                let woken = std::pin::pin!(shared.wake.notified());
                let readable = std::pin::pin!(self.ring.readable_mut());
                if let Either::Left((guard, _)) = select(readable, woken).await {
                    let mut guard = guard?;
                    guard.get_inner_mut().0.reap(&mut self.cqes);
                    if self.cqes.is_empty() {
                        guard.clear_ready();
                    }
                }
            }
            self.collect();
        }
    }

    fn device(&self) -> Arc<dyn ReplyDevice> {
        self.entries[self.current as usize].clone()
    }
}

impl Drop for UringReader {
    /// Submits the replies still queued (the answer to a DESTROY, read last): the reader loop
    /// ends right after it routes one.
    fn drop(&mut self) {
        let _ = self.flush();
        self.finish(ReaderEnd::Stopped);
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "a unit test: a failed unwrap is the failure it reports"
    )]

    use super::*;

    fn entry(shared: &Arc<Shared>, payload: usize) -> Arc<EntryReply> {
        Arc::new(EntryReply {
            entry: 0,
            bufs: EntryBufs::new(payload),
            shared: shared.clone(),
            unique: AtomicU64::new(0),
            answered: AtomicBool::new(true),
        })
    }

    /// The entry now holds request `unique`, as the queue's thread sets it on delivery.
    fn deliver(reply: &EntryReply, unique: u64) {
        reply.unique.store(unique, Ordering::Release);
        reply.answered.store(false, Ordering::Release);
    }

    fn out_header(reply: &EntryReply) -> (i32, u64) {
        let mut h = [0u8; OUT_HEADER_LEN];
        assert!(reply.bufs.header.read(IN_OUT, &mut h));
        (
            i32::from_le_bytes(h[4..8].try_into().unwrap()),
            u64::from_le_bytes(h[8..16].try_into().unwrap()),
        )
    }

    #[test]
    fn an_entry_takes_one_reply_per_request() {
        let shared = Shared::new(0);
        let reply = entry(&shared, 8192);
        let err = reply.send(&[&encode_out_header(1, None, 0)]).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(2), "no request yet: ENOENT");
        deliver(&reply, 5);
        reply
            .send(&[&encode_out_header(5, None, 3), b"abc"])
            .unwrap();
        let err = reply.send(&[&encode_out_header(5, None, 0)]).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(2), "a second reply: ENOENT");
        assert_eq!(shared.lock().commits, vec![0]);
        assert_eq!(out_header(&reply), (0, 5));
        assert_eq!(reply.bufs.header.u32_at(PAYLOAD_SZ), 3);
    }

    #[test]
    fn a_reply_without_a_whole_header_is_answered_eio() {
        let shared = Shared::new(0);
        let reply = entry(&shared, 8192);
        deliver(&reply, 6);
        let err = reply.send(&[b"short"]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(out_header(&reply), (-5, 6));
        assert_eq!(reply.bufs.header.u32_at(PAYLOAD_SZ), 0);
        assert_eq!(
            shared.lock().commits,
            vec![0],
            "committed, so the entry comes back"
        );
    }

    #[test]
    fn a_header_split_over_parts_and_empty_parts_are_put_together() {
        let shared = Shared::new(0);
        let reply = entry(&shared, 8192);
        deliver(&reply, 7);
        let header = encode_out_header(7, None, 3);
        reply
            .send(&[&header[..3], &[], &header[3..], b"z", &[], b"yx"])
            .unwrap();
        let mut p = [0u8; 3];
        assert!(reply.bufs.payload.read(0, &mut p));
        assert_eq!(&p, b"zyx");
        assert_eq!(out_header(&reply), (0, 7));
        assert_eq!(reply.bufs.header.u32_at(PAYLOAD_SZ), 3);
        // A payload exactly the buffer's size fits.
        deliver(&reply, 8);
        let full = vec![1u8; 8192];
        reply
            .send(&[&encode_out_header(8, None, 8192), &full])
            .unwrap();
        assert_eq!(reply.bufs.header.u32_at(PAYLOAD_SZ), 8192);
    }

    #[test]
    fn once_the_queue_has_ended_a_reply_is_refused() {
        let shared = Shared::new(0);
        let reply = entry(&shared, 8192);
        shared.lock().ended = true;
        deliver(&reply, 8);
        let err = reply.send(&[&encode_out_header(8, None, 0)]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        assert!(shared.lock().commits.is_empty());
    }

    #[test]
    fn a_reply_from_any_thread_wakes_the_queue() {
        let shared = Shared::new(0);
        let reply = entry(&shared, 8192);
        deliver(&reply, 9);
        reply.send(&[&encode_out_header(9, None, 0)]).unwrap();
        let n = std::pin::pin!(shared.wake.notified());
        assert!(
            futures::FutureExt::now_or_never(n).is_some(),
            "woken from this thread"
        );
        let r2 = reply.clone();
        deliver(&reply, 10);
        std::thread::spawn(move || r2.send(&[&encode_out_header(10, None, 0)]).unwrap())
            .join()
            .unwrap();
        let n = std::pin::pin!(shared.wake.notified());
        assert!(
            futures::FutureExt::now_or_never(n).is_some(),
            "woken from another"
        );
        assert_eq!(shared.lock().commits, vec![0, 0]);
    }
}
