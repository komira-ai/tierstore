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
//! `/readyz`, `/usagez` and `/metricz`), `oci` (the read-only registry front), `trees` (the
//! gRPC API of node-local trees, see [`trees`](super::trees)), `mount-provider` (jobscheduler's
//! mount provider on a Unix socket, see [`provider::config`](super::provider::config), which listens
//! on no address and takes params of its own) and `fs` (the trees as a file system, see
//! [`config::fs`](super::config::fs), which listens on no address either and takes params of its
//! own). Every other entry says where it listens with one of two params:
//! `port`, the wildcard of IPv4 (`0.0.0.0:<port>`), or `addresses`, a socket address or a list
//! of them. Both are refused, and so is neither, but for a `trees` interface, which listens on
//! port 9600 of the wildcard when it says neither. An address that two interfaces (or one
//! interface twice) listen on is refused too, but for a port of 0, which asks the system for a
//! port.
//!
//! The entries of a type are served together: one server, one drain, and for `oci` one limit
//! on connections, per type, on every address of the entries of that type. So the `s3` entries take no other
//! param, and the `oci` entries, which also take `tls_cert_file`, `tls_key_file`, `prefix`,
//! `max_connections` and `connection_lifetime`, must agree on those five (one registry front has
//! one certificate and one prefix); one that differs from the first is refused. The `trees`
//! entries are served together in the same way and take `tls_cert_file`, `tls_key_file`,
//! `max_connections` and `connection_lifetime`, which they must agree on (the longest call is
//! `trees.max_request_bytes` of the `[trees]` table). An `s3` interface has no TLS params yet. At
//! least one `s3` interface is needed. A `trees` interface is a build node's alone: its `[trees]`
//! keep the trees.
//!
//! The keys of the release before stay readable: `server.listen` is an `s3` interface at those
//! addresses and the `[oci]` table (`listen` and the same five params, with `listen` needed) is an
//! `oci` interface, so a file written for that release starts as it did ([`with_legacy`]). The
//! entries they make follow the ones the file lists, and every rule above holds for them too.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde::de::IgnoredAny;

use super::config::fs::owner::Owner;
use super::config::fs::{FsInterface, fs_of, provider_mountpoint, refuse_fs_params};
use super::config::{ConfigError, invalid, refuse, require, within};
use super::config_units::Dur;
use super::config_unknown::Strict;
use super::listen::Addrs;
use super::provider::ProviderFlags;
use super::provider::config::provider_of;
use super::treefs::{NfsTable, PutTable};
use super::trees::{DEFAULT_PORT, TreesFlags};

/// `params.upstream` of an `oci` interface: the registry the front pulls an object from when its
/// store has none (see `oci::upstream`). The password is in the file `password_file`, never in the
/// configuration file.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpstreamTable {
    /// The registry's address, `https://host[:port]` or `http://host[:port]`, with no path.
    pub url: Option<String>,
    /// The user name the registry is logged in to with; with `password_file`, or neither.
    pub username: Option<String>,
    /// The file that holds the password (or token) of `username`, readable by the user the
    /// server runs as and no one else.
    pub password_file: Option<PathBuf>,
    /// PEM certificates the registry's certificate may be issued by, besides the system's.
    pub ca_file: Option<PathBuf>,
    /// How long the upstream has for each step of a pull: to answer a request, and to send the
    /// next piece of a body (`30s` if left out).
    pub timeout: Option<Dur>,
    /// The most redirects one request follows (4 if left out).
    pub max_redirects: Option<u32>,
    /// How long the whole re-check of a tag the store has may take, all its requests together;
    /// past it the stored tag is served (`5s` if left out).
    pub recheck_timeout: Option<Dur>,
}

