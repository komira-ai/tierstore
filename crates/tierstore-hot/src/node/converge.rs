//! Converge and Index (10.3 step 4): once a seal is committed, each copy makes its bytes equal
//! to `[0, length)` and becomes a sealed block.
//!
//! A copy at least as long as the seal whose first `length` bytes have the committed running
//! crc is cut to it (the unacknowledged tail goes). A shorter copy fetches the missing bytes from
//! a copy that has them and checks the running crc. A copy whose prefix does not match (damage)
//! fetches the whole `[0, length)` into a new file that replaces it by rename, never a rewrite in
//! place. Then the trailer, a deterministic function of the bytes, is appended and made durable,
//! `blk/<block_id>.idx` holds its bytes, the block table says `sealed`, and every record within
//! the length is indexed: records the writer had not acknowledged (their client's ack was lost)
//! become visible, the same object a retry would write, and `hot_unacked_records_adopted_total`
//! counts them. Records beyond the length were never acknowledged and are never indexed.

use std::sync::Arc;

use bytes::Bytes;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_format::block::BlockBuilder;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::record::scan;
use tierstore_format::seal::SealProposal;
use tierstore_format::trailer::BlockKind;
use tierstore_format::{FormatError, crc32c_append};
use tierstore_index::blocks::{BlockInfo, BlockState};
use tierstore_store_api::hot::{HotError, Unavailable};
use tierstore_store_api::index::{EntryLocation, Fact, IndexEntry};
use tierstore_store_api::sched::DriveClass;

use super::{FileInfo, FileKind, Node, io_err, other_err, unavailable};

/// The bytes read per step when a segment is verified or its trailer built.
pub(crate) const WINDOW: u64 = 8 << 20;

