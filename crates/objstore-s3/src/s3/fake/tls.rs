//! The service's socket, over plain HTTP or over HTTPS ([`FakeS3::start_tls`]), and a store that
//! reaches the service by a host name ([`FakeS3::store_resolving`]): requests to a host under that
//! name are taken in virtual-hosted style, the bucket in the host and the key the whole path.

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
    #[expect(
        clippy::expect_used,
        reason = "a test double: a port that cannot be bound is the failure the test reports, as documented under Panics of its callers"
    )]
    pub(super) async fn listen(tls: Option<TlsAcceptor>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("address");
        let scheme = if tls.is_some() { "https" } else { "http" };
        let state = Arc::new(Mutex::new(State::default()));
        let changed = Arc::new(Notify::new());
        let (shared, wake) = (Arc::clone(&state), Arc::clone(&changed));
        tokio::spawn(async move {
            loop {
                let accepted = listener.accept().await;
                let serving = accepted.map(|(sock, _)| {
                    connection(sock, tls.clone(), Arc::clone(&shared), Arc::clone(&wake))
                });
                serving.map(tokio::spawn).ok();
            }
        });
        Self {
            endpoint: format!("{scheme}://{addr}"),
            addr,
            state,
            changed,
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
) -> Option<()> {
    match tls {
        None => serve(sock, state, changed).await,
        Some(tls) => serve(tls.accept(sock).await.ok()?, state, changed).await,
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
