//! The pkg's one script, `postinstall` ([`POSTINSTALL`], the file
//! `packaging/macos-pkg/postinstall`), which makes the account the jobs run as, `_tierstore`, the
//! directories under `/usr/local/tierstore` and the mount point of `tierstore fs` (which it leaves
//! as it is while it is mounted); the file says what it does, and the tests of
//! `tests/postinstall.rs` run it.
//!
//! `release macos-pkg` writes it into the scripts directory it hands `pkgbuild` (`--scripts`),
//! which archives it into the component and names it in `PackageInfo`. The read-back
//! ([`package_info_scripts_problems`], [`scripts_problems`]) holds the built pkg to exactly that
//! script: `PackageInfo` names `postinstall` and no `preinstall`, and the `Scripts` directory
//! `pkgutil --expand` makes of the component's scripts archive holds `postinstall` alone, a
//! regular file of mode 0755 with exactly these bytes.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::{Error, TYPE_BITS};

/// The script's name, in the scripts directory and in the expanded component's `Scripts`.
pub(super) const NAME: &str = "postinstall";

/// The script the pkg holds.
pub const POSTINSTALL: &str = include_str!("../../../../../packaging/macos-pkg/postinstall");

/// The mode of the script: the installer runs it.
pub(super) const SCRIPT_MODE: u32 = 0o755;

/// The directory of an expanded component that holds its scripts.
pub(super) const SCRIPTS: &str = "Scripts";

/// The element `pkgbuild` writes in `PackageInfo` for a `postinstall` in the scripts directory.
const POSTINSTALL_ELEMENT: &str = "<postinstall file=\"./postinstall\"";

/// What is wrong with the scripts the `PackageInfo` text `text` names, one line each: it names
/// the `postinstall` and no `preinstall`.
pub(super) fn package_info_scripts_problems(text: &str, problems: &mut Vec<String>) {
    if text.contains("<preinstall") {
        problems.push(
            "PackageInfo names a preinstall element: tierstore's pkg has one script, its \
             postinstall"
                .to_owned(),
        );
    }
    if !text.contains(POSTINSTALL_ELEMENT) {
        problems.push(format!(
            "PackageInfo names no postinstall script ({POSTINSTALL_ELEMENT}/>): nothing would \
             make the account and the directories of the job"
        ));
    }
}

/// What is wrong with the `Scripts` directory `dir` of an expanded component, one line each: it
/// holds exactly `postinstall`, a regular file (not a link, which is not followed) of mode 0755
/// with the bytes of [`POSTINSTALL`]. The owner is not checked: the installer runs the script as
/// root whoever owns it, and installs nothing from it.
///
/// # Errors
/// The directory or the script cannot be read.
pub(super) fn scripts_problems(dir: &Path, problems: &mut Vec<String>) -> Result<(), Error> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .and_then(|read| read.collect::<Result<Vec<_>, _>>())
        .map_err(Error::spawn(format!("read {}", dir.display())))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut found = false;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != NAME {
            problems.push(format!(
                "{SCRIPTS} holds {name:?}: tierstore's pkg has one script, its {NAME}"
            ));
            continue;
        }
        found = true;
        let path = entry.path();
        let what = format!("read {}", path.display());
        let meta = std::fs::symlink_metadata(&path).map_err(Error::spawn(what.clone()))?;
        if !meta.file_type().is_file() {
            problems.push(format!(
                "{SCRIPTS}: {NAME} is of type {:o}, not a regular file",
                meta.mode() & TYPE_BITS
            ));
            continue;
        }
        let mode = meta.mode() & !TYPE_BITS;
        if mode != SCRIPT_MODE {
            problems.push(format!(
                "{SCRIPTS}: {NAME} has mode {mode:o}, not {SCRIPT_MODE:o}"
            ));
        }
        let bytes = std::fs::read(&path).map_err(Error::spawn(what))?;
        if bytes != POSTINSTALL.as_bytes() {
            problems.push(format!(
                "{SCRIPTS}: {NAME} is not the postinstall release macos-pkg writes"
            ));
        }
    }
    if !found {
        problems.push(format!("{SCRIPTS} holds no {NAME}"));
    }
    Ok(())
}
