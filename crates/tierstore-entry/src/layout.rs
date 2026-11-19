//! What the Linux store and the in-process store share about where an entry lives (section 2) and
//! how a holder's region is checked (5.1).
//!
//! Layout: `<cache>/packs-v2/aa/bb/<hash>.pack` (the sparse data file) and its journal and
//! snapshot beside it, where `<hash>` is the hex sha256 of the pack id and `aa`, `bb` its first
//! two bytes. The id includes `pack_root`, so a changed pack never reuses a file (8).

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tierstore_fetch_api::ids::{PackId, PackRoot};
use tierstore_fetch_api::page::CounterRegion;

/// The directory of the entries under the cache directory.
pub const PACKS_DIR: &str = "packs-v2";

/// Why a store could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The entries' directory could not be made.
    #[error("making {}: {source}", path.display())]
    Dir {
        /// The directory.
        path: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
}

/// Makes the entries' directory `cache/packs-v2` if it is missing; its path.
pub(crate) fn packs_dir(cache: &Path) -> Result<PathBuf, StoreError> {
    let dir = cache.join(PACKS_DIR);
    std::fs::create_dir_all(&dir).map_err(|source| StoreError::Dir {
        path: dir.clone(),
        source,
    })?;
    Ok(dir)
}

/// The hex sha256 that names `pack`'s files: of a version tag, the key's length and bytes, and
/// the root's kind and bytes, so no two ids give the same input.
#[must_use]
pub(crate) fn file_hash(pack: &PackId) -> String {
    let mut h = Sha256::new();
    h.update(b"tierstore-entry/1\0");
    h.update((pack.key.0.len() as u64).to_le_bytes());
    h.update(pack.key.0.as_bytes());
    match pack.root {
        PackRoot::Digest(d) => {
            h.update([0]);
            h.update(d);
        }
        PackRoot::Etag(g) => {
            h.update([1]);
            h.update(g.0);
        }
    }
    h.finalize().iter().fold(String::new(), |mut s, b| {
        // Writing to a String does not fail.
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The directory of `pack`'s files under the entries' directory `packs`, `packs/aa/bb`, and the
/// hash that names them.
pub(crate) fn entry_dir(packs: &Path, pack: &PackId) -> (PathBuf, String) {
    let hash = file_hash(pack);
    let dir = packs.join(&hash[0..2]).join(&hash[2..4]);
    (dir, hash)
}

/// Unlinks `path`; a path that is already gone is not an error.
pub(crate) fn unlink(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path).or_else(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(e)
        }
    })
}

/// The end of a holder's `region` of a pack of `groups` 16-cell groups, or `None` when the
/// region is misaligned, overflows or does not count the pack's groups (`attach`'s first check).
pub(crate) fn region_end(region: CounterRegion, groups: u32) -> Option<u64> {
    region
        .end()
        .filter(|_| region.is_valid() && region.groups == groups)
}
