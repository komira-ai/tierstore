//! The configuration file of the `tierstore` binary: `--config <path>`, a TOML file that holds
//! what describes the data path, in named tables. It is the only way to say it: the binary takes
//! no flag for it (see [`REMOVED`] for the refusal of one that used to).
//!
//! The file is read into the settings of a start ([`Cli`], or those of a build node), with the
//! defaults the keys document.
//!
//! There are three shapes, told apart by the tables the file has:
//!
//! - a storage node: `[hot_store]` and `[cold_store]`;
//! - a build node: `[local_cache]` and `[cold_store]` and no `[hot_store]`, a read cache in
//!   front of one upstream (see [`config_cache`](super::config_cache) and
//!   [`local_cache`](super::local_cache));
//! - a storage node with a cache: all three tables, a disk cache in front of the node's own
//!   proxy (see [`config_combined`](super::config_combined)).
//!
//! The tables:
//!
//! - `[[interface]]`: the fronts, one entry each: `type = "s3"` or `type = "oci"`, and `params`
//!   (see [`config_interface`](super::config_interface)). At least one `s3` entry is needed. A
//!   build node serves the registry from its cache.
//!   The keys of the release before, `server.listen` and the `[oci]` table, are still read and
//!   make an `s3` and an `oci` interface (see [`config_interface`](super::config_interface)).
//! - `[server]`: the id, the bucket and region, the key files, the limits on requests and
//!   bodies.
//! - `[hot_store]`: the hot tier and what only it has: its budget and water marks, the
//!   reconciler and its lease, and the heartbeat of the proxy.
//! - `[cold_store]`: the cold tier.
//! - `[local_cache]`: the cache directory, its budget and marks (a build node, or a storage node
//!   that keeps a cache in front of itself).
//! - `[[policy]]`: the per-prefix settings. With `[local_cache]` `cache = true` keeps the keys
//!   under the prefix in the cache; without it a policy is read and checked and has no effect,
//!   and `cache = true` is refused.
//!
//! Any other table or key is read past: the load succeeds, ignores it and warns on stderr with
//! its name and never its value. A key whose name says it holds a secret is refused, unknown or
//! not: keys and credentials are paths of files (a key ending in `_file` or `_files`).
//!
//! Per-run knobs are flags only: `--config`, `--check-config` and the two parts of the drain.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use super::cli::{
    Cli, DEFAULT_MAX_CONCURRENT_PUTS, DEFAULT_MAX_REQUEST_BYTES, DEFAULT_PUT_MIN_BYTES_PER_SEC,
    DEFAULT_PUT_PERMIT_WAIT, DEFAULT_REGION,
};
use super::cli_heartbeat::HeartbeatFlags;
use super::config_cache::{CacheSettings, LocalCacheTable};
use super::config_combined::{CombinedSettings, combined};
use super::config_entry::entry_tuning;
use super::config_handoff::handoff_of;
use super::config_interface::{
    InterfaceTable, Interfaces, OciTable, UpstreamTable, interfaces, with_legacy,
};
use super::config_io::io_tuning;
use super::config_policy::{Policy, check_policies};
pub(super) use super::config_tier::{TierParts, tier};
use super::config_tuning::{S3Client, hot_tuning, s3_client};
use super::config_units::{Dur, Size};
use super::front::{FrontSettings, S3Tls};
use super::keys::DEFAULT_MIN_SECRET_BYTES;
use super::listen::Addrs;
use super::lock::check_lock_path;

