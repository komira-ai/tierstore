//! Where the rpm tree of the shared repository keeps what, as object keys. Pure functions.
//!
//! Under `rpm/` (docs/design/distribution.md, section 8): `pool/<product>/<file>.rpm`, written
//! once and shared by both suites and both architectures, and for each suite and architecture
//! `<suite>/<arch>/repodata/` with `repomd.xml`, its detached signature `repomd.xml.asc`, and the
//! `<sha256>-{primary,filelists,other}.xml.gz` they name. Only `repomd.xml` and `repomd.xml.asc` are
//! ever replaced, each with `If-Match` on the ETag that was read; every other object is named by its
//! content and written once.

use crate::Error;

pub use crate::channels::apt::state::{
    ABSENT, ETAGS_FILE, Suite, etag_of, parse_etags, render_etags,
};

/// The key prefix of everything the rpm tree serves.
pub const KEY_ROOT: &str = "rpm/";

/// The pool directory this product owns.
pub const POOL_DIR: &str = "pool/tierstore";

/// The architectures of the repositories, sorted.
pub const ARCHS: [&str; 2] = ["aarch64", "x86_64"];

pub const REPOMD: &str = "repomd.xml";
pub const REPOMD_ASC: &str = "repomd.xml.asc";

/// The path of a file of the `repodata` of `arch`, relative to the suite.
pub fn path(arch: &str, file: &str) -> String {
    format!("{arch}/repodata/{file}")
}

/// The object key of a file of the `repodata` of `arch` in `suite`.
pub fn repodata_key(suite: Suite, arch: &str, file: &str) -> String {
    format!("{KEY_ROOT}{}/{}", suite.name(), path(arch, file))
}

/// The object key of a package file in this product's pool directory.
///
/// # Errors
/// A name with a separator or a leading dot, or one that is not a `.rpm`: it would leave the
/// directory.
pub fn pool_key(file: &str) -> Result<String, Error> {
    if file.is_empty() || file.starts_with('.') || file.contains('/') || !file.ends_with(".rpm") {
        return Err(Error::Failed(format!(
            "{file:?} is not the name of a package file for the pool"
        )));
    }
    Ok(format!("{KEY_ROOT}{POOL_DIR}/{file}"))
}
