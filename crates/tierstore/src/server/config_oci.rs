//! The `[oci]` table of the configuration file, and the flags it stands for: the read-only OCI
//! front's table is the front, `listen` is needed in it, and the other keys are refused without
//! it, as the `--oci-*` flags are.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::config::{ConfigError, addresses, invalid, refuse, require, within};
use super::config_units::Dur;
use super::listen::Addrs;

/// The default of `--oci-prefix`.
pub(super) const DEFAULT_OCI_PREFIX: &str = "oci/";

/// The default of `--oci-max-connections`.
pub(super) const DEFAULT_OCI_CONNECTIONS: u32 = 64;

/// The default of `--oci-connection-lifetime`.
pub(super) const DEFAULT_OCI_LIFETIME: Duration = Duration::from_secs(3600);

/// How long a client has to finish the TLS handshake with the OCI front.
pub(super) const OCI_HANDSHAKE: Duration = Duration::from_secs(10);

/// `[oci.upstream]`: the registry the front pulls an object from when its store has none (see
/// `oci::upstream`). The password is in the file `password_file`, never in this one.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpstreamTable {
    /// The registry's address, `https://host[:port]` or `http://host[:port]`, with no path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The user name the registry is logged in to with; with `password_file`, or neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// The file that holds the password (or token) of `username`, readable by the user the
    /// server runs as and no one else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_file: Option<PathBuf>,
    /// PEM certificates the registry's certificate may be issued by, besides the system's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
    /// How long the upstream has for each step of a pull: to answer a request, and to send the
    /// next piece of a body (`30s` if left out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Dur>,
    /// The most redirects one request follows (4 if left out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_redirects: Option<u32>,
}

/// `[oci]`: the read-only OCI front. The table is the front: `listen` is needed in it, and the
/// other keys are refused without it, as the `--oci-*` flags are.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciTable {
    /// One address, or a list of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<Addrs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_lifetime: Option<Dur>,
    /// Whether clients must present an access key of the server (`--oci-require-auth`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_auth: Option<bool>,
    /// The most bytes of a stored chunk read in one call at the end of an upload
    /// (`--oci-stage-read`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_read: Option<u64>,
    /// The registry a miss is pulled from. It has no flag form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<UpstreamTable>,
}

/// The OCI front's table checked as its flags are: the values of the `--oci-*` flags.
#[derive(Clone, Debug)]
pub(super) struct OciFlags {
    /// The addresses; none when there is no front.
    pub listen: Vec<SocketAddr>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub prefix: Option<String>,
    pub max_connections: Option<u32>,
    pub connection_lifetime: Option<Duration>,
    /// Whether the front asks its clients for an access key of the server.
    pub require_auth: bool,
    pub stage_read: Option<u64>,
    /// The registry a miss is pulled from, checked by `upstream`; none pulls nothing.
    pub upstream: Option<UpstreamTable>,
}

/// The flags `[oci]` stands for: none when the table is left out. A table without `listen` is
/// refused (naming the key it has that needs it, if it has one), the certificate and the key go
/// together, and the count is at least one.
pub(super) fn oci(table: Option<OciTable>) -> Result<OciFlags, ConfigError> {
    let Some(table) = table else {
        return Ok(OciFlags {
            listen: Vec::new(),
            tls_cert: None,
            tls_key: None,
            prefix: None,
            max_connections: None,
            connection_lifetime: None,
            require_auth: false,
            stage_read: None,
            upstream: None,
        });
    };
    if table.listen.is_none() {
        let others = [
            ("tls_cert_file", table.tls_cert_file.is_some()),
            ("tls_key_file", table.tls_key_file.is_some()),
            ("prefix", table.prefix.is_some()),
            ("max_connections", table.max_connections.is_some()),
            ("connection_lifetime", table.connection_lifetime.is_some()),
            ("require_auth", table.require_auth.is_some()),
            ("stage_read", table.stage_read.is_some()),
            ("upstream", table.upstream.is_some()),
        ];
        refuse("oci", "it needs oci.listen", &others)?;
    }
    let listen = require("oci", "listen", table.listen)?;
    let listen = addresses("oci.listen", listen)?;
    match (&table.tls_cert_file, &table.tls_key_file) {
        (Some(_), None) => {
            let why = "it is needed with oci.tls_cert_file: give both, or neither";
            return Err(invalid("oci.tls_key_file", why));
        }
        (None, Some(_)) => {
            let why = "it is needed with oci.tls_key_file: give both, or neither";
            return Err(invalid("oci.tls_cert_file", why));
        }
        _ => {}
    }
    let max_connections = match table.max_connections {
        Some(count) => Some(within("oci", "max_connections", count, 1, u32::MAX)?),
        None => None,
    };
    let stage_read = match table.stage_read {
        Some(bytes) => Some(within("oci", "stage_read", bytes, 1, u64::MAX)?),
        None => None,
    };
    Ok(OciFlags {
        listen,
        tls_cert: table.tls_cert_file,
        tls_key: table.tls_key_file,
        prefix: table.prefix,
        max_connections,
        connection_lifetime: table.connection_lifetime.map(|dur| dur.0),
        require_auth: table.require_auth.unwrap_or(false),
        stage_read,
        upstream: table.upstream.map(upstream).transpose()?,
    })
}

/// The `[oci.upstream]` table, checked: it needs `url`, `username` and `password_file` go
/// together, and the timeout is longer than zero.
fn upstream(table: UpstreamTable) -> Result<UpstreamTable, ConfigError> {
    require("oci.upstream", "url", table.url.clone())?;
    match (&table.username, &table.password_file) {
        (Some(_), None) => {
            let why = "it is needed with oci.upstream.username: give both, or neither";
            return Err(invalid("oci.upstream.password_file", why));
        }
        (None, Some(_)) => {
            let why = "it is needed with oci.upstream.password_file: give both, or neither";
            return Err(invalid("oci.upstream.username", why));
        }
        _ => {}
    }
    if table.timeout.is_some_and(|timeout| timeout.0.is_zero()) {
        return Err(invalid(
            "oci.upstream.timeout",
            "it must be longer than zero",
        ));
    }
    Ok(table)
}

#[cfg(test)]
#[path = "config_upstream_tests.rs"]
mod tests;
