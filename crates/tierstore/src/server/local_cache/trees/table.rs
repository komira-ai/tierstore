//! `[trees]`: the keys of the node-local trees, each with its default, none required. The table
//! is read here, beside the settings it makes, not among the `config_*` modules of the server,
//! whose `mod.rs` is at the cap of a file.
//!
//! The table is the build node's: a file that has it and has no `[local_cache]`, or has
//! `[hot_store]`, is refused, as the keys of the prefetch queue are. Each key has a range, and
//! `ttl` may not be above `ttl_max`.

use std::time::Duration;

use serde::Deserialize;

use super::config::{Prefetch, PreparePrefetch, TreesConfig};
use crate::server::config::{ConfigError, invalid, within};
use crate::server::config_tuning::HOUR;
use crate::server::config_units::{Dur, Size};

/// The shortest `ttl`, `ttl_max` and `create_timeout`.
const SECOND: Duration = Duration::from_secs(1);

/// The longest `ttl` and `ttl_max`: a week.
const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);

/// A kibibyte, a mebibyte, a gibibyte and a tebibyte.
const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const TIB: u64 = 1 << 40;

/// `[trees]`.
#[derive(Debug, Default, Deserialize)]
pub(in crate::server) struct TreesTable {
    /// Whether the node makes trees (default `true`).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The lease of a placement made with no `ttl` (1s to 7d, default `4h`).
    #[serde(default)]
    pub ttl: Option<Dur>,
    /// The longest `ttl` taken (1s to 7d, default `24h`, not below `ttl`).
    #[serde(default)]
    pub ttl_max: Option<Dur>,
    /// The entries of one create, below the root (1 to 16777215, default 16777215).
    #[serde(default)]
    pub max_entries: Option<u64>,
    /// The names on a path (1 to 4096, default 4096).
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// The children of one directory (1 to 1000000, default 1000000).
    #[serde(default)]
    pub max_dir_entries: Option<u32>,
    /// The body of one create (1KiB to 1GiB, default `256MiB`).
    #[serde(default)]
    pub max_request_bytes: Option<Size>,
    /// The file bytes of one tree (1MiB to 1TiB, default `32GiB`).
    #[serde(default)]
    pub max_bytes: Option<Size>,
    /// The placements alive at once (1 to 1000000, default 1024).
    #[serde(default)]
    pub max_placements: Option<u32>,
    /// The ranged reads of creates at once, node-wide (1 to 1024, default 8).
    #[serde(default)]
    pub fill_concurrency: Option<u32>,
    /// What a create with no `prefetch` does: `none`, `background` or `wait` (default `wait`).
    #[serde(default)]
    pub prefetch_default: Option<Prefetch>,
    /// What the mount provider's `Prepare` does about the files of a tree: `background` or `none`
    /// (default `background`). Read as text and checked by `prepare_prefetch`, so that a value
    /// it does not take is refused naming the key.
    #[serde(default)]
    pub prepare_prefetch: Option<String>,
    /// The longest a create that waits for its files waits (1s to 1h, default `120s`).
    #[serde(default)]
    pub create_timeout: Option<Dur>,
    /// The failed entries an answer names (1 to 1000, default 20).
    #[serde(default)]
    pub max_failures: Option<u32>,
}

/// The duration of `trees.key` from `low` to `high`; `default` if the file leaves it out.
fn time(
    key: &str,
    value: Option<Dur>,
    default: Duration,
    (low, high): (Duration, Duration),
) -> Result<Duration, ConfigError> {
    let got = value.map_or(default, |dur| dur.0);
    if got < low || got > high {
        let why = format!(
            "{}ms is not between {}ms and {}ms",
            got.as_millis(),
            low.as_millis(),
            high.as_millis()
        );
        return Err(invalid(&format!("trees.{key}"), why));
    }
    Ok(got)
}

/// The number `value` of `trees.key` from `low` to `high`; `default` if the file leaves it out.
fn count(
    key: &str,
    value: Option<u32>,
    default: usize,
    range: (u32, u32),
) -> Result<usize, ConfigError> {
    let default = u32::try_from(default).unwrap_or(u32::MAX);
    let got = within("trees", key, value.unwrap_or(default), range.0, range.1)?;
    Ok(got as usize)
}

