//! An exhaustive model of the hot tier's replication protocol, ack rule and majority seal
//! (design 10.3 steps 1 to 5, and the majority commit of 10.2), explored by
//! [`tierstore_model::explore`] with [`crate::net::SimNet`] as the network.
//!
//! # What is modelled
//!
//! Three storage nodes (ids 1, 2, 3; node 1 is the writer) and one segment. Its copies are on
//! nodes `1..=replicas`; with `replicas = 2` node 3 is a voter that holds no copy. The writer
//! appends up to `records` records (of 3, 2 and 4 bytes, so offsets are not record counts), each
//! one a batch: it begins the batch (sends the replicate message to every replica, a step),
//! syncs its own copy (a step: a crash between the two is "before local sync"), a replica
//! checks the batch with [`check_batch`] and acknowledges with its committed length, and the
//! writer acknowledges the client (a step) when the record is durable on `min_acks` copies.
//! The running crc32c is the real one over modelled bytes.
//!
//! A seal is the single-decree agreement of 10.3 step 4: prepare and a durable promise with
//! the copy's committed length, verified length and running crc, [`choose`] over the promises
//! of a majority (a separate step, so the explorer picks which promises it uses), accept with
//! a durable accept, commit on a majority of durable accepts, a `sealed` broadcast, and
//! converge (a copy truncates, or fetches from a healthy copy at least as long, and then equals
//! `[0, length)`). A node that has committed refuses prepares and accepts with the committed
//! proposal, which the sealer adopts. A node that learns of no commit pulls it from a peer
//! (the restart path of "Hot restart"). Any node may seal, up to `attempts` times each: the
//! writer early at its last acknowledged offset (the floor of [`choose`]), a replica taking
//! over at what the rule gives.
//!
//! The faults the explorer chooses: loss of any message in flight (at most `max_losses`
//! in all), a crash of the writer at any step and its return (durable state stays: copy
//! bytes, promise, accepted, committed; volatile state is lost, and so are messages in flight
//! to it), the damage of one copy inside its acknowledged prefix (verified length 0). The
//! partitions are fixed per scenario: [`Config::cuts`] are directed links cut at the start (an
//! asymmetric partition is a one-way cut).
//!
//! # Properties
//!
//! The rule id starts each message, as the explorer's `Trace::rule` reads it.
//!
//! - S1: at most one committed proposal; all committed seals are equal.
//! - S2: a node's committed seal never changes.
//! - S3: every acknowledged record is within every committed length. With `min_acks` below
//!   `replicas` it is the documented weaker behaviour and the explorer finds its loss.
//! - S4: a replica never appends a batch whose ballot is below its promise.
//! - S5: destage (a modelled step, enabled by [`Config::destage`]) reads a segment only after a
//!   seal committed, and what it reads is the committed proposal.
//! - S6: a node alone in its partition never commits and never acknowledges, with `min_acks`
//!   above 1 (its client gets 503).
//! - S7: a proposal accepted or committed describes the segment: its crc is the crc32c of the
//!   segment's bytes `[0, length)`.
//! - Liveness: from every state in which some node has committed, delivery, pull and converge
//!   steps reach a state in which every running node that a chain of working links connects
//!   to a node that knows the commit has learned it and holds exactly `[0, length)`. The
//!   explorer reports a liveness failure under its own rule id `A4`. A seal whose bytes no
//!   undamaged copy holds any more (damage after the seal was chosen, on the only copy of a tail
//!   that was never acknowledged) is excused.
//!
//! # Variants that show the model has teeth
//!
//! [`Variant`] switches the design off in one place each; the tests assert the explorer
//! reports the matching rule for each: `IgnoreAccepted` (Choose ignores accepted proposals: S1),
//! `NoFence` (a replica ignores the ballot fence: S4), `CommitOnOne` (a seal needs one node,
//! not a majority: S1, S2 or S6), `AckOnOne` (the writer acknowledges on its own copy: S6),
//! `Longest` (the longest copy counted though damaged: S7), `DestageEarly` and `DestageUnconverged` (S5), `NoConverge`
//! (no converge step: liveness). A state that breaks several rules is reported under the first
//! of S2, S4, S1, S3, S5, S6, S7.
//!
//! # Bounds and reductions
//!
//! Three nodes and one segment of at most 3 records; the scenarios of the tests lose at most
//! one message, crash the writer at most once, damage at most one copy, and let a node start at
//! most 2 seal attempts (the writer twice, or the writer and one replica once each). Full
//! connectivity with two sealers at once is not explored (it is far above a million states); the
//! two-sealers scenarios are the partitioned ones of the design, and a single taking-over
//! replica is explored with the writer's crash under full connectivity. Each scenario of the
//! tests explores from a few hundred to a few hundred thousand states and runs in seconds.
//!
//! Two reductions keep the graph small, both exact: the client acknowledgement happens in the
//! step that completes the ack rule (a crash between the replica's sync and the client's
//! acknowledgement is a crash with the acknowledgement in flight), and a message whose
//! delivery would change nothing (a promise for a finished attempt, an acknowledgement already
//! known, a seal the node already holds, and so on) is dropped as soon as it is dead, so two
//! paths that differ only in such a message are one state ([`SealModel::settle`]). A replica's
//! refusal of a batch for its offset or crc is not sent: the writer takes no action on it and
//! stalls, and the early seal follows (10.3 step 5). Messages to a crashed node are lost.
//! [`SealModel::receive`] gives a node any message in any state, which is how the tests show
//! that a stale message changes nothing.
//!
//! Section 14 rows: the replication-protocol row (rejects, crash of the writer at each step,
//! take-over, the returned old writer), the two-sealers row (partitions, S1 to S6, damage,
//! `min_acks = 2` with a silent copy), the weaker-acknowledgement row, and the minority part of
//! seal-and-roll.

