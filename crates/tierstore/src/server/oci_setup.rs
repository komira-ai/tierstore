//! The OCI front's start-up: its flags and files checked, and its TLS, limits and counters made,
//! for a storage node, a build node and a combined node alike (see `oci_setup_of`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use super::ServerConfig;
use super::cli::{Cli, Error};
use super::config::OciFlags;
use super::config_oci::UpstreamTable;
use super::front::InvalidFlags;
use super::keys::read_private_text;
use super::listen::has_duplicate;
use super::oci::{
    self, OciAuth, OciError, OciLimits, OciMetrics, OciMode, OciPrefix, Upstream, UpstreamSettings,
};

/// The OCI front, as far as the flags and files go: the proxy it reads is not open yet.
pub(super) struct OciSetup {
    /// The addresses, which are at least one.
    pub(super) listen: Vec<SocketAddr>,
    /// `None` serves plain HTTP.
    pub(super) acceptor: Option<tokio_rustls::TlsAcceptor>,
    pub(super) prefix: OciPrefix,
    pub(super) metrics: Arc<OciMetrics>,
    pub(super) limits: OciLimits,
    /// The keys the front asks of a client, with `--oci-require-auth`; `None` serves anonymously.
    pub(super) auth: Option<OciAuth>,
    /// The most bytes of a stored chunk read in one call at the end of a push.
    pub(super) stage_read: u64,
    /// The registry a missing manifest or blob is pulled from; `None` pulls nothing.
    pub(super) upstream: Option<Upstream>,
}

/// The default of `--oci-prefix`.
pub(super) const DEFAULT_OCI_PREFIX: &str = "oci/";

/// The default of `--oci-max-connections`.
const DEFAULT_OCI_CONNECTIONS: u32 = 64;

/// The default of `--oci-connection-lifetime`.
const DEFAULT_OCI_LIFETIME: Duration = Duration::from_secs(3600);

/// The default of `oci.upstream.timeout`.
const DEFAULT_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// The default of `oci.upstream.max_redirects`.
const DEFAULT_UPSTREAM_REDIRECTS: u32 = 4;

/// How long a client has to finish the TLS handshake with the OCI front.
const OCI_HANDSHAKE: Duration = Duration::from_secs(10);

/// Checks the OCI front's flags and files, and makes its TLS, if `--oci-listen` is given. A prefix,
/// certificate or key that cannot work is an `InvalidFlags` (exit 2, as a restart cannot mend it);
/// a file that cannot be read is exit 1. `warn` says whether the warning for a plain-text listener
/// is printed; `--check-config` leaves it out, as it prints nothing when the file is fine.
pub(super) fn oci_setup(
    cli: &Cli,
    server: &ServerConfig,
    warn: bool,
) -> Result<Option<OciSetup>, Error> {
    let flags = OciFlags {
        listen: cli.oci_listen.clone(),
        tls_cert: cli.oci_tls_cert.clone(),
        tls_key: cli.oci_tls_key.clone(),
        prefix: cli.oci_prefix.clone(),
        max_connections: cli.oci_max_connections,
        connection_lifetime: cli.oci_connection_lifetime,
        require_auth: cli.oci_require_auth,
        stage_read: cli.oci_stage_read,
        upstream: cli.oci_upstream.clone(),
    };
    oci_setup_of(&flags, server, warn)
}

