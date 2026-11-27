//! The service's socket, over plain HTTP or over HTTPS ([`FakeS3::start_tls`]), and a store that
//! reaches the service by a host name ([`FakeS3::store_resolving`]): requests to a host under that
//! name are taken in virtual-hosted style, the bucket in the host and the key the whole path.
//! More fronts of one service, each a socket of its own over the same objects and uploads
//! ([`FakeS3::start_front`]).

use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_rustls::TlsAcceptor;

use super::super::endpoint::system_roots;
use super::super::{S3Config, S3ConfigError, S3Store, S3Tuning};
use super::escape::unescape;
use super::{FakeS3, Request, State, serve};

impl FakeS3 {
    /// Starts the service on a free loopback port, on the current runtime, serving HTTPS with the
    /// certificate and key of `server`: its endpoint is `https://127.0.0.1:<port>`. A client that
    /// refuses the certificate reaches nothing.
    ///
    /// # Panics
    /// No loopback port could be bound.
    pub async fn start_tls(server: Arc<rustls::ServerConfig>) -> Self {
        Self::listen(Some(TlsAcceptor::from(server))).await
    }

    /// Starts the service on a free loopback port, serving plain HTTP, or HTTPS with `tls`.
    pub(super) async fn listen(tls: Option<TlsAcceptor>) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        Self::listen_on(tls, state, Arc::new(Notify::new()), 0).await
    }

    /// Starts another front of this service, over plain HTTP on a free loopback port of its own,
    /// on the current runtime: a request to it reads and changes the same objects and uploads as
    /// one to this front, under the same settings and failures ([`FakeS3::fail`] fails the next
    /// such request whichever front takes it), and [`FakeS3::requests`] of each front gives the
    /// requests that front took. For a test of a store reached through several endpoints that
    /// are front doors of one store.
    ///
    /// # Panics
    /// No loopback port could be bound.
    pub async fn start_front(&self) -> Self {
        let front = {
            let mut state = self.lock();
            state.fronts += 1;
            state.fronts
        };
        let (state, changed) = (Arc::clone(&self.state), Arc::clone(&self.changed));
        Self::listen_on(None, state, changed, front).await
    }

    /// Serves `state` as front `front` on a free loopback port, plain HTTP or HTTPS with `tls`.
    #[expect(
        clippy::expect_used,
        reason = "a test double: a port that cannot be bound is the failure the test reports, as documented under Panics of its callers"
    )]
    async fn listen_on(
        tls: Option<TlsAcceptor>,
        state: Arc<Mutex<State>>,
        changed: Arc<Notify>,
        front: usize,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("address");
        let scheme = if tls.is_some() { "https" } else { "http" };
        let (shared, wake) = (Arc::clone(&state), Arc::clone(&changed));
        tokio::spawn(async move {
            loop {
                let accepted = listener.accept().await;
                let serving = accepted.map(|(sock, _)| {
                    let (shared, wake) = (Arc::clone(&shared), Arc::clone(&wake));
                    connection(sock, tls.clone(), shared, wake, front)
                });
                serving.map(tokio::spawn).ok();
            }
        });
        Self {
            endpoint: format!("{scheme}://{addr}"),
            addr,
            state,
            changed,
            front,
        }
    }

    /// A store with `config`, whose requests to the host `name`, and to `<bucket>.<name>`, reach
    /// this service with no lookup of the name. `config.endpoint` names the host `name` and this
    /// service's port (`https://<name>:<port>`). From now on the service takes a request to a host
    /// under `name` in virtual-hosted style.
    ///
    /// # Errors
    /// As [`S3Store::new`].
    pub fn store_resolving(&self, config: S3Config, name: &str) -> Result<S3Store, S3ConfigError> {
        self.lock().hosted_under = Some(name.to_owned());
        let hosted = format!("{}.{name}", config.bucket);
        let addr = self.addr;
        let resolving =
            |client: reqwest::ClientBuilder| client.resolve(name, addr).resolve(&hosted, addr);
        S3Store::build(config, S3Tuning::default(), &[], resolving, system_roots)
    }
}

/// Serves one connection, over TLS with `tls` (a client that refuses the certificate ends the
/// handshake, and nothing is served), or plain.
async fn connection(
    sock: TcpStream,
    tls: Option<TlsAcceptor>,
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    front: usize,
) -> Option<()> {
    match tls {
        None => serve(sock, state, changed, front).await,
        Some(tls) => serve(tls.accept(sock).await.ok()?, state, changed, front).await,
    }
}

/// The key `request` names: in path style the path after the bucket, in virtual-hosted style (a
/// host under [`State::hosted_under`]) the whole path.
pub(super) fn key_of(state: &State, request: &Request) -> String {
    let host = request.headers.get("host").map_or("", String::as_str);
    let name = host.rsplit_once(':').map_or(host, |(name, _port)| name);
    let hosted = (state.hosted_under.as_ref()).is_some_and(|under| {
        name.strip_suffix(under.as_str())
            .is_some_and(|b| b.ends_with('.'))
    });
    let path = request.path.trim_start_matches('/');
    if hosted {
        return unescape(path);
    }
    // `/bucket/key`: the key is everything after the bucket.
    unescape(path.split_once('/').map_or("", |(_, key)| key))
}
