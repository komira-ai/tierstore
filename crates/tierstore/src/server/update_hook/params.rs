//! The `--params <file>` table.
//!
//! hostkeeper writes one TOML file of scalars per run and passes it to every step: the
//! manifest's opaque `[params]` and its own reserved keys, which are dotted and so quoted in the
//! file (`"updater.staged_release" = "..."`). This hook reads:
//!
//! - `updater.staged_release` and `updater.target_version` (required): the version directory
//!   hostkeeper verified and unpacked, and the version its binary must report;
//! - `updater.previous_release` and `updater.previous_version` (both or neither): the release
//!   the host ran before; neither on a first install;
//! - `updater.promotion_ceiling_secs` (optional): read, and never a reason to fail, since
//!   hostkeeper applies the ceiling itself;
//! - `service_manager` (optional): `"systemd"` (the default) or `"launchd"`. Any other value is
//!   kept as [`ServiceManager::Unknown`], which every step refuses with the reason
//!   `service_manager_unknown`;
//! - `launchd_label` (required with `service_manager = "launchd"`): the label of the one job;
//! - `config_file` (optional here, required by the `prepare` that reads it): the path of the
//!   node's tierstore configuration;
//! - `mode` (optional): the string `"restart"`, the one value accepted, which makes an update of
//!   the systemd arm a plain restart even when both releases could hand their sockets over. Set
//!   on a storage node whose hot tier is a `dir:` tier, where the lease's compare-and-swap is
//!   atomic within one process only, so two instances must never run side by side.
//!
//! Any other key is ignored when it is a scalar, so an older hook keeps working when hostkeeper
//! adds a reserved key or the manifest a key of its own.

use std::io;
use std::path::{Path, PathBuf};

use super::plain::is_plain;

/// The reserved key of the verified staged release directory.
pub const STAGED_RELEASE_KEY: &str = "updater.staged_release";
/// The reserved key of the version being installed.
pub const TARGET_VERSION_KEY: &str = "updater.target_version";
/// The reserved key of the release the host ran before; absent on a first install.
pub const PREVIOUS_RELEASE_KEY: &str = "updater.previous_release";
/// The reserved key of the version the host ran before; absent on a first install.
pub const PREVIOUS_VERSION_KEY: &str = "updater.previous_version";
/// The reserved key of hostkeeper's ceiling on a pending promotion.
pub const PROMOTION_CEILING_KEY: &str = "updater.promotion_ceiling_secs";
/// The key that names the service manager.
pub const SERVICE_MANAGER_KEY: &str = "service_manager";
/// The key of the label of the launchd job.
pub const LAUNCHD_LABEL_KEY: &str = "launchd_label";
/// The key of the node's configuration file.
pub const CONFIG_FILE_KEY: &str = "config_file";
/// The key that forces a plain restart.
pub const MODE_KEY: &str = "mode";
/// The one value [`MODE_KEY`] takes.
pub const RESTART_MODE: &str = "restart";
/// The value of [`SERVICE_MANAGER_KEY`] for systemd.
pub const SYSTEMD: &str = "systemd";
/// The value of [`SERVICE_MANAGER_KEY`] for launchd.
pub const LAUNCHD: &str = "launchd";