/// The key of each flag that the file holds, or held before the flags went: the flag's name
/// without `--`, then the keys that stand for it, the first of them the one an error message
/// names in its place. A tier flag (`--hot`) has a key for each part of its spec. `--listen`,
/// `--oci-listen` and `--print-config` have no entry: see [`removed`].
pub(super) const KEYS: &[(&str, &[&str])] = &[
    ("proxy-id", &["server.proxy_id"]),
    ("slot", &["server.slot"]),
    (
        "hot",
        &["hot_store.kind", "hot_store.path", "hot_store.bucket"],
    ),
    (
        "cold",
        &["cold_store.kind", "cold_store.path", "cold_store.bucket"],
    ),
    ("hot-endpoint", &["hot_store.endpoint"]),
    ("hot-region", &["hot_store.region"]),
    ("hot-credentials-file", &["hot_store.credentials_file"]),
    ("cold-endpoint", &["cold_store.endpoint"]),
    ("cold-region", &["cold_store.region"]),
    ("cold-credentials-file", &["cold_store.credentials_file"]),
    ("s3-request-timeout", &["server.s3_request_timeout"]),
    ("hot-bytes", &["hot_store.budget"]),
    ("high-water", &["hot_store.high_water"]),
    ("low-water", &["hot_store.low_water"]),
    ("critical", &["hot_store.critical"]),
    ("bucket", &["server.bucket"]),
    ("region", &["server.region"]),
    ("access-key-file", &["server.access_key_file"]),
    ("tls-cert", &["server.tls_cert_file"]),
    ("tls-key", &["server.tls_key_file"]),
    ("read-only-key-file", &["server.read_only_key_files"]),
    ("presign-key-file", &["server.presign_key_files"]),
    ("gc-key-file", &["server.gc_key_files"]),
    ("scoped-key-file", &["server.scoped_key_files"]),
    ("presign-max-expires", &["server.presign_max_expires"]),
    ("grant-key-file", &["server.grant_key_files"]),
    ("grant-max-lifetime", &["server.grant_max_lifetime"]),
    ("copy-parallel", &["hot_store.copy_parallel"]),
    ("lease-ttl", &["hot_store.lease_ttl"]),
    ("heartbeat", &["hot_store.heartbeat"]),
    ("departure", &["hot_store.departure"]),
    ("deadline-margin", &["hot_store.deadline_margin"]),
    ("lock-file", &["hot_store.lock_file"]),
    ("usage-stale-after", &["hot_store.usage_stale_after"]),
    ("max-request-bytes", &["server.max_request_bytes"]),
    ("max-concurrent-puts", &["server.max_concurrent_puts"]),
    ("put-permit-wait", &["server.put_permit_wait"]),
    ("put-min-bytes-per-sec", &["server.put_min_bytes_per_sec"]),
    ("put-rate-grace", &["server.put_rate_grace"]),
    ("body-idle-timeout", &["server.body_idle_timeout"]),
    ("hot-conditional-delete", &["hot_store.conditional_delete"]),
    (
        "cold-conditional-delete",
        &["cold_store.conditional_delete"],
    ),
    ("min-secret-bytes", &["server.min_secret_bytes"]),
    ("skip-backend-check", &["cold_store.skip_checks"]),
    ("backend-check-wait", &["server.backend_check_wait"]),
    // The params of an `oci` interface; the names end in `_file`, as every key that holds a path to a
    // secret does.
    ("oci-tls-cert", &["params.tls_cert_file"]),
    ("oci-tls-key", &["params.tls_key_file"]),
    ("oci-prefix", &["params.prefix"]),
    ("oci-max-connections", &["params.max_connections"]),
    ("oci-connection-lifetime", &["params.connection_lifetime"]),
    ("oci-require-auth", &["params.require_auth"]),
    ("oci-stage-read", &["params.stage_read"]),
    ("oci-max-manifest-bytes", &["params.max_manifest_bytes"]),
    ("oci-read-chunk-bytes", &["params.read_chunk_bytes"]),
    ("oci-echo-bytes", &["params.echo_bytes"]),
];

/// The default of `--drain-lb-wait`, which a start from a file takes when the flag is not given.
pub(super) const DEFAULT_DRAIN_LB_WAIT: Duration = Duration::from_secs(10);

/// The default of `--drain-deadline`.
pub(super) const DEFAULT_DRAIN_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// Why a start from the file `--config` names is refused. Every one of
/// them but an unreadable file is a start that a restart cannot mend; all exit with status 2.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("reading the config file {}: {source}", .path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What the system said.
        source: std::io::Error,
    },
    /// The file is not TOML, or lacks a key it must have, or has a value of the wrong type (the
    /// message names the key). The message holds the place and the key, never
    /// the line or the value: the line may hold a secret, and the message goes to a journal.
    #[error("line {line}, column {column}: {what}")]
    Syntax {
        /// The line of the fault, from 1.
        line: usize,
        /// The column of the fault, from 1.
        column: usize,
        /// What is wrong, without the value.
        what: String,
    },
    /// A key that holds a secret.
    #[error(
        "{key}: a secret is never in the file; give the path of the file that holds it (a key whose name ends in _file)"
    )]
    InlineSecret {
        /// The key, with its table.
        key: String,
    },
    /// A key whose value, or whose absence, the settings cannot be made of.
    #[error("{key}: {why}")]
    Invalid {
        /// The key, with its table.
        key: String,
        /// What is wrong with it.
        why: String,
    },
    /// A flag that the file replaced: the message says which key holds what the flag held.
    #[error("{0}")]
    Removed(String),
    /// A start without `--config`.
    #[error(
        "--config is needed: the settings are in a file (docs/configuration.md), and tierstore takes no flag for them"
    )]
    NoConfig,
    /// `--check-config` checks the file `--config` names; there is nothing to check without it.
    #[error("--check-config checks the file that --config names; give --config")]
    CheckWithoutConfig,
    /// A refusal inside the file `path`.
    #[error("config file {}: {source}", .path.display())]
    In {
        /// The file.
        path: PathBuf,
        /// What is wrong in it.
        source: Box<ConfigError>,
    },
}

