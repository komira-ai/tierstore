//! `params.owner` of an `fs` interface: `user:group`, as `fs.owner` of `tierstore fs` takes it, or
//! a table that names the job user, whose ids differ from node to node:
//!
//! ```toml
//! owner = { subids_of = "builder", id = 1000 }
//! ```
//!
//! On a Linux node a job runs as id `id` (1000) of a user namespace over the job user's
//! subordinate ids, so the host ids every entry of a tree reports are the job user's first
//! subordinate uid and gid plus `id`. The table is resolved at load: the first line of
//! `/etc/subuid` (and of `/etc/subgid`) that names the user, by name or by uid, and has a range of
//! at least 65536 ids, plus `id`. A user with no such range in either file, an `id` outside that
//! range, and an id past 4294967294 (4294967295 is no id) are each refused by name. With it the
//! file is the same on every node of a group.
//!
//! The two files are read through [`Host`], which a test replaces with fixture text.

use std::io;
use std::path::Path;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};

use crate::server::config::{ConfigError, invalid};
use crate::server::treefs::{Database, Lookup, id_of};

use super::Host;

/// The file of subordinate uids.
pub(in crate::server) const SUBUID: &str = "/etc/subuid";

/// The file of subordinate gids.
pub(in crate::server) const SUBGID: &str = "/etc/subgid";

/// The fewest ids a range of the job user's must hold: the ids of one user namespace.
const LEAST_RANGE: u64 = 65536;

/// The largest id an owner may be: 4294967295 is `(uid_t)-1`, which means no id.
const MOST_ID: u32 = u32::MAX - 1;

/// `params.owner` as the file says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) enum Owner {
    /// `user:group`, each a number or a name of this system.
    Pair(String),
    /// The job user's subordinate ids.
    Subids(Subids),
}

/// `owner = { subids_of, id }`: the id `id` of the user namespace over the subordinate ids of the
/// user `subids_of` (a name of this system, or a uid).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server) struct Subids {
    pub subids_of: String,
    pub id: u64,
}

impl<'de> Deserialize<'de> for Owner {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match toml::Value::deserialize(deserializer)? {
            toml::Value::String(text) => Ok(Self::Pair(text)),
            toml::Value::Table(table) => Subids::deserialize(toml::Value::Table(table))
                .map(Self::Subids)
                .map_err(|error| D::Error::custom(error.message())),
            other => Err(D::Error::custom(format!(
                "expected \"user:group\" or a table {{ subids_of, id }}, found {}",
                other.type_str()
            ))),
        }
    }
}

/// Reads the file at `path`: what the system's [`Host`] reads the subordinate id files with.
///
/// # Errors
/// What the system says of a file that cannot be read.
pub(in crate::server) fn read_text(path: &Path) -> io::Result<String> {
    std::fs::read_to_string(path)
}

/// `subids`, the value of `key`, as the `uid:gid` it names on the system `host` asks.
///
/// # Errors
/// A user name the system does not have; a subordinate id file that cannot be read; a user with no
/// range of at least 65536 ids in one of them; an `id` outside the range; an id past 4294967294.
/// Each names `key` and, but the first, the file.
pub(in crate::server) fn resolved(
    key: &str,
    subids: &Subids,
    host: Host,
) -> Result<String, ConfigError> {
    let lookup: Lookup<'_> = &host.names;
    let uid = id_of(key, Database::User, &subids.subids_of, lookup)?;
    let user = owner_in(key, subids, uid, SUBUID, host)?;
    let group = owner_in(key, subids, uid, SUBGID, host)?;
    Ok(format!("{user}:{group}"))
}

/// The id `subids` names in `file`, the subordinate id file of uids or of gids, whose user has the
/// uid `uid`.
fn owner_in(
    key: &str,
    subids: &Subids,
    uid: u32,
    file: &str,
    host: Host,
) -> Result<u32, ConfigError> {
    let user = &subids.subids_of;
    let text = (host.read)(Path::new(file))
        .map_err(|error| invalid(key, format!("{file} could not be read: {error}")))?;
    let Some((start, count)) = first_range(&text, user, uid) else {
        let why = format!(
            "the user {user:?} (uid {uid}) has no range of at least {LEAST_RANGE} ids in {file}"
        );
        return Err(invalid(key, why));
    };
    let id = subids.id;
    if id >= count {
        let why = format!(
            "id {id} is outside the range of {count} ids of the user {user:?} in {file}, which starts at {start}"
        );
        return Err(invalid(key, why));
    }
    let sum = u128::from(start) + u128::from(id);
    match u32::try_from(sum) {
        Ok(owner) if owner <= MOST_ID => Ok(owner),
        _ => {
            let why = format!(
                "{start} + {id} in {file} is {sum}, past {MOST_ID}, the largest id of a user or a group"
            );
            Err(invalid(key, why))
        }
    }
}

/// The start and the count of the first range of `text`, a subordinate id file (`name:start:count`
/// lines), that is the user's, named by `user` or by its uid `uid`, and holds at least 65536 ids.
/// A line that is not three parts, or whose start or count is not a number, is no range.
pub(in crate::server) fn first_range(text: &str, user: &str, uid: u32) -> Option<(u64, u64)> {
    let uid = uid.to_string();
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split(':');
            let (Some(name), Some(start), Some(count), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return None;
            };
            if name != user && name != uid {
                return None;
            }
            Some((start.parse().ok()?, count.parse().ok()?))
        })
        .find(|(_, count)| *count >= LEAST_RANGE)
}

#[cfg(test)]
#[path = "config_fs_owner_tests.rs"]
mod tests;
