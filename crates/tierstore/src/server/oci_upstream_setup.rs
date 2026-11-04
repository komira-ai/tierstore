//! The client of `params.upstream` of an `oci` interface, made at the start: its password and CA
//! certificates are read here, once.

use std::time::Duration;

use super::cli::Error;
use super::config_interface::UpstreamTable;
use super::front::{InvalidFlags, refuse};
use super::keys::read_private_text;
use super::oci::{OciError, Upstream, UpstreamSettings};

/// The default of `params.upstream.timeout`.
const DEFAULT_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// The default of `params.upstream.max_redirects`.
const DEFAULT_UPSTREAM_REDIRECTS: u32 = 4;

/// The client for `params.upstream`: a password file that others can read, that is missing or
/// empty is an `InvalidFlags` (exit 2, as a restart cannot mend it), and a CA file that cannot be
/// read is exit 1.
pub(super) fn upstream_of(table: &UpstreamTable) -> Result<Upstream, Error> {
    let login = match (&table.username, &table.password_file) {
        (Some(user), Some(path)) => {
            let text =
                read_private_text("params.upstream.password_file", path).map_err(InvalidFlags)?;
            let password = text.trim_end_matches(['\n', '\r']).to_owned();
            if password.is_empty() {
                let why = format!(
                    "params.upstream.password_file {}: it is empty",
                    path.display()
                );
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
                flag: "params.upstream.ca_file",
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
    Upstream::new(settings).map_err(refuse)
}
