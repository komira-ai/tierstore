//! The `--params <file>` table.
//!
//! The declaration's `[hook.params]` table is opaque to the updater, which writes it, with its
//! own reserved keys, to a TOML file of scalars. This hook reads:
//!
//! - `promotion_bound_secs` (required, 1 to a day): how long `verify` waits, after `cleanup` has
//!   stopped the previous instance, for the new one to say `read-write`;
//! - `defer_retry_secs` (default 300, at most a day): what a deferred `prepare` asks the
//!   updater to wait; zero means "retry now";
//! - `conntrack_percent` (default 80, 1 to 100): the share of `nf_conntrack_max` that
//!   `nf_conntrack_count` may reach before `prepare` defers;
//! - `updater.staged_release` and `updater.target_version` (required, written by the updater);
//! - `updater.promotion_ceiling_secs` (optional): read, and never a reason to fail: the updater
//!   applies the ceiling itself.
//!
//! Any other key is ignored when it is a scalar, so an older hook keeps working when the updater
//! adds a reserved key. The reserved keys contain a dot, so in the file they are quoted keys.

use std::io;
use std::path::{Path, PathBuf};

use super::plain::is_plain;

/// The key of the wait for the new instance to take the cache lock.
pub const PROMOTION_BOUND_KEY: &str = "promotion_bound_secs";
/// The key of the time a deferred `prepare` asks the updater to wait.
pub const DEFER_RETRY_KEY: &str = "defer_retry_secs";
/// The key of the share of the conntrack table `prepare` accepts.
pub const CONNTRACK_PERCENT_KEY: &str = "conntrack_percent";
/// The reserved key of the updater's ceiling on a pending promotion.
pub const PROMOTION_CEILING_KEY: &str = "updater.promotion_ceiling_secs";
/// The reserved key of the verified staged release directory.
pub const STAGED_RELEASE_KEY: &str = "updater.staged_release";
/// The reserved key of the version being installed.
pub const TARGET_VERSION_KEY: &str = "updater.target_version";

/// The longest wait or retry delay a key may name, in seconds: a day.
pub const MAX_SECS: u64 = 86_400;
/// The default of [`DEFER_RETRY_KEY`].
pub const DEFAULT_DEFER_RETRY_SECS: u64 = 300;
/// The default of [`CONNTRACK_PERCENT_KEY`].
pub const DEFAULT_CONNTRACK_PERCENT: u64 = 80;
/// The longest version string.
pub const MAX_VERSION_LEN: usize = 128;