/// The knobs of one run, which are flags even when the rest is in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PerRun {
    /// `--drain-lb-wait`.
    pub drain_lb_wait: Duration,
    /// `--drain-deadline`.
    pub drain_deadline: Duration,
}

/// What a tier is made of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Kind {
    /// Objects kept in the process only.
    #[default]
    Memory,
    /// A directory of the local file system: needs `path`.
    Dir,
    /// A bucket of an S3-compatible store: needs `bucket`, `endpoint` and `credentials_file`.
    S3,
}

/// The whole file.
#[derive(Debug, Deserialize)]
pub(super) struct File {
    pub server: ServerTable,
    #[serde(default)]
    pub hot_store: Option<TierTable>,
    #[serde(default)]
    pub cold_store: Option<TierTable>,
    /// The fronts: `[[interface]]`.
    #[serde(default)]
    pub interface: Vec<InterfaceTable>,
    /// The registry front as the release before wrote it; an `oci` interface (read-only, see
    /// [`with_legacy`]).
    #[serde(default)]
    pub oci: Option<OciTable>,
    /// The cache of a build node.
    #[serde(default)]
    pub local_cache: Option<LocalCacheTable>,
    #[serde(default)]
    pub policy: Vec<Policy>,
}

/// `[server]`: what the process is, its limits, and two retired no-op `grant_*` keys. A key that
/// is left out has its default.
#[derive(Debug, Deserialize)]
pub(super) struct ServerTable {
    /// The address of the S3 front as the release before wrote it: an `s3` interface (see
    /// [`with_legacy`]).
    #[serde(default)]
    pub listen: Option<Addrs>,
    pub proxy_id: String,
    #[serde(default)]
    pub slot: Option<u16>,
    pub bucket: String,
    pub region: Option<String>,
    pub access_key_file: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_file: Option<PathBuf>,
    #[serde(default)]
    pub read_only_key_files: Vec<PathBuf>,
    #[serde(default)]
    pub presign_key_files: Vec<PathBuf>,
    #[serde(default)]
    pub gc_key_files: Vec<PathBuf>,
    #[serde(default)]
    pub scoped_key_files: Vec<PathBuf>,
    pub presign_max_expires: Option<Dur>,
    #[serde(default)]
    pub grant_key_files: Vec<PathBuf>,
    #[serde(default)]
    pub grant_max_lifetime: Option<Dur>,
    pub max_request_bytes: Option<u64>,
    pub max_concurrent_puts: Option<u32>,
    pub put_permit_wait: Option<Dur>,
    pub put_min_bytes_per_sec: Option<u64>,
    pub put_rate_grace: Option<Dur>,
    pub body_idle_timeout: Option<Dur>,
    pub min_secret_bytes: Option<u32>,
    pub s3_request_timeout: Option<Dur>,
    /// Deprecated and ignored (the start-up self-test was removed); accepted so that an old file
    /// still loads.
    #[serde(default)]
    pub backend_check_wait: Option<Dur>,
    pub s3_connect_timeout: Option<Dur>,
    pub s3_min_bytes_per_sec: Option<u64>,
    pub s3_pool_idle_timeout: Option<Dur>,
    pub s3_read_retries: Option<u32>,
    pub s3_read_retry_wait: Option<Dur>,
    // The keys of `config_io`.
    pub header_timeout: Option<Dur>,
    pub accept_pause: Option<Dur>,
    pub listen_backlog: Option<i32>,
    pub door_grace: Option<Dur>,
    pub door_look: Option<Dur>,
    pub put_pieces_queued: Option<u32>,
    pub oci_handshake_timeout: Option<Dur>,
    pub s3_part_bytes: Option<Size>,
    pub s3_multipart_threshold: Option<Size>,
    pub s3_chunks_waiting: Option<u32>,
    pub dir_copy_chunk: Option<Size>,
    pub dir_read_chunk: Option<Size>,
    // The keys of `config_handoff`.
    #[serde(default)]
    pub handoff_socket: Option<PathBuf>,
    #[serde(default)]
    pub handoff_wait: Option<Dur>,
}

