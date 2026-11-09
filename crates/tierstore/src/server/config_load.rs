//! Reading a file of settings: the text of a configuration file turned into [`Settings`]
//! ([`settings_from_text`], [`from_text`], [`load`]), the message for a flag that is no longer
//! one ([`removed`], [`keys_for_flags`]). Moved here from `server/config.rs` unchanged, which
//! re-exports the names, so that the file stays under the limit of AGENTS.md R12.

use std::path::Path;

use super::cli::Cli;
use super::config::{ConfigError, File, KEYS, PerRun, Settings, invalid, settings};
use super::config_syntax::{find_secret, syntax};
use super::config_unknown::announce_unknown_keys;

/// The settings of the TOML `text`, with the knobs of this run.
///
/// # Errors
/// A [`ConfigError`]: text that is not a file of this format.
pub(super) fn settings_from_text(text: &str, per_run: PerRun) -> Result<Settings, ConfigError> {
    let table: toml::Table = toml::from_str(text).map_err(|e| syntax(text, &e))?;
    find_secret("", &table)?;
    let file: File = toml::from_str(text).map_err(|e| syntax(text, &e))?;
    settings(file, per_run)
}

/// The settings of a storage-node file.
///
/// # Errors
/// A [`ConfigError`]: text that is not a file of this format, or is a build node's.
pub(super) fn from_text(text: &str, per_run: PerRun) -> Result<Cli, ConfigError> {
    match settings_from_text(text, per_run)? {
        Settings::Storage(cli) => Ok(*cli),
        Settings::Cache(_) => {
            let why =
                "is the file of a build node, which these settings of a storage node cannot hold";
            Err(invalid("local_cache", why))
        }
        Settings::Combined(_) => {
            let why = "is in a file with [hot_store] too, which these settings of a storage node cannot hold";
            Err(invalid("local_cache", why))
        }
    }
}

impl Cli {
    /// The settings of a storage node's configuration file, given as its text, with the knobs of
    /// the run at their defaults. Nothing is read of the files it names.
    ///
    /// # Errors
    /// The refusal of a file that is not one of this format, or is the file of a build node.
    pub fn from_config_text(text: &str) -> Result<Self, ConfigError> {
        from_text(text, PerRun::default())
    }
}

/// What a flag that is no longer one is refused with: the key of the file that holds what it
/// held, or none if `flag` (without `--`) was never one of the data path's.
pub(super) fn removed(flag: &str) -> Option<String> {
    let instead = match flag {
        "listen" => {
            "declare an [[interface]] with type = \"s3\" and params port or addresses".to_owned()
        }
        "oci-listen" => {
            "declare an [[interface]] with type = \"oci\" and params port or addresses".to_owned()
        }
        "print-config" => {
            return Some(
                "--print-config was removed: there are no flags to print a file for; write the file (docs/configuration.md)"
                    .to_owned(),
            );
        }
        other => {
            let (_, keys) = KEYS.iter().find(|(name, _)| *name == other)?;
            let of = if other.starts_with("oci-") {
                " of an oci interface"
            } else {
                ""
            };
            format!("set {}{of}", keys.join(", "))
        }
    };
    Some(format!(
        "--{flag} was removed: the file given with --config holds it; {instead}"
    ))
}

/// The settings of the file at `path`, with the knobs of this run.
///
/// # Errors
/// A [`ConfigError`] that names the file: it cannot be read, or is not a file of this format.
pub(super) fn load(path: &Path, per_run: PerRun) -> Result<Settings, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) => {
            let path = path.to_owned();
            return Err(ConfigError::Read { path, source });
        }
    };
    let settings = settings_from_text(&text, per_run).map_err(|source| ConfigError::In {
        path: path.to_owned(),
        source: Box::new(source),
    })?;
    announce_unknown_keys(&text);
    Ok(settings)
}

/// `message` with the name of each flag in it replaced by the key that stands for it in the
/// file, so that a refusal in a start from a file speaks of the keys the operator wrote.
pub(super) fn keys_for_flags(message: &str) -> String {
    let mut names: Vec<(&str, &str)> = KEYS
        .iter()
        .filter_map(|(flag, keys)| keys.first().map(|key| (*flag, *key)))
        .collect();
    names.sort_by_key(|(flag, _)| std::cmp::Reverse(flag.len()));
    let mut text = message.to_owned();
    for (flag, key) in names {
        text = text.replace(&format!("--{flag}"), key);
    }
    text
}
