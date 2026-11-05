//! The S3 front of a start, whatever it fronts: the settings of the address, the names, the key
//! files and the limits, and the server settings made of them.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::Clock;

use super::ServerConfig;
use super::cli::{Cli, Error};
use super::config_io::IoTuning;
use super::gc::{ensure_distinct as gc_distinct, gc_keys, ids as gc_ids};
use super::keys::{ensure_distinct, ensure_secrets_long_enough, read_credentials, read_only_keys};
use super::listen::joined;
use super::oci::{self, OciError};
use super::presign::{ensure_distinct as presign_distinct, presign_keys};
use super::scoped::{ensure_distinct as scoped_distinct, ids as scoped_ids, scoped_keys};

/// Flags that cannot go together, which only the files they name show: the process exits with
/// status 2, as for flags the parser refused, since a restart cannot mend them.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct InvalidFlags(pub String);

/// The access key id and the secret access key in the file of `server.access_key_file`.
pub(super) fn read_key(path: &Path) -> Result<(String, String), String> {
    read_credentials("--access-key-file", path)
}

/// The certificate and the key that make the S3 front serve HTTPS (`server.tls_cert_file` and
/// `server.tls_key_file`). Left out, it serves plain HTTP.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct S3Tls {
    /// The certificate chain (PEM) of the `s3` interfaces, leaf first, issued by the CA the clients
    /// trust. With it every connection of the front (the loopback socket a build node adds as
    /// well) is HTTPS. The leaf must name the front: a DNS subjectAltName (any), or else an IP
    /// subjectAltName for each `s3` address; it may have both. The files are read once, so a new
    /// certificate takes a restart. Give it with `tls_key` or not at all.
    pub tls_cert: Option<PathBuf>,
    /// The private key (PEM) of `tls_cert`. A file any user can read (a mode with the permission
    /// bits for others) is refused. Give it with `tls_cert` or not at all.
    pub tls_key: Option<PathBuf>,
}

impl S3Tls {
    /// The two files, or `None` when there are none.
    ///
    /// # Errors
    /// Only one of the two is given.
    pub(super) fn files(&self) -> Result<Option<(&Path, &Path)>, InvalidFlags> {
        match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => Ok(Some((cert.as_path(), key.as_path()))),
            (None, None) => Ok(None),
            _ => Err(InvalidFlags(
                "--tls-cert and --tls-key go together: give both, or neither".into(),
            )),
        }
    }
}

/// Writes to `out` the one line that says the front serves plain HTTP at `listen`, if any of the
/// addresses is not loopback (127/8, `::1` or an IPv4-mapped loopback address; `0.0.0.0` and
/// `::` are not loopback). Off loopback a connection can be read and changed by whoever is on
/// its path, so the line names those addresses and advises a certificate. Nothing is written for
/// an address that is loopback.
pub(super) fn warn_plain(listen: &[SocketAddr], mut out: impl Write) {
    let open: Vec<SocketAddr> = listen
        .iter()
        .copied()
        .filter(|addr| !addr.ip().to_canonical().is_loopback())
        .collect();
    if !open.is_empty() {
        // A warning that cannot be written is not worth stopping a start for.
        let _ = writeln!(
            out,
            "tierstore: warning: the s3 interface at {} serves plain HTTP on a non-loopback address; \
             set server.tls_cert_file and server.tls_key_file to serve HTTPS",
            joined(&open)
        );
    }
}

/// What the S3 front is made of, which a storage node and a build node both are: the address,
/// the names, the key files and the limits.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct FrontSettings {
    /// The addresses of the S3 front, which are at least one.
    pub listen: Vec<SocketAddr>,
    pub proxy_id: String,
    pub slot: Option<u16>,
    pub bucket: String,
    pub region: String,
    pub access_key_file: PathBuf,
    pub read_only_key_file: Vec<PathBuf>,
    pub presign_key_file: Vec<PathBuf>,
    pub gc_key_file: Vec<PathBuf>,
    pub scoped_key_file: Vec<PathBuf>,
    pub presign_max_expires: Duration,
    pub grant_key_file: Vec<PathBuf>,
    pub grant_max_lifetime: Option<Duration>,
    pub max_request_bytes: u64,
    pub max_concurrent_puts: u32,
    pub delete_concurrency: u32,
    pub put_permit_wait: Duration,
    pub put_min_bytes_per_sec: u64,
    pub put_rate_grace: Duration,
    pub body_idle_timeout: Duration,
    pub aws_chunked_max_line_bytes: u64,
    pub min_secret_bytes: u32,
    pub drain_lb_wait: Duration,
    pub drain_deadline: Duration,
    pub tls: S3Tls,
    /// The sockets, the pauses and the piece queue that `[server]` tunes.
    pub io: IoTuning,
}

impl Cli {
    /// The settings of the S3 front.
    pub(super) fn front(&self) -> FrontSettings {
        FrontSettings {
            listen: self.listen.clone(),
            proxy_id: self.proxy_id.clone(),
            slot: self.slot,
            bucket: self.bucket.clone(),
            region: self.region.clone(),
            access_key_file: self.access_key_file.clone(),
            read_only_key_file: self.read_only_key_file.clone(),
            presign_key_file: self.presign_key_file.clone(),
            gc_key_file: self.gc_key_file.clone(),
            scoped_key_file: self.scoped_key_file.clone(),
            presign_max_expires: self.presign_max_expires,
            grant_key_file: self.grant_key_file.clone(),
            grant_max_lifetime: self.grant_max_lifetime,
            max_request_bytes: self.max_request_bytes,
            max_concurrent_puts: self.max_concurrent_puts,
            delete_concurrency: self.delete_concurrency,
            put_permit_wait: self.put_permit_wait,
            put_min_bytes_per_sec: self.put_min_bytes_per_sec,
            put_rate_grace: self.put_rate_grace,
            body_idle_timeout: self.body_idle_timeout,
            aws_chunked_max_line_bytes: self.aws_chunked_max_line_bytes,
            min_secret_bytes: self.min_secret_bytes,
            drain_lb_wait: self.drain_lb_wait,
            drain_deadline: self.drain_deadline,
            tls: self.tls.clone(),
            io: self.io_tuning,
        }
    }
}

