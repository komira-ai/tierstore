//! `farm-check --buck2 <path>`: the farm lane's check before its build (`.github/workflows/farm.yml`,
//! docs/farm.md). `buck2 audit config buildrules_re.linux_x86_64_properties --style json` must give
//! the key a value that is not blank: the worker property set farm-connect wrote, which `.buckconfig`
//! hands to buildrules. Without one buck2 has no execution platform for the farm, and this says so
//! before the build starts. It prints the line it checked.
//!
//! The build itself is not run here: farm.yml runs `buck2 build //:ci` through komira's buck2-farm
//! action, which runs it again after a cut or dead connection to the farm (its watchdog ends a run
//! whose connections to the farm have all gone silent) and fails it unless every summary line says
//! `local: 0`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The key of the worker property set buildrules reads.
pub const PROPERTIES_KEY: &str = "buildrules_re.linux_x86_64_properties";

/// What [`check`] prints when the key has a value.
pub const SET_LINE: &str = "buildrules_re.linux_x86_64_properties is set: buck2 builds on the farm";

/// Why the farm lane cannot build.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("cannot run {program:?}: {why}")]
    Start { program: PathBuf, why: String },
    #[error("`buck2 audit config` failed ({status}): {stderr}")]
    Audit { status: String, stderr: String },
    #[error(
        "buildrules_re.linux_x86_64_properties is empty: farm-connect wrote no [komira_re] \
         linux_x86_64_properties, so buck2 has no farm to build on"
    )]
    NoWorkers,
}

/// Whether `json`, what `buck2 audit config <key> --style json` printed, gives
/// [`PROPERTIES_KEY`] a value that is not blank.
pub fn has_value(json: &str) -> bool {
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json)
        .ok()
        .and_then(|config| config.get(PROPERTIES_KEY).cloned())
        .and_then(|value| value.as_str().map(|v| !v.trim().is_empty()))
        .unwrap_or(false)
}

/// Asks the buck2 at `buck2` for [`PROPERTIES_KEY`]: [`SET_LINE`] when it has a value.
///
/// # Errors
/// buck2 cannot be started, its audit fails, or the key has no value.
pub fn check(buck2: &Path) -> Result<String, Error> {
    let audit = Command::new(buck2)
        .args(["audit", "config", PROPERTIES_KEY, "--style", "json"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::Start {
            program: buck2.to_owned(),
            why: e.to_string(),
        })?;
    if !audit.status.success() {
        return Err(Error::Audit {
            status: audit.status.to_string(),
            stderr: String::from_utf8_lossy(&audit.stderr).trim().to_owned(),
        });
    }
    if !has_value(&String::from_utf8_lossy(&audit.stdout)) {
        return Err(Error::NoWorkers);
    }
    Ok(SET_LINE.to_owned())
}

#[cfg(test)]
mod tests;
