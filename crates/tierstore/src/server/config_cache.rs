//! The build-node shape of the configuration file: `[local_cache]` and `[cold_store]` and no
//! `[hot_store]`, a read cache in front of one upstream (see [`local_cache`](super::local_cache)).
//!
//! ```toml
//! [server]                      # as for a storage node, without backend_check_wait
//! proxy_id        = "tierstore-cache-1"
//! bucket          = "tierstore"
//! access_key_file = "/etc/tierstore-cache/access.key"
//!
//! [local_cache]
//! path       = "/var/lib/tierstore-cache"
//! layout     = "digest-v1"
//! budget     = "40GiB"
//! high_water = 90
//! low_water  = 75
//! critical   = 95
//! free_floor = 10
//! max_concurrent_fills = 4
//! max_concurrent_keeps = 8
//! keep_wait            = "5s"
//! keep_deadline        = "60s"
//! write_through        = false  # true: take writes and send them to the upstream
//! # fill_socket        = "/var/lib/tierstore/fill.sock"   # default: fill.sock in the state dir
//! # fill_socket_concurrency = 16
//! # fill_socket_wait        = "60s"
//! # prefetch_concurrency    = 2
//! # prefetch_queue          = 16384
//! # prefetch_queue_ttl      = "120s"
//! # hold_max                = "4h"
//! # hold_max_count          = 256
//! # hydrate_max_trees       = 64
//! # hydrate_max_blobs       = 500000
//! # hydrate_max_bytes       = "32GiB"
//!
//! [cold_store]                  # the upstream: an S3 endpoint
//! kind             = "s3"
//! bucket           = "tierstore"
//! endpoint         = "http://192.0.2.20:9300"
//! credentials_file = "/etc/tierstore-cache/upstream-read.key"
//!
//! [[interface]]
//! type   = "s3"
//! params = { addresses = ["127.0.0.1:9400"] }
//!
//! [[interface]]                 # optional: the registry front, served from the cache
//! type   = "oci"
//! params = { addresses = ["127.0.0.1:5000"] }
//!
//! [[policy]]
//! prefix = "oci/"             # covers the registry blobs, which the oci interface needs cached
//! cache  = true
//! ```
//!
//! `write_through = true` makes the node take the writes its keys may do (create-only puts,
//! multipart uploads, presigned puts, deletes) and send each to the upstream, which must then be
//! written with a key that may write, and keeps under a `cache = true` policy the objects it
//! puts (`max_concurrent_keeps` of them copied at once): the start does not test the upstream, so a
//! key that may not write fails the first write (see [`cache_cli`](super::cache_cli)). Left out or
//! `false`, the node refuses every write with 403 and its upstream key may be a read-only one.
//!
//! The file is read into a [`CacheSettings`]. An `oci` interface is the
//! storage node's, with its rules (see [`config`](super::config)). What it refuses, besides what
//! every file does: a layout that is not `digest-v1`,
//! `cache = false`, marks out of order, no `[[policy]]` with `cache = true`, and the keys of the
//! storage node that do nothing here: `skip_checks`, the reconciler's keys and
//! `backend_check_wait`. With an `oci` interface it also refuses a front whose blobs (the keys under
//! `<prefix>blobs/sha256/`) no `[[policy]]` with `cache = true` keeps.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};

use super::cli::DEFAULT_OCI_PREFIX;
use super::config::{
    ConfigError, OciFlags, PerRun, ServerTable, TierParts, TierTable, duration, front_of, invalid,
    refuse, require, tier, within,
};
use super::config_cache_tuning;
use super::config_fill::{check_fill_outside_cache, fill_socket_of};
use super::config_handoff::{check_outside_cache, handoff_of};
use super::config_hot_only::refuse_hot_keys;
use super::config_interface::Interfaces;
use super::config_policy::{Policy, check_policies, require_keeping};
use super::config_tuning::S3Client;
use super::config_units::{Dur, Size, parse_size};
use super::front::FrontSettings;
use super::handoff::Handoff;
use super::local_cache::{CacheConfig, SCRUB_MIN_ROUND, SCRUB_RATE};

/// The only layout of the cache directory.
const LAYOUT: &str = "digest-v1";