/// `[hot_store]` or `[cold_store]`: the tier and how to reach it, and the keys only one of the
/// two has (the others are refused in the other).
#[derive(Debug, Default, Deserialize)]
pub(super) struct TierTable {
    pub kind: Kind,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub credentials_file: Option<PathBuf>,
    /// Whether a conditional delete is sent as a DELETE with `If-Match`; the default is the one
    /// of the kind (`true` for `memory` and `dir`, `false` for `s3`), and `false` is refused for
    /// the two that delete in one atomic step.
    #[serde(default)]
    pub conditional_delete: Option<bool>,
    // Only `[hot_store]` has these.
    #[serde(default)]
    pub budget: Option<Size>,
    #[serde(default)]
    pub high_water: Option<u8>,
    #[serde(default)]
    pub low_water: Option<u8>,
    #[serde(default)]
    pub critical: Option<u8>,
    #[serde(default)]
    pub copy_parallel: Option<u32>,
    #[serde(default)]
    pub lease_ttl: Option<Dur>,
    #[serde(default)]
    pub reconcile_interval: Option<Dur>,
    #[serde(default)]
    pub lock_file: Option<PathBuf>,
    #[serde(default)]
    pub usage_stale_after: Option<Dur>,
    #[serde(default)]
    pub journal_linger: Option<Dur>,
    #[serde(default)]
    pub journal_retention: Option<Dur>,
    #[serde(default)]
    pub journal_batch_entries: Option<u32>,
    #[serde(default)]
    pub usage_refresh: Option<Dur>,
    #[serde(default)]
    pub journal_page_size: Option<u32>,
    /// Accepted and ignored: nothing reads it since the reservations went.
    #[serde(default)]
    pub copy_rounds: Option<u32>,
    #[serde(default)]
    pub compare_range: Option<Size>,
    #[serde(default)]
    pub delete_rounds: Option<u32>,
    /// Accepted and ignored: nothing reads it since the reservations went.
    #[serde(default)]
    pub listing_heads: Option<u32>,
    #[serde(default)]
    pub upload_listing_calls: Option<u32>,
    #[serde(default)]
    pub abandoned_upload_after: Option<Dur>,
    pub retry_min: Option<Dur>,
    #[serde(default)]
    pub retry_max: Option<Dur>,
    #[serde(default)]
    pub stray_grace: Option<Dur>,
    #[serde(default)]
    pub stray_interval: Option<Dur>,
    #[serde(default)]
    pub copy_back_ttl: Option<Dur>,
    #[serde(default)]
    pub usage_interval: Option<Dur>,
    #[serde(default)]
    pub usage_scan_interval: Option<Dur>,
    #[serde(default)]
    pub proxy_usage_stale_after: Option<Dur>,
    #[serde(default)]
    pub format_refresh: Option<Dur>,
    #[serde(default)]
    pub delete_wait: Option<Dur>,
    #[serde(default)]
    pub delete_poll_min: Option<Dur>,
    #[serde(default)]
    pub delete_poll_max: Option<Dur>,
    #[serde(default)]
    pub stray_rate: Option<u32>,
    #[serde(default)]
    pub copy_back_max: Option<u32>,
    #[serde(default)]
    pub scan_pages: Option<u32>,
    #[serde(default)]
    pub candidate_limit: Option<u32>,
    #[serde(default)]
    pub tag_record_limit: Option<u32>,
    #[serde(default)]
    pub entry_rounds: Option<u32>,
    pub heartbeat: Option<Dur>,
    #[serde(default)]
    pub departure: Option<Dur>,
    #[serde(default)]
    pub deadline_margin: Option<Dur>,
    #[serde(default)]
    pub registration_retention: Option<Dur>,
    // Only `[cold_store]` has these.
    /// Deprecated and ignored (the start-up self-test was removed); accepted so that an old file
    /// still loads.
    #[serde(default)]
    pub skip_checks: Option<Vec<String>>,
    /// Only the `[cold_store]` of a build node: the upstream, as a list of one in place of
    /// `endpoint`.
    #[serde(default)]
    pub endpoints: Option<Vec<String>>,
}