impl Node {
    /// Up to `len` bytes of `path` from `offset`, through the queue.
    pub(crate) async fn read(
        &self,
        class: DriveClass,
        path: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, HotError> {
        let (dev, p) = (Arc::clone(&self.dev), path.to_owned());
        self.sched
            .run(class, move || dev.read_at(&p, offset, len))
            .await?
            .map_err(io_err("hot read"))
    }

    /// The crc-verified prefix of the first `len` bytes of `path` (records whose header, key and
    /// data crcs hold, from offset 0) and the running crc32c through it.
    pub(crate) async fn verify_prefix(&self, path: &str, len: u64) -> Result<(u64, u32), HotError> {
        let window = WINDOW.max(2 * (self.cfg.large_min + (1 << 17)));
        let (mut pos, mut crc) = (0u64, 0u32);
        let mut carry: Vec<u8> = Vec::new();
        loop {
            let at = pos + carry.len() as u64;
            let eof;
            if at < len {
                let more = self
                    .read(DriveClass::Write, path, at, window.min(len - at))
                    .await?;
                eof = more.is_empty() || at + more.len() as u64 >= len;
                carry.extend_from_slice(&more);
            } else {
                eof = true;
            }
            let found = scan(&carry);
            let used = found.valid_len as usize;
            crc = crc32c_append(crc, &carry[..used]);
            pos += found.valid_len;
            carry.drain(..used);
            // A record cut by the window is read whole with the next one; damage, zeros or the
            // end of the bytes end the verified prefix.
            match found.stop {
                None if carry.is_empty() && !eof => {}
                Some(FormatError::Short { .. }) if !eof => {}
                _ => break,
            }
        }
        Ok((pos.min(len), crc))
    }

    /// Reads `[from, to)` of `path` in windows and hands each to `each`; `false` when the file
    /// ends before `to` or `each` says stop.
    pub(crate) async fn stream(
        &self,
        path: &str,
        from: u64,
        to: u64,
        each: &mut (dyn FnMut(&[u8]) -> bool + Send),
    ) -> Result<bool, HotError> {
        let mut at = from;
        while at < to {
            let got = self
                .read(DriveClass::Write, path, at, WINDOW.min(to - at))
                .await?;
            if got.is_empty() || !each(&got) {
                return Ok(false);
            }
            at += got.len() as u64;
        }
        Ok(true)
    }

    /// The crc32c of `[0, len)` of `path` (fewer bytes if the file is shorter: then `None`).
    async fn crc_of(&self, path: &str, len: u64) -> Result<Option<u32>, HotError> {
        let mut crc = 0u32;
        let whole = self
            .stream(path, 0, len, &mut |b| {
                crc = crc32c_append(crc, b);
                true
            })
            .await?;
        Ok(whole.then_some(crc))
    }

    /// The other nodes to fetch from: `sources` first, then every other voter.
    fn fetch_order(&self, sources: &[NodeId]) -> Vec<NodeId> {
        let mut order: Vec<NodeId> = sources.iter().copied().filter(|s| *s != self.id).collect();
        for v in self.voters() {
            if v != self.id && !order.contains(&v) {
                order.push(v);
            }
        }
        order
    }

    /// Fetches `range` of `file` from the nodes of `order`, one window at a time (each window
    /// from the first node that has it whole), writes it at the same offsets of `dest` and
    /// makes it durable; the crc32c of the bytes fetched, appended to `crc`. Nothing larger than
    /// a window is held.
    pub(crate) async fn fetch_into(
        &self,
        file: HotName,
        order: &[NodeId],
        range: ByteRange,
        dest: &str,
        mut crc: u32,
    ) -> Result<u32, HotError> {
        let end = range.offset + range.len;
        let mut at = range.offset;
        while at < end {
            let window = ByteRange {
                offset: at,
                len: WINDOW.min(end - at),
            };
            let mut got = None;
            for node in order {
                if let Ok(bytes) = self.peers.fetch(*node, file, window).await
                    && bytes.len() as u64 == window.len
                {
                    got = Some(bytes);
                    break;
                }
            }
            let bytes = got.ok_or_else(|| unavailable(Unavailable::TooFewCopies))?;
            crc = crc32c_append(crc, &bytes);
            let (dev, p) = (Arc::clone(&self.dev), dest.to_owned());
            self.sched
                .run(DriveClass::Write, move || dev.write_at(&p, at, &bytes))
                .await?
                .map_err(io_err("fetched write"))?;
            at = window.offset + window.len;
        }
        let (dev, p) = (Arc::clone(&self.dev), dest.to_owned());
        self.sched
            .run(DriveClass::Write, move || dev.fdatasync(&p))
            .await?
            .map_err(io_err("fetched sync"))?;
        Ok(crc)
    }

    /// An empty `<path>.repair`, made or cut.
    pub(crate) async fn repair_file(&self, path: &str) -> Result<String, HotError> {
        let tmp = format!("{path}.repair");
        let (dev, p) = (Arc::clone(&self.dev), tmp.clone());
        self.sched
            .run(DriveClass::Write, move || match dev.create(&p) {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => dev.set_len(&p, 0),
                other => other,
            })
            .await?
            .map_err(io_err("repair file"))?;
        Ok(tmp)
    }

    /// Renames `tmp` over `path` and makes the rename durable.
    pub(crate) async fn rename_over(&self, tmp: &str, path: &str) -> Result<(), HotError> {
        let (dev, t, p) = (Arc::clone(&self.dev), tmp.to_owned(), path.to_owned());
        self.sched
            .run(DriveClass::Write, move || {
                dev.rename(&t, &p, false)?;
                dev.fsync_dir(crate::dev::dir_of(&p))
            })
            .await?
            .map_err(io_err("replace copy"))
    }

    /// Converges this node's copy of `segment` to the committed `proposal` and seals it here;
    /// nothing to do without a copy, or when it is sealed here already.
    pub(crate) async fn converge(
        self: &Arc<Self>,
        segment: BlockId,
        proposal: SealProposal,
        sources: &[NodeId],
    ) -> Result<(), HotError> {
        let _held = self.seg_locks.lock(&segment.raw().to_le_bytes()).await;
        let path = HotName::Segment(segment).path();
        if self.lock().files.contains_key(&path) {
            return Ok(());
        }
        // This node's own writer of the segment is fenced by now; its last local write ends
        // before the copy is cut and sealed.
        let own = self.lock().writing.get(&segment).cloned();
        if let Some(seg) = own {
            seg.idle().await;
        }
        let (dev, p) = (Arc::clone(&self.dev), path.clone());
        let len = match self
            .sched
            .run(DriveClass::Write, move || dev.len(&p))
            .await?
        {
            Ok(len) => len,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err("segment length")(e)),
        };
        let l = proposal.length;
        let fixed = if len >= l {
            if self.crc_of(&path, l).await? == Some(proposal.running_crc) {
                self.set_len(&path, l).await?;
                true
            } else {
                false
            }
        } else {
            let have = self.crc_of(&path, len).await?.unwrap_or_default();
            let range = ByteRange {
                offset: len,
                len: l - len,
            };
            let order = self.fetch_order(sources);
            let fetched = self
                .fetch_into(HotName::Segment(segment), &order, range, &path, have)
                .await;
            matches!(fetched, Ok(crc) if crc == proposal.running_crc)
        };
        if !fixed {
            self.refetch(segment, &path, proposal, sources).await?;
        }
        self.seal_here(segment, &path, l, sources).await
    }

    async fn set_len(&self, path: &str, len: u64) -> Result<(), HotError> {
        let (dev, p) = (Arc::clone(&self.dev), path.to_owned());
        self.sched
            .run(DriveClass::Write, move || {
                dev.set_len(&p, len)?;
                dev.fdatasync(&p)
            })
            .await?
            .map_err(io_err("segment truncate"))
    }

    async fn write_sync(&self, path: &str, at: u64, bytes: Bytes) -> Result<(), HotError> {
        let (dev, p) = (Arc::clone(&self.dev), path.to_owned());
        self.sched
            .run(DriveClass::Write, move || {
                dev.write_at(&p, at, &bytes)?;
                dev.fdatasync(&p)
            })
            .await?
            .map_err(io_err("segment write"))
    }