mod replica;
mod steps;
mod types;

use tierstore_format::ids::NodeId;
use tierstore_format::seal::Ballot;
use tierstore_model::explore::{Model, Succ};
use tierstore_store_api::view::majority;

use crate::net::SimNet;

pub use replica::{Batch, ReplicaView, check_batch, choose};
pub use types::{Answer, Cp, Msg, Peer, Prop, Reject, Sealer, World, Writer, ix, nid};

/// The writer's node id.
pub const WRITER: NodeId = NodeId(1);

/// The byte lengths of the records the writer appends, in order.
const RECORD_LENS: [usize; 3] = [3, 2, 4];

/// The one place a variant leaves the design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Variant {
    /// The design.
    #[default]
    Design,
    /// Choose ignores the proposals nodes accepted.
    IgnoreAccepted,
    /// A replica does not refuse a batch whose ballot is below its promise.
    NoFence,
    /// A seal needs one node, its own, instead of a majority: one promise and one accept.
    CommitOnOne,
    /// The writer acknowledges a record once its own copy is durable, whatever `min_acks` is.
    AckOnOne,
    /// Choose counts copies with damage and proposes the longest.
    Longest,
    /// Destage may read an open segment.
    DestageEarly,
    /// Destage may read a sealed segment whose copy has not converged to the seal.
    DestageUnconverged,
    /// No copy converges after the commit.
    NoConverge,
}

/// One scenario: the protocol settings and the faults the explorer may use.
#[derive(Debug, Clone)]
pub struct Config {
    /// Copies of the segment: 2 (node 3 is a voter only) or 3.
    pub replicas: u8,
    /// Copies a record must be durable on before the client is acknowledged.
    pub min_acks: u8,
    /// Records the writer appends: 1 to 3.
    pub records: usize,
    /// At most this many messages are lost, anywhere.
    pub max_losses: u8,
    /// Directed links cut at the start (`from`, `to`).
    pub cuts: Vec<(NodeId, NodeId)>,
    /// Seal attempts each node may start (index = id - 1).
    pub attempts: [u8; 3],
    /// Whether the writer may crash once and return.
    pub crash_writer: bool,
    /// The node whose copy may be damaged (once, inside its acknowledged prefix).
    pub damage: Option<NodeId>,
    /// Whether destage is a step of the model.
    pub destage: bool,
    /// The variant; the design by default.
    pub variant: Variant,
}

impl Default for Config {
    /// Three copies, `min_acks = 3`, two records, no faults, the writer and node 2 may each
    /// seal once.
    fn default() -> Self {
        Self {
            replicas: 3,
            min_acks: 3,
            records: 2,
            max_losses: 0,
            cuts: Vec::new(),
            attempts: [1, 1, 0],
            crash_writer: false,
            damage: None,
            destage: false,
            variant: Variant::Design,
        }
    }
}