/// [`oci_setup`] for the values the flags or the `[oci]` table of a file make: the one place the
/// front's rules are applied, for a storage node and for a build node.
pub(super) fn oci_setup_of(
    flags: &OciFlags,
    server: &ServerConfig,
    warn: bool,
) -> Result<Option<OciSetup>, Error> {
    if flags.listen.is_empty() {
        return Ok(None);
    }
    let listen = flags.listen.clone();
    if has_duplicate(&listen) {
        return Err(InvalidFlags("--oci-listen is given the same address twice".into()).into());
    }
    let lifetime = flags.connection_lifetime.unwrap_or(DEFAULT_OCI_LIFETIME);
    if lifetime.is_zero() {
        return Err(
            InvalidFlags("--oci-connection-lifetime must be longer than zero".into()).into(),
        );
    }
    let prefix = flags.prefix.as_deref().unwrap_or(DEFAULT_OCI_PREFIX);
    let prefix = OciPrefix::new(prefix).map_err(refused)?;
    let plain = flags.tls_cert.is_none() && flags.tls_key.is_none();
    let in_the_clear = listen
        .iter()
        .find(|addr| oci::plain_warning(**addr).is_some());
    if let (true, true, Some(addr)) = (flags.require_auth, plain, in_the_clear) {
        let why = format!(
            "--oci-require-auth sends each client's secret as HTTP Basic, which plain HTTP at \
             {addr} would carry in the clear: give --oci-tls-cert and --oci-tls-key, or listen \
             on loopback"
        );
        return Err(InvalidFlags(why).into());
    }
    let acceptor = match (&flags.tls_cert, &flags.tls_key) {
        (Some(cert), Some(key)) => Some(oci::acceptor_for(cert, key, &listen).map_err(refused)?),
        (None, None) => {
            let warnings = listen.iter().filter_map(|addr| oci::plain_warning(*addr));
            for warning in warnings.filter(|_| warn) {
                eprintln!("{warning}");
            }
            None
        }
        _ => {
            let why = "--oci-tls-cert and --oci-tls-key go together: give both, or neither";
            return Err(refused(OciError::Missing(why.to_owned())));
        }
    };
    let upstream = flags.upstream.as_ref().map(upstream_of).transpose()?;
    let mode = if acceptor.is_some() {
        OciMode::Https
    } else {
        OciMode::Http
    };
    let connections = flags.max_connections.unwrap_or(DEFAULT_OCI_CONNECTIONS);
    let limits = OciLimits {
        max_connections: connections as usize,
        handshake_timeout: OCI_HANDSHAKE,
        header_timeout: server.header_timeout,
        write_idle: server.body_idle_timeout,
        lifetime,
        drain_deadline: server.drain_deadline,
    };
    Ok(Some(OciSetup {
        listen,
        acceptor,
        prefix: prefix.clone(),
        metrics: Arc::new(OciMetrics::with_mode(mode)),
        limits,
        auth: flags.require_auth.then(|| OciAuth::from_server(server)),
        stage_read: flags.stage_read.unwrap_or(oci::DEFAULT_STAGE_READ),
        upstream,
    }))
}

/// The error for a front that cannot start: flags and files a restart cannot mend are an
/// `InvalidFlags` (exit 2), the rest is what it is.
fn refused(why: OciError) -> Error {
    if why.is_invalid_flags() {
        InvalidFlags(why.to_string()).into()
    } else {
        why.into()
    }
}

/// The client for `[oci.upstream]`: its password and CA certificates are read here, once.
fn upstream_of(table: &UpstreamTable) -> Result<Upstream, Error> {
    let login = match (&table.username, &table.password_file) {
        (Some(user), Some(path)) => {
            let text =
                read_private_text("oci.upstream.password_file", path).map_err(InvalidFlags)?;
            let password = text.trim_end_matches(['\n', '\r']).to_owned();
            if password.is_empty() {
                let why = format!("oci.upstream.password_file {}: it is empty", path.display());
                return Err(InvalidFlags(why).into());
            }
            Some((user.clone(), password))
        }
        _ => None,
    };
    let ca_pem = match &table.ca_file {
        Some(path) => {
            let read = std::fs::read(path);
            let pem = read.map_err(|source| OciError::Read {
                flag: "oci.upstream.ca_file",
                path: path.clone(),
                source,
            });
            Some(pem?)
        }
        None => None,
    };
    let settings = UpstreamSettings {
        url: table.url.clone().unwrap_or_default(),
        login,
        ca_pem,
        timeout: table
            .timeout
            .map_or(DEFAULT_UPSTREAM_TIMEOUT, |timeout| timeout.0),
        max_redirects: table.max_redirects.unwrap_or(DEFAULT_UPSTREAM_REDIRECTS),
    };
    Upstream::new(settings).map_err(refused)
}
