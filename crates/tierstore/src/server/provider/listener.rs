//! The socket of the provider: gRPC over HTTP/2 on a Unix socket, mode 0600 (see
//! [`socket`](super::socket)), bound only while the cache holds the lock of its directory, so that
//! of the two instances of a hand-over the one that serves the cache is the one that answers,
//! replacing the file the instance before it left, and removing its own file when it goes if the
//! name still leads to it. The agent asks again when it finds the socket closed.

use std::path::Path;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tierstore_trees_api::provider::v1::mount_provider_server::MountProviderServer;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tonic::codegen::Service as _;

use super::super::ACCEPT_PAUSE;
use super::super::local_cache::{LocalCache, Mode};
use super::Provider;
use super::service::ProviderFront;
use super::socket::{REBIND_PAUSE, SocketFile, bind_socket, pause_if};

/// A socket that is bound, and the connections it has accepted.
struct Listening {
    listener: UnixListener,
    connections: JoinSet<()>,
    /// Dropped after the listener and the connections: the file goes last.
    _file: SocketFile,
}

impl Listening {
    /// Takes the next connection and serves it on a task of its own, or reaps one that ended.
    async fn step(&mut self, service: &MountProviderServer<ProviderFront>) {
        tokio::select! {
            accepted = self.listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let _ = self.connections.spawn(connection(stream, service.clone()));
                }
                Err(why) => {
                    tracing::warn!("tierstore: the mount provider's socket cannot accept: {why}");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                }
            },
            Some(_) = self.connections.join_next() => {}
        }
    }
}

/// One connection, served until it closes.
async fn connection(stream: UnixStream, service: MountProviderServer<ProviderFront>) {
    let service = service_fn(move |request: hyper::Request<Incoming>| {
        let mut service = service.clone();
        async move { service.call(request).await }
    });
    let _ = http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

/// One step of `socket`, or never when there is none.
async fn step(socket: Option<&mut Listening>, service: &MountProviderServer<ProviderFront>) {
    match socket {
        Some(socket) => socket.step(service).await,
        None => std::future::pending().await,
    }
}

/// Binds the socket, saying on stderr why it could not.
fn bind(path: &Path) -> Option<Listening> {
    match bind_socket(path) {
        Ok((listener, file)) => {
            tracing::info!("tierstore: the mount provider is at {}", path.display());
            Some(Listening {
                listener,
                connections: JoinSet::new(),
                _file: file,
            })
        }
        Err(why) => {
            tracing::warn!(
                "tierstore: the mount provider's socket at {} cannot be bound: {why}",
                path.display()
            );
            None
        }
    }
}

/// Serves `provider` on the socket at `path` until `halt` is notified: bound while `cache` is
/// read-write, closed while it is not. At the halt the connections are closed and the reads of
/// files not begun are not made.
pub(in crate::server) async fn serve(
    provider: Provider,
    cache: &LocalCache,
    path: &Path,
    halt: Arc<Notify>,
) {
    let service = MountProviderServer::new(ProviderFront {
        provider: provider.clone(),
    });
    let mut modes = cache.watch_mode();
    let mut socket: Option<Listening> = None;
    loop {
        let writable = *modes.borrow_and_update() == Mode::ReadWrite;
        if !writable {
            socket = None;
        } else if socket.is_none() {
            socket = bind(path);
        }
        let rebind = writable && socket.is_none();
        tokio::select! {
            biased;
            () = halt.notified() => break,
            _ = modes.changed() => {}
            () = pause_if(rebind, REBIND_PAUSE) => {}
            () = step(socket.as_mut(), &service) => {}
        }
    }
    provider.halt();
}
