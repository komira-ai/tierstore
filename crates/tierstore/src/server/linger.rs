//! The close of a connection during the drain: a lingering close, and the record of whether a
//! request is on the wire.
//!
//! A connection that the server closes with request bytes it has not read is reset by the
//! kernel, and a reset can destroy an answer that the client has not read yet. During the drain
//! the server therefore shuts only its write half (the client reads a clean end of stream) and
//! goes on reading, and drops what it reads, until the client closes its end or a bound passes.

use std::io::{self, IoSlice};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use hyper::body::{Body, Frame, SizeHint};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::runtime::Handle;
use tokio::sync::{Notify, oneshot};

use super::drain::Drain;

/// What a connection's task needs to know of its stream: whether a request is on the wire.
pub(super) struct Wire {
    /// Bytes of a request have been read and its answer has not been written and flushed in full.
    on: AtomicBool,
    /// Notified when the answer of the request on the wire is flushed.
    answered: Notify,
    /// The streamed bodies of the request on the wire that are not over (see [`WireHold`]): the
    /// answer is not complete while one is.
    streams: AtomicUsize,
    /// A write has been taken since the request on the wire was read.
    wrote: AtomicBool,
    /// Bytes have been written and not flushed.
    dirty: AtomicBool,
    drain: Arc<Drain>,
}

/// A streamed answer's claim on the wire: while it is held, a flush does not end the request on
/// the wire, since hyper flushes after each frame of a body. It is dropped with the body, which
/// hyper drops when the body has ended, has failed, or the connection has gone, and then the
/// request is off the wire if every byte it wrote has been flushed (otherwise the flush that
/// follows ends it).
pub(super) struct WireHold(Arc<Wire>);

impl Drop for WireHold {
    fn drop(&mut self) {
        let wire = &self.0;
        let last = wire.streams.fetch_sub(1, Ordering::SeqCst) == 1;
        if last && wire.wrote.load(Ordering::SeqCst) && !wire.dirty.load(Ordering::SeqCst) {
            wire.clear();
        }
    }
}

impl Wire {
    /// The claim of a streamed answer on the wire of its connection.
    pub(super) fn hold(self: &Arc<Self>) -> WireHold {
        let _ = self.streams.fetch_add(1, Ordering::SeqCst);
        WireHold(Arc::clone(self))
    }

    /// The wire is clear of the request, if there was one.
    fn clear(&self) {
        self.wrote.store(false, Ordering::SeqCst);
        if self.on.swap(false, Ordering::SeqCst) {
            self.drain.unanswered.fetch_sub(1, Ordering::SeqCst);
            self.answered.notify_waiters();
            self.drain.idle.notify_waiters();
        }
    }

    /// Whether a request is on the wire.
    pub(super) fn is_on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    /// Resolves, once the wire is clear of the request that is on it; at once if none is.
    pub(super) async fn wait_clear(&self) {
        loop {
            let answered = self.answered.notified();
            tokio::pin!(answered);
            let _ = answered.as_mut().enable();
            if !self.is_on() {
                return;
            }
            answered.await;
        }
    }
}

/// A body that holds the wire of its connection until it is dropped (see [`WireHold`]): the
/// answer of the OCI front, whose bodies are streamed. A body that hyper sends as one frame holds
/// the wire for no longer than its flush would.
pub(super) struct OnWire<B> {
    body: B,
    _hold: WireHold,
}

impl<B> OnWire<B> {
    /// `body`, holding `wire`.
    pub(super) fn new(body: B, wire: &Arc<Wire>) -> Self {
        Self {
            body,
            _hold: wire.hold(),
        }
    }
}

