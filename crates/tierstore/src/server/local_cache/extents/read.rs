//! What a cache with entries by extents answers: the start-up read of the directory, the span a
//! range is in, and the window of an ordinary read that an entry serves.

use std::path::{Path, PathBuf};

use crate::ByteRange;

use super::super::index::Index;
use super::super::layout::{Found, Name, data_path};
use super::super::metrics::{Counters, add};
use super::super::stream::window;
use super::super::{CacheConfig, Inner, LocalCache, locked};
use super::files;
use super::table::Object;

/// The index of the objects a scan found, the least recently used first.
fn index_of(mut found: Vec<Found>) -> Index {
    found.sort_by(|a, b| (a.used, &a.hex).cmp(&(b.used, &b.hex)));
    let mut index = Index::default();
    for found in found {
        let sidecar = found.sidecar;
        // Found on the disk: how long ago the upstream last held this version is not known.
        let (size, tag, modified) = (sidecar.size, sidecar.tag, sidecar.modified);
        index.insert_from(&found.hex, (size, tag, modified), None, sidecar.origin);
    }
    index
}

/// The entries by extents that the directory `extents-v1` holds, read back; the files that are
/// not part of a usable entry (or of an object `found` whole in `obj/`) are removed and counted in
/// `discarded_at_start`. `writable` is whether this cache holds the lock: a cache that does not
/// reads none, since the holder owns the directory.
pub(in crate::server::local_cache) fn scan(
    config: &CacheConfig,
    counters: &Counters,
    found: &[Found],
    writable: bool,
) -> Vec<files::Found> {
    if !writable {
        return Vec::new();
    }
    let whole = found.iter().map(|object| object.hex.as_str()).collect();
    let scanned = files::scan(&config.path, &whole);
    add(&counters.discarded_at_start, scanned.discarded);
    scanned.found
}

/// The index of the objects the scan of `obj/` found and the entries `scan` found, in one order of
/// use by the times of their files. An entry of a name `distrusted` says is removed: an object a
/// delete or put through this cache, while it was read-only, said is gone (see `Inner::promote`).
pub(in crate::server::local_cache) fn build_index(
    root: &Path,
    found: Vec<Found>,
    partials: Vec<files::Found>,
    distrusted: &dyn Fn(&str) -> bool,
) -> Index {
    let (gone, partials): (Vec<_>, Vec<_>) = partials.into_iter().partition(|f| distrusted(&f.hex));
    for entry in gone {
        files::remove(root, &entry.hex);
    }
    if partials.is_empty() {
        return index_of(found);
    }
    let mut order: Vec<(u64, String, Option<Found>, Option<files::Found>)> = Vec::new();
    order.extend(
        found
            .into_iter()
            .map(|f| (f.used, f.hex.clone(), Some(f), None)),
    );
    order.extend(
        partials
            .into_iter()
            .map(|f| (f.used, f.hex.clone(), None, Some(f))),
    );
    order.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let mut index = Index::default();
    for (_, hex, whole, partial) in order {
        if let Some(found) = whole {
            let sidecar = found.sidecar;
            let meta = (sidecar.size, sidecar.tag, sidecar.modified);
            index.insert_from(&hex, meta, None, sidecar.origin);
        }
        if let Some(found) = partial {
            let sidecar = found.sidecar;
            let (key, size) = (sidecar.key, sidecar.size);
            let (tag, modified) = (sidecar.tag, sidecar.modified);
            let object = Object {
                key,
                size,
                tag,
                modified,
            };
            index.extent_found(&hex, object, found.map);
        }
    }
    index
}

/// Where the bytes of a range are: the file of the pack and the place in it. The span is the
/// pack's own bytes, not a copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// The file that holds the range: the pack's file in `obj/` if the pack is whole in the
    /// cache, else its file in `extents-v1/`.
    pub path: PathBuf,
    /// Where the range starts in that file, which is where it starts in the pack.
    pub offset: u64,
    /// Whether the pack is whole in the cache (the file is its whole object).
    pub whole: bool,
}

impl LocalCache {
    /// The file that holds `[offset, offset + len)` of the pack and where it starts, counted as
    /// used; none if the cache does not hold the range.
    pub fn span_of(&self, pack: &str, offset: u64, len: u64) -> Option<Span> {
        let inner = &self.inner;
        let name = inner.cacheable(pack)?;
        let end = offset.checked_add(len).filter(|_| len > 0)?;
        let mut index = locked(&inner.index);
        let now = inner.clock.now();
        if index.get(&name.hex).is_some_and(|entry| end <= entry.size) {
            let _ = index.used(&name.hex, now);
            let path = data_path(&inner.config.path, &name.hex);
            return Some(Span {
                path,
                offset,
                whole: true,
            });
        }
        let held = index.partials.get(&name.hex)?;
        if !held.map.covers(offset, end) {
            return None;
        }
        let _ = index.extent_used(&name.hex, now);
        let path = files::data_of(&inner.config.path, &name.hex);
        Some(Span {
            path,
            offset,
            whole: false,
        })
    }

    /// The number of entries by extents, and the bytes they have filled.
    #[cfg(test)]
    pub fn extents_held(&self) -> (usize, u64) {
        let index = locked(&self.inner.index);
        (index.partials.len(), index.partials.bytes())
    }
}

impl Inner {
    /// The size of the object and the window `range` asks of it, if an entry by extents holds the
    /// whole window and an ordinary read may be served from it, counted as used: a read of a
    /// range only, and only where no check of old hits against the upstream is set (an entry has
    /// no age to check).
    pub(in crate::server::local_cache) fn extent_window(
        &self,
        name: &Name,
        range: Option<ByteRange>,
    ) -> Option<(u64, (u64, u64))> {
        let range = range?;
        if self.config.revalidate_after.is_some() {
            return None;
        }
        let mut index = locked(&self.index);
        let entry = index.partials.get(&name.hex)?;
        let size = entry.object.size;
        let (offset, len) = window(Some(range), size).ok()?;
        if !entry.map.covers(offset, offset + len) {
            return None;
        }
        let _ = index.extent_used(&name.hex, self.clock.now());
        Some((size, (offset, len)))
    }

    /// The path of the data file of the entry `hex`.
    pub(in crate::server::local_cache) fn extent_path(&self, hex: &str) -> PathBuf {
        files::data_of(&self.config.path, hex)
    }

    /// Touches the file of the entry `hex` to `at`, for the order of use a restart reads back.
    pub(in crate::server::local_cache) fn touch_extent(&self, hex: &str, at: u64) {
        let _ = self.disk.touch(&self.extent_path(hex), at);
    }
}