/// Why a params file was refused.
#[derive(Debug, thiserror::Error)]
pub enum ParamsError {
    /// The file could not be read.
    #[error("cannot read the params file: {0}")]
    Read(#[from] io::Error),
    /// The text is not TOML.
    #[error("the params file is not a TOML table: {0}")]
    Syntax(String),
    /// A value is an array, a table or a date.
    #[error("params key {0:?} is not a scalar (a dotted key must be quoted)")]
    NotScalar(String),
    /// A key this hook needs is absent.
    #[error("params key {0:?} is required")]
    Missing(&'static str),
    /// A key that must be a whole number is not one.
    #[error("params key {0:?} must be a whole number, not negative")]
    NotNumber(String),
    /// A number is outside the range the key allows.
    #[error("params key {key:?} must be from {min} to {max}")]
    OutOfRange {
        /// The key.
        key: String,
        /// The least allowed.
        min: u64,
        /// The most allowed.
        max: u64,
    },
    /// A key that must be text is not.
    #[error("params key {0:?} must be a string")]
    NotText(String),
    /// The staged release is not an absolute path of plain components.
    #[error("params key {0:?} must be an absolute path of plain components")]
    NotPlain(&'static str),
    /// The target version holds a character a version does not.
    #[error("params key {0:?} is not a version of letters, digits and . _ + ~ -")]
    BadVersion(&'static str),
}

/// What the hook reads of its params file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    /// How long `verify` waits for `read-write` after the commit.
    pub promotion_bound_secs: u64,
    /// What a deferred `prepare` asks the updater to wait.
    pub defer_retry_secs: u64,
    /// The share of the conntrack table `prepare` accepts, in percent.
    pub conntrack_percent: u64,
    /// The updater's ceiling on a pending promotion; the hook never acts on it.
    pub promotion_ceiling_secs: Option<u64>,
    /// The verified staged release directory.
    pub staged_release: PathBuf,
    /// The version being installed.
    pub target_version: String,
}

impl Params {
    /// Reads a params file.
    ///
    /// # Errors
    /// [`ParamsError`]: the file cannot be read, or its text is refused.
    pub fn read(path: &Path) -> Result<Params, ParamsError> {
        Params::parse(&std::fs::read_to_string(path)?)
    }

    /// Reads the text of a params file.
    ///
    /// # Errors
    /// [`ParamsError`]: the text is not a flat table of scalars with the required keys.
    pub fn parse(text: &str) -> Result<Params, ParamsError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| ParamsError::Syntax(e.message().to_owned()))?;
        if let Some((key, _)) = table.iter().find(|(_, value)| {
            !matches!(
                value,
                toml::Value::Boolean(_)
                    | toml::Value::Integer(_)
                    | toml::Value::Float(_)
                    | toml::Value::String(_)
            )
        }) {
            return Err(ParamsError::NotScalar(key.clone()));
        }
        let bound = number(&table, PROMOTION_BOUND_KEY)?
            .ok_or(ParamsError::Missing(PROMOTION_BOUND_KEY))?;
        let defer = number(&table, DEFER_RETRY_KEY)?.unwrap_or(DEFAULT_DEFER_RETRY_SECS);
        let percent = number(&table, CONNTRACK_PERCENT_KEY)?.unwrap_or(DEFAULT_CONNTRACK_PERCENT);
        ranged(PROMOTION_BOUND_KEY, bound, 1, MAX_SECS)?;
        ranged(DEFER_RETRY_KEY, defer, 0, MAX_SECS)?;
        ranged(CONNTRACK_PERCENT_KEY, percent, 1, 100)?;
        let staged =
            text_of(&table, STAGED_RELEASE_KEY)?.ok_or(ParamsError::Missing(STAGED_RELEASE_KEY))?;
        let staged_release = PathBuf::from(staged);
        if !is_plain(&staged_release) {
            return Err(ParamsError::NotPlain(STAGED_RELEASE_KEY));
        }
        let target_version =
            text_of(&table, TARGET_VERSION_KEY)?.ok_or(ParamsError::Missing(TARGET_VERSION_KEY))?;
        if !is_version(&target_version) {
            return Err(ParamsError::BadVersion(TARGET_VERSION_KEY));
        }
        Ok(Params {
            promotion_bound_secs: bound,
            defer_retry_secs: defer,
            conntrack_percent: percent,
            promotion_ceiling_secs: number(&table, PROMOTION_CEILING_KEY)?,
            staged_release,
            target_version,
        })
    }
}

/// A version string: letters, digits and `. _ + ~ -`, not starting with `-`, no `..`.
fn is_version(text: &str) -> bool {
    (1..=MAX_VERSION_LEN).contains(&text.len())
        && !text.starts_with('-')
        && !text.contains("..")
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'~' | b'-'))
}

fn ranged(key: &str, value: u64, min: u64, max: u64) -> Result<(), ParamsError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ParamsError::OutOfRange {
            key: key.to_owned(),
            min,
            max,
        })
    }
}

/// `key` as a whole, non-negative number; `None` when it is absent.
fn number(table: &toml::Table, key: &str) -> Result<Option<u64>, ParamsError> {
    table
        .get(key)
        .map(|value| {
            value
                .as_integer()
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| ParamsError::NotNumber(key.to_owned()))
        })
        .transpose()
}

/// `key` as a string; `None` when it is absent.
fn text_of(table: &toml::Table, key: &str) -> Result<Option<String>, ParamsError> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ParamsError::NotText(key.to_owned()))
        })
        .transpose()
}
