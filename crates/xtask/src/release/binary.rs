//! `release check-binary`: the linux-x86_64 binary of a release says the tag it is released
//! under and the commit it was built from.

use std::path::Path;

use crate::Error;
use crate::build_commit::Said;

/// Checks the text `tierstore --version` printed against the release `version`
/// (`<major>.<minor>.<patch>`) of `commit`.
pub fn check_said(_said: &str, _version: &str, _commit: &str) -> Result<(), Error> {
    todo!("check_said")
}

/// The `release check-binary` verb: unpacks the linux-x86_64 tarball of `commit` in `dir`
/// into `scratch`, runs it with `--version`, and refuses it unless it says `tag` and `commit`.
pub fn check_binary(
    _dir: &Path,
    _tag: &str,
    _commit: &str,
    _scratch: &Path,
    _run: fn(&Path) -> Result<Said, Error>,
) -> Result<Vec<String>, Error> {
    todo!("check_binary")
}

/// `build-commit --version`: the binary at `binary` says `version` and `commit`.
pub fn check_built(
    _binary: &Path,
    _version: &str,
    _commit: &str,
    _run: fn(&Path) -> Result<Said, Error>,
) -> Result<String, Error> {
    todo!("check_built")
}

#[cfg(test)]
mod tests;
