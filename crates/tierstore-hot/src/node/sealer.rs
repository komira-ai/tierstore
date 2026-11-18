//! The sealer's side of the majority seal (10.3 step 4): prepare, choose, accept, commit, the
//! `sealed` broadcast and this node's converge.
//!
//! The sealer is the writer for a size, age or early seal, or any node taking over a segment
//! whose writer stopped answering. It promises its own ballot first, so its own writer of the
//! segment is fenced before the floor (its last acknowledged offset) is read, then asks every
//! other voter at once and waits for each answer (a promise, a refusal or an error: the peer
//! transport times out). With promises from a majority it chooses ([`crate::choose`]) and
//! sends the accept to every voter; a majority of durable accepts commits. Any answer naming a
//! committed seal is adopted unchanged. A refusal for a higher ballot makes the sealer try again
//! above it, a few times.

use std::sync::Arc;

use futures::future::join_all;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::seal::{Ballot, SealProposal};
use tierstore_index::backend::Backend;
use tierstore_metrics::labels::SealRound;
use tierstore_store_api::hot::{
    Committed, CopyState, HotError, Promise, SealReason, SealRefused, Unavailable,
};
use tierstore_store_api::proto::{SealAnswer, SealMessage};
use tierstore_store_api::view::majority;

use super::{Node, unavailable};
use crate::choose::choose;

/// Rounds a sealer tries before it gives up (a later tick tries again).
const SEAL_ROUNDS: usize = 3;

/// The step of the seal a message asks a voter.
#[derive(Clone, Copy)]
enum Step {
    Prepare,
    Accept(SealProposal),
}

/// What one voter said to a prepare or an accept.
enum Vote {
    Yes(Option<Promise>),
    Higher(Ballot),
    Committed(SealProposal),
    Silent,
}

fn vote_of(r: Result<Option<Promise>, SealRefused>) -> Vote {
    match r {
        Ok(p) => Vote::Yes(p),
        Err(SealRefused::Promised { promised }) => Vote::Higher(promised),
        Err(SealRefused::Committed(p)) => Vote::Committed(p),
    }
}

fn vote_of_answer(answer: SealAnswer, ballot: Ballot) -> Vote {
    match answer {
        SealAnswer::Promise { accepted, copy } => Vote::Yes(Some(Promise {
            ballot,
            accepted,
            copy: copy.map(|c| CopyState {
                committed: c.committed,
                verified: c.verified,
                running_crc: c.running_crc,
            }),
        })),
        SealAnswer::Ok => Vote::Yes(None),
        SealAnswer::Promised { ballot } => Vote::Higher(ballot),
        SealAnswer::Committed { proposal } => Vote::Committed(proposal),
    }
}

/// Seals `segment` under a ballot above every one this node has seen, unless a seal of this node
/// is already working on it.
pub(crate) async fn seal_segment<B: Backend + 'static>(
    n: &Arc<Node<B>>,
    segment: BlockId,
    reason: SealReason,
) -> Result<Committed, HotError> {
    if !n.lock().sealing.insert(segment) {
        return Err(unavailable(Unavailable::Minority));
    }
    let ballot = n.next_ballot(segment);
    let out = seal(n, segment, ballot, reason).await;
    n.lock().sealing.remove(&segment);
    out
}

/// `HotStore::seal`: seals `segment` by majority under `ballot`.
pub(crate) async fn seal<B: Backend + 'static>(
    n: &Arc<Node<B>>,
    segment: BlockId,
    ballot: Ballot,
    reason: SealReason,
) -> Result<Committed, HotError> {
    let mut ballot = ballot;
    for _ in 0..SEAL_ROUNDS {
        n.saw_ballot(segment, ballot);
        n.metrics
            .hot_seal_rounds_total
            .with(SealRound::Prepare)
            .inc();
        let voters = n.voters();
        let need = majority(voters.len());
        let votes = n.ask(&voters, segment, ballot, Step::Prepare).await;
        let floor = n.lock().writing.get(&segment).map_or(0, |s| s.lock().acked);
        let mut promises = Vec::new();
        let mut higher = None;
        for (node, v) in votes {
            match v {
                Vote::Committed(p) => return adopt(n, segment, ballot, p, &voters).await,
                Vote::Yes(Some(p)) => promises.push((node, p)),
                Vote::Higher(b) => higher = higher.max(Some(b)),
                Vote::Yes(None) | Vote::Silent => {}
            }
        }
        if promises.len() < need {
            match higher {
                Some(b) => {
                    ballot = above(n, segment, b);
                    continue;
                }
                None => return Err(unavailable(Unavailable::Minority)),
            }
        }
        let only: Vec<Promise> = promises.iter().map(|(_, p)| *p).collect();
        let Some(proposal) = choose(&only, n.cfg.replicas, n.cfg.min_acks, floor) else {
            return Err(unavailable(Unavailable::TooFewCopies));
        };
        let votes = n
            .ask(&voters, segment, ballot, Step::Accept(proposal))
            .await;
        let mut oks = 0;
        let mut higher = None;
        for (_, v) in votes {
            match v {
                Vote::Committed(p) => return adopt(n, segment, ballot, p, &voters).await,
                Vote::Yes(_) => oks += 1,
                Vote::Higher(b) => higher = higher.max(Some(b)),
                Vote::Silent => {}
            }
        }
        if oks >= need {
            let holders: Vec<NodeId> = promises
                .iter()
                .filter(|(_, p)| p.copy.is_some_and(|c| c.verified >= proposal.length))
                .map(|(id, _)| *id)
                .collect();
            n.metrics.hot_seal_total.with(reason.into()).inc();
            commit(n, segment, proposal, &voters, &holders).await?;
            n.note_copies(
                segment,
                promises.iter().filter(|(_, p)| p.copy.is_some()).count(),
            );
            return Ok(Committed {
                segment,
                ballot,
                proposal,
            });
        }
        match higher {
            Some(b) => ballot = above(n, segment, b),
            None => return Err(unavailable(Unavailable::Minority)),
        }
    }
    Err(unavailable(Unavailable::Minority))
}