pub(super) fn invalid(key: &str, why: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.to_owned(),
        why: why.into(),
    }
}

/// The keys of `table` that are in `keys` and given, refused with `why`.
pub(super) fn refuse(table: &str, why: &str, keys: &[(&str, bool)]) -> Result<(), ConfigError> {
    match keys.iter().find(|(_, given)| *given) {
        Some((key, _)) => Err(invalid(&format!("{table}.{key}"), why)),
        None => Ok(()),
    }
}

/// The value of a key that must be given.
pub(super) fn require<T>(table: &str, key: &str, value: Option<T>) -> Result<T, ConfigError> {
    match value {
        Some(value) => Ok(value),
        None => Err(invalid(&format!("{table}.{key}"), "it is needed")),
    }
}

/// `value` if it is between `low` and `high`, both included.
pub(super) fn within<T: PartialOrd + fmt::Display>(
    table: &str,
    key: &str,
    value: T,
    low: T,
    high: T,
) -> Result<T, ConfigError> {
    if value < low || value > high {
        let why = format!("{value} is not between {low} and {high}");
        return Err(invalid(&format!("{table}.{key}"), why));
    }
    Ok(value)
}

/// The duration of a key, or the default of its flag.
pub(super) fn duration(value: Option<Dur>, default: Duration) -> Duration {
    value.map_or(default, |dur| dur.0)
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

/// What a file makes: the settings of one of the three shapes.
#[derive(Debug)]
pub(super) enum Settings {
    /// A storage node: the settings of a file with both tiers.
    Storage(Box<Cli>),
    /// A build node: a read cache, which has no flags.
    Cache(Box<CacheSettings>),
    /// A storage node with a local cache in front of its proxy, which has no flags.
    Combined(Box<CombinedSettings>),
}

/// Refuses the keys only `[hot_store]` has, in `cold`.
pub(super) fn refuse_hot_keys(cold: &TierTable) -> Result<(), ConfigError> {
    let cold_has = [
        ("budget", cold.budget.is_some()),
        ("high_water", cold.high_water.is_some()),
        ("low_water", cold.low_water.is_some()),
        ("critical", cold.critical.is_some()),
        ("copy_parallel", cold.copy_parallel.is_some()),
        ("lease_ttl", cold.lease_ttl.is_some()),
        ("lock_file", cold.lock_file.is_some()),
        ("usage_stale_after", cold.usage_stale_after.is_some()),
        ("journal_linger", cold.journal_linger.is_some()),
        ("journal_retention", cold.journal_retention.is_some()),
        (
            "journal_batch_entries",
            cold.journal_batch_entries.is_some(),
        ),
        ("usage_refresh", cold.usage_refresh.is_some()),
        ("journal_page_size", cold.journal_page_size.is_some()),
        ("copy_rounds", cold.copy_rounds.is_some()),
        ("compare_range", cold.compare_range.is_some()),
        ("delete_rounds", cold.delete_rounds.is_some()),
        ("listing_heads", cold.listing_heads.is_some()),
        ("upload_listing_calls", cold.upload_listing_calls.is_some()),
        (
            "abandoned_upload_after",
            cold.abandoned_upload_after.is_some(),
        ),
        ("retry_min", cold.retry_min.is_some()),
        ("retry_max", cold.retry_max.is_some()),
        ("stray_grace", cold.stray_grace.is_some()),
        ("stray_interval", cold.stray_interval.is_some()),
        ("copy_back_ttl", cold.copy_back_ttl.is_some()),
        ("usage_interval", cold.usage_interval.is_some()),
        ("usage_scan_interval", cold.usage_scan_interval.is_some()),
        (
            "proxy_usage_stale_after",
            cold.proxy_usage_stale_after.is_some(),
        ),
        ("format_refresh", cold.format_refresh.is_some()),
        ("delete_wait", cold.delete_wait.is_some()),
        ("delete_poll_min", cold.delete_poll_min.is_some()),
        ("delete_poll_max", cold.delete_poll_max.is_some()),
        ("stray_rate", cold.stray_rate.is_some()),
        ("copy_back_max", cold.copy_back_max.is_some()),
        ("scan_pages", cold.scan_pages.is_some()),
        ("candidate_limit", cold.candidate_limit.is_some()),
        ("tag_record_limit", cold.tag_record_limit.is_some()),
        ("entry_rounds", cold.entry_rounds.is_some()),
        ("heartbeat", cold.heartbeat.is_some()),
        ("departure", cold.departure.is_some()),
        ("deadline_margin", cold.deadline_margin.is_some()),
        ("reconcile_interval", cold.reconcile_interval.is_some()),
        (
            "registration_retention",
            cold.registration_retention.is_some(),
        ),
    ];
    refuse("cold_store", "only [hot_store] has it", &cold_has)
}

/// The retired keys of `[hot_store]` that `hot` sets, as `hot_store.<key>`: the keys of the
/// journal and of the journal reconciler (`journal_linger`, `journal_retention`,
/// `reconcile_interval`, `registration_retention`, `journal_batch_entries`, `journal_page_size`,
/// `copy_rounds` and `delete_rounds`).
pub(super) fn retired_keys(hot: &TierTable) -> Vec<&'static str> {
    let set = [
        ("hot_store.journal_linger", hot.journal_linger.is_some()),
        (
            "hot_store.journal_retention",
            hot.journal_retention.is_some(),
        ),
        (
            "hot_store.reconcile_interval",
            hot.reconcile_interval.is_some(),
        ),
        (
            "hot_store.registration_retention",
            hot.registration_retention.is_some(),
        ),
        (
            "hot_store.journal_batch_entries",
            hot.journal_batch_entries.is_some(),
        ),
        (
            "hot_store.journal_page_size",
            hot.journal_page_size.is_some(),
        ),
        ("hot_store.copy_rounds", hot.copy_rounds.is_some()),
        ("hot_store.delete_rounds", hot.delete_rounds.is_some()),
    ];
    set.into_iter()
        .filter_map(|(key, given)| given.then_some(key))
        .collect()
}

