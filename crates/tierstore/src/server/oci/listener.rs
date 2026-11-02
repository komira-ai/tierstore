//! The OCI front's socket: HTTPS connections, or plain HTTP ones, bounded in number,
//! in the time a client has to send a request, in how long a client may leave an answer unread,
//! and in lifetime; and its drain, which is the S3 front's (see [`OciDrain`]).

use std::convert::Infallible;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use http_body_util::BodyExt as _;
use hyper::Request;
use hyper::body::Incoming as RequestBody;
use hyper::header::{CONNECTION, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore, oneshot, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use super::{OciError, OciFront, OciMode};
use crate::server::drain::{self, Drain};
use crate::server::idle::Watched;
use crate::server::linger::{Lingering, OnWire};
use crate::server::listen::{self, Listeners};
use crate::server::{ACCEPT_PAUSE, Incoming};

/// A connection's byte stream, whichever way it is carried: one code path serves HTTP over it.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// The bounds on the connections of a listener.
#[derive(Clone, Copy, Debug)]
pub struct OciLimits {
    /// The most connections open at once; the others wait in the kernel's backlog.
    pub max_connections: usize,
    /// How long a client has to finish the TLS handshake. Short, since a connection that never
    /// speaks holds a slot until it is closed. A plain listener has no handshake to time.
    pub handshake_timeout: Duration,
    /// How long a client has to send the head of a request, and so how long a connection may
    /// wait, idle, for its next request; and how long a lingering close in the drain waits for
    /// the client to close its end.
    pub header_timeout: Duration,
    /// How long a client may go without taking a byte of an answer before the connection is
    /// closed.
    pub write_idle: Duration,
    /// The longest a connection lives. A request being answered when it ends is finished first,
    /// for at most [`drain_deadline`](Self::drain_deadline) more.
    pub lifetime: Duration,
    /// How long, after a stop or at the end of a connection's lifetime, the answer being sent is
    /// waited for before the connection is cut.
    pub drain_deadline: Duration,
}

/// The bound sockets of the front, ready to serve it over TLS, or over plain HTTP. They share
/// one limit on connections and one drain.
pub struct OciListener {
    listeners: Listeners,
    /// `None` serves plain HTTP.
    acceptor: Option<TlsAcceptor>,
    front: Arc<OciFront>,
    limits: OciLimits,
    shared: Shared,
}

/// The start of the drain of an [`OciListener`], which the S3 front's drain begins at the same
/// time: from [`OciDrain::begin`] on, every answer carries `Connection: close`, and the listener
/// goes on accepting and serving. The stop given to [`OciListener::serve`] ends the settle (and
/// begins the drain, if it had not begun): the listener takes no new connection, closes each
/// connection that has no request on the wire with a lingering close (it shuts its end for
/// writing, so that the client reads a clean end of stream, and reads and drops what the client
/// still sends until the client closes its end, for [`OciLimits::header_timeout`] at most), and
/// closes each other connection in the same way once its answer is out. A connection whose client
/// writes after the end of the stream is counted in
/// [`OciMetrics::late_requests`](super::OciMetrics::late_requests).
#[derive(Clone)]
pub(crate) struct OciDrain(Arc<Drain>);

impl OciDrain {
    /// Begins the drain; calling it again does nothing more.
    pub(crate) fn begin(&self) {
        self.0.begin();
    }
}

impl OciListener {
    /// Binds `addr`, to serve HTTPS.
    ///
    /// # Errors
    /// [`OciError::Bind`] if the address cannot be bound.
    pub async fn bind(
        addr: SocketAddr,
        acceptor: TlsAcceptor,
        front: Arc<OciFront>,
        limits: OciLimits,
    ) -> Result<Self, OciError> {
        Self::bind_as(addr, Some(acceptor), front, limits).await
    }

    /// Binds `addr`, to serve plain HTTP; any address is bound (the start warns about one that
    /// is not loopback, see [`plain_warning`](super::plain_warning)).
    ///
    /// # Errors
    /// [`OciError::Bind`] if the address cannot be bound.
    pub async fn bind_plain(
        addr: SocketAddr,
        front: Arc<OciFront>,
        limits: OciLimits,
    ) -> Result<Self, OciError> {
        Self::bind_as(addr, None, front, limits).await
    }

    async fn bind_as(
        addr: SocketAddr,
        acceptor: Option<TlsAcceptor>,
        front: Arc<OciFront>,
        limits: OciLimits,
    ) -> Result<Self, OciError> {
        Self::bind_all(&[addr], acceptor, front, limits).await
    }

    /// Binds a socket for each of `addrs`, to serve HTTPS with `acceptor`, or plain HTTP
    /// without one: one front, one limit on connections and one stop for all of them. An
    /// IPv6 address is bound for IPv6 only, so that `0.0.0.0:P` and `[::]:P` can both be.
    ///
    /// # Errors
    /// [`OciError::Bind`] for the first address that cannot be bound.
    pub async fn bind_all(
        addrs: &[SocketAddr],
        acceptor: Option<TlsAcceptor>,
        front: Arc<OciFront>,
        limits: OciLimits,
    ) -> Result<Self, OciError> {
        let bound = listen::bind_all(addrs);
        let bound = bound.map_err(|(addr, source)| OciError::Bind { addr, source })?;
        let shared = Shared::new(drain_of(&front, limits), limits.max_connections);
        Ok(Self {
            listeners: Listeners::new(bound),
            acceptor,
            front,
            limits,
            shared,
        })
    }

    /// The handle that begins the listener's drain (see [`OciDrain`]).
    #[must_use]
    pub(crate) fn drain(&self) -> OciDrain {
        OciDrain(Arc::clone(&self.shared.drain))
    }

    /// How the listener carries its connections.
    #[must_use]
    pub fn mode(&self) -> OciMode {
        if self.acceptor.is_some() {
            OciMode::Https
        } else {
            OciMode::Http
        }
    }

    /// The address of the first socket, the one the front was first asked to bind.
    ///
    /// # Errors
    /// The socket's address cannot be read, or the front has no socket.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        let first = self.local_addrs()?.into_iter().next();
        first.ok_or_else(|| std::io::Error::other("the front has no socket"))
    }

    /// The address each socket is bound to, in the order they were asked for.
    ///
    /// # Errors
    /// A socket's address cannot be read.
    pub fn local_addrs(&self) -> std::io::Result<Vec<SocketAddr>> {
        self.listeners.local_addrs()
    }

    /// Serves connections until `stop` turns true, the end of the settle: then it takes no more,
    /// lets the connections that are open finish the request they are answering and closes them
    /// with a lingering close (its write half shut, then what the client sends read and dropped
    /// until it closes, for [`OciLimits::header_timeout`] at most), waits for all of that for
    /// [`OciLimits::drain_deadline`], and cuts the connections that are left, lingering ones
    /// included. Returns how many were cut.
    pub async fn serve(self, stop: watch::Receiver<bool>) -> usize {
        let Self {
            listeners,
            acceptor,
            front,
            limits,
            shared,
        } = self;
        accept_loop(listeners, acceptor, front, limits, stop, shared).await
    }

    /// The drain and the connection slots of the listener, for a test to watch.
    #[cfg(test)]
    pub(super) fn shared(&self) -> Shared {
        self.shared.clone()
    }
}