impl<B: Body + Unpin> Body for OnWire<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<B::Data>, B::Error>>> {
        Pin::new(&mut self.get_mut().body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// One connection that the drain has closed and that waits, reading, for its peer to close.
struct Hold(Arc<Drain>);

impl Hold {
    fn enter(drain: &Arc<Drain>) -> Self {
        drain.lingering.fetch_add(1, Ordering::SeqCst);
        drain.idle.notify_waiters();
        Self(Arc::clone(drain))
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.lingering.fetch_sub(1, Ordering::SeqCst);
        self.0.idle.notify_waiters();
    }
}

/// A byte stream that, when it is shut down after the drain has begun, is read to the peer's end
/// (for [`Drain::linger_bound`] at most: `ServerConfig::header_timeout` on the S3 front,
/// `OciLimits::header_timeout` on the OCI front) instead of being closed at once. It also keeps
/// the [`Wire`] of the connection, and the count of the front's connections that have a request
/// on the wire.
pub(super) struct Lingering<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> {
    inner: Option<T>,
    drain: Arc<Drain>,
    wire: Arc<Wire>,
    /// Set when the shutdown begins during the drain.
    hold: Option<Hold>,
    /// Where the lingering close goes when the stream is dropped: `None` spawns it on a task of
    /// its own (the S3 front), `Some` hands it to the connection's task, which runs it and so
    /// keeps the connection's slot and its place in the listener's shutdown (the OCI front).
    handoff: Option<oneshot::Sender<Lingered<T>>>,
}

/// A stream whose lingering close is to be run: [`Lingered::run`].
pub(super) struct Lingered<T> {
    io: T,
    hold: Hold,
}

impl<T: AsyncRead + Unpin> Lingered<T> {
    /// Reads the stream to the peer's end (see [`linger`]).
    pub(super) async fn run(self) {
        linger(self.io, self.hold).await;
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Lingering<T> {
    /// `inner`, and the wire that it keeps, for a front that drains as `drain` says.
    pub(super) fn new(inner: T, drain: &Arc<Drain>) -> (Self, Arc<Wire>) {
        let wire = Arc::new(Wire {
            on: AtomicBool::new(false),
            answered: Notify::new(),
            streams: AtomicUsize::new(0),
            wrote: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            drain: Arc::clone(drain),
        });
        let stream = Self {
            inner: Some(inner),
            drain: Arc::clone(drain),
            wire: Arc::clone(&wire),
            hold: None,
            handoff: None,
        };
        (stream, wire)
    }

    /// [`Lingering::new`], whose lingering close, if the drain gives it one, is handed to
    /// `handoff` when the stream is dropped, for the connection's task to run, instead of being
    /// spawned.
    pub(super) fn handing_over(
        inner: T,
        drain: &Arc<Drain>,
        handoff: oneshot::Sender<Lingered<T>>,
    ) -> (Self, Arc<Wire>) {
        let (mut stream, wire) = Self::new(inner, drain);
        stream.handoff = Some(handoff);
        (stream, wire)
    }

    fn io(&mut self) -> Pin<&mut T> {
        Pin::new(
            self.inner
                .as_mut()
                .expect("the stream is there until the drop"),
        )
    }

    /// Notes a write that was taken.
    fn wrote(&self, written: &Poll<io::Result<usize>>) {
        if matches!(written, Poll::Ready(Ok(_))) {
            self.wire.wrote.store(true, Ordering::SeqCst);
            self.wire.dirty.store(true, Ordering::SeqCst);
        }
    }

    /// A request has been read: it is on the wire until its answer is written in full.
    fn arrived(&mut self) {
        if !self.wire.on.swap(true, Ordering::SeqCst) {
            self.drain.unanswered.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> AsyncRead for Lingering<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = this.io().poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.arrived();
        }
        polled
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> AsyncWrite for Lingering<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let written = this.io().poll_write(cx, buf);
        this.wrote(&written);
        written
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let written = this.io().poll_write_vectored(cx, bufs);
        this.wrote(&written);
        written
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.as_ref().is_some_and(T::is_write_vectored)
    }

    /// A flush that is taken after a write has taken everything the connection had buffered, so
    /// the answer to the request on the wire is out.
    ///
    /// This holds for an answer that is one `Full` frame, which hyper queues whole: its flush
    /// stays pending until the last byte is written. A streamed answer is flushed after each
    /// frame, so it holds the wire ([`WireHold`], put on the body by `Server::handle_on` and by the
    /// OCI front's connection, see [`OnWire`]) and the
    /// mark is cleared by the flush after it has ended, or by the end of the body if the last
    /// bytes were flushed already.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let flushed = this.io().poll_flush(cx);
        if matches!(flushed, Poll::Ready(Ok(()))) {
            this.wire.dirty.store(false, Ordering::SeqCst);
            // Not while a streamed body is still being sent: hyper flushes after each frame.
            let streaming = this.wire.streams.load(Ordering::SeqCst) > 0;
            if this.wire.wrote.load(Ordering::SeqCst) && !streaming {
                this.wire.clear();
            }
        }
        flushed
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.drain.is_ready() {
            // Counted before the end of the stream is sent, so that a client that has read it
            // finds the connection held. Once, however many times the shutdown is polled.
            this.hold.get_or_insert_with(|| Hold::enter(&this.drain));
        }
        this.io().poll_shutdown(cx)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Drop for Lingering<T> {
    fn drop(&mut self) {
        self.wire.clear();
        let (inner, hold) = (self.inner.take(), self.hold.take());
        let Some(lingered) = inner.zip(hold).map(|(io, hold)| Lingered { io, hold }) else {
            return;
        };
        match self.handoff.take() {
            // A task that is gone has nothing to run it with: the stream is dropped.
            Some(handoff) => drop(handoff.send(lingered)),
            // Outside a runtime there is nothing to read with: the stream and the hold are dropped.
            None => drop(Handle::try_current().map(|handle| handle.spawn(lingered.run()))),
        }
    }
}

/// Reads `io` and drops what it reads until the peer closes its end or the bound passes. The
/// first bytes are a request that came too late to be answered: the server's end was already
/// shut. Counted once for the connection, not for each read. The count is not exact: the unread
/// body of a request that was answered early (a refusal sent before its body was read) also
/// arrives after the end of the stream and is counted, because the mark of the request on the
/// wire is clear once its answer is out.
async fn linger<T: AsyncRead + Unpin>(mut io: T, hold: Hold) {
    let drain = Arc::clone(&hold.0);
    let bound = tokio::time::sleep(drain.linger_bound());
    tokio::pin!(bound);
    let mut late = false;
    let mut piece = [0_u8; 1024];
    loop {
        tokio::select! {
            read = io.read(&mut piece) => match read {
                Ok(1..) => {
                    if late {
                        drain.idle.notify_waiters();
                    } else {
                        drain.count_late();
                    }
                    late = true;
                }
                _ => break,
            },
            () = &mut bound => break,
        }
    }
    drop(hold);
}
