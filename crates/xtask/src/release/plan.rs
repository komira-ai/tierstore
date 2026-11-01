//! `release plan` and `release manual`: which tag a push to `main` is released as, or whether
//! it is released at all.

use std::path::Path;

use super::publish::Gh;
use crate::Error;

/// The paths whose change alone makes no release.
pub const DOCS_PATHS: [&str; 4] = ["docs/", "**/*.md", "crates/*/docs/", "crates/*/site/"];

/// What `release plan` is asked.
pub struct Request<'a> {
    pub repo: &'a str,
    pub commit: &'a str,
    pub version: Option<&'a str>,
    pub freeze: &'a str,
    pub output: Option<&'a Path>,
}

/// Whether `path` is under [`DOCS_PATHS`].
pub fn is_docs_path(_path: &str) -> bool {
    todo!("is_docs_path")
}

/// Whether a change of exactly these files makes no release.
pub fn docs_only(_files: &[String]) -> bool {
    todo!("docs_only")
}

/// The tag of the next release.
pub fn next_tag(_latest: Option<&str>, _version_file: Option<&str>) -> Result<String, Error> {
    todo!("next_tag")
}

/// The text of the `VERSION` file at `path`, none when there is no such file.
pub fn read_version(_path: &Path) -> Result<Option<String>, Error> {
    todo!("read_version")
}

/// The `release plan` verb: the lines to print.
pub fn plan(_gh: &dyn Gh, _request: &Request) -> Result<Vec<String>, Error> {
    todo!("plan")
}

/// The `release manual` verb: the lines to print.
pub fn manual(
    _tag: &str,
    _commit: &str,
    _note: &str,
    _output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    todo!("manual")
}

#[cfg(test)]
mod tests;
