//! Hot scrub (10.3 "Hot scrub"): sealed segments, large objects and cache segments are re-read
//! once per `hot.scrub_period`, every record crc and the block's trailer (its entries, cell crcs
//! and blake3, a deterministic function of the bytes) checked. A bad copy of a segment or large
//! object is replaced by another node's copy, verified the same way, through a new file renamed
//! over it, never rewritten in place; a bad cache segment is dropped.

use bytes::Bytes;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_format::block::BlockBuilder;
use tierstore_format::crc32c;
use tierstore_format::digest::GenerationHasher;
use tierstore_format::names::HotName;
use tierstore_format::record::{RECORD_HEADER_LEN, RecordHeader};
use tierstore_format::trailer::BlockKind;
use tierstore_store_api::hot::HotError;
use tierstore_store_api::sched::DriveClass;

use super::converge::WINDOW;
use super::{FileInfo, FileKind, HotNode, Node};

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

/// Whether `bytes` are a good copy of `f`.
fn good(f: &FileInfo, bytes: &[u8], geometry: tierstore_format::block::BlockGeometry) -> bool {
    if bytes.len() as u64 != f.bytes {
        return false;
    }
    let (data, rest) = bytes.split_at((f.data_len as usize).min(bytes.len()));
    match f.kind {
        FileKind::Segment(_) | FileKind::Cache => BlockBuilder::new(geometry, BlockKind::Segment)
            .is_ok_and(|mut b| {
                b.feed(data).is_ok()
                    && (f.kind == FileKind::Cache || b.finish().is_ok_and(|s| s.bytes == rest))
            }),
        FileKind::Large(file) => RecordHeader::decode(bytes).is_ok_and(|h| {
            let body = bytes.get(RECORD_HEADER_LEN + usize::from(h.key_len)..);
            body.is_some_and(|data| {
                let mut g = GenerationHasher::new();
                g.update(data);
                h.data_len == file.size
                    && crc32c(data) == h.data_crc
                    && g.finish() == file.generation
            })
        }),
    }
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
    async fn read_whole(&self, path: &str, len: u64) -> Result<Vec<u8>, HotError> {
        let mut out = Vec::with_capacity(len as usize);
        while (out.len() as u64) < len {
            let at = out.len() as u64;
            let got = self
                .read(DriveClass::Background, path, at, WINDOW.min(len - at))
                .await?;
            if got.is_empty() {
                break;
            }
            out.extend_from_slice(&got);
        }
        Ok(out)
    }

    /// A good copy of `path` from another node, if one has it.
    async fn good_copy(&self, f: &FileInfo, file: HotName) -> Option<Bytes> {
        for node in self.voters().into_iter().filter(|v| *v != self.id) {
            let range = ByteRange {
                offset: 0,
                len: f.bytes,
            };
            if let Ok(bytes) = self.peers.fetch(node, file, range).await
                && good(f, &bytes, self.cfg.geometry)
            {
                return Some(bytes);
            }
        }
        None
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
            let bytes = n.read_whole(&path, f.bytes).await?;
            if !good(&f, &bytes, n.cfg.geometry) {
                report.bad += 1;
                n.metrics.hot_scrub_bad_total.with(()).inc();
                let Some(file) = name_of(&f) else {
                    n.drop_file(&path).await?;
                    report.dropped += 1;
                    continue;
                };
                if let Some(copy) = n.good_copy(&f, file).await {
                    n.replace(&path, copy).await?;
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
