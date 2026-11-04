//! The `[oci]` table checked as the `--oci-*` flags are (see `config`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use super::config::{ConfigError, OciTable, addresses, invalid, refuse, require, within};

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
    Ok(OciFlags {
        listen,
        tls_cert: table.tls_cert_file,
        tls_key: table.tls_key_file,
        prefix: table.prefix,
        max_connections,
        connection_lifetime: table.connection_lifetime.map(|dur| dur.0),
        require_auth: table.require_auth.unwrap_or(false),
    })
}
