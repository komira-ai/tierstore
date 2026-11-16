//! `server.proxy_id_from_facts`: the proxy id as a prefix and the host id that hostkeeper writes
//! into its facts file.
//!
//! The facts file is one JSON object, `{"schema":1,"host_id":"web-04",...}`. This release reads
//! `schema` (which must be the integer 1) and `host_id` (a non-empty string) and ignores every
//! other field, so a later hostkeeper may add fields without a change here; `schema` is raised
//! only for a change that a reader of schema 1 would misread. The proxy id is the prefix and the
//! host id joined.
//!
//! The loaders of text read no file: they make the id from a stand-in host id (`STAND_IN`), so
//! that the rest of the file is checked and a prefix that cannot make an id is refused. [`load`]
//! (in `config_load.rs`) reads the facts file, once, for a start and for `--check-config`.
//!
//! A refusal names the key, the path and the host id, and nothing else of the facts file.
//!
//! [`load`]: super::load

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::server::config::{ConfigError, invalid};

/// The key every refusal of this module is made under.
const KEY: &str = "server.proxy_id_from_facts";

/// The host id a loader of text makes the proxy id with, as it reads no facts file.
const STAND_IN: &str = "host";

/// `proxy_id_from_facts = { path = "...", prefix = "..." }`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server) struct FactsId {
    /// The facts file: an absolute path.
    pub path: PathBuf,
    /// What comes before the host id in the proxy id; it may be empty.
    pub prefix: String,
}

/// The proxy id of `[server]`: the `proxy_id` given, or the prefix of `from_facts` and `host`
/// (the host id of the facts file, or the stand-in when the file was not read).
///
/// # Errors
/// Both keys given, neither given, a facts path that is not absolute, or an id with a `/`.
pub(in crate::server) fn proxy_id_of(
    given: Option<&str>,
    from_facts: Option<&FactsId>,
    host: Option<&str>,
) -> Result<String, ConfigError> {
    let facts = match (given, from_facts) {
        (Some(id), None) => return Ok(id.to_owned()),
        (Some(_), Some(_)) => {
            let why = "give proxy_id or proxy_id_from_facts, not both";
            return Err(invalid(KEY, why));
        }
        (None, None) => {
            let why = "it is needed; give proxy_id or proxy_id_from_facts";
            return Err(invalid("server.proxy_id", why));
        }
        (None, Some(facts)) => facts,
    };
    if !facts.path.is_absolute() {
        return Err(invalid(KEY, "path is not an absolute path"));
    }
    let id = format!("{}{}", facts.prefix, host.unwrap_or(STAND_IN));
    if id.contains('/') {
        let why = format!("the proxy id {id:?} has a \"/\", which an id may not");
        return Err(invalid(KEY, why));
    }
    Ok(id)
}

/// The path of the facts file that the text of a file asks for: the `path` of
/// `server.proxy_id_from_facts` when it is absolute and `server.proxy_id` is not given. Anything
/// else is left to the read of the whole file, which refuses it.
pub(super) fn facts_path(text: &str) -> Option<PathBuf> {
    #[derive(Deserialize)]
    struct PathOnly {
        path: PathBuf,
    }
    #[derive(Deserialize)]
    struct Server {
        proxy_id: Option<toml::Value>,
        proxy_id_from_facts: Option<PathOnly>,
    }
    #[derive(Deserialize)]
    struct Top {
        server: Option<Server>,
    }
    let top: Top = toml::from_str(text).ok()?;
    let server = top.server?;
    let facts = server.proxy_id_from_facts?;
    (server.proxy_id.is_none() && facts.path.is_absolute()).then_some(facts.path)
}

/// The host id in the facts file at `path`.
///
/// # Errors
/// The file cannot be read, is not a JSON object, has no `schema` of 1, or has no `host_id` that
/// is a non-empty string.
pub(super) fn read_host_id(path: &Path) -> Result<String, ConfigError> {
    let refuse = |what: &str| invalid(KEY, format!("the facts file {}: {what}", path.display()));
    let bytes =
        std::fs::read(path).map_err(|e| refuse(&format!("cannot be read: {}", e.kind())))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| refuse("is not JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| refuse("is not a JSON object"))?;
    if object.get("schema").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(refuse("schema is not the integer 1"));
    }
    let host = object
        .get("host_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| refuse("host_id is not a string"))?;
    if host.is_empty() {
        return Err(refuse("host_id is empty"));
    }
    Ok(host.to_owned())
}

#[cfg(test)]
#[path = "config_facts_tests.rs"]
mod tests;