/// One `[[interface]]` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InterfaceTable {
    /// `s3`, `oci`, `trees`, `mount-provider` or `fs`.
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
    pub require_auth: Option<bool>,
    pub stage_read: Option<u64>,
    pub max_manifest_bytes: Option<u64>,
    pub read_chunk_bytes: Option<u64>,
    pub echo_bytes: Option<u64>,
    pub upstream: Option<UpstreamTable>,
    pub socket: Option<PathBuf>,
    pub mountpoint: Option<PathBuf>,
    pub fetch_timeout: Option<Dur>,
    // The params of an `fs` interface alone; `mountpoint` and `fetch_timeout` are its too.
    pub owner: Option<Owner>,
    pub client_group: Option<String>,
    pub runtime_dir: Option<PathBuf>,
    pub control_socket: Option<PathBuf>,
    pub threads: Option<u32>,
    pub handoff_wait: Option<Dur>,
    pub recover_wait: Option<Dur>,
    pub reattach_concurrency: Option<u32>,
    pub tree_idle_secs: Option<u64>,
    pub tree_list_ram_bytes: Option<u64>,
    pub nfs: Option<Strict<NfsTable>>,
    pub put: Option<Strict<PutTable>>,
    // Keys of the two-process layout's `[fs]` table that are not params, each refused by name with why, whatever its value.
    pub fill_socket: Option<IgnoredAny>,
    pub cache_path: Option<IgnoredAny>,
    pub handoff_socket: Option<IgnoredAny>,
}

/// `[oci]`, the table of the release before: the registry front, which is an `oci` interface.
#[derive(Debug, Deserialize)]
pub(super) struct OciTable {
    /// One address, or a list of them; needed in the table.
    pub listen: Option<Addrs>,
    pub tls_cert_file: Option<PathBuf>,
    pub tls_key_file: Option<PathBuf>,
    pub prefix: Option<String>,
    pub max_connections: Option<u32>,
    pub connection_lifetime: Option<Dur>,
}

/// The settings of the OCI front, as the `oci` interfaces make them: the values the front checks
/// at start.
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
    /// The most bytes of a stored chunk read in one call at the end of an upload.
    pub stage_read: Option<u64>,
    /// The largest manifest, in bytes.
    pub max_manifest_bytes: Option<u64>,
    /// The most bytes asked of the store at once for a blob.
    pub read_chunk_bytes: Option<u64>,
    /// The longest piece of request text an error repeats, in bytes.
    pub echo_bytes: Option<u64>,
    /// The registry a miss is pulled from, checked by `config_interface`; none pulls nothing.
    pub upstream: Option<UpstreamTable>,
}

impl OciFlags {
    /// No front.
    pub(super) fn none() -> Self {
        Self {
            listen: Vec::new(),
            tls_cert: None,
            tls_key: None,
            prefix: None,
            max_connections: None,
            connection_lifetime: None,
            require_auth: false,
            stage_read: None,
            max_manifest_bytes: None,
            read_chunk_bytes: None,
            echo_bytes: None,
            upstream: None,
        }
    }
}

/// What the interfaces of a file stand for.
#[derive(Debug)]
pub(super) struct Interfaces {
    /// The addresses of the S3 front, which are at least one.
    pub s3: Vec<SocketAddr>,
    /// The OCI front: no address when there is no `oci` interface.
    pub oci: OciFlags,
    /// The trees front: no address when there is no `trees` interface.
    pub trees: TreesFlags,
    /// The mount provider, if there is a `mount-provider` interface.
    pub provider: Option<ProviderFlags>,
    /// The file system, if there is an `fs` interface; a build node's settings make it whole.
    pub fs: Option<FsInterface>,
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
        ("require_auth", params.require_auth.is_some()),
        ("stage_read", params.stage_read.is_some()),
        ("max_manifest_bytes", params.max_manifest_bytes.is_some()),
        ("read_chunk_bytes", params.read_chunk_bytes.is_some()),
        ("echo_bytes", params.echo_bytes.is_some()),
        ("upstream", params.upstream.is_some()),
    ];
    super::config::refuse(&place, "not for type \"s3\"", &others)?;
    refuse_provider_params(&place, "s3", params)
}