/// A step of the model; the explorer's action type.
#[derive(Debug, Clone, Copy)]
pub enum Act {
    /// The writer sends the batch `[start, end)` to the replicas.
    Begin {
        /// The batch's first offset.
        start: u64,
        /// The batch's end.
        end: u64,
    },
    /// The writer's own `fdatasync` of the batch.
    LocalSync {
        /// The batch's first offset.
        start: u64,
        /// The batch's end.
        end: u64,
    },
    /// The message at `idx` of the network arrives.
    Deliver {
        /// The index in flight.
        idx: usize,
        /// The sender.
        from: NodeId,
        /// The receiver.
        to: NodeId,
        /// The message.
        msg: Msg,
    },
    /// The message at `idx` is lost.
    Lose {
        /// The index in flight.
        idx: usize,
        /// The sender.
        from: NodeId,
        /// The receiver.
        to: NodeId,
        /// The message.
        msg: Msg,
    },
    /// The writer crashes.
    Crash,
    /// The writer returns.
    Return,
    /// A copy is damaged inside its acknowledged prefix.
    Damage(NodeId),
    /// A node starts a seal attempt (prepare).
    StartSeal(NodeId),
    /// A node chooses its proposal from the promises it has (or none).
    Choose {
        /// The sealer.
        node: NodeId,
        /// Its ballot.
        ballot: Ballot,
        /// The proposal, or `None` when the rule makes none.
        prop: Option<Prop>,
    },
    /// A copy truncates to, or fetches up to, the sealed length.
    Converge {
        /// The node.
        node: NodeId,
        /// The sealed length.
        length: u64,
    },
    /// A node that has not learned the commit reads it from a peer.
    Pull {
        /// The node.
        node: NodeId,
        /// The committed proposal.
        prop: Prop,
    },
    /// A node's copy is read by the destager.
    Destage(NodeId),
}

/// The model; see the module documentation.
#[derive(Debug, Clone)]
pub struct SealModel {
    cfg: Config,
    /// The segment's bytes after all records.
    payload: Vec<u8>,
    /// The offsets of the record boundaries, 0 first.
    offs: Vec<u64>,
}

