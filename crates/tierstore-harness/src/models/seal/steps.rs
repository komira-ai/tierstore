//! The effect of each step of the seal model, and how a node handles each message.

use std::collections::{BTreeMap, BTreeSet};

use tierstore_format::ids::NodeId;
use tierstore_format::seal::Ballot;

use super::replica::{Batch, ReplicaView, check_batch};
use super::types::{Answer, Cp, Msg, Peer, Prop, Reject, Sealer, World, Writer, ix, nid};
use super::{Act, SealModel, Variant, WRITER};

impl SealModel {
    /// The state after the message `msg` from `from` reaches node `to`, whatever the state: a
    /// message that tells the node nothing new (a stale reply) changes nothing. Messages in
    /// flight that become dead are dropped, as after every step.
    #[must_use]
    pub fn receive(&self, s: &World, from: NodeId, to: NodeId, msg: Msg) -> World {
        let mut n = s.clone();
        self.handle(&mut n, from, to, msg);
        self.drop_dead(&mut n);
        n
    }

    /// The state without the messages in flight whose delivery would change nothing.
    #[must_use]
    pub fn settle(&self, s: &World) -> World {
        let mut n = s.clone();
        self.drop_dead(&mut n);
        n
    }

    /// The state after `act`; the step is enabled (see `SealModel::enabled`).
    pub(super) fn apply(&self, s: &World, act: Act) -> World {
        let mut n = s.clone();
        match act {
            Act::Begin { start, end } => self.begin(&mut n, start, end),
            Act::LocalSync { start, end } => self.local_sync(&mut n, start, end),
            Act::Deliver { idx, from, to, msg } => {
                let _ = n.net.take(idx);
                self.handle(&mut n, from, to, msg);
            }
            Act::Lose { idx, .. } => {
                let _ = n.net.lose(idx);
                n.losses += 1;
            }
            Act::Crash => {
                let node = &mut n.nodes[0];
                node.up = false;
                node.sealer = Sealer::Idle;
                node.seen = node.promised;
                n.writer = Writer {
                    writing: false,
                    sent: node.len,
                    pending: None,
                    rep: [0; 3],
                    known: 0,
                };
                n.net.crash(WRITER);
                n.crashed = true;
            }
            Act::Return => {
                let node = &mut n.nodes[0];
                node.up = true;
                n.writer.writing = node.committed.is_none();
            }
            Act::Damage(d) => {
                n.nodes[ix(d)].bad = true;
                n.damaged = true;
            }
            Act::StartSeal(id) => self.start_seal(&mut n, ix(id)),
            Act::Choose { node, ballot, prop } => self.take_choice(&mut n, ix(node), ballot, prop),
            Act::Converge { node, length } => {
                let copy = &mut n.nodes[ix(node)];
                copy.len = length;
                copy.bad = false;
            }
            Act::Pull { node, prop } => self.adopt(&mut n, ix(node), prop),
            Act::Destage(id) => {
                let len = n.nodes[ix(id)].len;
                n.destaged = Some(Prop {
                    length: len,
                    crc: self.crc_at(len),
                });
            }
        }
        self.drop_dead(&mut n);
        n
    }

    /// Sends `msg` from `from` to node `to` (an index) unless that node is down: what is sent
    /// to a crashed node is lost.
    fn post(&self, n: &mut World, from: NodeId, to: usize, msg: Msg) {
        if n.nodes[to].up {
            let _ = n.net.send(from, nid(to), msg);
        }
    }

    /// Sends `msg` from node `from` (an index) to the two others.
    fn post_others(&self, n: &mut World, from: usize, msg: Msg) {
        for to in (0..3).filter(|j| *j != from) {
            self.post(n, nid(from), to, msg);
        }
    }