/// A rate in the file: a size and `/s`, such as `50MiB/s`, or `off` (which is 0 bytes a second).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rate(pub u64);

impl<'de> Deserialize<'de> for Rate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(RateVisitor)
    }
}

struct RateVisitor;

impl Visitor<'_> for RateVisitor {
    type Value = Rate;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a rate such as \"50MiB/s\", or \"off\"")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Rate, E> {
        if text == "off" {
            return Ok(Rate(0));
        }
        let size = text.strip_suffix("/s").map(parse_size);
        match size {
            Some(Ok(bytes)) => Ok(Rate(bytes)),
            // An invalid value, not a custom message: the refusal of a file names the key and
            // the kind of value it takes and never shows what was written (see `syntax`).
            _ => Err(E::invalid_value(de::Unexpected::Str(text), &self)),
        }
    }
}

/// `[local_cache]`.
#[derive(Debug, Deserialize)]
pub(super) struct LocalCacheTable {
    /// The cache directory, on the file system that the jobs' scratch is on.
    pub path: std::path::PathBuf,
    /// The layout of the directory: `digest-v1`, the only one.
    #[serde(default)]
    pub layout: Option<String>,
    /// The bytes the cache is meant to hold.
    #[serde(default)]
    pub budget: Option<Size>,
    #[serde(default)]
    pub high_water: Option<u8>,
    #[serde(default)]
    pub low_water: Option<u8>,
    #[serde(default)]
    pub critical: Option<u8>,
    /// The percent of the file system that is kept free.
    #[serde(default)]
    pub free_floor: Option<u8>,
    /// The most bytes of objects that trees may pin (0 to the budget, default half the budget).
    #[serde(default)]
    pub pin_limit: Option<Size>,
    /// The most fills that run at once (1 to 64, default 4).
    #[serde(default)]
    pub max_concurrent_fills: Option<u8>,
    /// The most create-only puts that are copied into the cache at once (1 to 64, default 8).
    #[serde(default)]
    pub max_concurrent_keeps: Option<u8>,
    /// The longest a put waits for its copy to be put in the cache once the upstream has the
    /// object, such as `5s` (default `5s`, above 0). Past it the put is answered and the copy is
    /// finished in the background.
    #[serde(default)]
    pub keep_wait: Option<Dur>,
    /// The longest a keep runs in all before it is abandoned, such as `60s` (default `60s`,
    /// above 0 and not below `keep_wait`).
    #[serde(default)]
    pub keep_deadline: Option<Dur>,
    /// Whether the node takes writes and sends them to the upstream (default `false`).
    #[serde(default)]
    pub write_through: Option<bool>,
    /// The most an object may be for a ranged read of it to fill the whole of it (1 byte up to
    /// the budget, default 64 MiB). A larger object is passed through and kept nowhere.
    #[serde(default)]
    pub whole_fill_limit: Option<Size>,
    /// Whether the cache is on: `true`, the default. `false` is refused.
    #[serde(default)]
    pub cache: Option<bool>,
    /// The rate at which the scrub re-reads the objects (`SCRUB_RATE`, 16 MiB/s, if not given);
    /// `"off"` (or `"0B/s"`) turns the scrub off.
    #[serde(default)]
    pub scrub_rate: Option<Rate>,
    /// The least time between the starts of two rounds of the scrub (`SCRUB_MIN_ROUND`, one hour,
    /// if not given); `"0s"` lets a round start as soon as the last is done.
    #[serde(default)]
    pub scrub_min_round: Option<Dur>,
    /// How often the eviction pass runs (1ms to 1h, default `10s`).
    #[serde(default)]
    pub pass_interval: Option<Dur>,
    /// How often a cache that does not hold the lock of its directory tries it, and one that holds
    /// it checks the lock file (1ms to 1h, default `1s`).
    #[serde(default)]
    pub lock_interval: Option<Dur>,
    /// The time between two ticks of the scrub (1ms to 1h, default `1s`).
    #[serde(default)]
    pub scrub_tick: Option<Dur>,
    /// The most pages of the upstream one listing reads to make up for the names it leaves out
    /// (1 to 1000000, default 100).
    #[serde(default)]
    pub listing_calls: Option<u32>,
    /// The most keys a read-only cache remembers it deleted or replaced (1 to 100000000, default
    /// 65536).
    #[serde(default)]
    pub forget_limit: Option<u32>,
    /// How old a hit may be, since it was filled or last checked, before it is checked against
    /// the upstream with one HEAD, such as `1h` (left out: never, a hit is served from the disk
    /// until the entry is evicted; `0s` checks every hit).
    #[serde(default)]
    pub revalidate_after: Option<Dur>,
    /// Whether a create-only put asks the upstream for the key before it reads the body
    /// (default `true`; `false` for an upstream that refuses a key it holds before the body).
    #[serde(default)]
    pub head_before_put: Option<bool>,
    /// The bytes a fill reads from the upstream at a time (4KiB to 1GiB, default 8MiB).
    #[serde(default)]
    pub fill_chunk: Option<Size>,
    /// The most bytes of a put's body that wait for the disk before its copy is dropped (1 byte
    /// to 1GiB, default 4MiB).
    #[serde(default)]
    pub keep_queue_bytes: Option<Size>,
    /// The fill socket: an absolute path of at most 103 bytes. Left out, `fill.sock` in the
    /// service's state directory when it has one; with neither, there is no fill socket.
    #[serde(default)]
    pub fill_socket: Option<std::path::PathBuf>,
    /// The most fills the fill socket runs at once (1 to 1024, default 16).
    #[serde(default)]
    pub fill_socket_concurrency: Option<u32>,
    /// The longest a request of the fill socket waits for one of those fills to come free (1ms to
    /// 1h, default `60s`).
    #[serde(default)]
    pub fill_socket_wait: Option<Dur>,
    /// The blobs the prefetch queue fills at once (0 to 64, default 2; 0 turns the queue off).
    #[serde(default)]
    pub prefetch_concurrency: Option<u32>,
    /// The blobs that may wait in the prefetch queue (1 to 1000000, default 16384).
    #[serde(default)]
    pub prefetch_queue: Option<u32>,
    /// The longest a blob waits in the prefetch queue (1s to 1h, default `120s`).
    #[serde(default)]
    pub prefetch_queue_ttl: Option<Dur>,
    /// The longest a hold of the prefetch queue lives (1m to 24h, default `4h`).
    #[serde(default)]
    pub hold_max: Option<Dur>,
    /// The most holds open at once (1 to 100000, default 256).
    #[serde(default)]
    pub hold_max_count: Option<u32>,
    /// The trees one call may expand into the queue (1 to 4096, default 64).
    #[serde(default)]
    pub hydrate_max_trees: Option<u32>,
    /// The blobs one call may queue, all its trees together (1 to 10000000, default 500000).
    #[serde(default)]
    pub hydrate_max_blobs: Option<u32>,
    /// The bytes of the blobs one call may queue (1MiB to 1TiB, default 32GiB).
    #[serde(default)]
    pub hydrate_max_bytes: Option<Size>,
}