impl SealModel {
    /// A model of the scenario `cfg`.
    ///
    /// # Panics
    ///
    /// When `cfg` is not a scenario of the design: `replicas` not 2 or 3, `min_acks` not in
    /// `1..=replicas`, or `records` not in `1..=3` (a test bug).
    #[must_use]
    pub fn new(cfg: Config) -> Self {
        assert!(
            (2..=3).contains(&cfg.replicas) && (1..=cfg.replicas).contains(&cfg.min_acks),
            "the seal model needs 2 or 3 replicas and 1 to replicas acks"
        );
        assert!(
            (1..=RECORD_LENS.len()).contains(&cfg.records),
            "the seal model appends 1 to 3 records"
        );
        let mut payload = Vec::new();
        let mut offs = vec![0];
        for (i, len) in RECORD_LENS.iter().take(cfg.records).enumerate() {
            payload.resize(payload.len() + len, 0x11 * u8::try_from(i + 1).unwrap_or(1));
            offs.push(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        }
        Self { cfg, payload, offs }
    }

    /// The scenario.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The crc32c of the segment's bytes `[0, len)`.
    #[must_use]
    pub fn crc_at(&self, len: u64) -> u32 {
        let end = usize::try_from(len).unwrap_or(usize::MAX);
        crc32c::crc32c(&self.payload[..end.min(self.payload.len())])
    }

    /// The number of nodes a seal needs, for its promises and for its accepts: a majority of
    /// the three storage nodes (10.2), or one with `CommitOnOne`.
    fn quorum(&self) -> usize {
        if self.cfg.variant == Variant::CommitOnOne {
            1
        } else {
            majority(3)
        }
    }

    /// Whether node `i` (an index) holds a copy.
    fn holds(&self, i: usize) -> bool {
        i < usize::from(self.cfg.replicas)
    }

    /// Whether the working link between nodes `a` and `b` (indices) exists: both run, and
    /// neither direction is cut.
    fn linked(&self, s: &World, a: usize, b: usize) -> bool {
        s.nodes[a].up
            && s.nodes[b].up
            && !s.net.is_cut(nid(a), nid(b))
            && !s.net.is_cut(nid(b), nid(a))
    }

    /// The nodes (as flags) a chain of working links connects to `from`, `from` included.
    fn reach(&self, s: &World, from: usize) -> [bool; 3] {
        let mut r = [false; 3];
        r[from] = true;
        for _ in 0..2 {
            for a in 0..3 {
                for b in 0..3 {
                    if r[a] && a != b && self.linked(s, a, b) {
                        r[b] = true;
                    }
                }
            }
        }
        r
    }

    /// Whether node `i` is cut off from both others in both directions.
    fn isolated(&self, s: &World, i: usize) -> bool {
        (0..3)
            .filter(|j| *j != i)
            .all(|j| s.net.is_cut(nid(i), nid(j)) && s.net.is_cut(nid(j), nid(i)))
    }

    /// The end of the record that starts at `start`, if there is one.
    fn next_end(&self, start: u64) -> Option<u64> {
        self.offs.iter().copied().find(|o| *o > start)
    }

    /// The number of copies the record ending at `off` is durable on, from the writer's view.
    fn durable_copies(&self, s: &World, off: u64) -> u8 {
        let own = &s.nodes[0];
        let others: u8 = (1..usize::from(self.cfg.replicas))
            .map(|i| u8::from(s.writer.rep[i] >= off))
            .sum();
        u8::from(!own.bad && own.len >= off) + others
    }

    /// Whether node `i`'s copy is the committed seal `p`.
    fn matches(&self, s: &World, i: usize, p: Prop) -> bool {
        s.nodes[i].len == p.length && !s.nodes[i].bad
    }

    /// Whether a healthy copy at least `p.length` long is one node `i` can reach.
    fn has_source(&self, s: &World, i: usize, p: Prop) -> bool {
        (0..usize::from(self.cfg.replicas)).any(|m| {
            m != i && self.linked(s, i, m) && !s.nodes[m].bad && s.nodes[m].len >= p.length
        })
    }

    /// Whether some copy holds the seal's bytes undamaged.
    fn bytes_survive(&self, s: &World, p: Prop) -> bool {
        (0..usize::from(self.cfg.replicas)).any(|m| !s.nodes[m].bad && s.nodes[m].len >= p.length)
    }

    /// Whether the proposal describes the segment.
    fn prop_ok(&self, p: Prop) -> bool {
        self.crc_at(p.length) == p.crc
    }

    /// The enabled steps of a state, each with whether it is background work.
    fn enabled(&self, s: &World, out: &mut Vec<(Act, bool)>) {
        for (idx, env) in s.net.in_flight().iter().enumerate() {
            let (from, to, msg) = (env.from, env.to, env.msg);
            out.push((Act::Deliver { idx, from, to, msg }, true));
            if s.losses < self.cfg.max_losses {
                out.push((Act::Lose { idx, from, to, msg }, false));
            }
        }
        let w = &s.writer;
        if s.nodes[0].up {
            let begin = self
                .next_end(w.sent)
                .filter(|_| w.writing && w.pending.is_none());
            if let Some(end) = begin {
                out.push((Act::Begin { start: w.sent, end }, false));
            }
            if let Some((start, end)) = w.pending {
                out.push((Act::LocalSync { start, end }, false));
            }
            if self.cfg.crash_writer && !s.crashed {
                out.push((Act::Crash, false));
            }
        } else {
            out.push((Act::Return, false));
        }
        if let Some(d) = self.cfg.damage
            && !s.damaged
            && s.nodes[ix(d)].len > 0
        {
            out.push((Act::Damage(d), false));
        }
        for (i, node) in s.nodes.iter().enumerate() {
            if !node.up {
                continue;
            }
            let id = nid(i);
            if node.committed.is_none() && node.attempts < self.cfg.attempts[i] {
                out.push((Act::StartSeal(id), false));
            }
            if let Sealer::Preparing { ballot, promises } = &node.sealer
                && promises.len() >= self.quorum()
            {
                let floor = if i == 0 { w.known } else { 0 };
                let prop = choose(
                    promises,
                    self.cfg.replicas,
                    self.cfg.min_acks,
                    floor,
                    self.cfg.variant,
                );
                out.push((
                    Act::Choose {
                        node: id,
                        ballot: *ballot,
                        prop,
                    },
                    false,
                ));
            }
            match node.committed {
                None => {
                    let known =
                        (0..3).find_map(|m| s.nodes[m].committed.filter(|_| self.linked(s, i, m)));
                    if let Some(prop) = known {
                        out.push((Act::Pull { node: id, prop }, true));
                    }
                }
                Some(p) if self.holds(i) => {
                    let truncate = node.len > p.length && !node.bad;
                    let fetch = !self.matches(s, i, p) && self.has_source(s, i, p);
                    if (truncate || fetch) && self.cfg.variant != Variant::NoConverge {
                        out.push((
                            Act::Converge {
                                node: id,
                                length: p.length,
                            },
                            true,
                        ));
                    }
                }
                Some(_) => {}
            }
            if self.cfg.destage && s.destaged.is_none() && self.holds(i) {
                let readable = node
                    .committed
                    .map_or(self.cfg.variant == Variant::DestageEarly, |p| {
                        self.matches(s, i, p) || self.cfg.variant == Variant::DestageUnconverged
                    });
                if readable {
                    out.push((Act::Destage(id), false));
                }
            }
        }
    }
}

impl Model for SealModel {
    type S = World;
    type A = Act;