    /// Removes the messages in flight whose delivery would change nothing, so that two paths
    /// that differ only in such a message are one state: a refusal of the writer that it has
    /// already acted on, an acknowledgement already known, a promise or accept for an attempt
    /// that is over, a refusal that tells nothing new, a seal the node already holds.
    fn drop_dead(&self, n: &mut World) {
        let dead: Vec<usize> = n
            .net
            .in_flight()
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                let node = &n.nodes[ix(e.to)];
                match e.msg {
                    Msg::Reject(_) => !n.writer.writing,
                    Msg::Ack { committed } => n.writer.rep[ix(e.from)] >= committed,
                    Msg::Promise { ballot, .. } => !matches!(
                        &node.sealer,
                        Sealer::Preparing { ballot: b, .. } if *b == ballot
                    ),
                    Msg::Accepted { ballot } => !matches!(
                        &node.sealer,
                        Sealer::Accepting { ballot: b, .. } if *b == ballot
                    ),
                    Msg::Refused { ballot, promised } => {
                        node.seen >= promised && node.sealer.ballot() != Some(ballot)
                    }
                    Msg::Committed(p) | Msg::Sealed(p) => {
                        node.committed == Some(p) && node.sealer == Sealer::Idle
                    }
                    Msg::Replicate { .. } | Msg::Prepare { .. } | Msg::Accept { .. } => false,
                }
            })
            .map(|(i, _)| i)
            .collect();
        for i in dead.into_iter().rev() {
            let _ = n.net.lose(i);
        }
    }

    /// The writer acknowledges the client up to the longest record that is durable on
    /// `min_acks` copies (a replica's acknowledgement or the writer's own sync completes it).
    fn ack_now(&self, n: &mut World) {
        let need = if self.cfg.variant == Variant::AckOnOne {
            1
        } else {
            self.cfg.min_acks
        };
        let best = self
            .offs
            .iter()
            .copied()
            .filter(|o| *o > n.acked && self.durable_copies(n, *o) >= need)
            .max();
        if let Some(len) = best {
            n.acked = len;
            n.writer.known = len;
        }
    }

    /// What node `i` reports to a prepare.
    fn answer(&self, i: usize, p: &Peer) -> Answer {
        let verified = if p.bad { 0 } else { p.len };
        Answer {
            accepted: p.accepted,
            copy: self.holds(i).then(|| Cp {
                committed: p.len,
                verified,
                crc: self.crc_at(verified),
            }),
        }
    }

    fn begin(&self, n: &mut World, start: u64, end: u64) {
        n.writer.pending = Some((start, end));
        n.writer.sent = end;
        let msg = Msg::Replicate {
            ballot: Ballot::opening(WRITER),
            start,
            len: end - start,
            crc: self.crc_at(end),
        };
        for to in 1..usize::from(self.cfg.replicas) {
            self.post(n, WRITER, to, msg);
        }
    }

    fn local_sync(&self, n: &mut World, start: u64, end: u64) {
        n.writer.pending = None;
        let opening = Ballot::opening(WRITER);
        if self
            .append(n, 0, opening, start, end - start, self.crc_at(end))
            .is_err()
        {
            n.writer.writing = false;
        }
        self.ack_now(n);
    }

    /// Node `i` receives a batch: the replica's check, and the append when it passes.
    fn append(
        &self,
        n: &mut World,
        i: usize,
        ballot: Ballot,
        start: u64,
        len: u64,
        crc: u32,
    ) -> Result<u64, Reject> {
        let node = &n.nodes[i];
        let from = usize::try_from(start).unwrap_or(usize::MAX);
        let to = usize::try_from(start + len).unwrap_or(usize::MAX);
        let view = ReplicaView {
            committed: node.len,
            copy_crc: self.crc_at(node.len) ^ (u32::MAX * u32::from(node.bad)),
            promised: node.promised,
            sealed: node.committed.is_some(),
        };
        let batch = Batch {
            ballot,
            start,
            running_crc: crc,
            bytes: &self.payload[from.min(self.payload.len())..to.min(self.payload.len())],
        };
        let fence = self.cfg.variant != Variant::NoFence;
        match check_batch(&view, fence, &batch) {
            Ok(committed) => {
                n.s4 |= ballot < node.promised;
                n.nodes[i].len = committed;
                Ok(committed)
            }
            Err(e) => {
                let reject = Reject::from(e);
                n.fenced |= reject == Reject::Ballot;
                Err(reject)
            }
        }
    }

    /// Node `to` handles `msg` from `from`.
    fn handle(&self, n: &mut World, from: NodeId, to: NodeId, msg: Msg) {
        let ti = ix(to);
        match msg {
            Msg::Replicate {
                ballot,
                start,
                len,
                crc,
            } => {
                // A refusal for the offset or the crc changes nothing at the writer (it stalls
                // and the early seal follows), so the replica's reply is left out of the model.
                let reply = match self.append(n, ti, ballot, start, len, crc) {
                    Ok(committed) => Some(Msg::Ack { committed }),
                    Err(reject @ (Reject::Ballot | Reject::Sealed)) => Some(Msg::Reject(reject)),
                    Err(Reject::Offset | Reject::Crc) => None,
                };
                if let Some(reply) = reply {
                    self.post(n, to, ix(from), reply);
                }
            }
            Msg::Ack { committed } => {
                let rep = &mut n.writer.rep[ix(from)];
                *rep = (*rep).max(committed);
                self.ack_now(n);
            }
            Msg::Reject(_) => n.writer.writing = false,
            Msg::Prepare { ballot } => {
                let node = &mut n.nodes[ti];
                let reply = if let Some(p) = node.committed {
                    Msg::Committed(p)
                } else if ballot > node.promised {
                    node.promised = ballot;
                    node.sealer = Sealer::Idle;
                    Msg::Promise {
                        ballot,
                        answer: self.answer(ti, node),
                    }
                } else {
                    Msg::Refused {
                        ballot,
                        promised: node.promised,
                    }
                };
                self.post(n, to, ix(from), reply);
            }
            Msg::Promise { ballot, answer } => {
                if let Sealer::Preparing {
                    ballot: mine,
                    promises,
                } = &mut n.nodes[ti].sealer
                    && *mine == ballot
                {
                    promises.insert(from, answer);
                }
            }
            Msg::Refused { ballot, promised } => {
                let node = &mut n.nodes[ti];
                node.seen = node.seen.max(promised);
                if node.sealer.ballot() == Some(ballot) {
                    node.sealer = Sealer::Idle;
                }
            }
            Msg::Committed(p) | Msg::Sealed(p) => self.adopt(n, ti, p),
            Msg::Accept { ballot, prop } => {
                let node = &mut n.nodes[ti];
                let reply = if let Some(p) = node.committed {
                    Msg::Committed(p)
                } else if ballot >= node.promised {
                    node.promised = ballot;
                    node.accepted = Some((ballot, prop));
                    if node.sealer.ballot().is_some_and(|mine| mine < ballot) {
                        node.sealer = Sealer::Idle;
                    }
                    Msg::Accepted { ballot }
                } else {
                    Msg::Refused {
                        ballot,
                        promised: node.promised,
                    }
                };
                self.post(n, to, ix(from), reply);
            }
            Msg::Accepted { ballot } => {
                if let Sealer::Accepting {
                    ballot: mine,
                    accepts,
                    ..
                } = &mut n.nodes[ti].sealer
                    && *mine == ballot
                {
                    accepts.insert(from);
                }
                self.try_commit(n, ti);
            }
        }
    }

    /// Node `i` learns the committed seal `p`: it keeps it, stops sealing and (the writer)
    /// stops appending. A different seal than the one it holds is an S2 violation.
    fn adopt(&self, n: &mut World, i: usize, p: Prop) {
        let node = &mut n.nodes[i];
        match node.committed {
            None => node.committed = Some(p),
            Some(held) => n.s2 |= held != p,
        }
        node.sealer = Sealer::Idle;
        if i == 0 {
            n.writer.writing = false;
        }
    }

    /// Node `i` commits its proposal when enough nodes accepted it, and broadcasts `sealed`.
    fn try_commit(&self, n: &mut World, i: usize) {
        let need = self.quorum();
        let done = match &n.nodes[i].sealer {
            Sealer::Accepting { prop, accepts, .. } if accepts.len() >= need => Some(*prop),
            _ => None,
        };
        if let Some(p) = done {
            self.adopt(n, i, p);
            self.post_others(n, i, Msg::Sealed(p));
        }
    }

    /// Node `i` starts a seal attempt under a ballot above any it has seen: it promises itself
    /// durably and sends prepare to the others.
    fn start_seal(&self, n: &mut World, i: usize) {
        let node = &mut n.nodes[i];
        node.attempts += 1;
        let ballot = Ballot {
            round: node.promised.max(node.seen).round + 1,
            node: nid(i),
        };
        node.promised = ballot;
        node.seen = ballot;
        let mine = self.answer(i, node);
        node.sealer = Sealer::Preparing {
            ballot,
            promises: BTreeMap::from([(nid(i), mine)]),
        };
        if i == 0 {
            n.writer.writing = false;
        }
        self.post_others(n, i, Msg::Prepare { ballot });
    }

    /// Node `i` moves from prepare to accept with `prop`, or gives up when the rule makes none
    /// (the segment waits; writes go to a new segment).
    fn take_choice(&self, n: &mut World, i: usize, ballot: Ballot, prop: Option<Prop>) {
        let Some(prop) = prop else {
            n.nodes[i].sealer = Sealer::Idle;
            return;
        };
        let node = &mut n.nodes[i];
        node.accepted = Some((ballot, prop));
        node.sealer = Sealer::Accepting {
            ballot,
            prop,
            accepts: BTreeSet::from([nid(i)]),
        };
        self.post_others(n, i, Msg::Accept { ballot, prop });
        self.try_commit(n, i);
    }
}
