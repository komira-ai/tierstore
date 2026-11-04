//! `[[interface]]`: the fronts a `tierstore` serves, one entry each.
//!
//! ```toml
//! [[interface]]
//! type = "s3"
//! params = { port = 9300 }
//!
//! [[interface]]
//! type = "oci"
//! params = { addresses = ["127.0.0.1:5000"], prefix = "oci/" }
//! ```
//!
//! An interface is a front over the same proxy. The types are `s3` (the S3 API, `/healthz`,
//! `/readyz`, `/usagez` and `/metricz`) and `oci` (the read-only registry front); `fs` comes
//! later and is refused today. Every entry says where it listens with one of two params:
//! `port`, the wildcard of IPv4 (`0.0.0.0:<port>`), or `addresses`, a socket address or a list
//! of them. Both, or neither, are refused, and so is an address that two interfaces (or one
//! interface twice) listen on, but for a port of 0, which asks the system for a port.
//!
//! The entries of a type are served together: one server, one drain, and for `oci` one limit
//! on connections, per type, on every address of the entries of that type. So the `s3` entries take no other
//! param, and the `oci` entries, which also take `tls_cert_file`, `tls_key_file`, `prefix`,
//! `max_connections` and `connection_lifetime`, must agree on those five (one registry front has
//! one certificate and one prefix); one that differs from the first is refused. An `s3`
//! interface has no TLS params yet. At least one `s3` interface is needed.
//!
//! The keys of the release before stay readable: `server.listen` is an `s3` interface at those
//! addresses and the `[oci]` table (`listen` and the same five params, with `listen` needed) is an
//! `oci` interface, so a file written for that release starts as it did ([`with_legacy`]). The
//! entries they make follow the ones the file lists, and every rule above holds for them too.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use serde::Deserialize;

use super::config::{ConfigError, OciFlags, invalid, refuse, require, within};
use super::config_units::Dur;
use super::listen::Addrs;

/// One `[[interface]]` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InterfaceTable {
    /// `s3` or `oci`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub params: Params,
}

/// The `params` of an interface; which of them a type takes is checked by [`interfaces`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Params {
    pub port: Option<u16>,
    pub addresses: Option<Addrs>,
    pub tls_cert_file: Option<PathBuf>,
    pub tls_key_file: Option<PathBuf>,
    pub prefix: Option<String>,
    pub max_connections: Option<u32>,
    pub connection_lifetime: Option<Dur>,
}

/// `[oci]`, the table of the release before: the registry front, which is an `oci` interface.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciTable {
    /// One address, or a list of them; needed in the table.
    pub listen: Option<Addrs>,
    pub tls_cert_file: Option<PathBuf>,
    pub tls_key_file: Option<PathBuf>,
    pub prefix: Option<String>,
    pub max_connections: Option<u32>,
    pub connection_lifetime: Option<Dur>,
}

/// What the interfaces of a file stand for.
#[derive(Debug)]
pub(super) struct Interfaces {
    /// The addresses of the S3 front, which are at least one.
    pub s3: Vec<SocketAddr>,
    /// The OCI front: no address when there is no `oci` interface.
    pub oci: OciFlags,
}

/// Where an interface listens: the wildcard of IPv4 at its `port`, or its `addresses`.
fn addresses_of(at: &str, params: &Params) -> Result<Vec<SocketAddr>, ConfigError> {
    let place = format!("{at}.params");
    match (params.port, &params.addresses) {
        (Some(_), Some(_)) => Err(invalid(&place, "give port or addresses, not both")),
        (None, None) => Err(invalid(&place, "it needs port or addresses")),
        (Some(port), None) => {
            let port = within(&place, "port", port, 1, u16::MAX)?;
            Ok(vec![SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port)])
        }
        (None, Some(list)) if list.0.is_empty() => Err(invalid(
            &format!("{place}.addresses"),
            "it needs at least one address",
        )),
        (None, Some(list)) => Ok(list.0.clone()),
    }
}

/// Refuses the params of `params` that an `s3` interface does not take.
fn check_s3(at: &str, params: &Params) -> Result<(), ConfigError> {
    let tls = [
        ("tls_cert_file", params.tls_cert_file.is_some()),
        ("tls_key_file", params.tls_key_file.is_some()),
    ];
    let place = format!("{at}.params");
    let why = "an s3 interface has no TLS yet";
    super::config::refuse(&place, why, &tls)?;
    let others = [
        ("prefix", params.prefix.is_some()),
        ("max_connections", params.max_connections.is_some()),
        ("connection_lifetime", params.connection_lifetime.is_some()),
    ];
    super::config::refuse(&place, "not for type \"s3\"", &others)
}

/// The settings of the front that an `oci` interface says, with `listen` its addresses.
fn oci_of(at: &str, params: &Params, listen: Vec<SocketAddr>) -> Result<OciFlags, ConfigError> {
    let place = format!("{at}.params");
    match (&params.tls_cert_file, &params.tls_key_file) {
        (Some(_), None) => {
            let why = "it is needed with tls_cert_file: give both, or neither";
            return Err(invalid(&format!("{place}.tls_key_file"), why));
        }
        (None, Some(_)) => {
            let why = "it is needed with tls_key_file: give both, or neither";
            return Err(invalid(&format!("{place}.tls_cert_file"), why));
        }
        _ => {}
    }
    let max_connections = match params.max_connections {
        Some(count) => Some(within(&place, "max_connections", count, 1, u32::MAX)?),
        None => None,
    };
    Ok(OciFlags {
        listen,
        tls_cert: params.tls_cert_file.clone(),
        tls_key: params.tls_key_file.clone(),
        prefix: params.prefix.clone(),
        max_connections,
        connection_lifetime: params.connection_lifetime.map(|dur| dur.0),
    })
}