/// A ballot of this node above `b`.
fn above<B: Backend + 'static>(n: &Node<B>, segment: BlockId, b: Ballot) -> Ballot {
    n.metrics
        .hot_seal_rounds_total
        .with(SealRound::Rejected)
        .inc();
    n.saw_ballot(segment, b);
    n.next_ballot(segment)
}

/// Adopts a seal another sealer committed.
async fn adopt<B: Backend + 'static>(
    n: &Arc<Node<B>>,
    segment: BlockId,
    ballot: Ballot,
    proposal: SealProposal,
    voters: &[NodeId],
) -> Result<Committed, HotError> {
    n.metrics
        .hot_seal_rounds_total
        .with(SealRound::Adopted)
        .inc();
    commit(n, segment, proposal, voters, &[]).await?;
    Ok(Committed {
        segment,
        ballot,
        proposal,
    })
}

/// Records the commit here, converges this node's copy, and broadcasts `sealed`.
async fn commit<B: Backend + 'static>(
    n: &Arc<Node<B>>,
    segment: BlockId,
    proposal: SealProposal,
    voters: &[NodeId],
    holders: &[NodeId],
) -> Result<(), HotError> {
    n.learn_from(segment, proposal, holders).await?;
    let sealed = voters.iter().filter(|v| **v != n.id).map(|v| {
        let msg = n.env(SealMessage::Sealed { segment, proposal });
        async move {
            if let Ok(msg) = msg {
                // A voter that misses the broadcast learns the commit at its next prepare or
                // restart.
                let _ = n.peers.seal(*v, msg).await;
            }
        }
    });
    join_all(sealed).await;
    Ok(())
}

impl<B: Backend + 'static> Node<B> {
    /// Sends `msg` to every voter (this node first, through its own handlers) and gathers what
    /// each said.
    async fn ask(
        self: &Arc<Self>,
        voters: &[NodeId],
        segment: BlockId,
        ballot: Ballot,
        step: Step,
    ) -> Vec<(NodeId, Vote)> {
        let mut out = Vec::with_capacity(voters.len());
        if voters.contains(&self.id) {
            let own = match step {
                Step::Prepare => vote_of(self.promise(segment, ballot).await.map(Some)),
                Step::Accept(proposal) => {
                    vote_of(self.accept(segment, ballot, proposal).await.map(|()| None))
                }
            };
            out.push((self.id, own));
        }
        let msg = match step {
            Step::Prepare => SealMessage::Prepare { segment, ballot },
            Step::Accept(proposal) => SealMessage::Accept {
                segment,
                ballot,
                proposal,
            },
        };
        let others = voters.iter().filter(|v| **v != self.id).map(|v| {
            let env = self.env(msg.clone());
            async move {
                let vote = match env {
                    Ok(env) => match self.peers.seal(*v, env).await {
                        Ok(answer) => {
                            self.observe(answer.stamp);
                            vote_of_answer(answer.body, ballot)
                        }
                        Err(_) => Vote::Silent,
                    },
                    Err(_) => Vote::Silent,
                };
                (*v, vote)
            }
        });
        out.extend(join_all(others).await);
        out
    }

    /// Notes how many copies of a sealed `segment` answered its seal: fewer than `hot.replicas`
    /// is an under-replicated block until re-replication restores it.
    pub(crate) fn note_copies(&self, segment: BlockId, copies: usize) {
        let mut st = self.lock();
        if copies < usize::from(self.cfg.replicas) {
            st.under_replicated.insert(segment);
        } else {
            st.under_replicated.remove(&segment);
        }
        self.metrics
            .hot_under_replicated_blocks
            .with(())
            .set(st.under_replicated.len() as f64);
    }
}
