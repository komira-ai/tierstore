//! HTTPS on the S3 front: the connection of a TLS listener, whose handshake runs as its first
//! bytes are read, and the source of such connections.
//!
//! The certificate and key (`server.tls_cert_file`, `server.tls_key_file`) are read once at start, by
//! [`oci::s3_acceptor`](super::oci::s3_acceptor). A listener of the front that has an acceptor
//! hands each connection to the server as a [`Handshaking`] stream, so that the server takes
//! connections as it does without TLS: the handshake is part of the first read of the
//! connection, and a client that does not finish it is held to the same limit as one that does
//! not send its request head (`ServerConfig::header_timeout`).

use std::future::Future as _;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tokio_rustls::{Accept, TlsAcceptor};

use super::Incoming;
use super::listen::Listeners;

/// Where the handshake of a connection is.
enum State {
    /// Under way.
    Accepting(Box<Accept<TcpStream>>),
    /// Done: the connection carries HTTP.
    Ready(Box<TlsStream<TcpStream>>),
    /// Failed or given up: the connection carries nothing.
    Closed,
}

/// A connection of a TLS listener: its handshake is driven by the first read or write, and the
/// stream is the TLS stream after it. A handshake that fails is reported on stderr (one line with
/// the peer's address) and ends the connection.
pub(crate) struct Handshaking {
    state: State,
    peer: SocketAddr,
}

impl Handshaking {
    /// The connection of `tcp`, from `peer`, with its handshake still to run.
    fn new(acceptor: &TlsAcceptor, tcp: TcpStream, peer: SocketAddr) -> Self {
        Self {
            state: State::Accepting(Box::new(acceptor.accept(tcp))),
            peer,
        }
    }

    /// Runs the handshake as far as it goes, and then gives the TLS stream.
    fn stream(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Pin<&mut TlsStream<TcpStream>>>> {
        if let State::Accepting(accept) = &mut self.state {
            match Pin::new(&mut **accept).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(tls)) => self.state = State::Ready(Box::new(tls)),
                Poll::Ready(Err(why)) => {
                    tracing::warn!("tierstore: s3: {}: TLS handshake: {why}", self.peer);
                    self.state = State::Closed;
                    return Poll::Ready(Err(why));
                }
            }
        }
        match &mut self.state {
            State::Ready(tls) => Poll::Ready(Ok(Pin::new(&mut **tls))),
            State::Accepting(_) | State::Closed => {
                Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
            }
        }
    }
}

impl AsyncRead for Handshaking {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.stream(cx) {
            Poll::Ready(Ok(tls)) => tls.poll_read(cx, buf),
            Poll::Ready(Err(why)) => Poll::Ready(Err(why)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Handshaking {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.stream(cx) {
            Poll::Ready(Ok(tls)) => tls.poll_write(cx, buf),
            Poll::Ready(Err(why)) => Poll::Ready(Err(why)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.stream(cx) {
            Poll::Ready(Ok(tls)) => tls.poll_flush(cx),
            Poll::Ready(Err(why)) => Poll::Ready(Err(why)),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Closes a connection whose handshake is done as TLS does, and drops one whose handshake is
    /// not: it has nothing to say to its peer.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let State::Ready(tls) = &mut this.state {
            return Pin::new(&mut **tls).poll_shutdown(cx);
        }
        this.state = State::Closed;
        Poll::Ready(Ok(()))
    }
}

/// The sockets of a front, as a source of TLS connections.
pub(crate) struct TlsListeners {
    listeners: Listeners,
    acceptor: TlsAcceptor,
}

impl TlsListeners {
    /// The connections of `listeners`, each to be served over TLS by `acceptor`.
    pub(crate) fn new(listeners: Listeners, acceptor: TlsAcceptor) -> Self {
        Self {
            listeners,
            acceptor,
        }
    }
}

impl Incoming for TlsListeners {
    type Stream = Handshaking;

    fn poll_incoming(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(Handshaking, SocketAddr)>> {
        let acceptor = &self.acceptor;
        let taken = self.listeners.poll_incoming(cx);
        taken.map_ok(|(tcp, peer)| (Handshaking::new(acceptor, tcp, peer), peer))
    }

    fn handshakes(&self) -> io::Result<usize> {
        self.listeners.handshakes()
    }
}

#[cfg(test)]
#[path = "s3_tls_tests.rs"]
mod tests;