/// What a build-node file stands for.
#[derive(Debug)]
pub(super) struct CacheSettings {
    /// The S3 front.
    pub front: FrontSettings,
    /// The limit on a request to an S3 upstream.
    pub s3_request_timeout: Duration,
    /// The connect timeout, body rate, idle-connection timeout and read retries of the client of
    /// the upstream.
    pub s3_client: S3Client,
    /// The upstream: the spec and the flags of an S3 bucket.
    pub upstream: TierParts,
    /// The cache.
    pub cache: CacheConfig,
    /// The OCI front, as the `oci` interfaces make it: `listen` is empty for none.
    pub oci: OciFlags,
    /// The rate of the scrub in bytes a second: the default if the file gave none; none if the
    /// file turned it off.
    pub scrub_rate: Option<u64>,
    /// The least time between the starts of two rounds of the scrub.
    pub scrub_min_round: Duration,
    /// The hand-off of the listening sockets (`server.handoff_socket` and `server.handoff_wait`),
    /// or none without `server.handoff_socket`.
    pub handoff: Option<Handoff>,
    /// The fill socket (`local_cache.fill_socket`), or none.
    pub fill_socket: Option<std::path::PathBuf>,
}

/// The percent in `value`, or `default`, from `low` to `high`.
fn percent(key: &str, value: Option<u8>, default: u8, low: u8) -> Result<u8, ConfigError> {
    within("local_cache", key, value.unwrap_or(default), low, 100)
}

