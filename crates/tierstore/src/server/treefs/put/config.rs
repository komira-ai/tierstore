//! The `[fs.put]` table: the settings of the put directory, `<mountpoint>/put/`. A file without the
//! table, or with `enabled = false`, has no put directory, so that a file written before this table
//! existed loads unchanged.
//!
//! | key | meaning | default |
//! |---|---|---|
//! | `fs.put.enabled` | serve `put/` | `false` |
//! | `fs.put.prefixes` | the key prefixes a job may write under, each ending in `/` (needed when enabled) | none |
//! | `fs.put.stage_dir` | where the bytes are staged; on the volume of `fs.cache_path` (needed when enabled) | none |
//! | `fs.put.max_object_bytes` | the most bytes of one object | no limit |
//! | `fs.put.max_dir_bytes` | the most staged bytes of one directory | no limit |
//! | `fs.put.max_dir_files` | the most names of one directory, staged or committed | no limit |
//! | `fs.put.min_free_bytes` | the free bytes of the stage's volume under which a create is refused | no limit |
//! | `fs.put.commit_deadline_secs` | how long a commit `rename` may take, waiting for the file's last handle to close and for the upload, before it fails | `30` |
//! | `fs.put.stage_ttl_secs` | how long a staged file is kept from its create | `43200` |
//! | `fs.put.committed_ttl_secs` | how long a committed name is shown from its commit | `600` |

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::journal::is_reserved;
use crate::server::config::{ConfigError, invalid, require, within};

use super::PutSettings;
use super::names::check_prefixes;

/// The seconds a commit may take when the file leaves out `fs.put.commit_deadline_secs`.
const DEFAULT_COMMIT_DEADLINE_SECS: u64 = 30;

/// The most `fs.put.commit_deadline_secs` may be: an hour.
const MOST_COMMIT_DEADLINE_SECS: u64 = 3600;

/// The seconds a staged file is kept when the file leaves out `fs.put.stage_ttl_secs`: 12 hours.
const DEFAULT_STAGE_TTL_SECS: u64 = 43_200;

/// The fewest `fs.put.stage_ttl_secs` may be: more than the 900 seconds that the design of the put
/// directory adds to the longest wall limit of a job.
const LEAST_STAGE_TTL_SECS: u64 = 901;

/// The seconds a committed name is shown when the file leaves out `fs.put.committed_ttl_secs`.
const DEFAULT_COMMITTED_TTL_SECS: u64 = 600;

/// The most either TTL may be: a week.
const MOST_TTL_SECS: u64 = 7 * 24 * 3600;

/// `[fs.put]`.
#[derive(Debug, Deserialize)]
pub(in crate::server) struct PutTable {
    enabled: Option<bool>,
    prefixes: Option<Vec<String>>,
    stage_dir: Option<PathBuf>,
    max_object_bytes: Option<u64>,
    max_dir_bytes: Option<u64>,
    max_dir_files: Option<u64>,
    min_free_bytes: Option<u64>,
    commit_deadline_secs: Option<u64>,
    stage_ttl_secs: Option<u64>,
    committed_ttl_secs: Option<u64>,
}

/// What the put directory runs with, when it is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct PutConfig {
    /// The directory the bytes are staged in.
    pub stage_dir: PathBuf,
    /// The settings of the staged-file store.
    pub store: PutSettings,
    /// How long a commit may take.
    pub commit_deadline: Duration,
}

/// A limit of the table: none, or a number of at least 1 (a limit of 0 would refuse everything).
fn limit(key: &str, value: Option<u64>) -> Result<Option<u64>, ConfigError> {
    match value {
        Some(0) => Err(invalid(&format!("fs.put.{key}"), "must be at least 1")),
        other => Ok(other),
    }
}

/// The settings of `table`: none when the table is left out or not enabled.
///
/// # Errors
/// A [`ConfigError`] that names the key: a prefix that is not a key prefix ending in `/` or is one
/// the proxy keeps for itself, a stage directory that is not absolute, a limit of 0, a number out
/// of its range, or a key needed when enabled and left out.
pub(in crate::server) fn of(table: Option<PutTable>) -> Result<Option<PutConfig>, ConfigError> {
    let Some(table) = table else {
        return Ok(None);
    };
    let limits = (
        limit("max_object_bytes", table.max_object_bytes)?,
        limit("max_dir_bytes", table.max_dir_bytes)?,
        limit("max_dir_files", table.max_dir_files)?,
    );
    let deadline = within(
        "fs.put",
        "commit_deadline_secs",
        table
            .commit_deadline_secs
            .unwrap_or(DEFAULT_COMMIT_DEADLINE_SECS),
        1,
        MOST_COMMIT_DEADLINE_SECS,
    )?;
    let stage_ttl = within(
        "fs.put",
        "stage_ttl_secs",
        table.stage_ttl_secs.unwrap_or(DEFAULT_STAGE_TTL_SECS),
        LEAST_STAGE_TTL_SECS,
        MOST_TTL_SECS,
    )?;
    let committed_ttl = within(
        "fs.put",
        "committed_ttl_secs",
        table
            .committed_ttl_secs
            .unwrap_or(DEFAULT_COMMITTED_TTL_SECS),
        1,
        MOST_TTL_SECS,
    )?;
    if !table.enabled.unwrap_or(false) {
        return Ok(None);
    }
    let prefixes = require("fs.put", "prefixes", table.prefixes)?;
    if prefixes.is_empty() {
        return Err(invalid(
            "fs.put.prefixes",
            "is empty: no key could be written",
        ));
    }
    check_prefixes(&prefixes).map_err(|prefix| {
        invalid(
            "fs.put.prefixes",
            format!("{prefix:?} is not a key prefix of names joined by `/` and ending in `/`"),
        )
    })?;
    if let Some(prefix) = prefixes.iter().find(|prefix| is_reserved(prefix)) {
        return Err(invalid(
            "fs.put.prefixes",
            format!("{prefix:?} is under a prefix the proxy keeps for itself"),
        ));
    }
    let stage_dir = require("fs.put", "stage_dir", table.stage_dir)?;
    if !stage_dir.is_absolute() {
        return Err(invalid("fs.put.stage_dir", "must be an absolute path"));
    }
    let config = PutConfig {
        stage_dir,
        store: PutSettings {
            prefixes,
            max_object_bytes: limits.0,
            max_dir_bytes: limits.1,
            max_dir_files: limits.2,
            min_free_bytes: table.min_free_bytes,
            stage_ttl: Duration::from_secs(stage_ttl),
            committed_ttl: Duration::from_secs(committed_ttl),
        },
        commit_deadline: Duration::from_secs(deadline),
    };
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        return Err(invalid(
            "fs.put.enabled",
            "the put directory is served by the FUSE frontend (Linux) alone in this release",
        ));
    }
    #[cfg(target_os = "linux")]
    Ok(Some(config))
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