/// Why the settings of the removed read grant are ignored; the end of the operator's warning.
const GRANT_IGNORED: &str = "is ignored: the read grant was removed; use a presigned GET URL";

/// Says on the operator's stderr, once for each setting, that a setting of the removed read grant
/// is accepted and ignored: an old unit or file still starts, and its owner is told to move on.
/// The key files are not read.
fn warn_grant_ignored(front: &FrontSettings) {
    if !front.grant_key_file.is_empty() {
        tracing::warn!("tierstore: server.grant_key_files {GRANT_IGNORED}");
    }
    if front.grant_max_lifetime.is_some() {
        tracing::warn!("tierstore: server.grant_max_lifetime {GRANT_IGNORED}");
    }
}

/// The settings of the server for `front`: reads its key files (which must differ in id) and its
/// certificate and key, and refuses a bucket that is the name of a probe path and a body timeout
/// of zero. A certificate or key that cannot work is an `InvalidFlags` (exit 2); one that cannot
/// be read is exit 1. `warn` says whether the warning for a plain HTTP front off loopback is
/// printed on stderr: `--check-config` leaves it out, as it prints nothing when the file is fine.
pub(super) fn server_config(
    front: &FrontSettings,
    clock: Arc<dyn Clock>,
    warn: bool,
) -> Result<ServerConfig, Error> {
    if matches!(
        front.bucket.as_str(),
        "healthz" | "readyz" | "usagez" | "metricz"
    ) {
        let name = &front.bucket;
        return Err(format!("--bucket {name:?} is the name of a probe path").into());
    }
    warn_grant_ignored(front);
    if front.body_idle_timeout.is_zero() {
        return Err("--body-idle-timeout must be longer than zero".into());
    }
    let (key_id, secret) = read_key(&front.access_key_file)?;
    let read_only = read_only_keys(&front.read_only_key_file)?;
    ensure_distinct(&key_id, &read_only).map_err(InvalidFlags)?;
    let presign = presign_keys(&front.presign_key_file)?;
    let gc = gc_keys(&front.gc_key_file)?;
    let scoped = scoped_keys(&front.scoped_key_file)?;
    let mut others: Vec<&str> = std::iter::once(key_id.as_str())
        .chain(read_only.iter().map(|key| key.id.as_str()))
        .collect();
    gc_distinct(&others, &gc).map_err(InvalidFlags)?;
    others.extend(gc_ids(&gc));
    scoped_distinct(&others, &scoped).map_err(InvalidFlags)?;
    others.extend(scoped_ids(&scoped));
    presign_distinct(&others, &presign).map_err(InvalidFlags)?;
    let secrets = std::iter::once(("--access-key-file", key_id.as_str(), secret.as_str()))
        .chain(
            read_only
                .iter()
                .map(|k| ("--read-only-key-file", k.id.as_str(), k.secret.as_str())),
        )
        .chain(
            presign
                .iter()
                .map(|k| ("--presign-key-file", k.id(), k.secret())),
        )
        .chain(gc.iter().map(|k| ("--gc-key-file", k.id(), k.secret())))
        .chain(
            scoped
                .iter()
                .map(|k| ("--scoped-key-file", k.id(), k.secret())),
        );
    ensure_secrets_long_enough(front.min_secret_bytes, secrets)
        .map_err(|short| InvalidFlags(short.to_string()))?;
    let mut server = ServerConfig::new(&front.bucket, &key_id, &secret, clock);
    match front.tls.files()? {
        Some((cert, key)) => {
            let acceptor = oci::s3_acceptor(cert, key, &front.listen).map_err(refuse)?;
            server.tls = Some(acceptor);
        }
        None if warn => warn_plain(&front.listen, std::io::stderr()),
        None => {}
    }
    server.read_only_keys = read_only;
    server.presign_keys = presign;
    server.gc_keys = gc;
    server.scoped_keys = scoped;
    server.presign_max_expires = front.presign_max_expires;
    server.slot = front.slot;
    server.region = front.region.clone();
    server.max_request_bytes = usize::try_from(front.max_request_bytes).unwrap_or(usize::MAX);
    server.body_idle_timeout = front.body_idle_timeout;
    server.aws_chunked_max_line_bytes =
        usize::try_from(front.aws_chunked_max_line_bytes).unwrap_or(usize::MAX);
    server.max_concurrent_puts = front.max_concurrent_puts as usize;
    server.delete_concurrency = front.delete_concurrency as usize;
    server.put_permit_wait = front.put_permit_wait;
    server.put_min_bytes_per_sec = front.put_min_bytes_per_sec;
    server.put_rate_grace = front.put_rate_grace;
    server.drain_lb_wait = front.drain_lb_wait;
    server.drain_deadline = front.drain_deadline;
    front.io.apply(&mut server);
    Ok(server)
}

/// The refusal for a certificate or key: a file that cannot be read is an error a restart may mend,
/// and the rest are flags and files that are wrong as they are (exit 2).
pub(super) fn refuse(why: OciError) -> Error {
    if why.is_invalid_flags() {
        InvalidFlags(why.to_string()).into()
    } else {
        why.into()
    }
}

#[cfg(test)]
#[path = "front_tests.rs"]
mod tests;