/// Refuses the params of a `mount-provider` interface, and those of an `fs` interface, in an
/// interface of type `kind`.
fn refuse_provider_params(place: &str, kind: &str, params: &Params) -> Result<(), ConfigError> {
    let theirs = [
        ("socket", params.socket.is_some()),
        ("mountpoint", params.mountpoint.is_some()),
        ("fetch_timeout", params.fetch_timeout.is_some()),
    ];
    refuse(place, &format!("not for type \"{kind}\""), &theirs)?;
    refuse_fs_params(place, kind, params)
}

/// The settings of the front that a `trees` interface says, with `listen` its addresses: the TLS
/// files go together, and the numbers are at least 1.
fn trees_of(at: &str, params: &Params, listen: Vec<SocketAddr>) -> Result<TreesFlags, ConfigError> {
    let place = format!("{at}.params");
    let others = [
        ("prefix", params.prefix.is_some()),
        ("require_auth", params.require_auth.is_some()),
        ("stage_read", params.stage_read.is_some()),
        ("max_manifest_bytes", params.max_manifest_bytes.is_some()),
        ("read_chunk_bytes", params.read_chunk_bytes.is_some()),
        ("echo_bytes", params.echo_bytes.is_some()),
        ("upstream", params.upstream.is_some()),
    ];
    refuse(&place, "not for type \"trees\"", &others)?;
    refuse_provider_params(&place, "trees", params)?;
    together(&place, params)?;
    let max_connections = match params.max_connections {
        Some(count) => Some(within(&place, "max_connections", count, 1, u32::MAX)?),
        None => None,
    };
    if params
        .connection_lifetime
        .is_some_and(|dur| dur.0.is_zero())
    {
        let why = "it must be longer than zero";
        return Err(invalid(&format!("{place}.connection_lifetime"), why));
    }
    Ok(TreesFlags {
        listen,
        tls_cert: params.tls_cert_file.clone(),
        tls_key: params.tls_key_file.clone(),
        max_connections,
        connection_lifetime: params.connection_lifetime.map(|dur| dur.0),
    })
}

/// Refuses a `tls_cert_file` without a `tls_key_file`, and the other way round.
fn together(place: &str, params: &Params) -> Result<(), ConfigError> {
    match (&params.tls_cert_file, &params.tls_key_file) {
        (Some(_), None) => {
            let why = "it is needed with tls_cert_file: give both, or neither";
            Err(invalid(&format!("{place}.tls_key_file"), why))
        }
        (None, Some(_)) => {
            let why = "it is needed with tls_key_file: give both, or neither";
            Err(invalid(&format!("{place}.tls_cert_file"), why))
        }
        _ => Ok(()),
    }
}

/// The settings of the front that an `oci` interface says, with `listen` its addresses.
fn oci_of(at: &str, params: &Params, listen: Vec<SocketAddr>) -> Result<OciFlags, ConfigError> {
    let place = format!("{at}.params");
    refuse_provider_params(&place, "oci", params)?;
    together(&place, params)?;
    let max_connections = match params.max_connections {
        Some(count) => Some(within(&place, "max_connections", count, 1, u32::MAX)?),
        None => None,
    };
    let stage_read = match params.stage_read {
        Some(bytes) => Some(within(&place, "stage_read", bytes, 1, u64::MAX)?),
        None => None,
    };
    let size = |name: &str, given: Option<u64>| {
        given
            .map(|bytes| within(&place, name, bytes, 1, u64::MAX))
            .transpose()
    };
    let max_manifest_bytes = size("max_manifest_bytes", params.max_manifest_bytes)?;
    let read_chunk_bytes = size("read_chunk_bytes", params.read_chunk_bytes)?;
    let echo_bytes = size("echo_bytes", params.echo_bytes)?;
    let upstream = params
        .upstream
        .clone()
        .map(|table| upstream(&place, table))
        .transpose()?;
    Ok(OciFlags {
        listen,
        tls_cert: params.tls_cert_file.clone(),
        tls_key: params.tls_key_file.clone(),
        prefix: params.prefix.clone(),
        max_connections,
        connection_lifetime: params.connection_lifetime.map(|dur| dur.0),
        require_auth: params.require_auth.unwrap_or(false),
        stage_read,
        max_manifest_bytes,
        read_chunk_bytes,
        echo_bytes,
        upstream,
    })
}