/// The longest version string.
pub const MAX_VERSION_LEN: usize = 255;
/// The longest launchd label.
pub const MAX_LABEL_LEN: usize = 255;

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
    NotNumber(&'static str),
    /// A key that must be text is not.
    #[error("params key {0:?} must be a string")]
    NotText(&'static str),
    /// A path is not an absolute path of plain components.
    #[error("params key {0:?} must be an absolute path of plain components")]
    NotPlain(&'static str),
    /// A version holds a character a version does not.
    #[error("params key {0:?} is not a version of letters, digits and . _ + ~ -")]
    BadVersion(&'static str),
    /// The previous release without the previous version, or the other way round.
    #[error(
        "{PREVIOUS_RELEASE_KEY} and {PREVIOUS_VERSION_KEY} go together: a first install has neither, an update both"
    )]
    PreviousPair,
    /// `mode` is not the string `"restart"`.
    #[error("params key {0:?} must be the string \"restart\", the one mode it can force")]
    BadMode(&'static str),
    /// The launchd label holds a character a label does not.
    #[error("params key {0:?} is not a label of 1 to 255 letters, digits and . _ -")]
    BadLabel(&'static str),
}

/// The service manager the hook drives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceManager {
    /// systemd: two instances of one template, `tierstore@a` and `tierstore@b`.
    Systemd,
    /// launchd: one job.
    Launchd {
        /// Its label (`launchd_label`).
        label: String,
    },
    /// A value this hook does not know, as the file spells it: every step refuses it.
    Unknown(String),
}

/// The release the host ran before an update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Previous {
    /// Its version directory.
    pub release: PathBuf,
    /// Its version.
    pub version: String,
}

/// What the hook reads of its params file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    /// The verified staged release directory.
    pub staged_release: PathBuf,
    /// The version being installed.
    pub target_version: String,
    /// The release the host ran before; `None` on a first install.
    pub previous: Option<Previous>,
    /// hostkeeper's ceiling on a pending promotion; the hook never acts on it.
    pub promotion_ceiling_secs: Option<u64>,
    /// The service manager.
    pub service_manager: ServiceManager,
    /// The node's configuration file.
    pub config_file: Option<PathBuf>,
    /// Whether the params say `mode = "restart"`.
    pub declared_restart: bool,
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
        let staged_release =
            path_of(&table, STAGED_RELEASE_KEY)?.ok_or(ParamsError::Missing(STAGED_RELEASE_KEY))?;
        let target_version = version_of(&table, TARGET_VERSION_KEY)?
            .ok_or(ParamsError::Missing(TARGET_VERSION_KEY))?;
        let previous = match (
            path_of(&table, PREVIOUS_RELEASE_KEY)?,
            version_of(&table, PREVIOUS_VERSION_KEY)?,
        ) {
            (Some(release), Some(version)) => Some(Previous { release, version }),
            (None, None) => None,
            _ => return Err(ParamsError::PreviousPair),
        };
        let promotion_ceiling_secs = table
            .get(PROMOTION_CEILING_KEY)
            .map(|value| {
                value
                    .as_integer()
                    .and_then(|n| u64::try_from(n).ok())
                    .ok_or(ParamsError::NotNumber(PROMOTION_CEILING_KEY))
            })
            .transpose()?;
        let service_manager = match table.get(SERVICE_MANAGER_KEY) {
            None => ServiceManager::Systemd,
            Some(toml::Value::String(word)) if word == SYSTEMD => ServiceManager::Systemd,
            Some(toml::Value::String(word)) if word == LAUNCHD => {
                let label = text_of(&table, LAUNCHD_LABEL_KEY)?
                    .ok_or(ParamsError::Missing(LAUNCHD_LABEL_KEY))?;
                if !is_label(&label) {
                    return Err(ParamsError::BadLabel(LAUNCHD_LABEL_KEY));
                }
                ServiceManager::Launchd { label }
            }
            Some(toml::Value::String(word)) => ServiceManager::Unknown(word.clone()),
            Some(other) => ServiceManager::Unknown(other.to_string()),
        };
        let declared_restart = match table.get(MODE_KEY) {
            None => false,
            Some(toml::Value::String(text)) if text == RESTART_MODE => true,
            Some(_) => return Err(ParamsError::BadMode(MODE_KEY)),
        };
        Ok(Params {
            staged_release,
            target_version,
            previous,
            promotion_ceiling_secs,
            service_manager,
            config_file: path_of(&table, CONFIG_FILE_KEY)?,
            declared_restart,
        })
    }

    /// Whether this is a first install: no previous release.
    #[must_use]
    pub fn first_install(&self) -> bool {
        self.previous.is_none()
    }
}

/// A version string: letters, digits and `. _ + ~ -`, not starting with `-`, no `..`.
pub(super) fn is_version(text: &str) -> bool {
    (1..=MAX_VERSION_LEN).contains(&text.len())
        && !text.starts_with('-')
        && !text.contains("..")
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'~' | b'-'))
}

/// A launchd label: 1 to [`MAX_LABEL_LEN`] letters, digits and `. _ -`, starting with a letter
/// or digit. Anything else is refused before it reaches `launchctl`.
pub(super) fn is_label(text: &str) -> bool {
    (1..=MAX_LABEL_LEN).contains(&text.len())
        && text
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `key` as a string; `None` when it is absent.
fn text_of(table: &toml::Table, key: &'static str) -> Result<Option<String>, ParamsError> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or(ParamsError::NotText(key))
        })
        .transpose()
}

/// `key` as an absolute path of plain components; `None` when it is absent.
fn path_of(table: &toml::Table, key: &'static str) -> Result<Option<PathBuf>, ParamsError> {
    text_of(table, key)?
        .map(|text| {
            let path = PathBuf::from(text);
            if is_plain(&path) {
                Ok(path)
            } else {
                Err(ParamsError::NotPlain(key))
            }
        })
        .transpose()
}

/// `key` as a version; `None` when it is absent.
fn version_of(table: &toml::Table, key: &'static str) -> Result<Option<String>, ParamsError> {
    text_of(table, key)?
        .map(|text| {
            if is_version(&text) {
                Ok(text)
            } else {
                Err(ParamsError::BadVersion(key))
            }
        })
        .transpose()
}