    /// Replaces this node's copy by the whole `[0, length)` of another copy, fetched window by
    /// window into a new file that is renamed over it once its running crc is the committed one.
    async fn refetch(
        &self,
        segment: BlockId,
        path: &str,
        proposal: SealProposal,
        sources: &[NodeId],
    ) -> Result<(), HotError> {
        let range = ByteRange {
            offset: 0,
            len: proposal.length,
        };
        let tmp = self.repair_file(path).await?;
        let order = self.fetch_order(sources);
        let fetched = self
            .fetch_into(HotName::Segment(segment), &order, range, &tmp, 0)
            .await;
        if !matches!(fetched, Ok(crc) if crc == proposal.running_crc) {
            self.unlink(&tmp).await;
            return Err(unavailable(Unavailable::TooFewCopies));
        }
        self.rename_over(&tmp, path).await
    }

    /// Writes `bytes` as the new contents of `path`: a `.repair` file, `fdatasync`, rename over,
    /// directory `fsync`.
    pub(crate) async fn replace(&self, path: &str, bytes: Bytes) -> Result<(), HotError> {
        let tmp = self.repair_file(path).await?;
        self.write_sync(&tmp, 0, bytes).await?;
        self.rename_over(&tmp, path).await
    }

    /// Appends the trailer of `[0, length)` to the copy, writes `blk/<id>.idx`, records the
    /// block as sealed (with `holders`, the copies that answered the seal whole, when known) and
    /// indexes its records.
    async fn seal_here(
        self: &Arc<Self>,
        segment: BlockId,
        path: &str,
        length: u64,
        holders: &[NodeId],
    ) -> Result<(), HotError> {
        let mut builder = BlockBuilder::new(self.cfg.geometry, BlockKind::Segment)
            .map_err(other_err("segment"))?;
        let mut at = 0u64;
        while at < length {
            let got = self
                .read(DriveClass::Write, path, at, WINDOW.min(length - at))
                .await?;
            if got.is_empty() {
                return Err(unavailable(Unavailable::TooFewCopies));
            }
            builder.feed(&got).map_err(other_err("segment records"))?;
            at += got.len() as u64;
        }
        let sealed = builder.finish().map_err(other_err("segment trailer"))?;
        let total = length + sealed.bytes.len() as u64;
        self.write_sync(path, length, Bytes::from(sealed.bytes.clone()))
            .await?;
        self.replace(
            &HotName::BlockIndex(segment).path(),
            Bytes::from(sealed.bytes),
        )
        .await?;
        // The copies that answered the seal with the whole length (the sealer knows them), and
        // this one; a node that learns the seal from the broadcast keeps the row it has.
        let hot_copies = if holders.is_empty() {
            self.index
                .block(segment)
                .ok()
                .flatten()
                .map_or_else(|| vec![self.id], |info| info.hot_copies)
        } else {
            let mut copies: Vec<NodeId> = holders.to_vec();
            copies.push(self.id);
            copies.sort_unstable();
            copies.dedup();
            copies
        };
        let mut info = BlockInfo::new(BlockState::Sealed);
        info.length = total;
        info.hot_copies = hot_copies;
        self.index
            .put_block(segment, &info)
            .map_err(other_err("block table"))?;
        let (bound, own) = {
            let st = self.lock();
            match (st.writing.get(&segment), st.copies.get(&segment)) {
                (Some(seg), _) => (seg.lock().acked, Some(Arc::clone(seg))),
                (None, Some(c)) => (c.noticed, None),
                (None, None) => (length, None),
            }
        };
        let adopted = sealed
            .trailer
            .entries
            .iter()
            .filter(|e| e.offset >= bound)
            .count();
        let mut facts = sealed.trailer.entries.into_iter().map(|e| Fact::Record {
            entry: IndexEntry {
                generation: e.generation,
                location: EntryLocation::Block {
                    block: segment,
                    offset: e.offset,
                },
                length: e.length,
                stamp: e.stamp,
                flags: e.flags,
                prior_blocks: Vec::new(),
                promoted: None,
            },
            key: e.key,
        });
        self.index
            .replay(&mut facts)
            .map_err(other_err("index records"))?;
        self.metrics
            .hot_unacked_records_adopted_total
            .with(())
            .add(adopted as u64);
        self.metrics.hot_seal_bytes.with(()).observe(total as f64);
        let now = self.clock.now_ms();
        let mut st = self.lock();
        if let Some(seg) = own {
            st.writing.remove(&seg.id);
        }
        st.copies.remove(&segment);
        st.unsealed.remove(&segment);
        // The seal state stays in `seals/`; a later prepare or batch reads it from there.
        st.votes.remove(&segment);
        st.rounds.remove(&segment);
        st.files.insert(
            path.to_owned(),
            FileInfo {
                kind: FileKind::Segment(segment),
                bytes: total,
                data_len: length,
                dirty: true,
                last_read_ms: now,
                last_scrub_ms: now,
            },
        );
        Ok(())
    }
}