/// The `params.upstream` table, checked: it needs `url`, `username` and `password_file` go
/// together, and the timeout is longer than zero.
fn upstream(place: &str, table: UpstreamTable) -> Result<UpstreamTable, ConfigError> {
    let place = format!("{place}.upstream");
    require(&place, "url", table.url.clone())?;
    match (&table.username, &table.password_file) {
        (Some(_), None) => {
            let why = "it is needed with username: give both, or neither";
            return Err(invalid(&format!("{place}.password_file"), why));
        }
        (None, Some(_)) => {
            let why = "it is needed with password_file: give both, or neither";
            return Err(invalid(&format!("{place}.username"), why));
        }
        _ => {}
    }
    if table.timeout.is_some_and(|timeout| timeout.0.is_zero()) {
        let why = "it must be longer than zero";
        return Err(invalid(&format!("{place}.timeout"), why));
    }
    Ok(table)
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
        ("require_auth", first.require_auth == next.require_auth),
        ("stage_read", first.stage_read == next.stage_read),
        (
            "max_manifest_bytes",
            first.max_manifest_bytes == next.max_manifest_bytes,
        ),
        (
            "read_chunk_bytes",
            first.read_chunk_bytes == next.read_chunk_bytes,
        ),
        ("echo_bytes", first.echo_bytes == next.echo_bytes),
        ("upstream", first.upstream == next.upstream),
    ];
    same.iter().find(|(_, same)| !same).map(|(name, _)| *name)
}

