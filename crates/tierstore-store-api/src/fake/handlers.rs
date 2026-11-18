//! The routes of the fake storage node: what each answers from the node's state.

use std::ops::Range;

use bytes::Bytes;
use futures::stream::StreamExt;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_fetch_api::upstream::DriveSignals;
use tierstore_format::crc32c;
use tierstore_format::descriptor::StripeDescriptor;
use tierstore_format::free::StripeFree;

use super::lock;
use super::node::{FakeStorageNode, Replica, Stored, Tier, Voter, opening};
use super::script::Route;
use crate::hot::ReplicaBatch;
use crate::peer::{GetTarget, PeerBody, PeerError, PeerGet};
use crate::proto::{
    CommitAnswer, CommitRecord, CopyWire, Envelope, KeyCheck, KeyFacts, KeyRecord, LocateAnswer,
    LocateRequest, LocateResponse, PingAnswer, ReplicaAnswer, SealAnswer, SealMessage, TombPage,
    TombPull,
};

/// How a range of a GET maps onto an object's bytes.
#[derive(Debug, Clone, Copy)]
struct Mapping {
    /// Where the served span starts in the GET's coordinates (the file or the block).
    base: u64,
    /// The span's length.
    length: u64,
    /// Where the span starts in the object.
    object_base: u64,
}

impl Mapping {
    /// The object's bytes `range` selects, if the range is inside the span and the object.
    fn window(self, range: ByteRange, object_len: usize) -> Option<Range<usize>> {
        let rel = range.offset.checked_sub(self.base)?;
        let rel_end = rel.saturating_add(range.len);
        let start = self.object_base.saturating_add(rel);
        let end = self.object_base.saturating_add(rel_end);
        if rel_end > self.length || end > object_len as u64 {
            return None;
        }
        Some(start as usize..end as usize)
    }
}

/// The spans of `object` that `target` names.
fn mappings(object: &Stored, target: &GetTarget) -> Vec<Mapping> {
    let extent = |e: &crate::proto::ColdExtent| Mapping {
        base: e.block_offset,
        length: e.length,
        object_base: e.object_offset,
    };
    match (&object.tier, target) {
        (Tier::Hot(copies), GetTarget::Hot { file }) => copies
            .iter()
            .filter(|c| &c.file == file)
            .map(|c| Mapping {
                base: c.offset,
                length: c.length,
                object_base: 0,
            })
            .collect(),
        (Tier::Cold(extents), GetTarget::Cold { drive, shard }) => extents
            .iter()
            .filter(|e| e.drive == *drive && &e.shard == shard)
            .map(extent)
            .collect(),
        (Tier::Cold(extents), GetTarget::Degraded { block, .. }) => extents
            .iter()
            .filter(|e| e.block == *block)
            .map(extent)
            .collect(),
        _ => Vec::new(),
    }
}

impl Replica {
    /// Applies a batch under the replication rules of 10.3: sealed, ballot below the promise, a
    /// start that is not the committed length, a running crc that does not match; else append.
    fn apply(&mut self, batch: &ReplicaBatch) -> ReplicaAnswer {
        if self.sealed.is_some() {
            return ReplicaAnswer::Sealed;
        }
        if batch.ballot < self.promised {
            return ReplicaAnswer::Ballot {
                ballot: batch.ballot,
                promised: self.promised,
            };
        }
        let committed = self.bytes.len() as u64;
        if batch.start != committed {
            return ReplicaAnswer::Offset {
                start: batch.start,
                committed,
            };
        }
        let mut all = self.bytes.clone();
        all.extend_from_slice(&batch.bytes);
        let computed = crc32c(&all);
        if computed != batch.running_crc {
            return ReplicaAnswer::Crc {
                sent: batch.running_crc,
                computed,
            };
        }
        self.bytes = all;
        self.promised = self.promised.max(batch.ballot);
        ReplicaAnswer::Acked {
            committed: self.bytes.len() as u64,
        }
    }
}

impl FakeStorageNode {
    /// `POST /store/v1/locate`: per key, in order: `Indexing` while the index is not built, the
    /// stored object's tier, or `Absent` (unknown or deleted).
    pub async fn locate(&self, req: LocateRequest) -> Result<Envelope<LocateResponse>, PeerError> {
        let entered = self.enter(Route::Locate, None).await?;
        let st = lock(&self.state);
        let answers = req
            .keys
            .iter()
            .map(|key| match (st.indexing, st.objects.get(key)) {
                (true, _) => LocateAnswer::Indexing,
                (false, None) => LocateAnswer::Absent,
                (false, Some(o)) => match &o.tier {
                    Tier::Hot(copies) => LocateAnswer::Hot {
                        generation: o.generation,
                        size: o.bytes.len() as u64,
                        copies: copies.clone(),
                    },
                    Tier::Cold(extents) => LocateAnswer::Cold {
                        generation: o.generation,
                        size: o.bytes.len() as u64,
                        extents: extents.clone(),
                    },
                },
            })
            .collect();
        Ok(self.envelope(&entered, LocateResponse { answers }))
    }

