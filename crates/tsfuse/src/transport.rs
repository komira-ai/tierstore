//! The transports and the reader loop they share (5.2): a [`Transport`] delivers one request at a
//! time into the reader's buffer and names the device its reply goes to; [`serve`] routes each
//! request (INTERRUPT to the token table, FORGET dropped, DESTROY answered, the rest registered and
//! handed to [`Fs::handle`]) until the transport ends or the session is stopped.
//!
//! [`DevReader`] is the `/dev/fuse` transport: one descriptor (the mount's or a clone) driven by
//! `AsyncFd` on its reader's own current-thread runtime. Clones share the connection's pending
//! queue, so a new request wakes every reader and all but one read `EAGAIN` and wait again.

use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::{Either, select};
use rustix::io::Errno as Sys;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;

use crate::abi::Operation;
use crate::dev::DevFd;
use crate::fs::{Fs, Request};
use crate::opcode::Opcode;
use crate::reply::{Errno, ReplyDevice, encode_out_header};
use crate::tokens::Tokens;

/// Why a reader stopped reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReaderEnd {
    /// The session was stopped ([`crate::Stopper::stop`]); the device is open and the mount
    /// stays, so another process can resume it.
    Stopped,
    /// The connection ended (`ENODEV`): the file system was unmounted, or the connection was
    /// aborted through `/sys/fs/fuse/connections/<n>/abort` (whose reads end with `ENODEV`
    /// unless INIT asked for `FUSE_ABORT_ERROR`, which this library does not); or the device's
    /// other end is gone.
    Unmounted,
    /// A read ended with `ECONNABORTED`: it took a request while the connection was ending (an
    /// abort, or the release of a lazily detached mount racing the reader).
    Aborted,
    /// The kernel sent DESTROY, which was answered.
    Destroyed,
}

/// What a transport delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// A request of this many bytes is at the start of the buffer.
    Request(usize),
    /// The transport has ended; the reader ends with it.
    Ended(ReaderEnd),
}

/// A source of requests for one reader, and the device that reader's replies go to. The
/// `/dev/fuse` transport is [`DevReader`]; the io_uring transport (T-F5) is the other.
pub trait Transport {
    /// Waits for the next request and reads it into `buf`.
    fn receive(&mut self, buf: &mut [u8]) -> impl Future<Output = io::Result<Received>>;

    /// The device the reply to a request this transport delivered is written to.
    fn device(&self) -> Arc<dyn ReplyDevice>;
}

/// The stop flag every reader of a session watches.
#[derive(Debug, Default)]
pub(crate) struct Stop {
    stopped: AtomicBool,
    notify: Notify,
}

impl Stop {
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Stops the readers started so far, because reader `index` could not be started.
    pub(crate) fn failed_start(&self, index: usize, source: io::Error) -> crate::SessionError {
        self.stop();
        crate::SessionError::Start { index, source }
    }

