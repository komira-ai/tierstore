//! `[cache]`: the root and the budget of the new cache, `packs-v2/` (sections 7, 8 and 13 of
//! `docs/design/fetch-cache-serve.md`), whose read path a build node builds at its start and
//! attaches to the S3 front's large objects (see `local_cache::large::stack`).
//!
//! ```toml
//! [cache]
//! root   = "/var/lib/tierstore-cache"   # packs-v2/ is made in it; it may be local_cache.path
//! budget = "185GiB"                     # what packs-v2 holds; half of it while digest-v1 files remain
//! ```
//!
//! The table is a build node's alone (a file with `[local_cache]` and `[cold_store]` and no
//! `[hot_store]`), over a cold store of kind `s3`: the engine reads the parts of a large object
//! with ranged GETs that carry `If-Match`, which only that kind is read with. Both keys are
//! required: `root` an absolute path, `budget` a size of at least one byte. A file without the
//! table builds no new cache, as the release before did not. A key of the table that this
//! release does not know is warned of and left out (`config_unknown`), as in every table. The
//! release before refuses a file with the table (it refuses a table it does not know), so a host
//! that rolls back takes the table out first.

use std::path::PathBuf;

use serde::Deserialize;
use tierstore_journal::BootId;

use super::super::config::{ConfigError, invalid, require, within};
use super::super::config_tier::TierParts;
use super::super::config_units::Size;

/// The name of the table.
const TABLE: &str = "cache";

/// `[cache]`.
#[derive(Debug, Deserialize)]
pub(in crate::server) struct CacheTable {
    /// The directory `packs-v2/` is made in.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// The bytes the new cache may hold.
    #[serde(default)]
    pub budget: Option<Size>,
}

/// What `[cache]` stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) struct ReadPath {
    /// The directory `packs-v2/` is made in (`cache.root`).
    pub root: PathBuf,
    /// The bytes `packs-v2/` may hold (`cache.budget`), before the overlap halves it.
    pub budget: u64,
    /// The file the boot id is read from (not a key: [`BootId::LINUX_PATH`]; a test names a file
    /// of its own).
    pub boot_id_file: PathBuf,
}

/// The read path of the table `table` of a file whose shape is a build node's if `build_node`;
/// none without the table.
///
/// # Errors
/// [`ConfigError::Invalid`] naming the key: the table in a file of another shape (`cache`), a
/// missing or relative `cache.root`, a missing or zero `cache.budget`.
pub(in crate::server) fn read_path_of(
    table: Option<CacheTable>,
    build_node: bool,
) -> Result<Option<ReadPath>, ConfigError> {
    let Some(table) = table else {
        return Ok(None);
    };
    if !build_node {
        let why = "the new cache is a build node's: a file with [local_cache] and [cold_store] \
                   and no [hot_store]";
        return Err(invalid(TABLE, why));
    }
    let root = require(TABLE, "root", table.root)?;
    if !root.is_absolute() {
        return Err(invalid("cache.root", "must be an absolute path"));
    }
    let budget = require(TABLE, "budget", table.budget)?.0;
    let budget = within(TABLE, "budget", budget, 1, u64::MAX)?;
    Ok(Some(ReadPath {
        root,
        budget,
        boot_id_file: PathBuf::from(BootId::LINUX_PATH),
    }))
}

/// `read`, checked against the upstream of the build node, `upstream`.
///
/// # Errors
/// [`ConfigError::Invalid`] naming `cache.root` when there is a read path and the cold store is
/// not of kind `s3`.
pub(in crate::server) fn over_upstream(
    read: Option<ReadPath>,
    upstream: &TierParts,
) -> Result<Option<ReadPath>, ConfigError> {
    if read.is_some() && !upstream.spec.starts_with("s3:") {
        let why = "the new cache reads the cold store with ranged GETs that carry If-Match: it \
                   needs [cold_store] kind = \"s3\"";
        return Err(invalid("cache.root", why));
    }
    Ok(read)
}

#[cfg(test)]
#[path = "config_read_path_tests.rs"]
mod tests;