/// What the accept loop shares with its connections: the drain, the connection slots, and the
/// count of connections that hold one.
#[derive(Clone)]
pub(super) struct Shared {
    pub(super) drain: Arc<Drain>,
    /// One permit for each connection that may be open at once, lingering ones included. The
    /// accept loop takes one before each accept, and gives it to the connection it accepts.
    pub(super) slots: Arc<Semaphore>,
    /// The connections that hold a slot: given one by the accept loop and not ended, their
    /// lingering close included. Not the permit the accept loop holds while it waits to accept.
    pub(super) held: Arc<AtomicUsize>,
}

impl Shared {
    /// What a listener with `drain` and room for `max_connections` shares.
    pub(super) fn new(drain: Drain, max_connections: usize) -> Self {
        Self {
            drain: Arc::new(drain),
            slots: Arc::new(Semaphore::new(max_connections)),
            held: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// A connection's slot: its permit, counted in [`Shared::held`] until the connection's task ends.
struct Slot {
    _permit: Result<OwnedSemaphorePermit, AcquireError>,
    held: Arc<AtomicUsize>,
}

impl Slot {
    fn give(permit: Result<OwnedSemaphorePermit, AcquireError>, held: &Arc<AtomicUsize>) -> Self {
        held.fetch_add(1, Ordering::SeqCst);
        Self {
            _permit: permit,
            held: Arc::clone(held),
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The drain of a listener of `front`: its lingering closes wait for the header timeout, and it
/// counts the late requests into the front's counters.
pub(super) fn drain_of(front: &OciFront, limits: OciLimits) -> Drain {
    Drain::new(limits.header_timeout, front.metrics().late_counter())
}

/// [`OciListener::serve`] over any source of connections: the listener of a socket, and in the
/// tests one that fails.
pub(super) async fn accept_loop<I>(
    mut incoming: I,
    acceptor: Option<TlsAcceptor>,
    front: Arc<OciFront>,
    limits: OciLimits,
    mut stop: watch::Receiver<bool>,
    shared: Shared,
) -> usize
where
    I: Incoming<Stream = TcpStream> + Send,
{
    let Shared { drain, slots, held } = shared;
    let mut connections = JoinSet::new();
    loop {
        // A slot is taken before the next accept, so a flood waits in the backlog and not
        // in tasks.
        let slot = tokio::select! {
            slot = Arc::clone(&slots).acquire_owned() => slot,
            () = stopped(&mut stop) => break,
        };
        tokio::select! {
            accepted = poll_fn(|cx| incoming.poll_incoming(cx)) => match accepted {
                Ok((tcp, peer)) => {
                    let serving = Serving {
                        acceptor: acceptor.clone(),
                        front: Arc::clone(&front),
                        limits,
                        drain: Arc::clone(&drain),
                    };
                    let slot = Slot::give(slot, &held);
                    connections.spawn(serving.connection(tcp, peer, slot));
                }
                Err(error) => {
                    eprintln!("tierstore: oci: accept: {error}");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                }
            },
            Some(_) = connections.join_next() => {}
            () = stopped(&mut stop) => break,
        }
    }
    drop(incoming);
    drain.settle();
    // A connection's task runs its lingering close too, so that the deadline cuts it.
    let finished = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(limits.drain_deadline, finished)
        .await
        .is_ok()
    {
        return 0;
    }
    let cut = connections.len();
    connections.abort_all();
    cut
}

/// One plain connection served as the accept loop serves one, holding `permit`, for a test that
/// watches the permit with no accept loop to take it.
#[cfg(test)]
pub(super) async fn one_connection(
    front: Arc<OciFront>,
    limits: OciLimits,
    drain: Arc<Drain>,
    tcp: TcpStream,
    permit: OwnedSemaphorePermit,
) {
    let peer = tcp.peer_addr().expect("a peer");
    let serving = Serving {
        acceptor: None,
        front,
        limits,
        drain,
    };
    let held = Arc::new(AtomicUsize::new(0));
    serving
        .connection(tcp, peer, Slot::give(Ok(permit), &held))
        .await;
}

/// Resolves once `stop` is true, or its sender is gone.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

/// What a connection needs besides its socket.
struct Serving {
    acceptor: Option<TlsAcceptor>,
    front: Arc<OciFront>,
    limits: OciLimits,
    drain: Arc<Drain>,
}

impl Serving {
    /// The byte stream of the connection: `tcp` itself when the listener is plain, and else the
    /// stream of its TLS handshake, which has [`OciLimits::handshake_timeout`] to finish. `None`
    /// (and a line on stderr) if the handshake fails or takes too long.
    async fn open(&self, tcp: TcpStream, peer: SocketAddr) -> Option<Box<dyn Io>> {
        let Some(acceptor) = &self.acceptor else {
            return Some(Box::new(tcp));
        };
        let handshake = tokio::time::timeout(self.limits.handshake_timeout, acceptor.accept(tcp));
        match handshake.await {
            Ok(Ok(tls)) => Some(Box::new(tls)),
            Ok(Err(error)) => {
                eprintln!("tierstore: oci: {peer}: TLS handshake: {error}");
                None
            }
            Err(_) => {
                eprintln!("tierstore: oci: {peer}: TLS handshake took too long");
                None
            }
        }
    }

    /// One connection, until its client leaves, it breaks, its lifetime ends or the settle is
    /// over, and then its lingering close, if the drain gave it one; the slot is held throughout,
    /// so that a connection that lingers counts against [`OciLimits::max_connections`], and the
    /// listener cuts it at the drain deadline as it cuts any other.
    async fn connection(self, tcp: TcpStream, peer: SocketAddr, _slot: Slot) {
        let limits = self.limits;
        let Some(stream) = self.open(tcp, peer).await else {
            return;
        };
        let watched = Watched::new(stream, limits.write_idle, peer);
        let (handoff, lingered) = oneshot::channel();
        let (stream, wire) = Lingering::handing_over(watched, &self.drain, handoff);
        let (front, drain, on_wire) = (
            Arc::clone(&self.front),
            Arc::clone(&self.drain),
            Arc::clone(&wire),
        );
        let service = service_fn(move |request: Request<RequestBody>| {
            let (front, drain, wire) =
                (Arc::clone(&front), Arc::clone(&drain), Arc::clone(&on_wire));
            async move {
                let (method, uri, headers) = (request.method(), request.uri(), request.headers());
                let mut answer = front.handle(method, uri.path(), headers).await;
                if !drain.is_ready() {
                    let close = HeaderValue::from_static("close");
                    answer.headers_mut().insert(CONNECTION, close);
                }
                // The drain does not close the connection in the middle of a blob.
                let answer = answer.map(|body| OnWire::new(body, &wire).boxed_unsync());
                Ok::<_, Infallible>(answer)
            }
        });
        let settled = self.drain.settled.subscribe();
        // The HTTP connection, which owns the stream: when it ends, the stream is dropped, and a
        // stream that the drain shut hands its lingering close over.
        let http = async move {
            let serving = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(limits.header_timeout)
                .serve_connection(TokioIo::new(stream), service);
            tokio::pin!(serving);
            let lifetime = tokio::time::sleep(limits.lifetime);
            tokio::pin!(lifetime);
            let lifetime_ended = tokio::select! {
                // A connection that ends badly has nothing to tell the other side.
                _ = serving.as_mut() => return,
                () = &mut lifetime => true,
                // The settle is over, and the connection has no request on the wire, or has just
                // answered it: it closes, and its stream lingers (see `OciDrain`).
                () = drain::time_to_close(&wire, settled) => false,
            };
            // The request being answered is finished, and then the connection closes.
            serving.as_mut().graceful_shutdown();
            if lifetime_ended {
                // A client that takes the answer a few bytes at a time must not keep the slot past
                // the lifetime for ever. (After the settle, the listener cuts what is left.)
                let _ = tokio::time::timeout(limits.drain_deadline, serving).await;
            } else {
                let _ = serving.await;
            }
        };
        http.await;
        // Nothing is handed over when the drain did not shut the stream.
        if let Ok(lingered) = lingered.await {
            lingered.run().await;
        }
    }
}
