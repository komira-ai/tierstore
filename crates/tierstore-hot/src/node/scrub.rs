//! Hot scrub (10.3 "Hot scrub"): sealed segments, large objects and cache segments are re-read
//! once per `hot.scrub_period`, every record crc and the block's trailer (its entries, cell crcs
//! and blake3, a deterministic function of the bytes) checked. A bad copy of a segment or large
//! object is replaced by another node's copy, fetched window by window into a new file, verified
//! the same way and renamed over it, never rewritten in place; a bad cache segment is dropped.
//! Every check reads its file in windows: nothing holds a whole file.

use std::sync::Arc;

use tierstore_fetch_api::ids::ByteRange;
use tierstore_format::block::BlockBuilder;
use tierstore_format::crc32c_append;
use tierstore_format::digest::GenerationHasher;
use tierstore_format::names::HotName;
use tierstore_format::record::{RECORD_HEADER_LEN, RecordHeader};
use tierstore_format::trailer::BlockKind;
use tierstore_store_api::hot::HotError;
use tierstore_store_api::sched::DriveClass;

use super::{FileInfo, FileKind, HotNode, Node, other_err};

/// What a scrub pass found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrubReport {
    /// Files read.
    pub checked: u64,
    /// Files found bad.
    pub bad: u64,
    /// Bad files replaced by a good copy.
    pub repaired: u64,
    /// Bad cache segments dropped.
    pub dropped: u64,
}

/// The name of a scrubbed file whose copy another node can send (a cache segment has none: a
/// bad one is dropped).
fn name_of(f: &FileInfo) -> Option<HotName> {
    match f.kind {
        FileKind::Segment(id) => Some(HotName::Segment(id)),
        FileKind::Large(file) => Some(HotName::Large(file)),
        FileKind::Cache => None,
    }
}

impl Node {
    /// Whether `path` is a good copy of `f`: its length, and every record crc and the trailer
    /// of a segment, or the header, data crc and generation of a large object, read in windows.
    async fn good(&self, path: &str, f: &FileInfo) -> Result<bool, HotError> {
        let (dev, p) = (Arc::clone(&self.dev), path.to_owned());
        let len = self
            .sched
            .run(DriveClass::Background, move || dev.len(&p))
            .await?;
        if len.ok() != Some(f.bytes) {
            return Ok(false);
        }
        match f.kind {
            FileKind::Segment(_) | FileKind::Cache => {
                let mut b = BlockBuilder::new(self.cfg.geometry, BlockKind::Segment)
                    .map_err(other_err("segment"))?;
                let fed = self
                    .stream(path, 0, f.data_len, &mut |bytes| b.feed(bytes).is_ok())
                    .await?;
                if !fed || f.kind == FileKind::Cache {
                    return Ok(fed);
                }
                let trailer = self
                    .read(
                        DriveClass::Background,
                        path,
                        f.data_len,
                        f.bytes - f.data_len,
                    )
                    .await?;
                Ok(b.finish().is_ok_and(|s| s.bytes == trailer))
            }
            FileKind::Large(file) => {
                let head_len = (RECORD_HEADER_LEN as u64 + u64::from(u16::MAX)).min(f.bytes);
                let head = self.read(DriveClass::Background, path, 0, head_len).await?;
                let Ok(h) = RecordHeader::decode(&head) else {
                    return Ok(false);
                };
                let start = RECORD_HEADER_LEN as u64 + u64::from(h.key_len);
                let (mut g, mut crc) = (GenerationHasher::new(), 0u32);
                let whole = self
                    .stream(path, start.min(f.bytes), f.bytes, &mut |bytes| {
                        g.update(bytes);
                        crc = crc32c_append(crc, bytes);
                        true
                    })
                    .await?;
                // One comparison of everything the name and the header say, so no check is
                // skipped by another.
                let found = (whole, h.data_len, start + h.data_len, crc, g.finish());
                let named = (true, file.size, f.bytes, h.data_crc, file.generation);
                Ok(found == named)
            }
        }
    }

    /// Replaces `path` by a good copy of `file` from another node, fetched window by window
    /// into a `.repair` file, checked, and renamed over; `false` when no node has one.
    async fn repair(&self, path: &str, f: &FileInfo, file: HotName) -> Result<bool, HotError> {
        let range = ByteRange {
            offset: 0,
            len: f.bytes,
        };
        for node in self.voters().into_iter().filter(|v| *v != self.id) {
            let tmp = self.repair_file(path).await?;
            let fetched = self.fetch_into(file, &[node], range, &tmp, 0).await;
            if fetched.is_ok() && self.good(&tmp, f).await? {
                self.rename_over(&tmp, path).await?;
                return Ok(true);
            }
            self.unlink(&tmp).await;
        }
        Ok(false)
    }
}

impl HotNode {
    /// Scrubs every hot file whose last scrub is at least `hot.scrub_period` old.
    pub async fn scrub(&self) -> Result<ScrubReport, HotError> {
        let n = &self.n;
        let now = n.clock.now_ms();
        let due: Vec<(String, FileInfo)> = n
            .lock()
            .files
            .iter()
            .filter(|(_, f)| now.saturating_sub(f.last_scrub_ms) >= n.cfg.scrub_period_ms)
            .map(|(p, f)| (p.clone(), *f))
            .collect();
        let mut report = ScrubReport::default();
        for (path, f) in due {
            report.checked += 1;
            if !n.good(&path, &f).await? {
                report.bad += 1;
                n.metrics.hot_scrub_bad_total.with(()).inc();
                let Some(file) = name_of(&f) else {
                    n.drop_file(&path).await?;
                    report.dropped += 1;
                    continue;
                };
                if n.repair(&path, &f, file).await? {
                    report.repaired += 1;
                }
            }
            if let Some(info) = n.lock().files.get_mut(&path) {
                info.last_scrub_ms = now;
            }
        }
        Ok(report)
    }
}