/// Says on the operator's stderr, once for each key, that a retired key of `[hot_store]` is
/// accepted and ignored: a file written for the releases before the entries still starts.
fn warn_retired_keys(hot: &TierTable) {
    for key in retired_keys(hot) {
        tracing::warn!("tierstore: {key} is ignored: the journal and its reconciler were removed");
    }
}

#[cfg(test)]
impl Settings {
    /// The settings of a storage node, if that is what these are.
    pub(super) fn into_storage(self) -> Option<Cli> {
        match self {
            Settings::Storage(cli) => Some(*cli),
            Settings::Cache(_) | Settings::Combined(_) => None,
        }
    }

    /// The settings of a build node, if that is what these are.
    pub(super) fn into_cache(self) -> Option<CacheSettings> {
        match self {
            Settings::Storage(_) | Settings::Combined(_) => None,
            Settings::Cache(cache) => Some(*cache),
        }
    }

    /// The settings of a storage node with a local cache, if that is what these are.
    pub(super) fn into_combined(self) -> Option<CombinedSettings> {
        match self {
            Settings::Combined(combined) => Some(*combined),
            Settings::Storage(_) | Settings::Cache(_) => None,
        }
    }
}

/// The settings a file stands for, with the knobs of this run, by the shape its tables make.
pub(super) fn settings(file: File, per_run: PerRun) -> Result<Settings, ConfigError> {
    let File {
        mut server,
        hot_store,
        cold_store,
        interface,
        oci,
        local_cache,
        policy,
    } = file;
    if let Some(hot) = &hot_store {
        warn_retired_keys(hot);
    }
    let listed = with_legacy(interface, server.listen.take(), oci)?;
    let fronts = interfaces(&listed)?;
    match (hot_store, cold_store, local_cache) {
        (Some(hot), Some(cold), None) => {
            check_policies(&policy, false)?;
            let cli = storage(server, (hot, cold), fronts, per_run)?;
            Ok(Settings::Storage(Box::new(cli)))
        }
        (None, Some(cold), Some(cache)) => {
            let cache = CacheSettings::new(server, (cache, cold), fronts, &policy, per_run)?;
            Ok(Settings::Cache(Box::new(cache)))
        }
        (Some(hot), Some(cold), Some(cache)) => {
            let tiers = (hot, cold);
            let node = combined(server, tiers, cache, fronts, &policy, per_run)?;
            Ok(Settings::Combined(Box::new(node)))
        }
        _ => {
            let why = "the file needs both [hot_store] and [cold_store] (the storage-node shape), or [local_cache] and [cold_store] without [hot_store] (the build-node shape)";
            Err(invalid("hot_store, cold_store, local_cache", why))
        }
    }
}

