//! A replica's side of replication (10.3 steps 1 and 2): append a batch at its offset,
//! `fdatasync`, acknowledge with the committed length.
//!
//! A batch is refused when its ballot is below this node's promise for the segment (the writer
//! is fenced), when the segment's seal is committed here, when its start is not the copy's
//! committed length, or when the running crc32c of the copy's bytes and the batch is not the
//! one the batch carries. A refused batch changes nothing. The segment's lock is held from the
//! checks to the acknowledgement, so a promise is answered either before a batch is checked or
//! after it is durable, and the length the promise reports covers everything acknowledged.
//!
//! The records of a batch are durable here but not yet acknowledged by the writer, so they are
//! not indexed: a batch that starts at `s` says every record before `s` is acknowledged, and
//! those are indexed then. Records the writer never acknowledged become visible only through a
//! seal whose length covers them (10.3 step 4, Index).

use std::sync::Arc;

use tierstore_format::crc32c_append;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::record::{decode_record, scan};
use tierstore_index::backend::Backend;
use tierstore_index::blocks::{BlockInfo, BlockState};
use tierstore_store_api::hot::{ReplicaAck, ReplicaBatch, ReplicaReject};
use tierstore_store_api::index::{EntryLocation, Fact, Index, IndexEntry};
use tierstore_store_api::sched::DriveClass;

use super::Node;

/// A copy this node mirrors for another writer, while the segment is open.
#[derive(Debug, Clone)]
pub(crate) struct CopySt {
    /// The copy's committed (durable) length.
    pub(crate) committed: u64,
    /// The running crc32c of `[0, committed)`.
    crc: u32,
    /// Records durable here that the writer has not said are acknowledged.
    pending: Vec<(u64, Vec<u8>, IndexEntry)>,
    /// Every record before this offset is acknowledged and indexed here.
    pub(crate) noticed: u64,
    /// The node that opened the segment and writes it.
    pub(crate) writer: NodeId,
}

/// `PUT /store/v1/hot/replica`: one batch.
pub(crate) async fn replicate<B: Backend + 'static>(
    n: &Arc<Node<B>>,
    batch: ReplicaBatch,
) -> Result<ReplicaAck, ReplicaReject> {
    n.observe(batch.stamp);
    let segment = batch.segment;
    let _held = n.seg_locks.lock(&segment.raw().to_le_bytes()).await;
    let have = n.lock().copies.get(&segment).map_or(0, |c| c.committed);
    let offset = ReplicaReject::Offset {
        start: batch.start,
        committed: have,
    };
    let state = n.vote_state(segment).await.map_err(|_| offset)?;
    if state.committed.is_some() {
        return Err(ReplicaReject::Sealed);
    }
    if batch.ballot < state.promised {
        return Err(ReplicaReject::Ballot {
            ballot: batch.ballot,
            promised: state.promised,
        });
    }
    n.saw_ballot(segment, batch.ballot);
    let copy = n.lock().copies.get(&segment).cloned();
    let copy = match copy {
        Some(c) => c,
        None if batch.start == 0 => {
            let opened = open_copy(n, segment, batch.ballot.node)
                .await
                .map_err(|()| offset)?;
            // Kept from here on: a write that fails below leaves the file, and the retry must
            // find the copy rather than try to create the file again.
            n.lock().copies.insert(segment, opened.clone());
            opened
        }
        None => return Err(offset),
    };
    if batch.start != copy.committed {
        return Err(offset);
    }
    let computed = crc32c_append(copy.crc, &batch.bytes);
    if computed != batch.running_crc {
        return Err(ReplicaReject::Crc {
            sent: batch.running_crc,
            computed,
        });
    }
    if !batch.bytes.is_empty() {
        let (dev, path, bytes, at) = (
            Arc::clone(&n.dev),
            HotName::Segment(segment).path(),
            batch.bytes.clone(),
            batch.start,
        );
        let wrote = n
            .sched
            .run(DriveClass::Write, move || {
                dev.write_at(&path, at, &bytes)?;
                dev.fdatasync(&path)
            })
            .await;
        if !matches!(wrote, Ok(Ok(()))) {
            return Err(offset);
        }
    }
    let end = batch.start + batch.bytes.len() as u64;
    let records = records_of(segment, batch.start, &batch.bytes);
    let ready = {
        let mut st = n.lock();
        let c = st.copies.entry(segment).or_insert(copy);
        c.committed = end;
        c.crc = computed;
        c.pending.extend(records);
        c.noticed = c.noticed.max(batch.start);
        let noticed = c.noticed;
        let (ready, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut c.pending)
            .into_iter()
            .partition(|(rec_end, _, _)| *rec_end <= noticed);
        c.pending = rest;
        ready
    };
    if !ready.is_empty() {
        let mut facts = ready
            .into_iter()
            .map(|(_, key, entry)| Fact::Record { key, entry });
        // The index is a cache of these bytes: an entry it fails to take now is replayed from
        // the segment at the seal or at a restart.
        let _ = n.index.replay(&mut facts);
    }
    Ok(ReplicaAck { committed: end })
}

/// Creates this node's copy of `segment`, which `writer` opened.
async fn open_copy<B: Backend + 'static>(
    n: &Arc<Node<B>>,
    segment: BlockId,
    writer: NodeId,
) -> Result<CopySt, ()> {
    let (dev, path) = (Arc::clone(&n.dev), HotName::Segment(segment).path());
    n.sched
        .run(DriveClass::Write, move || {
            dev.create(&path)?;
            dev.fsync_dir("seg")
        })
        .await
        .map_err(drop)?
        .map_err(drop)?;
    let mut info = BlockInfo::new(BlockState::Open);
    info.hot_copies = if writer < n.id {
        vec![writer, n.id]
    } else {
        vec![n.id, writer]
    };
    n.index.put_block(segment, &info).map_err(drop)?;
    Ok(CopySt {
        committed: 0,
        crc: 0,
        pending: Vec::new(),
        noticed: 0,
        writer,
    })
}

/// The records of a batch that starts at `start` of `segment`, each with its end offset and the
/// index entry it makes. A batch holds whole records; anything after the last one that decodes
/// is not a record.
pub(crate) fn records_of(
    segment: BlockId,
    start: u64,
    bytes: &[u8],
) -> Vec<(u64, Vec<u8>, IndexEntry)> {
    let found = scan(bytes);
    found
        .records
        .iter()
        .filter_map(|(off, _)| {
            let (rec, len) = decode_record(&bytes[*off as usize..]).ok()?;
            let entry = IndexEntry {
                generation: rec.header.generation,
                location: EntryLocation::Block {
                    block: segment,
                    offset: start + off,
                },
                length: rec.header.data_len,
                stamp: rec.header.stamp,
                flags: rec.header.flags,
                prior_blocks: Vec::new(),
                promoted: None,
            };
            Some((start + off + len, rec.key.to_vec(), entry))
        })
        .collect()
}

impl<B: Backend + 'static> super::HotNode<B> {
    /// Whether this node holds a durable record of `key` in a copy it mirrors that its writer
    /// has not yet said is acknowledged: the key's answer here is then not a definitive miss
    /// (the record may be acknowledged already, and a seal would adopt it).
    pub fn pending(&self, key: &[u8]) -> bool {
        self.n
            .lock()
            .copies
            .values()
            .any(|c| c.pending.iter().any(|(_, k, _)| k == key))
    }
}
