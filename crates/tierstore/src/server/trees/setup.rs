//! The trees interface in the start of a node: [`TreesFlags`], what a `trees` interface says, and
//! [`trees_setup_of`], which checks its TLS files and makes what the listener needs.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_rustls::TlsAcceptor;

use super::TreeCore;
use super::listener::{BindError, TreesLimits, TreesListener};
use super::service::{Served, TreesFront};
use crate::server::ServerConfig;
use crate::server::cli::Error;
use crate::server::front::refuse;
use crate::server::oci::{self, plain_warning};

/// The port of a `trees` interface that gives neither `port` nor `addresses`.
pub(crate) const DEFAULT_PORT: u16 = 9600;

/// The default of `params.max_connections`.
pub(crate) const DEFAULT_CONNECTIONS: u32 = 64;

/// The default of `params.connection_lifetime`.
pub(crate) const DEFAULT_LIFETIME: Duration = Duration::from_secs(3600);

/// The default of `params.max_request_bytes`: the design's `trees.max_request_bytes`, 256 MiB.
pub(crate) const DEFAULT_MAX_REQUEST_BYTES: u64 = 256 << 20;

/// The settings of the trees front, as the `trees` interfaces make them: the values the front
/// checks at start. Checked by `config_interface`: the TLS files go together, and the numbers are
/// in range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TreesFlags {
    /// The addresses; none when there is no interface.
    pub(crate) listen: Vec<SocketAddr>,
    pub(crate) tls_cert: Option<PathBuf>,
    pub(crate) tls_key: Option<PathBuf>,
    pub(crate) max_connections: Option<u32>,
    pub(crate) connection_lifetime: Option<Duration>,
    pub(crate) max_request_bytes: Option<u64>,
}

/// The trees front, as far as the flags and files go: the node's core is not made yet.
pub(crate) struct TreesSetup {
    /// The addresses, which are at least one.
    pub(crate) listen: Vec<SocketAddr>,
    /// `None` serves plain HTTP/2.
    pub(super) acceptor: Option<TlsAcceptor>,
    pub(super) limits: TreesLimits,
    /// The keys that sign a call.
    config: Arc<ServerConfig>,
    pub(super) max_request_bytes: usize,
    /// The node's tree store. None until the node has one: calls are answered `NotServing`.
    pub(crate) core: Option<Arc<dyn TreeCore>>,
}

/// Checks the TLS files of the `trees` interfaces and makes the front's settings; `None` when
/// there is no interface. A certificate or key that cannot work is an `InvalidFlags` (exit 2, as a
/// restart cannot mend it); a file that cannot be read is exit 1. `warn` says whether the warning
/// for a plain-text listener is printed; `--check-config` leaves it out.
///
/// # Errors
/// A certificate or key that is refused (see `trees_acceptor`).
pub(crate) fn trees_setup_of(
    flags: &TreesFlags,
    server: &ServerConfig,
    warn: bool,
) -> Result<Option<TreesSetup>, Error> {
    if flags.listen.is_empty() {
        return Ok(None);
    }
    let listen = flags.listen.clone();
    let acceptor = match flags.tls_cert.as_ref().zip(flags.tls_key.as_ref()) {
        Some((cert, key)) => Some(oci::trees_acceptor(cert, key, &listen).map_err(refuse)?),
        None => {
            let in_the_clear = listen.iter().filter(|addr| plain_warning(**addr).is_some());
            for addr in in_the_clear.filter(|_| warn) {
                tracing::warn!(
                    "tierstore: warning: the trees interface at {addr} serves plain HTTP/2 on a \
                     non-loopback address; give params tls_cert_file and tls_key_file to serve TLS"
                );
            }
            None
        }
    };
    let limits = TreesLimits {
        max_connections: flags.max_connections.unwrap_or(DEFAULT_CONNECTIONS) as usize,
        handshake_timeout: server.oci_handshake_timeout,
        lifetime: flags.connection_lifetime.unwrap_or(DEFAULT_LIFETIME),
        drain_deadline: server.drain_deadline,
        listen: server.listen,
    };
    let max = flags.max_request_bytes.unwrap_or(DEFAULT_MAX_REQUEST_BYTES);
    Ok(Some(TreesSetup {
        listen,
        acceptor,
        limits,
        config: Arc::new(server.clone()),
        max_request_bytes: usize::try_from(max).unwrap_or(usize::MAX),
        core: None,
    }))
}

impl TreesSetup {
    /// Binds the sockets.
    ///
    /// # Errors
    /// An address that cannot be bound.
    pub(crate) async fn bind(self) -> Result<TreesListener, BindError> {
        let front = TreesFront::new(self.core);
        let served = Served::new(self.config, front, self.max_request_bytes);
        TreesListener::bind_all(&self.listen, self.acceptor, served, self.limits).await
    }
}