/// The settings of a storage-node file (and of the node of a file that also has `[local_cache]`).
pub(super) fn storage(
    server: ServerTable,
    tiers: (TierTable, TierTable),
    fronts: Interfaces,
    per_run: PerRun,
) -> Result<Cli, ConfigError> {
    let (hot, cold) = tiers;
    refuse_hot_keys(&cold)?;
    let why = "only the [cold_store] of a build node takes a list; give endpoint";
    refuse("hot_store", why, &[("endpoints", hot.endpoints.is_some())])?;
    refuse(
        "cold_store",
        why,
        &[("endpoints", cold.endpoints.is_some())],
    )?;
    let skipping = [("skip_checks", hot.skip_checks.is_some())];
    refuse("hot_store", "only [cold_store] has it", &skipping)?;
    let hot_parts = tier("hot_store", &hot)?;
    let cold_parts = tier("cold_store", &cold)?;
    let budget = require("hot_store", "budget", hot.budget)?.0;
    let hot_budget = within("hot_store", "budget", budget, 1, u64::MAX)?;
    let percent = |key: &str, value: Option<u8>, default: u8| {
        within("hot_store", key, value.unwrap_or(default), 1, 100)
    };
    let high_water = percent("high_water", hot.high_water, 80)?;
    let low_water = percent("low_water", hot.low_water, 70)?;
    let critical = percent("critical", hot.critical, 95)?;
    let copy_parallel = hot.copy_parallel.unwrap_or(16);
    let copy_parallel = within("hot_store", "copy_parallel", copy_parallel, 1, u32::MAX)?;
    let hot_tuning = hot_tuning(&hot)?;
    let entry_tuning = entry_tuning(&hot)?;
    let lock_file = hot.lock_file.clone();
    if let Some(path) = &lock_file {
        check_lock_path(path)
            .map_err(|why| invalid("hot_store.lock_file", format!("{path:?} {why}")))?;
    }
    let handoff = handoff_of(&server, lock_file.as_deref())?;
    let oci = fronts.oci;
    let shared = front_of(server, fronts.s3, per_run)?;
    let front = shared.front;
    Ok(Cli {
        listen: front.listen,
        proxy_id: front.proxy_id,
        slot: front.slot,
        hot: hot_parts.spec,
        cold: cold_parts.spec,
        hot_endpoint: hot_parts.endpoint,
        hot_region: hot_parts.region,
        hot_credentials_file: hot_parts.credentials_file,
        cold_endpoint: cold_parts.endpoint,
        cold_region: cold_parts.region,
        cold_credentials_file: cold_parts.credentials_file,
        hot_conditional_delete: hot_parts.conditional_delete,
        cold_conditional_delete: cold_parts.conditional_delete,
        s3_request_timeout: shared.s3_request_timeout,
        s3_client: shared.s3_client,
        hot_tuning,
        entry_tuning,
        hot_bytes: hot_budget,
        high_water,
        low_water,
        critical,
        bucket: front.bucket,
        region: front.region,
        access_key_file: front.access_key_file,
        read_only_key_file: front.read_only_key_file,
        presign_key_file: front.presign_key_file,
        gc_key_file: front.gc_key_file,
        scoped_key_file: front.scoped_key_file,
        presign_max_expires: front.presign_max_expires,
        grant_key_file: front.grant_key_file,
        grant_max_lifetime: front.grant_max_lifetime,
        drain_lb_wait: front.drain_lb_wait,
        drain_deadline: front.drain_deadline,
        tls: front.tls,
        copy_parallel,
        lease_ttl: duration(hot.lease_ttl, Duration::from_secs(30)),
        lock_file,
        handoff,
        usage_stale_after: duration(hot.usage_stale_after, Duration::from_secs(120)),
        heartbeat: HeartbeatFlags::from_table(&hot),
        max_request_bytes: front.max_request_bytes,
        max_concurrent_puts: front.max_concurrent_puts,
        put_permit_wait: front.put_permit_wait,
        put_min_bytes_per_sec: front.put_min_bytes_per_sec,
        put_rate_grace: front.put_rate_grace,
        body_idle_timeout: front.body_idle_timeout,
        min_secret_bytes: front.min_secret_bytes,
        io_tuning: front.io,
        skip_backend_check: cold.skip_checks.unwrap_or_default(),
        backend_check_wait: shared.backend_check_wait,
        oci_listen: oci.listen,
        oci_tls_cert: oci.tls_cert,
        oci_tls_key: oci.tls_key,
        oci_prefix: oci.prefix,
        oci_max_connections: oci.max_connections,
        oci_connection_lifetime: oci.connection_lifetime,
        oci_require_auth: oci.require_auth,
        oci_stage_read: oci.stage_read,
        oci_max_manifest_bytes: oci.max_manifest_bytes,
        oci_read_chunk_bytes: oci.read_chunk_bytes,
        oci_echo_bytes: oci.echo_bytes,
        oci_upstream: oci.upstream,
    })
}

