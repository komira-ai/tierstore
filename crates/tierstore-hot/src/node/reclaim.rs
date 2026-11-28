//! Reclaim and the dirty marks (10.5 "Reclaim of hot space", "Headroom and backpressure").
//!
//! Reclaim drops only clean bytes and never copies: cache segments first, whole, least recently
//! read first (one unlink each), then clean write segments and large files (destaged, their cold
//! copy committed), whole, least recently read first. Dirty bytes are never dropped.
//!
//! The marks are fractions of the hot device: above `hot.dirty_slow` dirty bytes a PUT answers
//! 503 `Retry-After` (`hot_put_throttled_total`); above `hot.dirty_reject` of total fill, after a
//! reclaim, it answers 507. In hot-only mode (no cold store) every hot byte counts as dirty.

use std::sync::Arc;

use tierstore_format::names::HotName;
use tierstore_metrics::labels::ReclaimKind;
use tierstore_store_api::hot::{HotError, Unavailable};
use tierstore_store_api::sched::DriveClass;

use super::{FileInfo, FileKind, HotNode, Node, io_err, unavailable};

/// What a reclaim dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reclaimed {
    /// Bytes of cache segments unlinked.
    pub cache_bytes: u64,
    /// Bytes of clean write segments and large files unlinked.
    pub clean_bytes: u64,
    /// The files unlinked, in order.
    pub files: Vec<String>,
}

/// The hot tier's fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    /// Bytes not yet committed to the cold tier (every byte in hot-only mode).
    pub dirty: u64,
    /// Every byte of the hot files.
    pub total: u64,
    /// The device's bytes.
    pub capacity: u64,
}

impl Node {
    pub(crate) fn fill(&self) -> Fill {
        let st = self.lock();
        let open: u64 = st.writing.values().map(|s| s.lock().written).sum::<u64>()
            + st.copies.values().map(|c| c.committed).sum::<u64>();
        let hot_only = self.cfg.hot_only;
        let (mut dirty, mut total) = (open, open);
        for f in st.files.values() {
            total += f.bytes;
            if f.dirty || hot_only {
                dirty += f.bytes;
            }
        }
        Fill {
            dirty,
            total,
            capacity: self.cfg.capacity,
        }
    }

    /// The marks a PUT passes before it writes: 503 above `dirty_slow`, 507 above
    /// `dirty_reject` of total fill once a reclaim has run.
    pub(crate) async fn admit_put(self: &Arc<Self>) -> Result<(), HotError> {
        let cap = self.cfg.capacity as f64;
        let fill = self.fill();
        let reject = (self.cfg.marks.reject * cap) as u64;
        if fill.total > reject {
            self.reclaim(fill.total - reject).await?;
            if self.fill().total > reject {
                return Err(HotError::Full);
            }
        }
        if fill.dirty as f64 > self.cfg.marks.slow * cap {
            self.metrics.hot_put_throttled_total.with(()).inc();
            return Err(unavailable(Unavailable::Throttled));
        }
        Ok(())
    }

    /// Drops clean bytes until `need` bytes are freed or nothing clean is left.
    pub(crate) async fn reclaim(&self, need: u64) -> Result<Reclaimed, HotError> {
        let mut out = Reclaimed::default();
        let order: Vec<(String, FileInfo)> = {
            let st = self.lock();
            let mut cache: Vec<_> = st
                .files
                .iter()
                .filter(|(_, f)| f.kind == FileKind::Cache)
                .map(|(p, f)| (p.clone(), *f))
                .collect();
            let mut clean: Vec<_> = st
                .files
                .iter()
                .filter(|(_, f)| f.kind != FileKind::Cache && !f.dirty)
                .map(|(p, f)| (p.clone(), *f))
                .collect();
            cache.sort_by_key(|(p, f)| (f.last_read_ms, p.clone()));
            clean.sort_by_key(|(p, f)| (f.last_read_ms, p.clone()));
            cache.into_iter().chain(clean).collect()
        };
        for (path, f) in order {
            if out.cache_bytes + out.clean_bytes >= need {
                break;
            }
            self.drop_file(&path).await?;
            let kind = match f.kind {
                FileKind::Cache => {
                    out.cache_bytes += f.bytes;
                    ReclaimKind::CacheSegment
                }
                FileKind::Segment(_) => {
                    out.clean_bytes += f.bytes;
                    ReclaimKind::CleanSegment
                }
                FileKind::Large(_) => {
                    out.clean_bytes += f.bytes;
                    ReclaimKind::CleanLargeFile
                }
            };
            self.metrics.hot_reclaim_bytes_total.with(kind).add(f.bytes);
            out.files.push(path);
        }
        Ok(out)
    }

    /// Unlinks `path` and forgets it.
    pub(crate) async fn drop_file(&self, path: &str) -> Result<(), HotError> {
        let (dev, p) = (Arc::clone(&self.dev), path.to_owned());
        self.sched
            .run(DriveClass::Background, move || {
                match dev.unlink(&p) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
                dev.fsync_dir(crate::dev::dir_of(&p))
            })
            .await?
            .map_err(io_err("reclaim"))?;
        self.lock().files.remove(path);
        Ok(())
    }
}

impl HotNode {
    /// The hot tier's fill now.
    pub fn fill(&self) -> Fill {
        self.n.fill()
    }

    /// Drops clean bytes (cache segments, then destaged copies, least recently read first) until
    /// `need` bytes are freed; dirty bytes are never dropped.
    pub async fn reclaim(&self, need: u64) -> Result<Reclaimed, HotError> {
        self.n.reclaim(need).await
    }

    /// The destage of `file` is committed (its cold copy is durable, 10.5 step 7): its bytes are
    /// clean. With `hot.keep_clean` they stay as a read cache until space is needed; without, the
    /// file is unlinked now. `false` when the node holds no such file.
    pub async fn mark_clean(&self, file: HotName) -> Result<bool, HotError> {
        let path = file.path();
        let known = match self.n.lock().files.get_mut(&path) {
            Some(f) => {
                f.dirty = false;
                true
            }
            None => false,
        };
        if known && !self.n.cfg.keep_clean {
            self.n.drop_file(&path).await?;
        }
        Ok(known)
    }

    /// Registers a cache segment of promoted copies (10.5 "Promote on read"): clean, single
    /// copy, never destaged, the first bytes reclaim drops.
    pub fn add_cache_segment(&self, boot: u64, seq: u64, bytes: u64) {
        let now = self.n.clock.now_ms();
        self.n.lock().files.insert(
            HotName::Cache { boot, seq }.path(),
            FileInfo {
                kind: FileKind::Cache,
                bytes,
                data_len: bytes,
                dirty: false,
                last_read_ms: now,
                last_scrub_ms: now,
            },
        );
    }
}
