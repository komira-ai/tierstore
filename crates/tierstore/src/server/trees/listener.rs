//! The trees front's sockets: HTTP/2 connections, over TLS or plain (h2c with prior knowledge, as
//! gRPC clients speak it), bounded in number and in lifetime, and stopped by the drain.

use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use super::service::Served;
use crate::server::listen::{self, Listeners};
use crate::server::{Incoming, ListenTuning};

/// A connection's byte stream, whichever way it is carried.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// An address that cannot be bound.
#[derive(Debug, thiserror::Error)]
#[error("binding {addr}: {source}")]
pub(crate) struct BindError {
    /// The address.
    pub(crate) addr: SocketAddr,
    /// Why not.
    #[source]
    pub(crate) source: io::Error,
}

/// The bounds on the connections of a listener.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TreesLimits {
    /// The most connections open at once; the others wait in the kernel's backlog.
    pub(crate) max_connections: usize,
    /// How long a client has to finish the TLS handshake.
    pub(crate) handshake_timeout: Duration,
    /// The longest a connection lives; the call being answered when it ends is finished first.
    pub(crate) lifetime: Duration,
    /// How long, after a stop or at the end of a connection's lifetime, the calls in flight are
    /// waited for before the connection is cut.
    pub(crate) drain_deadline: Duration,
    /// The backlog of the sockets and the pause after a failed accept.
    pub(crate) listen: ListenTuning,
}

/// The bound sockets of the front, ready to serve it.
pub(crate) struct TreesListener {
    listeners: Listeners,
    /// `None` serves plain HTTP/2.
    acceptor: Option<TlsAcceptor>,
    served: Served,
    limits: TreesLimits,
}

impl TreesListener {
    /// Binds a socket for each of `addrs`: one front, one limit on connections and one stop for all
    /// of them. The sockets share their addresses with other processes (`SO_REUSEPORT`): the
    /// trees sockets are not handed to a successor in a live update (see `handoff`), so an
    /// instance started while another still serves binds beside it, and the one that stops closes
    /// its own.
    ///
    /// # Errors
    /// [`BindError`] for the first address that cannot be bound.
    pub(crate) async fn bind_all(
        addrs: &[SocketAddr],
        acceptor: Option<TlsAcceptor>,
        served: Served,
        limits: TreesLimits,
    ) -> Result<Self, BindError> {
        let mut bound = Vec::new();
        for addr in addrs {
            let socket = listen::bind_sharing(*addr, limits.listen.backlog, true);
            bound.push(socket.map_err(|source| BindError {
                addr: *addr,
                source,
            })?);
        }
        Ok(Self {
            listeners: Listeners::new(bound),
            acceptor,
            served,
            limits,
        })
    }

    /// The address each socket is bound to, in the order they were asked for.
    ///
    /// # Errors
    /// A socket's address cannot be read.
    pub(crate) fn local_addrs(&self) -> io::Result<Vec<SocketAddr>> {
        self.listeners.local_addrs()
    }

    /// Serves connections until `stop` turns true. Then it closes its sockets, asks each
    /// connection to finish the calls it is answering and close, waits for them for
    /// [`TreesLimits::drain_deadline`], and cuts the connections that are left. Returns how many
    /// were cut.
    pub(crate) async fn serve(self, stop: watch::Receiver<bool>) -> usize {
        let Self {
            listeners,
            acceptor,
            served,
            limits,
        } = self;
        accept_loop(listeners, acceptor, served, limits, stop).await
    }
}

/// [`TreesListener::serve`] over any source of connections: the sockets of a listener, and in the
/// tests one that fails.
pub(super) async fn accept_loop<I>(
    mut incoming: I,
    acceptor: Option<TlsAcceptor>,
    served: Served,
    limits: TreesLimits,
    mut stop: watch::Receiver<bool>,
) -> usize
where
    I: Incoming<Stream = TcpStream> + Send,
{
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    let (shutdown, shut) = watch::channel(false);
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
                        served: served.clone(),
                        limits,
                        shut: shut.clone(),
                    };
                    connections.spawn(serving.connection(tcp, peer, slot));
                }
                Err(error) => {
                    tracing::warn!("tierstore: trees: accept: {error}");
                    tokio::time::sleep(limits.listen.accept_pause).await;
                }
            },
            Some(_) = connections.join_next() => {}
            () = stopped(&mut stop) => break,
        }
    }
    drop(incoming);
    let _ = shutdown.send(true);
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

/// Resolves once `stop` is true, or its sender is gone.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

/// What a connection needs besides its socket.
struct Serving {
    acceptor: Option<TlsAcceptor>,
    served: Served,
    limits: TreesLimits,
    shut: watch::Receiver<bool>,
}

impl Serving {
    /// The byte stream of the connection: `tcp` itself when the listener is plain, and else the
    /// stream of its TLS handshake, which has [`TreesLimits::handshake_timeout`] to finish.
    async fn open(&self, tcp: TcpStream, peer: SocketAddr) -> Option<Box<dyn Io>> {
        let Some(acceptor) = &self.acceptor else {
            return Some(Box::new(tcp));
        };
        let handshake = tokio::time::timeout(self.limits.handshake_timeout, acceptor.accept(tcp));
        match handshake.await {
            Ok(Ok(tls)) => Some(Box::new(tls)),
            Ok(Err(error)) => {
                tracing::warn!("tierstore: trees: {peer}: TLS handshake: {error}");
                None
            }
            Err(_) => {
                tracing::warn!("tierstore: trees: {peer}: TLS handshake took too long");
                None
            }
        }
    }

    /// One connection, until its client leaves, it breaks, its lifetime ends or the stop comes;
    /// the slot is held throughout.
    async fn connection(
        mut self,
        tcp: TcpStream,
        peer: SocketAddr,
        _slot: Result<OwnedSemaphorePermit, AcquireError>,
    ) {
        let Some(stream) = self.open(tcp, peer).await else {
            return;
        };
        let served = self.served.clone();
        let service = service_fn(move |request| {
            let served = served.clone();
            async move { served.handle(request).await }
        });
        let serving = http2::Builder::new(TokioExecutor::new())
            .timer(TokioTimer::new())
            .serve_connection(TokioIo::new(stream), service);
        tokio::pin!(serving);
        let lifetime = tokio::time::sleep(self.limits.lifetime);
        tokio::pin!(lifetime);
        tokio::select! {
            // A connection that ends badly has nothing to tell the other side.
            _ = serving.as_mut() => return,
            () = &mut lifetime => {}
            () = stopped(&mut self.shut) => {}
        }
        // The calls being answered are finished, and then the connection closes.
        serving.as_mut().graceful_shutdown();
        let _ = tokio::time::timeout(self.limits.drain_deadline, serving).await;
    }
}
