//! A voter's side of the majority seal (10.3 step 4): the durable seal state of each segment in
//! `seals/<block_id>.seal`, replaced atomically before the promise or acceptance it records is
//! answered (write a temporary name, `fdatasync`, rename over, `fsync` the directory).
//!
//! Every storage node votes, with or without a copy. A promise of a ballot fences every lower
//! one: this node's own writer of the segment stops acknowledging, and a replica copy refuses
//! batches under a lower ballot. A committed seal never changes: prepares and accepts are then
//! answered with it.

use std::sync::Arc;

use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::seal::{Ballot, SealProposal, SealState};
use tierstore_store_api::hot::{CopyState, HotError, Promise, SealRefused};
use tierstore_store_api::sched::DriveClass;

use super::writer::Close;
use super::{Node, io_err, other_err};

/// The promise of a segment nobody has prepared: every opening ballot `(1, writer)` is above it.
pub(crate) const NO_PROMISE: Ballot = Ballot {
    round: 0,
    node: NodeId(0),
};

impl Node {
    /// The highest ballot this node promised for `segment`, if it voted on it (from memory: every
    /// state this node wrote or read since it started is cached).
    pub(crate) fn promised(&self, segment: BlockId) -> Option<Ballot> {
        self.lock().votes.get(&segment).map(|v| v.promised)
    }

    /// The seal state of `segment`: the cached one, else the file's, else a fresh one.
    pub(crate) async fn vote_state(&self, segment: BlockId) -> Result<SealState, HotError> {
        if let Some(v) = self.lock().votes.get(&segment) {
            return Ok(*v);
        }
        let path = HotName::Seal(segment).path();
        let dev = Arc::clone(&self.dev);
        let bytes = self
            .sched
            .run(DriveClass::Write, move || {
                match dev.read_at(&path, 0, 1 << 16) {
                    Ok(b) => Ok(Some(b)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e),
                }
            })
            .await?
            .map_err(io_err("read seal state"))?;
        let state = match bytes {
            Some(b) => SealState::decode(&b).map_err(other_err("seal state"))?,
            None => SealState {
                block: segment,
                promised: NO_PROMISE,
                accepted: None,
                committed: None,
            },
        };
        self.lock().votes.insert(segment, state);
        Ok(state)
    }

    /// Makes `state` durable by the replace rule, then caches it.
    pub(crate) async fn persist_vote(&self, state: SealState) -> Result<(), HotError> {
        let path = HotName::Seal(state.block).path();
        let tmp = format!("{path}.tmp");
        let bytes = state.encode();
        let dev = Arc::clone(&self.dev);
        self.sched
            .run(DriveClass::Write, move || {
                match dev.create(&tmp) {
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        dev.set_len(&tmp, 0)?;
                    }
                    other => other?,
                }
                dev.write_at(&tmp, 0, &bytes)?;
                dev.fdatasync(&tmp)?;
                dev.rename(&tmp, &path, false)?;
                dev.fsync_dir("seals")
            })
            .await?
            .map_err(io_err("write seal state"))?;
        self.lock().votes.insert(state.block, state);
        Ok(())
    }

    /// `seal-prepare(segment, ballot)`.
    pub(crate) async fn promise(
        self: &Arc<Self>,
        segment: BlockId,
        ballot: Ballot,
    ) -> Result<Promise, SealRefused> {
        let refused = SealRefused::Promised { promised: ballot };
        let _held = self.seg_locks.lock(&segment.raw().to_le_bytes()).await;
        self.saw_ballot(segment, ballot);
        let mut state = self.vote_state(segment).await.map_err(|_| refused)?;
        if let Some(p) = state.committed {
            return Err(SealRefused::Committed(p));
        }
        if ballot < state.promised {
            return Err(SealRefused::Promised {
                promised: state.promised,
            });
        }
        state.promised = ballot;
        self.persist_vote(state).await.map_err(|_| refused)?;
        // Fence this node's own writer of the segment before its copy is read, so nothing it
        // acknowledges later lies beyond the length reported here.
        let own = self.lock().writing.get(&segment).cloned();
        if let Some(seg) = own.filter(|s| ballot > s.ballot) {
            self.roll(&seg, Close::Fenced);
        }
        let copy = self.copy_state(segment).await.map_err(|_| refused)?;
        Ok(Promise {
            ballot,
            accepted: state.accepted,
            copy,
        })
    }

    /// `seal-accept(segment, ballot, proposal)`.
    pub(crate) async fn accept(
        self: &Arc<Self>,
        segment: BlockId,
        ballot: Ballot,
        proposal: SealProposal,
    ) -> Result<(), SealRefused> {
        let refused = SealRefused::Promised { promised: ballot };
        let _held = self.seg_locks.lock(&segment.raw().to_le_bytes()).await;
        self.saw_ballot(segment, ballot);
        let mut state = self.vote_state(segment).await.map_err(|_| refused)?;
        if let Some(p) = state.committed {
            return Err(SealRefused::Committed(p));
        }
        if ballot < state.promised {
            return Err(SealRefused::Promised {
                promised: state.promised,
            });
        }
        state.promised = ballot;
        state.accepted = Some((ballot, proposal));
        self.persist_vote(state).await.map_err(|_| refused)
    }

    /// Records the committed seal of `segment` and converges this node's copy, if it holds one
    /// (the `sealed` broadcast).
    pub(crate) async fn learn(
        self: &Arc<Self>,
        segment: BlockId,
        proposal: SealProposal,
    ) -> Result<(), HotError> {
        self.learn_from(segment, proposal, &[]).await
    }

    /// [`Node::learn`], fetching missing bytes from `sources` first (the copies that answered the
    /// seal), else from every other voter.
    pub(crate) async fn learn_from(
        self: &Arc<Self>,
        segment: BlockId,
        proposal: SealProposal,
        sources: &[NodeId],
    ) -> Result<(), HotError> {
        {
            let _held = self.seg_locks.lock(&segment.raw().to_le_bytes()).await;
            let mut state = self.vote_state(segment).await?;
            if state.committed.is_none() {
                state.committed = Some(proposal);
                self.persist_vote(state).await?;
            }
        }
        let own = self.lock().writing.get(&segment).cloned();
        if let Some(seg) = own {
            seg.close(Close::Fenced);
            self.retire(&seg);
        }
        self.converge(segment, proposal, sources).await
    }

    /// The state of this node's copy of `segment` for a promise: its committed length and the
    /// crc-verified prefix with its running crc; `None` when it holds no copy.
    pub(crate) async fn copy_state(&self, segment: BlockId) -> Result<Option<CopyState>, HotError> {
        let committed = {
            let st = self.lock();
            match (st.writing.get(&segment), st.copies.get(&segment)) {
                (Some(seg), _) => Some(seg.lock().durable),
                (None, Some(c)) => Some(c.committed),
                (None, None) => None,
            }
        };
        let path = HotName::Segment(segment).path();
        let committed = match committed {
            Some(c) => c,
            None => {
                let dev = Arc::clone(&self.dev);
                let p = path.clone();
                let len = self
                    .sched
                    .run(DriveClass::Write, move || dev.len(&p))
                    .await?;
                match len {
                    Ok(l) => l,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(io_err("segment length")(e)),
                }
            }
        };
        let (verified, running_crc) = self.verify_prefix(&path, committed).await?;
        Ok(Some(CopyState {
            committed,
            verified,
            running_crc,
        }))
    }
}