/// Refuses an `oci` front whose blobs the cache would not keep: the keys it reads, under
/// `<prefix>blobs/sha256/`, must be held by the longest `[[policy]]` prefix that covers them with
/// `cache = true`. A front over a cache that keeps none of what it serves is every pull going to
/// the upstream; a registry that is meant to pass through is a storage node's front, or no
/// cache, so there is no use of this to keep.
pub(super) fn check_oci_is_kept(oci: &OciFlags, policy: &[Policy]) -> Result<(), ConfigError> {
    if oci.listen.is_empty() {
        return Ok(());
    }
    let prefix = oci.prefix.as_deref().unwrap_or(DEFAULT_OCI_PREFIX);
    let probe = format!("{prefix}blobs/sha256/");
    let holding = policy
        .iter()
        .filter(|policy| probe.starts_with(policy.prefix.as_str()));
    let longest = holding.max_by_key(|policy| policy.prefix.len());
    match longest {
        Some(policy) if policy.cache => Ok(()),
        Some(policy) => {
            let why = format!(
                "the blobs under {probe:?} are covered by the [[policy]] with prefix {:?}, which has cache = false, so every pull would go to the upstream; give a [[policy]] with prefix = {prefix:?} and cache = true",
                policy.prefix
            );
            Err(invalid("oci interface params.prefix", why))
        }
        None => {
            let why = format!(
                "no [[policy]] covers the blobs under {probe:?}, so every pull would go to the upstream; give a [[policy]] with prefix = {prefix:?} and cache = true"
            );
            Err(invalid("oci interface params.prefix", why))
        }
    }
}

/// What `[local_cache]` stands for: the cache, and the scrub that runs beside it.
#[derive(Debug)]
pub(super) struct CachePart {
    /// The cache.
    pub config: CacheConfig,
    /// The rate of the scrub in bytes a second: the default if the file gave none; none if the
    /// file turned it off.
    pub scrub_rate: Option<u64>,
    /// The least time between the starts of two rounds of the scrub.
    pub scrub_min_round: Duration,
}