    fn init(&self) -> Vec<World> {
        let opening = Ballot::opening(WRITER);
        let peer = || types::Peer {
            up: true,
            len: 0,
            bad: false,
            promised: opening,
            accepted: None,
            committed: None,
            seen: opening,
            sealer: Sealer::Idle,
            attempts: 0,
        };
        let mut net = SimNet::new();
        for (from, to) in &self.cfg.cuts {
            net.cut(*from, *to);
        }
        vec![World {
            nodes: [peer(), peer(), peer()],
            net,
            writer: Writer {
                writing: true,
                sent: 0,
                pending: None,
                rep: [0; 3],
                known: 0,
            },
            acked: 0,
            destaged: None,
            losses: 0,
            crashed: false,
            damaged: false,
            s4: false,
            s2: false,
            fenced: false,
        }]
    }

    fn successors(&self, s: &World, out: &mut Vec<Succ<World, Act>>) {
        let mut acts = Vec::new();
        self.enabled(s, &mut acts);
        out.extend(acts.into_iter().map(|(act, bg)| Succ {
            act,
            to: Ok(self.apply(s, act)),
            bg,
        }));
    }

    fn check(&self, s: &World) -> Option<String> {
        if s.s2 {
            return Some("S2 a node's committed seal was asked to change".into());
        }
        if s.s4 {
            return Some("S4 a replica appended a batch below its promise".into());
        }
        let committed: Vec<Prop> = s.nodes.iter().filter_map(|p| p.committed).collect();
        if let Some(pair) = committed.windows(2).find(|w| w[0] != w[1]) {
            return Some(format!(
                "S1 two different seals committed: {:?} and {:?}",
                pair[0], pair[1]
            ));
        }
        if let Some(p) = committed.iter().find(|p| p.length < s.acked) {
            return Some(format!(
                "S3 acknowledged length {} is beyond the committed seal {}",
                s.acked, p.length
            ));
        }
        if let Some(d) = s.destaged {
            match committed.first() {
                None => return Some("S5 destaged before any seal committed".into()),
                Some(p) if *p != d => {
                    return Some(format!("S5 destaged {d:?} but the seal is {p:?}"));
                }
                Some(_) => {}
            }
        }
        for i in 0..3 {
            if self.isolated(s, i) {
                if s.nodes[i].committed.is_some() {
                    return Some(format!(
                        "S6 node {} alone in its partition committed",
                        i + 1
                    ));
                }
                if i == 0 && self.cfg.min_acks > 1 && s.acked > 0 {
                    return Some("S6 the writer alone in its partition acknowledged".into());
                }
            }
        }
        let accepted = s.nodes.iter().filter_map(|p| p.accepted.map(|(_, p)| p));
        if let Some(p) = committed
            .iter()
            .copied()
            .chain(accepted)
            .find(|p| !self.prop_ok(*p))
        {
            return Some(format!("S7 proposal {p:?} does not describe the segment"));
        }
        None
    }

    fn terminal(&self, _: &World) -> Option<String> {
        None
    }

    fn quiescent(&self, s: &World) -> bool {
        s.nodes.iter().any(|p| p.committed.is_some())
    }

    fn converged(&self, s: &World) -> bool {
        // When the only copies of the sealed bytes were lost to damage after the seal was
        // chosen (a tail that was never acknowledged), no copy can converge to it.
        let lost = s
            .nodes
            .iter()
            .find_map(|p| p.committed)
            .is_some_and(|p| !self.bytes_survive(s, p));
        (0..3).all(|i| {
            let node = &s.nodes[i];
            let r = self.reach(s, i);
            let helped = (0..3).any(|j| r[j] && s.nodes[j].committed.is_some());
            if !node.up || !helped || lost {
                return true;
            }
            node.committed
                .is_some_and(|p| !self.holds(i) || self.matches(s, i, p))
        })
    }

    fn show_act(&self, a: &Act) -> String {
        format!("{a:?}")
    }

    fn show_state(&self, s: &World) -> String {
        let nodes: Vec<String> = s
            .nodes
            .iter()
            .enumerate()
            .map(|(i, p)| {
                format!(
                    "n{}[up={} len={} bad={} promised={}.{} accepted={:?} committed={:?} {:?}]",
                    i + 1,
                    p.up,
                    p.len,
                    p.bad,
                    p.promised.round,
                    p.promised.node,
                    p.accepted,
                    p.committed,
                    p.sealer
                )
            })
            .collect();
        format!(
            "{} writer={:?} acked={} net={:?}",
            nodes.join(" "),
            s.writer,
            s.acked,
            s.net.in_flight()
        )
    }
}
