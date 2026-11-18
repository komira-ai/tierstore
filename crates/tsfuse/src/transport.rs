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
    /// The file system was unmounted (`ENODEV`), or the device's other end is gone.
    Unmounted,
    /// The connection was aborted (`ECONNABORTED`): through
    /// `/sys/fs/fuse/connections/<n>/abort`, or by the kernel.
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

/// One read of `device`: the request or the end, or `None` to wait for readiness again.
pub(crate) fn read_once(device: &DevFd, buf: &mut [u8]) -> Option<io::Result<Received>> {
    loop {
        let got = match device.read(buf) {
            // `/dev/fuse` never reads 0 bytes; a socket standing in for it does once its peer
            // is gone.
            Ok(0) => Some(Ok(Received::Ended(ReaderEnd::Unmounted))),
            Ok(n) => Some(Ok(Received::Request(n))),
            // Another clone took the request.
            Err(Sys::AGAIN) => None,
            // A signal, or a request interrupted while it was being read: read again.
            Err(Sys::INTR | Sys::NOENT) => continue,
            Err(Sys::NODEV) => Some(Ok(Received::Ended(ReaderEnd::Unmounted))),
            Err(Sys::CONNABORTED) => Some(Ok(Received::Ended(ReaderEnd::Aborted))),
            Err(e) => Some(Err(e.into())),
        };
        return got;
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
        if let Some(end) = route(&buf[..len], device, fs, tokens) {
            return Ok(end);
        }
    }
}

/// Routes one request; `Some` when the reader ends with it (DESTROY).
pub(crate) fn route(
    bytes: &[u8],
    device: Arc<dyn ReplyDevice>,
    fs: &dyn Fs,
    tokens: &Tokens,
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
                let _ = tokens.interrupt(arg.unique, unique, &*device);
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
    use super::*;

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
