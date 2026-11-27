//! The copy of a pack that the cache holds whole, which a fill of a blob inside it reads from in
//! place of the upstream.
//!
//! A write-through put of a pack (the core's `put`, the put ingest) keeps the pack whole in
//! `obj/`, under the name of its key (see `keep`). The files of a tree of manifests name each blob
//! by its digest and the span of the pack it lies in, and the fill of one (`source`, `prefetch`)
//! is kept under the digest's name: another name. So the fill looks here first. If the cache's
//! index holds the pack whole, the span lies inside it, and its file is there at the size the
//! index records, the fill reads the span from that file, one `fill_chunk` at a time, as it would
//! from the upstream, and hashes it as it passes. Nothing is asked of the upstream.
//!
//! The bytes are checked as the upstream's are: a blob that does not hash to its digest is not
//! kept. A read of the copy that fails, or a copy whose span does not hash to the digest (a disk
//! that lost bytes; the scrub finds such a copy and removes it), costs the fill one try: it is
//! made again from the upstream, and said at warn. A copy that is not there, or is not the size
//! the index says, is passed over without a word: the fill goes to the upstream as it did before.
//!
//! The copy is not counted as used by the read: once the blobs are kept under their own names, the
//! pack is what the eviction may take first. An entry by extents of the pack (module `extents`) is
//! not looked at: the write-through put leaves the pack whole, and a range that an entry by
//! extents holds is the span a placed tree reads, not a blob of a tree of manifests.

use std::fs::File;
use std::sync::Arc;

use super::super::layout::{data_path, name_of};
use super::super::{Inner, locked};

impl Inner {
    /// The file of the pack `from`, open, if the cache holds the pack whole in `obj/` and the
    /// `size` bytes from `offset` lie inside it: the index has the entry, and the file is there
    /// at the size the entry records (a file a power cut left short is not read). None otherwise.
    pub(in super::super) async fn held_pack(
        &self,
        from: &str,
        offset: u64,
        size: u64,
    ) -> Option<Arc<File>> {
        let hex = name_of(from).hex;
        let whole = locked(&self.index).get(&hex)?.size;
        if offset.saturating_add(size) > whole {
            return None;
        }
        let file = tokio::fs::File::open(data_path(&self.config.path, &hex))
            .await
            .ok()?;
        let length = file.metadata().await.ok()?.len();
        let file = file.into_std().await;
        (length == whole).then(|| Arc::new(file))
    }
}

#[cfg(test)]
#[path = "held_tests.rs"]
mod tests;
