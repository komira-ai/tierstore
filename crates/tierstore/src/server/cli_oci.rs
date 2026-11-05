//! The OCI front's settings in the start of a node: [`oci_setup`] and [`oci_setup_of`] check the
//! flags and files of an `oci` interface and make its TLS. Moved here from `server/cli.rs`
//! unchanged, which re-exports the names, so that the file stays under the limit of AGENTS.md R12.

use std::sync::Arc;
use std::time::Duration;

use super::ServerConfig;
use super::cli::{Cli, Error, OciSetup};
use super::config::OciFlags;
use super::front::{InvalidFlags, refuse};
use super::oci::{self, OciAuth, OciError, OciLimits, OciMetrics, OciMode, OciPrefix};
use super::oci_upstream_setup::upstream_of;

/// The default of `params.prefix`.
pub(super) const DEFAULT_OCI_PREFIX: &str = "oci/";

/// The default of `params.max_connections`.
const DEFAULT_OCI_CONNECTIONS: u32 = 64;

/// The default of `params.connection_lifetime`.
const DEFAULT_OCI_LIFETIME: Duration = Duration::from_secs(3600);

/// Checks the OCI front's flags and files, and makes its TLS, if an `oci` interface is given. A prefix,
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
        max_manifest_bytes: cli.oci_max_manifest_bytes,
        read_chunk_bytes: cli.oci_read_chunk_bytes,
        echo_bytes: cli.oci_echo_bytes,
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
    let lifetime = flags.connection_lifetime.unwrap_or(DEFAULT_OCI_LIFETIME);
    if lifetime.is_zero() {
        return Err(
            InvalidFlags("--oci-connection-lifetime must be longer than zero".into()).into(),
        );
    }
    let prefix = flags.prefix.as_deref().unwrap_or(DEFAULT_OCI_PREFIX);
    let prefix = OciPrefix::new(prefix).map_err(refuse)?;
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
        (Some(cert), Some(key)) => Some(oci::acceptor_for(cert, key, &listen).map_err(refuse)?),
        (None, None) => {
            let warnings = listen.iter().filter_map(|addr| oci::plain_warning(*addr));
            for warning in warnings.filter(|_| warn) {
                tracing::warn!("{warning}");
            }
            None
        }
        _ => {
            let why = "--oci-tls-cert and --oci-tls-key go together: give both, or neither";
            return Err(refuse(OciError::Missing(why.to_owned())));
        }
    };
    let defaults = oci::OciSizes::default();
    let sizes = oci::OciSizes {
        manifest_bytes: flags.max_manifest_bytes.unwrap_or(defaults.manifest_bytes),
        chunk_bytes: flags.read_chunk_bytes.unwrap_or(defaults.chunk_bytes),
        echo_bytes: flags.echo_bytes.map_or(defaults.echo_bytes, |bytes| {
            usize::try_from(bytes).unwrap_or(usize::MAX)
        }),
    };
    let upstream = flags.upstream.as_ref().map(upstream_of).transpose()?;
    let upstream = upstream.map(|upstream| upstream.with_body_limit(sizes.manifest_bytes));
    let mode = if acceptor.is_some() {
        OciMode::Https
    } else {
        OciMode::Http
    };
    let connections = flags.max_connections.unwrap_or(DEFAULT_OCI_CONNECTIONS);
    let limits = OciLimits {
        max_connections: connections as usize,
        handshake_timeout: server.oci_handshake_timeout,
        listen: server.listen,
        header_timeout: server.header_timeout,
        write_idle: server.body_idle_timeout,
        lifetime,
        drain_deadline: server.drain_deadline,
    };
    Ok(Some(OciSetup {
        listen,
        acceptor,
        prefix,
        metrics: Arc::new(OciMetrics::with_mode(mode)),
        limits,
        auth: flags.require_auth.then(|| OciAuth::from_server(server)),
        stage_read: flags.stage_read.unwrap_or(oci::DEFAULT_STAGE_READ),
        sizes,
        upstream,
    }))
}