/// The settings of `[server]` that both shapes have, and the two limits of the tiers' requests
/// that are in the same table.
pub(super) struct Shared {
    pub front: FrontSettings,
    pub s3_request_timeout: Duration,
    pub s3_client: S3Client,
    pub backend_check_wait: Option<Duration>,
}

/// The `server.slot` key: a port, so not zero (a number past 65535 does not parse as one).
fn slot_of(slot: Option<u16>) -> Result<Option<u16>, ConfigError> {
    match slot {
        Some(0) => Err(invalid("server.slot", "0 is not between 1 and 65535")),
        other => Ok(other),
    }
}

/// The settings `[server]` makes: a key that is left out has the default of its flag.
pub(super) fn front_of(
    server: ServerTable,
    listen: Vec<SocketAddr>,
    per_run: PerRun,
) -> Result<Shared, ConfigError> {
    let mut s3_client = s3_client(&server)?;
    let io = io_tuning(&server, &mut s3_client)?;
    let max_request_bytes = server
        .max_request_bytes
        .unwrap_or(DEFAULT_MAX_REQUEST_BYTES);
    let max_request_bytes = within(
        "server",
        "max_request_bytes",
        max_request_bytes,
        1,
        u64::MAX,
    )?;
    let max_concurrent_puts = server
        .max_concurrent_puts
        .unwrap_or(DEFAULT_MAX_CONCURRENT_PUTS);
    let max_concurrent_puts = within(
        "server",
        "max_concurrent_puts",
        max_concurrent_puts,
        1,
        u32::MAX,
    )?;
    let front = FrontSettings {
        listen,
        proxy_id: server.proxy_id,
        slot: slot_of(server.slot)?,
        bucket: server.bucket,
        region: server.region.unwrap_or_else(|| DEFAULT_REGION.to_owned()),
        access_key_file: server.access_key_file,
        tls: S3Tls {
            tls_cert: server.tls_cert_file,
            tls_key: server.tls_key_file,
        },
        read_only_key_file: server.read_only_key_files,
        presign_key_file: server.presign_key_files,
        gc_key_file: server.gc_key_files,
        scoped_key_file: server.scoped_key_files,
        presign_max_expires: duration(server.presign_max_expires, Duration::from_secs(15 * 60)),
        grant_key_file: server.grant_key_files,
        grant_max_lifetime: server.grant_max_lifetime.map(|wait| wait.0),
        max_request_bytes,
        max_concurrent_puts,
        put_permit_wait: duration(server.put_permit_wait, DEFAULT_PUT_PERMIT_WAIT),
        put_min_bytes_per_sec: server
            .put_min_bytes_per_sec
            .unwrap_or(DEFAULT_PUT_MIN_BYTES_PER_SEC),
        put_rate_grace: duration(server.put_rate_grace, Duration::from_secs(30)),
        body_idle_timeout: duration(server.body_idle_timeout, Duration::from_secs(30)),
        min_secret_bytes: server.min_secret_bytes.unwrap_or(DEFAULT_MIN_SECRET_BYTES),
        drain_lb_wait: per_run.drain_lb_wait,
        drain_deadline: per_run.drain_deadline,
        io,
    };
    Ok(Shared {
        front,
        s3_request_timeout: duration(server.s3_request_timeout, Duration::from_secs(60)),
        s3_client,
        backend_check_wait: server.backend_check_wait.map(|wait| wait.0),
    })
}

#[cfg(test)]
pub(super) use super::config_load::{from_text, settings_from_text};
pub(super) use super::config_load::{keys_for_flags, load, removed};

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