/// The number of bytes `value` of `trees.key` from `low` to `high`; `default` if left out.
fn bytes(
    key: &str,
    value: Option<Size>,
    default: u64,
    range: (u64, u64),
) -> Result<u64, ConfigError> {
    let got = value.map_or(default, |size| size.0);
    within("trees", key, got, range.0, range.1)
}

/// The mode `value` of `trees.prepare_prefetch` names; `default` if the file leaves it out.
///
/// # Errors
/// A value other than `background` or `none`, naming the key and the values it takes and not
/// repeating the value, as no message of the file does.
fn prepare_prefetch(
    value: Option<&str>,
    default: PreparePrefetch,
) -> Result<PreparePrefetch, ConfigError> {
    let Some(value) = value else {
        return Ok(default);
    };
    PreparePrefetch::of_name(value).ok_or_else(|| {
        invalid(
            "trees.prepare_prefetch",
            "not a value this key takes (it is not shown); expected \"background\" or \"none\"",
        )
    })
}

/// The settings `table` asks for, on the defaults of [`TreesConfig`].
///
/// # Errors
/// A key out of its range, a `ttl` above `ttl_max`, or a `prepare_prefetch` it does not take;
/// each names the key.
fn trees_config(table: &TreesTable) -> Result<TreesConfig, ConfigError> {
    let base = TreesConfig::default();
    let ttl_range = (SECOND, WEEK);
    let ttl = time("ttl", table.ttl, base.ttl, ttl_range)?;
    let ttl_max = time("ttl_max", table.ttl_max, base.ttl_max, ttl_range)?;
    if ttl > ttl_max {
        return Err(invalid("trees.ttl", "must not be above ttl_max"));
    }
    let entries = table.max_entries.unwrap_or(base.max_entries);
    let depth = count("max_depth", table.max_depth, base.max_depth, (1, 4096))?;
    Ok(TreesConfig {
        enabled: table.enabled.unwrap_or(base.enabled),
        ttl,
        ttl_max,
        max_entries: within("trees", "max_entries", entries, 1, base.max_entries)?,
        max_depth: depth,
        max_dir_entries: count(
            "max_dir_entries",
            table.max_dir_entries,
            base.max_dir_entries,
            (1, 1_000_000),
        )?,
        max_request_bytes: bytes(
            "max_request_bytes",
            table.max_request_bytes,
            base.max_request_bytes,
            (KIB, GIB),
        )?,
        max_bytes: bytes("max_bytes", table.max_bytes, base.max_bytes, (MIB, TIB))?,
        max_placements: count(
            "max_placements",
            table.max_placements,
            base.max_placements,
            (1, 1_000_000),
        )?,
        fill_concurrency: count(
            "fill_concurrency",
            table.fill_concurrency,
            base.fill_concurrency,
            (1, 1024),
        )?,
        prefetch_default: table.prefetch_default.unwrap_or(base.prefetch_default),
        prepare_prefetch: prepare_prefetch(
            table.prepare_prefetch.as_deref(),
            base.prepare_prefetch,
        )?,
        create_timeout: time(
            "create_timeout",
            table.create_timeout,
            base.create_timeout,
            (SECOND, HOUR),
        )?,
        max_failures: count(
            "max_failures",
            table.max_failures,
            base.max_failures,
            (1, 1000),
        )?,
    })
}

/// The settings of the node's trees: those `table` asks for, or the defaults if the file has no
/// `[trees]`. Only a build node (a cache and no `[hot_store]`) has the table.
///
/// # Errors
/// A `[trees]` in the file of another shape of node; what [`trees_config`] refuses.
pub(in crate::server) fn check_trees(
    table: Option<&TreesTable>,
    build_node: bool,
) -> Result<TreesConfig, ConfigError> {
    match table {
        Some(_) if !build_node => Err(invalid(
            "trees",
            "only a build node has trees: a file with [local_cache] and [cold_store] and no [hot_store]",
        )),
        Some(table) => trees_config(table),
        None => Ok(TreesConfig::default()),
    }
}