impl CachePart {
    /// The settings of the `[local_cache]` table `table`, checked against the `[[policy]]` tables
    /// `policy` (the OCI front's blobs are checked by the caller, with `check_oci_is_kept`).
    pub(super) fn new(table: LocalCacheTable, policy: &[Policy]) -> Result<Self, ConfigError> {
        if table
            .layout
            .as_deref()
            .is_some_and(|layout| layout != LAYOUT)
        {
            let why = format!("the only layout is {LAYOUT:?}");
            return Err(invalid("local_cache.layout", why));
        }
        if table.cache == Some(false) {
            let why = "false turns the cache off; leave out [local_cache] to run without one";
            return Err(invalid("local_cache.cache", why));
        }
        let budget = require("local_cache", "budget", table.budget)?.0;
        let budget = within("local_cache", "budget", budget, 1, u64::MAX)?;
        let high = percent("high_water", table.high_water, 90, 1)?;
        let low = percent("low_water", table.low_water, 75, 1)?;
        let critical = percent("critical", table.critical, 95, 1)?;
        let floor = within(
            "local_cache",
            "free_floor",
            table.free_floor.unwrap_or(10),
            0,
            99,
        )?;
        if low >= high {
            return Err(invalid("local_cache.low_water", "must be below high_water"));
        }
        if high > critical {
            return Err(invalid(
                "local_cache.high_water",
                "must not be above critical",
            ));
        }
        let policies = policy
            .iter()
            .map(|policy| (policy.prefix.clone(), policy.cache))
            .collect();
        let mut cache = CacheConfig::new(table.path.clone(), budget, policies);
        cache.high_water = high;
        cache.low_water = low;
        cache.critical = critical;
        cache.free_floor = floor;
        if let Some(fills) = table.max_concurrent_fills {
            let fills = within("local_cache", "max_concurrent_fills", fills, 1, 64)?;
            cache.max_concurrent_fills = usize::from(fills);
        }
        if let Some(keeps) = table.max_concurrent_keeps {
            let keeps = within("local_cache", "max_concurrent_keeps", keeps, 1, 64)?;
            cache.max_concurrent_keeps = usize::from(keeps);
        }
        if let Some(wait) = table.keep_wait {
            if wait.0.is_zero() {
                let why =
                    "must be above 0; a short wait, such as 100ms, keeps the put from waiting";
                return Err(invalid("local_cache.keep_wait", why));
            }
            cache.keep_wait = wait.0;
        }
        if let Some(deadline) = table.keep_deadline {
            if deadline.0.is_zero() {
                return Err(invalid("local_cache.keep_deadline", "must be above 0"));
            }
            cache.keep_deadline = deadline.0;
        }
        if cache.keep_deadline < cache.keep_wait {
            let why = "must not be below keep_wait (the default is 60s: set both)";
            return Err(invalid("local_cache.keep_deadline", why));
        }
        cache.write_through = table.write_through.unwrap_or(false);
        cache.revalidate_after = table.revalidate_after.map(|after| after.0);
        cache.head_before_put = table.head_before_put.unwrap_or(true);
        config_cache_tuning::apply(&table, &mut cache)?;
        let scrub_min_round = duration(table.scrub_min_round, SCRUB_MIN_ROUND);
        if !scrub_min_round.is_zero() && scrub_min_round < cache.scrub_tick {
            let tick = cache.scrub_tick;
            let shown = if tick.subsec_millis() == 0 {
                format!("{}s", tick.as_secs())
            } else {
                format!("{}ms", tick.as_millis())
            };
            let why = format!(
                "must be at least {shown} (scrub_tick: the scrub ticks that often), or 0s for no \
                 minimum"
            );
            return Err(invalid("local_cache.scrub_min_round", why));
        }
        if let Some(limit) = table.whole_fill_limit {
            cache.whole_limit = within("local_cache", "whole_fill_limit", limit.0, 1, budget)?;
        }
        if let Some(limit) = table.pin_limit {
            cache.pin_limit = within("local_cache", "pin_limit", limit.0, 0, budget)?;
        }
        Ok(Self {
            config: cache,
            scrub_min_round,
            scrub_rate: Some(table.scrub_rate.map_or(SCRUB_RATE, |rate| rate.0)).filter(|n| *n > 0),
        })
    }
}

impl CacheSettings {
    /// The settings of the tables of a build-node file.
    pub(super) fn new(
        server: ServerTable,
        tables: (LocalCacheTable, TierTable),
        fronts: Interfaces,
        policy: &[Policy],
        per_run: PerRun,
    ) -> Result<Self, ConfigError> {
        let (table, cold) = tables;
        check_policies(policy, true)?;
        require_keeping(policy)?;
        let unused = [("backend_check_wait", server.backend_check_wait.is_some())];
        refuse("server", "not used by a build node", &unused)?;
        let handoff = handoff_of(&server, None, per_run.state_dir.as_deref())?;
        refuse_hot_keys(&cold)?;
        let skipping = [("skip_checks", cold.skip_checks.is_some())];
        refuse("cold_store", "not for a build node", &skipping)?;
        let upstream = tier("cold_store", &cold)?;
        let fill_socket = fill_socket_of(&table, handoff.as_ref(), per_run.state_dir.as_deref())?;
        let part = CachePart::new(table, policy)?;
        check_outside_cache(handoff.as_ref(), &part.config.path)?;
        if let Some(socket) = &fill_socket {
            check_fill_outside_cache(socket, &part.config.path)?;
        }
        let oci = fronts.oci;
        if oci.upstream.is_some() {
            let why =
                "a pull is stored, and a build node takes no writes: use a storage node's file";
            return Err(invalid("oci interface params.upstream", why));
        }
        let shared = front_of(server, fronts.s3, per_run)?;
        check_oci_is_kept(&oci, policy)?;
        Ok(Self {
            front: shared.front,
            s3_request_timeout: shared.s3_request_timeout,
            s3_client: shared.s3_client,
            upstream,
            cache: part.config,
            scrub_min_round: part.scrub_min_round,
            oci,
            scrub_rate: part.scrub_rate,
            handoff,
            fill_socket,
        })
    }
}