/// The first param of `next` that is not that of `first`, by name.
fn differing_trees(first: &TreesFlags, next: &TreesFlags) -> Option<&'static str> {
    let same = [
        ("tls_cert_file", first.tls_cert == next.tls_cert),
        ("tls_key_file", first.tls_key == next.tls_key),
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

/// Refuses the `trees`, `mount-provider` and `fs` interfaces of a file that is not a build node's
/// (`node` says what it is): the tree store is a build node's, kept beside its cache by its
/// `[trees]` table, so another node has nothing to serve the interface from.
///
/// # Errors
/// A [`ConfigError`] on the key `trees interface`, `mount-provider interface` or `fs interface`.
pub(super) fn no_trees(fronts: &Interfaces, node: &str) -> Result<(), ConfigError> {
    let build_node = "a build node ([local_cache] and [cold_store] without [hot_store]), whose [trees] keep the trees";
    if !fronts.trees.listen.is_empty() {
        let why = format!("{node} has no tree store: a trees interface needs {build_node}");
        return Err(invalid("trees interface", &why));
    }
    if fronts.provider.is_some() {
        let why =
            format!("{node} has no tree store: a mount-provider interface needs {build_node}");
        return Err(invalid("mount-provider interface", &why));
    }
    if fronts.fs.is_some() {
        let why = format!("{node} has no tree store: an fs interface needs {build_node}");
        return Err(invalid("fs interface", &why));
    }
    Ok(())
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

/// Why a `mount-provider` entry is refused in a file with no `fs` entry: the provider answers each
/// source with a directory under the `fs` interface's mount, and since 0.3.196 no other process
/// serves one (`tierstore fs` exits 2), so such a node would start and answer no job its tree. The
/// file of a Mac of the two-process layout is such a file.
pub(super) const NO_FS: &str = "a mount-provider needs an [[interface]] with type = \"fs\" in the same file: the provider answers directories under that interface's mount, and no other process serves one (tierstore fs is no longer a process of its own)";

/// The interfaces the entries of a file stand for.
///
/// # Errors
/// A [`ConfigError`] that names the entry by its place in the list (`interface[1]`): a type that
/// is not one, a missing or doubled place to listen, an address in use by another entry, a param
/// the type does not take, `oci` or `trees` entries that disagree, a second `mount-provider` or
/// `fs` entry, a `mount-provider` entry in a file with no `fs` entry ([`NO_FS`]), and a
/// provider's `mountpoint` that is not the `fs` interface's.
pub(super) fn interfaces(tables: &[InterfaceTable]) -> Result<Interfaces, ConfigError> {
    let mut s3 = Vec::new();
    let mut oci: Option<(usize, OciFlags)> = None;
    let mut trees: Option<(usize, TreesFlags)> = None;
    let mut provider: Option<ProviderFlags> = None;
    // The provider's place.
    let mut provider_at: Option<String> = None;
    // The provider's place, when it gives its own `mountpoint`.
    let mut provider_given: Option<String> = None;
    let mut fs: Option<FsInterface> = None;
    let mut taken: Vec<(SocketAddr, usize)> = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let at = format!("interface[{index}]");
        match table.kind.as_str() {
            "s3" | "oci" | "trees" => {}
            "mount-provider" if provider.is_some() => {
                let why = "a second mount-provider interface: a node has one mount provider";
                return Err(invalid(&format!("{at}.type"), why));
            }
            "mount-provider" => {
                provider = Some(provider_of(&at, &table.params)?);
                if table.params.mountpoint.is_some() {
                    provider_given = Some(at.clone());
                }
                provider_at = Some(at);
                continue;
            }
            "fs" if fs.is_some() => {
                let why = "a second fs interface: a node has one file system";
                return Err(invalid(&format!("{at}.type"), why));
            }
            "fs" => {
                fs = Some(fs_of(&at, &table.params)?);
                continue;
            }
            _ => {
                let why = "not an interface type; the types are \"s3\", \"oci\", \"trees\", \"mount-provider\" and \"fs\"";
                return Err(invalid(&format!("{at}.type"), why));
            }
        }
        let params = &table.params;
        let listen = if table.kind == "trees" && params.port.is_none() && params.addresses.is_none()
        {
            vec![SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), DEFAULT_PORT)]
        } else {
            addresses_of(&at, params)?
        };
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
        if table.kind == "trees" {
            let this = trees_of(&at, params, listen)?;
            match &mut trees {
                None => trees = Some((index, this)),
                Some((first_at, first)) => {
                    if let Some(name) = differing_trees(first, &this) {
                        let why = format!(
                            "it differs from interface[{first_at}]: one trees front serves every trees interface, so they share tls_cert_file, tls_key_file, max_connections and connection_lifetime"
                        );
                        return Err(invalid(&format!("{at}.params.{name}"), why));
                    }
                    first.listen.extend(this.listen);
                }
            }
            continue;
        }
        let this = oci_of(&at, params, listen)?;
        match &mut oci {
            None => oci = Some((index, this)),
            Some((first_at, first)) => {
                if let Some(name) = differing(first, &this) {
                    let why = format!(
                        "it differs from interface[{first_at}]: one registry front serves every oci interface, so they share tls_cert_file, tls_key_file, prefix, max_connections, connection_lifetime, require_auth, stage_read, max_manifest_bytes, read_chunk_bytes, echo_bytes and upstream"
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
    if let (Some(at), None) = (&provider_at, &fs) {
        return Err(invalid(&format!("{at}.type"), NO_FS));
    }
    provider_mountpoint(provider.as_mut(), provider_given.as_deref(), fs.as_ref())?;
    let oci = oci.map_or_else(OciFlags::none, |(_, flags)| flags);
    let trees = trees.map(|(_, flags)| flags).unwrap_or_default();
    Ok(Interfaces {
        s3,
        oci,
        trees,
        provider,
        fs,
    })
}

#[cfg(test)]
#[path = "config_interface_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "config_upstream_tests.rs"]
mod upstream_tests;

#[cfg(test)]
#[path = "config_trees_tests.rs"]
mod trees_tests;

#[cfg(test)]
#[path = "config_provider_tests.rs"]
mod provider_tests;
