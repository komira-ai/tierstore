//! The L2 sidecar of a tree's version 2 list, made as the storage tier's writer makes it: the
//! cell digests of every pack the list names, read from the store as each pack streams past.

use std::collections::BTreeMap;
use std::path::Path;

use futures::stream::{self, StreamExt as _, TryStreamExt as _};
use tierstore_list::{CellHasher, PackCells, PackInfo, sidecar_from_v2};

use crate::Error;
use crate::swfs::FilerFetch;

/// The cells of the pack `key`, which is `size` bytes: its whole body is read once, in the pieces
/// the connection gives, and each piece is hashed and dropped.
///
/// # Errors
/// The read failed, or the object is not `size` bytes long.
pub async fn pack_cells(fetch: &FilerFetch, key: &str, size: u64) -> Result<PackCells, Error> {
    if size == 0 {
        return Ok(PackCells::default());
    }
    let mut body = fetch.read_range(key, 0, size).await?.body;
    let mut hasher = CellHasher::new();
    while let Some(piece) = body.try_next().await? {
        hasher.update(&piece);
    }
    if hasher.size() != size {
        return Err(Error::parse(
            key,
            format!("read {} bytes of a pack of {size}", hasher.size()),
        ));
    }
    Ok(hasher.finish())
}

/// The sidecar of the version 2 list `v2`: the cells of every pack of `sizes` (the packs the
/// list names, with their sizes), `parallel` packs at a time.
///
/// # Errors
/// A pack could not be read ([`pack_cells`]), or the sidecar writer refused the list or the
/// cells (the error says why).
pub async fn build(
    v2: &[u8],
    fetch: &FilerFetch,
    sizes: &BTreeMap<String, u64>,
    parallel: usize,
) -> Result<Vec<u8>, Error> {
    let cells: BTreeMap<String, PackCells> = stream::iter(sizes)
        .map(|(key, size)| async move {
            pack_cells(fetch, key, *size)
                .await
                .map(|cells| (key.clone(), cells))
        })
        .buffer_unordered(parallel.max(1))
        .try_collect()
        .await?;
    sidecar_from_v2(v2, |key| {
        cells.get(key).map(|cells| PackInfo {
            cells: cells.clone(),
            ..PackInfo::default()
        })
    })
    .map_err(|e| Error::ReadPath {
        what: "sidecar",
        why: e.to_string(),
    })
}

/// The bytes of the file `path`, or `None` when there is no such file.
///
/// # Errors
/// The file exists and cannot be read.
pub fn read_or_none(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io("read", path, e)),
    }
}