    /// A hot, cold or degraded GET: the object with a span that `req.target` names and that holds
    /// the range (else the first named span, whose range is then refused with `416`), the
    /// `If-Match` rule, and one chunk of bytes. A `report` is logged when the GET is answered.
    pub async fn get(&self, req: PeerGet) -> Result<PeerBody, PeerError> {
        let entered = self.enter(Route::Get, None).await?;
        let mut st = lock(&self.state);
        let (generation, chunk) = {
            let spans: Vec<(&Stored, Mapping)> = st
                .objects
                .values()
                .flat_map(|o| mappings(o, &req.target).into_iter().map(move |m| (o, m)))
                .collect();
            let (object, span) = spans
                .iter()
                .find(|(o, m)| m.window(req.range, o.bytes.len()).is_some())
                .or(spans.first())
                .ok_or(PeerError::NotFound { node: self.id })?;
            if req
                .headers
                .generation
                .is_some_and(|want| want != object.generation)
            {
                return Err(PeerError::PreconditionFailed {
                    node: self.id,
                    actual: Some(object.generation),
                });
            }
            let window = span
                .window(req.range, object.bytes.len())
                .ok_or_else(|| self.protocol(416, "range not satisfiable"))?;
            (object.generation, object.bytes.slice(window))
        };
        if req.report.is_some() {
            st.reports.push(req.clone());
        }
        let signals = DriveSignals {
            service_ms: Some(u32::try_from(entered.service_ms).unwrap_or(u32::MAX)),
            queue_wait_ms: Some(0),
            ..DriveSignals::default()
        };
        let body = futures::stream::once(std::future::ready(Ok::<Bytes, PeerError>(chunk))).boxed();
        Ok(PeerBody {
            generation,
            signals,
            body,
        })
    }

    /// `PUT /store/v1/hot/replica`: appends a batch to the node's copy of the segment, or refuses
    /// it by the replication rules of 10.3. A refused batch never creates a copy.
    pub async fn replicate(
        &self,
        batch: Envelope<ReplicaBatch>,
    ) -> Result<Envelope<ReplicaAnswer>, PeerError> {
        let entered = self.enter(Route::Replicate, Some(batch.stamp)).await?;
        let body = &batch.body;
        let mut st = lock(&self.state);
        let mut copy = st
            .replicas
            .get(&body.segment)
            .cloned()
            .unwrap_or_else(|| opening(body.ballot));
        let answer = copy.apply(body);
        if matches!(answer, ReplicaAnswer::Acked { .. }) {
            st.replicas.insert(body.segment, copy);
        }
        Ok(self.envelope(&entered, answer))
    }

    /// `POST /store/v1/seal`: the voter of the majority seal (10.3 step 4). A committed seal is
    /// returned unchanged to every prepare and accept; a ballot below the promise is refused. A
    /// prepare or accept that raises the promise raises the promise of the node's copy too.
    pub async fn seal(
        &self,
        msg: Envelope<SealMessage>,
    ) -> Result<Envelope<SealAnswer>, PeerError> {
        let entered = self.enter(Route::Seal, Some(msg.stamp)).await?;
        let mut st = lock(&self.state);
        let state = &mut *st;
        let answer = match msg.body {
            SealMessage::Prepare { segment, ballot } => {
                let voter = state.voters.entry(segment).or_default();
                match refusal(voter, ballot) {
                    Some(refused) => refused,
                    None => {
                        voter.promised = Some(ballot);
                        raise_copy(&mut state.replicas, segment, ballot);
                        let copy = state.replicas.get(&segment).map(|r| {
                            let len = r.bytes.len() as u64;
                            CopyWire {
                                committed: len,
                                verified: len,
                                running_crc: crc32c(&r.bytes),
                            }
                        });
                        SealAnswer::Promise {
                            accepted: voter.accepted,
                            copy,
                        }
                    }
                }
            }
            SealMessage::Accept {
                segment,
                ballot,
                proposal,
            } => {
                let voter = state.voters.entry(segment).or_default();
                match refusal(voter, ballot) {
                    Some(refused) => refused,
                    None => {
                        voter.promised = Some(ballot);
                        voter.accepted = Some((ballot, proposal));
                        raise_copy(&mut state.replicas, segment, ballot);
                        SealAnswer::Ok
                    }
                }
            }
            SealMessage::Sealed { segment, proposal } => {
                state.voters.entry(segment).or_default().committed = Some(proposal);
                if let Some(copy) = state.replicas.get_mut(&segment) {
                    copy.bytes
                        .truncate(usize::try_from(proposal.length).unwrap_or(usize::MAX));
                    copy.sealed = Some(proposal);
                }
                SealAnswer::Ok
            }
        };
        Ok(self.envelope(&entered, answer))
    }