/// The first param of `next` that is not that of `first`, by name.
fn differing(first: &OciFlags, next: &OciFlags) -> Option<&'static str> {
    let same = [
        ("tls_cert_file", first.tls_cert == next.tls_cert),
        ("tls_key_file", first.tls_key == next.tls_key),
        ("prefix", first.prefix == next.prefix),
        (
            "max_connections",
            first.max_connections == next.max_connections,
        ),
        (
            "connection_lifetime",
            first.connection_lifetime == next.connection_lifetime,
        ),
    ];
    same.iter().find(|(_, same)| !same).map(|(name, _)| *name)
}

/// The entries of a file with the ones its `server.listen` and `[oci]` stand for after them.
///
/// # Errors
/// An `[oci]` table without `listen`, naming the key that needs it if the table has one.
pub(super) fn with_legacy(
    mut tables: Vec<InterfaceTable>,
    listen: Option<Addrs>,
    oci: Option<OciTable>,
) -> Result<Vec<InterfaceTable>, ConfigError> {
    if let Some(addresses) = listen {
        tables.push(InterfaceTable {
            kind: "s3".to_owned(),
            params: Params {
                addresses: Some(addresses),
                ..Params::default()
            },
        });
    }
    if let Some(table) = oci {
        let others = [
            ("tls_cert_file", table.tls_cert_file.is_some()),
            ("tls_key_file", table.tls_key_file.is_some()),
            ("prefix", table.prefix.is_some()),
            ("max_connections", table.max_connections.is_some()),
            ("connection_lifetime", table.connection_lifetime.is_some()),
        ];
        if table.listen.is_none() {
            refuse("oci", "it needs oci.listen", &others)?;
        }
        let addresses = require("oci", "listen", table.listen)?;
        tables.push(InterfaceTable {
            kind: "oci".to_owned(),
            params: Params {
                addresses: Some(addresses),
                tls_cert_file: table.tls_cert_file,
                tls_key_file: table.tls_key_file,
                prefix: table.prefix,
                max_connections: table.max_connections,
                connection_lifetime: table.connection_lifetime,
                ..Params::default()
            },
        });
    }
    Ok(tables)
}

/// The interfaces the entries of a file stand for.
///
/// # Errors
/// A [`ConfigError`] that names the entry by its place in the list (`interface[1]`): a type that
/// is not `s3` or `oci`, a missing or doubled place to listen, an address in use by another
/// entry, a param the type does not take, and `oci` entries that disagree.
pub(super) fn interfaces(tables: &[InterfaceTable]) -> Result<Interfaces, ConfigError> {
    let mut s3 = Vec::new();
    let mut oci: Option<(usize, OciFlags)> = None;
    let mut taken: Vec<(SocketAddr, usize)> = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let at = format!("interface[{index}]");
        match table.kind.as_str() {
            "s3" | "oci" => {}
            "fs" => {
                let why = "\"fs\" is not served yet; the types are \"s3\" and \"oci\"";
                return Err(invalid(&format!("{at}.type"), why));
            }
            _ => {
                let why = "not an interface type; the types are \"s3\" and \"oci\"";
                return Err(invalid(&format!("{at}.type"), why));
            }
        }
        let listen = addresses_of(&at, &table.params)?;
        for addr in listen.iter().filter(|addr| addr.port() != 0) {
            // A port of 0 asks the system for one: two of them are two ports.
            if let Some((_, other)) = taken.iter().find(|(used, _)| used == addr) {
                let why = if *other == index {
                    format!("the address {addr} is listed twice")
                } else {
                    format!("the address {addr} is already used by interface[{other}]")
                };
                return Err(invalid(&format!("{at}.params"), why));
            }
            taken.push((*addr, index));
        }
        if table.kind == "s3" {
            check_s3(&at, &table.params)?;
            s3.extend(listen);
            continue;
        }
        let this = oci_of(&at, &table.params, listen)?;
        match &mut oci {
            None => oci = Some((index, this)),
            Some((first_at, first)) => {
                if let Some(name) = differing(first, &this) {
                    let why = format!(
                        "it differs from interface[{first_at}]: one registry front serves every oci interface, so they share tls_cert_file, tls_key_file, prefix, max_connections and connection_lifetime"
                    );
                    return Err(invalid(&format!("{at}.params.{name}"), why));
                }
                first.listen.extend(this.listen);
            }
        }
    }
    if s3.is_empty() {
        let why = "it needs at least one interface with type = \"s3\"";
        return Err(invalid("interface", why));
    }
    let oci = oci.map_or_else(OciFlags::none, |(_, flags)| flags);
    Ok(Interfaces { s3, oci })
}

#[cfg(test)]
#[path = "config_interface_tests.rs"]
mod tests;