    /// Completes once [`Self::stop`] has been called.
    async fn wait(&self) {
        loop {
            // `notify_waiters` wakes a `Notified` from its creation on, so a stop between the
            // creation and the look below is not missed.
            let notified = self.notify.notified();
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// The `/dev/fuse` transport: one descriptor, non-blocking, on `AsyncFd`.
pub struct DevReader {
    fd: AsyncFd<Shared>,
}

/// The descriptor of a reader, shared with every replier of its requests.
struct Shared(Arc<DevFd>);

impl AsRawFd for Shared {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl DevReader {
    /// The reader of `device`, which it sets non-blocking. Must be called inside a tokio
    /// runtime with I/O enabled.
    ///
    /// # Errors
    ///
    /// The `fcntl` error, or the error of registering the descriptor with the runtime.
    #[expect(
        deprecated,
        reason = "tokio deprecates the safe registration because a value that does not own its \
                  descriptor could close it while registered; `Shared` owns it (an `Arc<DevFd>` \
                  holding the `OwnedFd`) for as long as the `AsyncFd` holds `Shared`, which is \
                  the condition of the `unsafe` replacement, so no `unsafe` is added here"
    )]
    pub fn new(device: Arc<DevFd>) -> io::Result<Self> {
        device.set_nonblocking(true)?;
        let fd = AsyncFd::with_interest(Shared(device), Interest::READABLE | Interest::ERROR)?;
        Ok(Self { fd })
    }
}

impl Transport for DevReader {
    async fn receive(&mut self, buf: &mut [u8]) -> io::Result<Received> {
        loop {
            // A connection that ends polls `EPOLLERR` only, hence the error interest.
            let mut ready = self.fd.ready(Interest::READABLE | Interest::ERROR).await?;
            match read_once(&self.fd.get_ref().0, buf) {
                Some(received) => return received,
                None => ready.clear_ready(),
            }
        }
    }

    fn device(&self) -> Arc<dyn ReplyDevice> {
        self.fd.get_ref().0.clone()
    }
}

/// What one `read(2)` of a device came to.
#[derive(Debug)]
pub(crate) enum ReadOutcome {
    /// A request, the end of the reader, or a read error.
    Got(io::Result<Received>),
    /// Nothing to read (`EAGAIN`: another clone took the request): wait for readiness again.
    Wait,
    /// Read again at once (`EINTR`: a signal; `ENOENT`: the request was interrupted while it
    /// was being read).
    Again,
}

/// The outcome of the result of one `read(2)` of a device.
pub(crate) fn classify(read: rustix::io::Result<usize>) -> ReadOutcome {
    match read {
        // `/dev/fuse` never reads 0 bytes; a socket standing in for it does once its peer is
        // gone.
        Ok(0) => ReadOutcome::Got(Ok(Received::Ended(ReaderEnd::Unmounted))),
        Ok(n) => ReadOutcome::Got(Ok(Received::Request(n))),
        Err(Sys::AGAIN) => ReadOutcome::Wait,
        Err(Sys::INTR | Sys::NOENT) => ReadOutcome::Again,
        // The kernel's `fuse_dev_do_read`: `ENODEV` once the connection has ended (an abort
        // included, without `FUSE_ABORT_ERROR`), `ECONNABORTED` for a request taken as it ends.
        Err(Sys::NODEV) => ReadOutcome::Got(Ok(Received::Ended(ReaderEnd::Unmounted))),
        Err(Sys::CONNABORTED) => ReadOutcome::Got(Ok(Received::Ended(ReaderEnd::Aborted))),
        Err(e) => ReadOutcome::Got(Err(e.into())),
    }
}

/// One read of `device`: the request or the end, or `None` to wait for readiness again.
pub(crate) fn read_once(device: &DevFd, buf: &mut [u8]) -> Option<io::Result<Received>> {
    loop {
        match classify(device.read(buf)) {
            ReadOutcome::Got(received) => return Some(received),
            ReadOutcome::Wait => return None,
            ReadOutcome::Again => {}
        }
    }
}

/// Reads and routes requests from `transport` until it ends or `stop` is set.
///
/// # Errors
///
/// A read error other than the ends of [`ReaderEnd`].
pub(crate) async fn serve<T: Transport>(
    mut transport: T,
    fs: &dyn Fs,
    tokens: &Tokens,
    stop: &Stop,
    every: &Every,
    buf: &mut [u8],
) -> io::Result<ReaderEnd> {
    loop {
        let next = {
            let stopped = pin!(stop.wait());
            let received = pin!(transport.receive(buf));
            match select(stopped, received).await {
                Either::Left(((), _)) => return Ok(ReaderEnd::Stopped),
                Either::Right((received, _)) => received?,
            }
        };
        let len = match next {
            Received::Request(len) => len,
            Received::Ended(end) => return Ok(end),
        };
        let device = transport.device();
        if let Some(end) = route(&buf[..len], device, fs, tokens, every) {
            return Ok(end);
        }
    }
}

/// Every device of a session, as one: a write is tried on each in turn until one takes it.
///
/// The `EAGAIN` that answers an INTERRUPT of a request not registered yet goes here. The kernel
/// looks for the target only in the processing queue of the device the reply is written to, and
/// with clones the target may sit in another reader's queue, read and not yet registered: of
/// all the devices exactly that one takes the reply (the others refuse it with `ENOENT`), and
/// the kernel queues the INTERRUPT again, to be read once the target is registered.
///
/// The session never remembers an INTERRUPT whose target it does not find, to cancel the target
/// when it registers: "not found" also means "already answered", the kernel reuses a unique once
/// its request is answered, and a remembered INTERRUPT would then cancel a later, unrelated
/// request. The kernel's own queue keeps the INTERRUPT while its target is unanswered, which is
/// the one case where it still matters.
pub(crate) struct Every(pub(crate) Vec<Arc<dyn ReplyDevice>>);

impl ReplyDevice for Every {
    /// Writes to each device until one takes the reply; the last refusal if none does.
    fn send(&self, parts: &[&[u8]]) -> io::Result<()> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, "a session with no device");
        for device in &self.0 {
            match device.send(parts) {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

/// Routes one request; `Some` when the reader ends with it (DESTROY). `every` is every device
/// of the session, for the `EAGAIN` of an INTERRUPT whose target is not registered.
pub(crate) fn route(
    bytes: &[u8],
    device: Arc<dyn ReplyDevice>,
    fs: &dyn Fs,
    tokens: &Tokens,
    every: &Every,
) -> Option<ReaderEnd> {
    let Some(req) = Request::parse(bytes) else {
        // A request whose header is cut short or names a length past the read: answer it if
        // its id can be read at all.
        let unique = bytes.get(8..16)?;
        let unique = u64::from_le_bytes(<[u8; 8]>::try_from(unique).ok()?);
        let _ = device.send(&[&encode_out_header(unique, Some(Errno::EIO), 0)]);
        return None;
    };
    let unique = req.header.unique;
    let answer = |errno: Errno| {
        // A device error here is a connection that is gone; the next read says so.
        let _ = device.send(&[&encode_out_header(unique, Some(errno), 0)]);
    };
    match req.header.op() {
        // Never answered: the kernel expects no reply, and inode numbers are record indexes.
        Some(Opcode::Forget | Opcode::BatchForget) => None,
        Some(Opcode::Interrupt) => {
            // An INTERRUPT with a short body has no target to cancel, and the kernel expects
            // no reply to it.
            if let Ok(Operation::Interrupt(arg)) = Operation::parse(&req) {
                let _ = tokens.interrupt(arg.unique, unique, every);
            }
            None
        }
        Some(Opcode::Destroy) => {
            let _ = device.send(&[&encode_out_header(unique, None, 0)]);
            Some(ReaderEnd::Destroyed)
        }
        // The session ran INIT before its readers started.
        Some(Opcode::Init) => {
            answer(Errno::EIO);
            None
        }
        Some(_) => {
            let replier = tokens.register(unique, device.clone());
            let _ = fs.handle(req, replier);
            None
        }
        None => {
            answer(Errno::ENOSYS);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A device that takes replies (recording them) or refuses each with `errno`.
    struct Dev(Option<i32>, Mutex<usize>);

    impl ReplyDevice for Dev {
        fn send(&self, _parts: &[&[u8]]) -> io::Result<()> {
            *self
                .1
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            self.0
                .map_or(Ok(()), |e| Err(io::Error::from_raw_os_error(e)))
        }
    }

    fn sends(d: &Arc<Dev>) -> usize {
        *d.1.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn each_answer_of_a_read_is_its_outcome() {
        use ReadOutcome::{Again, Got, Wait};
        use Received::{Ended, Request};
        assert!(matches!(classify(Ok(56)), Got(Ok(Request(56)))));
        assert!(matches!(
            classify(Ok(0)),
            Got(Ok(Ended(ReaderEnd::Unmounted)))
        ));
        assert!(matches!(
            classify(Err(Sys::NODEV)),
            Got(Ok(Ended(ReaderEnd::Unmounted)))
        ));
        assert!(matches!(
            classify(Err(Sys::CONNABORTED)),
            Got(Ok(Ended(ReaderEnd::Aborted)))
        ));
        assert!(matches!(classify(Err(Sys::AGAIN)), Wait));
        assert!(matches!(classify(Err(Sys::INTR)), Again));
        assert!(matches!(classify(Err(Sys::NOENT)), Again));
        let failed = format!("{:?}", classify(Err(Sys::INVAL)));
        assert!(failed.starts_with("Got(Err("), "{failed}");
    }

    /// A transport that delivers a scripted sequence.
    struct Scripted {
        script: std::collections::VecDeque<io::Result<(Vec<u8>, Option<ReaderEnd>)>>,
        device: Arc<Dev>,
    }

    impl Transport for Scripted {
        async fn receive(&mut self, buf: &mut [u8]) -> io::Result<Received> {
            let (bytes, end) = self.script.pop_front().unwrap_or(Ok((Vec::new(), None)))?;
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(end.map_or(Received::Request(bytes.len()), Received::Ended))
        }

        fn device(&self) -> Arc<dyn ReplyDevice> {
            self.device.clone()
        }
    }

    /// Answers every request `ENOSYS` and counts them.
    #[derive(Default)]
    struct Count(Mutex<usize>);

    impl Fs for Count {
        fn handle(&self, _req: Request<'_>, replier: crate::Replier) -> crate::Handled {
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            let _ = replier.error(Errno::ENOSYS);
            crate::Handled::Done
        }
    }

    #[test]
    fn a_reader_whose_connection_is_aborted_ends_aborted_after_serving_what_it_read() {
        let device = Arc::new(Dev(None, Mutex::new(0)));
        let mut getattr = vec![0u8; 56];
        getattr[0] = 56;
        getattr[4] = 3;
        getattr[8] = 7;
        let script = [
            Ok((getattr, None)),
            Ok((Vec::new(), Some(ReaderEnd::Aborted))),
        ];
        let transport = Scripted {
            script: script.into_iter().collect(),
            device: device.clone(),
        };
        let (fs, tokens, stop) = (Count::default(), Tokens::new(), Stop::default());
        let every = Every(vec![device.clone()]);
        let mut buf = vec![0u8; 8192];
        let end =
            futures::executor::block_on(serve(transport, &fs, &tokens, &stop, &every, &mut buf));
        assert_eq!(end.unwrap(), ReaderEnd::Aborted);
        assert_eq!(
            *fs.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            1
        );
        assert_eq!(
            sends(&device),
            1,
            "the request read before the abort was answered"
        );
    }

    #[test]
    fn a_read_error_of_the_transport_ends_the_reader_with_it() {
        let device = Arc::new(Dev(None, Mutex::new(0)));
        let script = [Err(io::Error::from_raw_os_error(5))];
        let transport = Scripted {
            script: script.into_iter().collect(),
            device,
        };
        let (fs, tokens, stop) = (Count::default(), Tokens::new(), Stop::default());
        let every = Every(Vec::new());
        let mut buf = vec![0u8; 8192];
        let end =
            futures::executor::block_on(serve(transport, &fs, &tokens, &stop, &every, &mut buf));
        assert_eq!(end.unwrap_err().raw_os_error(), Some(5));
    }

    #[test]
    fn every_tries_each_device_until_one_takes_the_reply() {
        let refuses = Arc::new(Dev(Some(2), Mutex::new(0)));
        let takes = Arc::new(Dev(None, Mutex::new(0)));
        let after = Arc::new(Dev(None, Mutex::new(0)));
        let every = Every(vec![refuses.clone(), takes.clone(), after.clone()]);
        every.send(&[b"x"]).unwrap();
        assert_eq!((sends(&refuses), sends(&takes), sends(&after)), (1, 1, 0));
    }

    #[test]
    fn every_fails_with_the_last_refusal_when_no_device_takes_it() {
        let first = Arc::new(Dev(Some(2), Mutex::new(0)));
        let second = Arc::new(Dev(Some(19), Mutex::new(0)));
        let every = Every(vec![first, second]);
        assert_eq!(every.send(&[b"x"]).unwrap_err().raw_os_error(), Some(19));
        let none = Every(Vec::new()).send(&[b"x"]).unwrap_err();
        assert_eq!(none.to_string(), "a session with no device");
    }

    #[test]
    fn a_failed_start_stops_the_readers_and_names_the_reader() {
        let stop = Stop::default();
        let err = stop.failed_start(2, io::Error::from_raw_os_error(11));
        assert!(stop.stopped.load(Ordering::SeqCst));
        assert!(
            err.to_string()
                .starts_with("starting FUSE reader 2 failed: "),
            "{err}"
        );
    }
}