    /// `POST /store/v1/key`: what the node holds of the key: its object's generation and the stamp
    /// of the put, its tombstone, and whether its index is not built.
    pub async fn key_check(
        &self,
        check: Envelope<KeyCheck>,
    ) -> Result<Envelope<KeyFacts>, PeerError> {
        let entered = self.enter(Route::KeyCheck, Some(check.stamp)).await?;
        let st = lock(&self.state);
        let key = &check.body.key;
        let records = st
            .objects
            .get(key)
            .map(|o| KeyRecord {
                generation: o.generation,
                stamp: o.stamp,
            })
            .into_iter()
            .collect();
        let facts = KeyFacts {
            records,
            tombstone: st.tombstones.get(key).copied(),
            indexing: st.indexing,
        };
        Ok(self.envelope(&entered, facts))
    }

    /// `POST /store/v1/descriptor`: a record that does not decode is `400`; a descriptor for a
    /// stripe with a free is `FreeWins`, one equal to the descriptor held is `Durable` again, any
    /// other at or below the held revision is `StaleRevision`; else the record is stored and `Durable`.
    pub async fn commit(
        &self,
        record: Envelope<CommitRecord>,
    ) -> Result<Envelope<CommitAnswer>, PeerError> {
        let entered = self.enter(Route::Commit, Some(record.stamp)).await?;
        let answer = match &record.body {
            CommitRecord::Descriptor { bytes } => {
                let d = StripeDescriptor::decode(&bytes.0).map_err(|e| self.protocol(400, e))?;
                self.store_descriptor(d)
            }
            CommitRecord::Free { bytes } => {
                let f = StripeFree::decode(&bytes.0).map_err(|e| self.protocol(400, e))?;
                lock(&self.state).frees.insert(f.stripe, f);
                CommitAnswer::Durable
            }
        };
        Ok(self.envelope(&entered, answer))
    }

    fn store_descriptor(&self, d: StripeDescriptor) -> CommitAnswer {
        let mut st = lock(&self.state);
        if st.frees.contains_key(&d.stripe) {
            return CommitAnswer::FreeWins;
        }
        if let Some(held) = st.descriptors.get(&d.stripe) {
            // A resend of the descriptor held is answered `Durable` again; another descriptor at
            // the same or a lower revision is stale.
            if *held == d {
                return CommitAnswer::Durable;
            }
            if held.revision >= d.revision {
                return CommitAnswer::StaleRevision {
                    held: held.revision,
                };
            }
        }
        st.descriptors.insert(d.stripe, d);
        CommitAnswer::Durable
    }

    /// `GET /store/v1/tomb`: the entries of `origin`'s log with a sequence number above `after`,
    /// at most `limit`, with the last sequence number returned (`after` when none) and whether
    /// more follow.
    pub async fn tomb_pull(&self, pull: TombPull) -> Result<Envelope<TombPage>, PeerError> {
        let entered = self.enter(Route::TombPull, None).await?;
        let st = lock(&self.state);
        let newer: Vec<_> = st
            .logs
            .get(&pull.origin)
            .map(|log| log.iter().filter(|(seq, _)| *seq > pull.after).collect())
            .unwrap_or_default();
        let limit = usize::try_from(pull.limit).unwrap_or(usize::MAX);
        let taken: Vec<_> = newer.iter().take(limit).collect();
        let page = TombPage {
            entries: taken.iter().map(|(_, e)| e.clone()).collect(),
            last: taken.last().map_or(pull.after, |(seq, _)| *seq),
            more: newer.len() > limit,
        };
        Ok(self.envelope(&entered, page))
    }

    /// `GET /store/v1/ping`: the node's indexing flag, its drives as set and the hot queue.
    pub async fn ping(&self) -> Result<Envelope<PingAnswer>, PeerError> {
        let entered = self.enter(Route::Ping, None).await?;
        let st = lock(&self.state);
        let answer = PingAnswer {
            indexing: st.indexing,
            drives: st.drives.clone(),
            hot_queued: st.hot_queued,
        };
        Ok(self.envelope(&entered, answer))
    }
}

/// A promise of the voter is a promise of the node's copy of the segment too (10.3): a later batch
/// under a lower ballot is refused with [`ReplicaAnswer::Ballot`]. A node with no copy has none to
/// raise.
fn raise_copy(
    replicas: &mut std::collections::BTreeMap<tierstore_format::ids::BlockId, Replica>,
    segment: tierstore_format::ids::BlockId,
    ballot: tierstore_format::seal::Ballot,
) {
    if let Some(copy) = replicas.get_mut(&segment) {
        copy.promised = copy.promised.max(ballot);
    }
}

/// Why a voter refuses `ballot`: its seal is committed, or it promised a higher ballot.
fn refusal(voter: &Voter, ballot: tierstore_format::seal::Ballot) -> Option<SealAnswer> {
    if let Some(proposal) = voter.committed {
        return Some(SealAnswer::Committed { proposal });
    }
    voter
        .promised
        .filter(|promised| *promised > ballot)
        .map(|ballot| SealAnswer::Promised { ballot })
}
