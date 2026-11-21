//! Reads on the hot tier (10.3 "Reads on the hot tier"): a byte range of any hot file (the
//! `GET /store/v1/hot/<file>` of 10.6, and the bytes a converging or repairing copy fetches),
//! and one record of a segment with its crc32c verified. Every read moves the file's last read
//! time, which reclaim orders by.

use std::sync::Arc;

use bytes::Bytes;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_format::ids::BlockId;
use tierstore_format::names::HotName;
use tierstore_format::record::{RECORD_HEADER_LEN, RecordHeader, decode_record};
use tierstore_store_api::hot::HotError;
use tierstore_store_api::sched::DriveClass;

use super::HotNode;

/// Why a hot read failed.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// No such file here (`404`).
    #[error("no such hot file")]
    NotFound,
    /// The record's crc32c does not match: the caller serves another copy and queues the repair
    /// of this one (10.3, 10.6 step 5).
    #[error("crc mismatch in the record at {offset}")]
    Corrupt {
        /// The record's offset.
        offset: u64,
    },
    /// The device or its queue failed.
    #[error(transparent)]
    Hot(#[from] HotError),
}

impl HotNode {
    /// Bytes `range` of `file`: fewer at its end, and at most one read window (8 MiB, or a
    /// whole record when `hot.large_min` is larger), so a caller that wants more asks again from
    /// where the answer ended and no read holds a whole file.
    pub async fn serve(&self, file: HotName, range: ByteRange) -> Result<Bytes, ReadError> {
        let path = file.path();
        let most = super::converge::WINDOW.max(self.n.cfg.large_min + (1 << 17));
        let range = ByteRange {
            offset: range.offset,
            len: range.len.min(most),
        };
        let (dev, p) = (Arc::clone(&self.n.dev), path.clone());
        let got = self
            .n
            .sched
            .run(DriveClass::Demand, move || {
                dev.read_at(&p, range.offset, range.len)
            })
            .await
            .map_err(HotError::from)?;
        let bytes = match got {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ReadError::NotFound),
            Err(e) => return Err(super::io_err("hot read")(e).into()),
        };
        self.touch(&path);
        Ok(Bytes::from(bytes))
    }

    /// The key and data of the record at `offset` of segment `block`, its crcs checked.
    pub async fn read_record(
        &self,
        block: BlockId,
        offset: u64,
    ) -> Result<(Vec<u8>, Bytes), ReadError> {
        let file = HotName::Segment(block);
        let head = self
            .serve(
                file,
                ByteRange {
                    offset,
                    len: RECORD_HEADER_LEN as u64,
                },
            )
            .await?;
        let header = RecordHeader::decode(&head).map_err(|_| ReadError::Corrupt { offset })?;
        let whole = self
            .serve(
                file,
                ByteRange {
                    offset,
                    len: header.record_len(),
                },
            )
            .await?;
        let (rec, _) = decode_record(&whole).map_err(|_| ReadError::Corrupt { offset })?;
        Ok((rec.key.to_vec(), Bytes::copy_from_slice(rec.data)))
    }

    fn touch(&self, path: &str) {
        let now = self.n.clock.now_ms();
        if let Some(f) = self.n.lock().files.get_mut(path) {
            f.last_read_ms = now;
        }
    }
}
