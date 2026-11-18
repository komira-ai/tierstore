//! The definitive-miss rule and the `LOCATE` coordinator (10.6, 10.2 on `min_acks`, 10.8 view
//! states, 4.6 lost path).
//!
//! Three layers:
//!
//! - [`decide`] / [`decide_with_threshold`]: the pure reference decision from per-node answers
//!   ([`NodeAnswer`]) to a [`Located`]. A hit is final; `N - min_acks + 1` absent answers
//!   ([`miss_threshold`], the frozen `definitive_miss_answers`) are a definitive miss; fewer is
//!   [`NotDefinitive::Indexing`] when some node answered `indexing`, else
//!   [`NotDefinitive::TooFewAnswers`]. A miss is judged on the absent answers alone: an `indexing`
//!   or down node outside the `N - min_acks + 1` that answered absent does not weaken it, since
//!   an acknowledged object sits on `min_acks` nodes and any `N - min_acks + 1` nodes include one.
//! - [`check_rule`]: an exhaustive check of a rule over every [`Scenario`] of a [`Config`]: every
//!   assignment of node conditions (up, `indexing`, down) against every truth (no acknowledged
//!   copy, or a copy on any set of at least `min_acks` nodes). The bound is `3^N` condition
//!   assignments times at most `2^N - N` truths (`N = 6`, `min_acks = 2`: 729 * 58 = 42282
//!   scenarios). It states four properties: a hit is final and only a hit is `Found`; safety (no
//!   definitive miss while a copy exists); decisiveness (no copy and enough absent answers is a
//!   miss); and the reason of a non-definitive answer matches the answers.
//! - [`Locator`] and [`Fill`]: the coordinator over a `PeerClient`. It asks the nodes in the
//!   caller's order (rank order; a `suspect` node last, a `down` node not at all), stops at the
//!   first hit or when the rule decides, and asks no node twice for one key in one round. A
//!   [`Fill`] keeps the answer per key so one fill is one `LOCATE` per key per node; the one
//!   deliberate second round is [`Fill::locate_again`] (4.6, before a fill is declared lost).
//!
//! [`fate`] maps the answer to what a fill does: resolve, wait, or be lost. The wait's limit
//! (`store.unavailable_wait`) belongs to the engine and is not modelled here.
//!
//! Section 14 rows: "Definitive miss (rule `N - min_acks + 1`)", and the coordinator's part of
//! "Pack lost mid-job".

use std::collections::BTreeMap;

use bytes::Bytes;
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_fetch_api::upstream::{
    DeviceRef, Located, Location, NotDefinitive, Source, SourceKind,
};
use tierstore_format::ids::{Generation, NodeId};
use tierstore_store_api::fake::FakeCluster;
use tierstore_store_api::peer::{PeerClient, PeerError};
use tierstore_store_api::proto::{Envelope, HotCopy, LocateAnswer, LocateRequest, LocateResponse};
use tierstore_store_api::view::{NodeState, definitive_miss_answers};

/// Settings no cluster can meet: `min_acks` is 0 or above the number of storage nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("min_acks {min_acks} cannot be met by {storage_nodes} storage nodes")]
pub struct SettingsError {
    /// `N`.
    pub storage_nodes: usize,
    /// `hot.min_acks`.
    pub min_acks: u8,
}

/// What one storage node said to a `LOCATE` of one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeAnswer {
    /// The object is indexed here.
    Hit(Location),
    /// Not in the node's index.
    Absent,
    /// The node's index is not built.
    Indexing,
    /// No answer: the node is down, unreachable, or answered with an error.
    NoAnswer,
}

/// The number of absent answers that make a miss definitive, `N - min_acks + 1`.
///
/// # Errors
///
/// [`SettingsError`] when `min_acks` is 0 or above `storage_nodes`.
pub fn miss_threshold(storage_nodes: usize, min_acks: u8) -> Result<usize, SettingsError> {
    definitive_miss_answers(storage_nodes, min_acks).ok_or(SettingsError {
        storage_nodes,
        min_acks,
    })
}

/// The decision of the rule with an explicit `threshold` of absent answers: the first hit is
/// `Found`; `threshold` absent answers are a definitive miss; else `Indexing` when any node
/// answered so, else too few answers. [`decide`] passes the frozen threshold; the exhaustive
/// check is run with other thresholds to show that it rejects a wrong rule.
#[must_use]
pub fn decide_with_threshold(threshold: usize, answers: &[NodeAnswer]) -> Located {
    if let Some(location) = answers.iter().find_map(|a| match a {
        NodeAnswer::Hit(location) => Some(location),
        _ => None,
    }) {
        return Located::Found(location.clone());
    }
    let absent = answers.iter().filter(|a| **a == NodeAnswer::Absent).count();
    if absent >= threshold {
        Located::DefinitiveMiss
    } else if answers.contains(&NodeAnswer::Indexing) {
        Located::NotDefinitive(NotDefinitive::Indexing)
    } else {
        Located::NotDefinitive(NotDefinitive::TooFewAnswers)
    }
}

/// The reference decision for `storage_nodes` storage nodes and `min_acks` from the answers
/// received so far.
///
/// # Errors
///
/// [`SettingsError`] when the settings cannot be met.
pub fn decide(
    storage_nodes: usize,
    min_acks: u8,
    answers: &[NodeAnswer],
) -> Result<Located, SettingsError> {
    Ok(decide_with_threshold(
        miss_threshold(storage_nodes, min_acks)?,
        answers,
    ))
}

/// What a fill does with the answer of its `LOCATE` (4.6, 10.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fate {
    /// The object is there: fetch it.
    Resolved(Location),
    /// Not a miss: wait and ask again; never `EIO` at once.
    Wait(NotDefinitive),
    /// A definitive miss: the pack is lost.
    Lost,
}

/// The fate of a fill whose `LOCATE` answered `located`.
#[must_use]
pub fn fate(located: Located) -> Fate {
    match located {
        Located::Found(location) => Fate::Resolved(location),
        Located::DefinitiveMiss => Fate::Lost,
        Located::NotDefinitive(why) => Fate::Wait(why),
    }
}

/// The generation of the objects the scenarios and the sample copies hold.
#[must_use]
pub const fn sample_generation() -> Generation {
    Generation([1; 16])
}

/// The hot copy on `node` that [`Scenario::cluster`] stores and [`sample_hit`] describes.
#[must_use]
pub fn sample_copy(node: NodeId) -> HotCopy {
    HotCopy {
        node,
        file: "seg-1".to_owned(),
        offset: 0,
        length: 1,
        promoted: false,
    }
}

/// The location a hit at `node` has in the scenarios: the one byte of [`sample_copy`].
#[must_use]
pub fn sample_hit(node: NodeId) -> Location {
    Location {
        generation: Some(sample_generation()),
        size: Some(1),
        sources: vec![Source {
            device: DeviceRef::Hot(node),
            kind: SourceKind::Hot {
                file: "seg-1".to_owned(),
                offset: 0,
            },
            extent: ByteRange { offset: 0, len: 1 },
        }],
        conflicts: Vec::new(),
    }
}

/// The condition of a node in a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    /// Up and indexed: answers from its index.
    Up,
    /// Up with its index not built.
    Indexing,
    /// Not answering.
    Down,
}

/// The storage-node count and `hot.min_acks` of a configuration to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// `N`.
    pub storage_nodes: usize,
    /// `hot.min_acks`.
    pub min_acks: u8,
}

/// One assignment of node conditions and one truth. Node `i` (0-based) is `NodeId(i + 1)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    /// The condition of each node.
    pub conds: Vec<Cond>,
    /// Whether each node durably holds an acknowledged copy (all false: none exists).
    pub holders: Vec<bool>,
}

impl Scenario {
    /// Whether an acknowledged copy exists anywhere.
    #[must_use]
    pub fn copy_exists(&self) -> bool {
        self.holders.iter().any(|h| *h)
    }

    /// What each node answers, in node order: a down node nothing, an `indexing` node `Indexing`
    /// (a holder too: its index is not built), an up node `Hit` when it holds a copy, else
    /// `Absent`.
    #[must_use]
    pub fn answers(&self) -> Vec<NodeAnswer> {
        self.conds
            .iter()
            .zip(&self.holders)
            .enumerate()
            .map(|(i, (cond, holds))| match (cond, holds) {
                (Cond::Down, _) => NodeAnswer::NoAnswer,
                (Cond::Indexing, _) => NodeAnswer::Indexing,
                (Cond::Up, true) => NodeAnswer::Hit(sample_hit(node_id(i))),
                (Cond::Up, false) => NodeAnswer::Absent,
            })
            .collect()
    }

    /// A fake cluster in this scenario on `clock`: the object `key` of one byte stored on each
    /// holder with [`sample_copy`], a down node crashed and an `indexing` node set so.
    #[must_use]
    pub fn cluster(&self, clock: ManualClock, key: &str) -> FakeCluster {
        let mut cluster = FakeCluster::new(clock);
        for (i, (cond, holds)) in self.conds.iter().zip(&self.holders).enumerate() {
            let id = node_id(i);
            let node = cluster.spawn(id);
            if *holds {
                node.put_hot(
                    key,
                    sample_generation(),
                    Bytes::from_static(b"x"),
                    vec![sample_copy(id)],
                );
            }
            match cond {
                Cond::Up => {}
                Cond::Indexing => node.set_indexing(true),
                Cond::Down => node.crash(),
            }
        }
        cluster
    }
}

fn node_id(index: usize) -> NodeId {
    NodeId(u8::try_from(index + 1).unwrap_or(u8::MAX))
}

/// Every scenario of `cfg`: first those with an acknowledged copy (every set of at least
/// `min_acks` holders), then the one with none; each against all `3^N` condition assignments.
///
/// # Errors
///
/// [`SettingsError`] when the settings cannot be met.
pub fn scenarios(cfg: Config) -> Result<Vec<Scenario>, SettingsError> {
    miss_threshold(cfg.storage_nodes, cfg.min_acks)?;
    let n = cfg.storage_nodes;
    let truths = (1u32..(1 << n))
        .filter(|mask| mask.count_ones() as usize >= usize::from(cfg.min_acks))
        .chain(std::iter::once(0));
    let mut out = Vec::new();
    for mask in truths {
        let holders: Vec<bool> = (0..n).map(|i| (mask >> i) & 1 == 1).collect();
        for mut code in 0..3usize.pow(u32::try_from(n).unwrap_or(u32::MAX)) {
            let mut conds = Vec::with_capacity(n);
            for _ in 0..n {
                conds.push([Cond::Up, Cond::Indexing, Cond::Down][code % 3]);
                code /= 3;
            }
            out.push(Scenario {
                conds,
                holders: holders.clone(),
            });
        }
    }
    Ok(out)
}

/// Which property a rule broke in a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    /// A hit was answered and the rule did not say `Found`.
    HitNotFinal,
    /// The rule said `Found` and no node hit.
    FoundWithoutHit,
    /// Safety: a definitive miss while an acknowledged copy exists.
    MissWhileCopyExists,
    /// Decisiveness: no copy, at least `N - min_acks + 1` absent answers, and no miss.
    NotDecisive,
    /// No copy, too few absent answers, and the answer is not the non-definitive reason the
    /// answers give (`Indexing` when a node answered so, else too few answers).
    WrongReason,
}

/// The first scenario a rule fails in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counterexample {
    /// The scenario.
    pub scenario: Scenario,
    /// What the nodes answered.
    pub answers: Vec<NodeAnswer>,
    /// What the rule decided.
    pub decided: Located,
    /// The property broken.
    pub violation: Violation,
}

/// A passed exhaustive check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checked {
    /// The number of scenarios the rule was run on.
    pub scenarios: usize,
}

/// Runs `rule` (from the answers of all nodes to a decision) on every scenario of `cfg` and
/// returns the first that breaks a property, or the count of scenarios when none does.
///
/// # Errors
///
/// [`SettingsError`] when the settings cannot be met; the inner `Err` is the counterexample.
pub fn check_rule(
    cfg: Config,
    rule: &dyn Fn(&[NodeAnswer]) -> Located,
) -> Result<Result<Checked, Box<Counterexample>>, SettingsError> {
    let threshold = miss_threshold(cfg.storage_nodes, cfg.min_acks)?;
    let all = scenarios(cfg)?;
    let total = all.len();
    for scenario in all {
        let answers = scenario.answers();
        let decided = rule(&answers);
        if let Some(violation) = judge(threshold, &scenario, &answers, &decided) {
            return Ok(Err(Box::new(Counterexample {
                scenario,
                answers,
                decided,
                violation,
            })));
        }
    }
    Ok(Ok(Checked { scenarios: total }))
}

fn judge(
    threshold: usize,
    scenario: &Scenario,
    answers: &[NodeAnswer],
    decided: &Located,
) -> Option<Violation> {
    let hit = answers.iter().any(|a| matches!(a, NodeAnswer::Hit(_)));
    let found = matches!(decided, Located::Found(_));
    if hit && !found {
        return Some(Violation::HitNotFinal);
    }
    if found && !hit {
        return Some(Violation::FoundWithoutHit);
    }
    let miss = *decided == Located::DefinitiveMiss;
    if scenario.copy_exists() {
        return miss.then_some(Violation::MissWhileCopyExists);
    }
    let absent = answers.iter().filter(|a| **a == NodeAnswer::Absent).count();
    if absent >= threshold {
        return (!miss).then_some(Violation::NotDecisive);
    }
    let reason = if answers.contains(&NodeAnswer::Indexing) {
        NotDefinitive::Indexing
    } else {
        NotDefinitive::TooFewAnswers
    };
    (*decided != Located::NotDefinitive(reason)).then_some(Violation::WrongReason)
}

/// One node's answer from the result of its `LOCATE` of one key: an error, or a response without
/// an answer for the key, is no answer; a hot or cold answer is a hit whose location lists a
/// promoted copy as [`SourceKind::HotClean`], the other copies as [`SourceKind::Hot`], and each
/// cold extent as one [`SourceKind::Cold`] source on its data shard's drive. The degraded-read
/// sources of a cold extent are not carried over.
#[must_use]
pub fn node_answer(result: Result<Envelope<LocateResponse>, PeerError>) -> NodeAnswer {
    let Ok(envelope) = result else {
        return NodeAnswer::NoAnswer;
    };
    match envelope.body.answers.into_iter().next() {
        None => NodeAnswer::NoAnswer,
        Some(LocateAnswer::Absent) => NodeAnswer::Absent,
        Some(LocateAnswer::Indexing) => NodeAnswer::Indexing,
        Some(LocateAnswer::Hot {
            generation,
            size,
            copies,
        }) => NodeAnswer::Hit(location(
            generation,
            size,
            copies
                .into_iter()
                .map(|c| Source {
                    device: DeviceRef::Hot(c.node),
                    kind: if c.promoted {
                        SourceKind::HotClean {
                            file: c.file,
                            offset: c.offset,
                        }
                    } else {
                        SourceKind::Hot {
                            file: c.file,
                            offset: c.offset,
                        }
                    },
                    extent: ByteRange {
                        offset: 0,
                        len: c.length,
                    },
                })
                .collect(),
        )),
        Some(LocateAnswer::Cold {
            generation,
            size,
            extents,
        }) => NodeAnswer::Hit(location(
            generation,
            size,
            extents
                .into_iter()
                .map(|e| Source {
                    device: DeviceRef::Drive(e.node, e.drive),
                    kind: SourceKind::Cold {
                        block: e.block,
                        shard: e.shard,
                        offset: e.block_offset,
                    },
                    extent: ByteRange {
                        offset: e.object_offset,
                        len: e.length,
                    },
                })
                .collect(),
        )),
    }
}

fn location(generation: Generation, size: u64, sources: Vec<Source>) -> Location {
    Location {
        generation: Some(generation),
        size: Some(size),
        sources,
        conflicts: Vec::new(),
    }
}

/// The `LOCATE` coordinator of one cluster view: the storage nodes in the order to ask them and
/// the miss threshold of `hot.min_acks`.
pub struct Locator<'a, P: PeerClient + ?Sized> {
    peers: &'a P,
    nodes: Vec<(NodeId, NodeState)>,
    threshold: usize,
}

impl<'a, P: PeerClient + ?Sized> Locator<'a, P> {
    /// A coordinator over `peers` for the storage nodes `nodes`, in the caller's preference
    /// order (all `N` of them, `down` ones too: they count toward `N`). A `suspect` node is asked
    /// after the others; a `down` node is not asked and counts as no answer.
    ///
    /// # Errors
    ///
    /// [`SettingsError`] when `min_acks` cannot be met by `nodes.len()` nodes.
    pub fn new(
        peers: &'a P,
        nodes: Vec<(NodeId, NodeState)>,
        min_acks: u8,
    ) -> Result<Self, SettingsError> {
        let threshold = miss_threshold(nodes.len(), min_acks)?;
        Ok(Self {
            peers,
            nodes,
            threshold,
        })
    }

    /// One round of `LOCATE` for `key`: each node at most once, in order (`suspect` last, `down`
    /// skipped), stopping at the first hit, or as soon as the absent answers reach `N - min_acks
    /// + 1`; otherwise all nodes are asked and the answer is the rule's on all of them.
    pub async fn locate(&self, key: &str) -> Located {
        let (live, suspect): (Vec<_>, Vec<_>) = self
            .nodes
            .iter()
            .filter(|(_, state)| *state != NodeState::Down)
            .partition(|(_, state)| *state != NodeState::Suspect);
        let down = self.nodes.len() - live.len() - suspect.len();
        let mut answers = vec![NodeAnswer::NoAnswer; down];
        for (node, _) in live.into_iter().chain(suspect) {
            let req = LocateRequest {
                keys: vec![key.to_owned()],
            };
            answers.push(node_answer(self.peers.locate(node, req).await));
            let decided = decide_with_threshold(self.threshold, &answers);
            if !matches!(decided, Located::NotDefinitive(_)) {
                return decided;
            }
        }
        decide_with_threshold(self.threshold, &answers)
    }
}

/// One fill's `LOCATE`s: the answer per key, so a key is asked of each node once per fill.
pub struct Fill<'l, 'a, P: PeerClient + ?Sized> {
    locator: &'l Locator<'a, P>,
    answers: BTreeMap<String, Located>,
}

impl<'l, 'a, P: PeerClient + ?Sized> Fill<'l, 'a, P> {
    /// A fill that has located nothing yet.
    #[must_use]
    pub fn new(locator: &'l Locator<'a, P>) -> Self {
        Self {
            locator,
            answers: BTreeMap::new(),
        }
    }

    /// The location of `key`: the round of the first call, then its answer again with no call to
    /// any node.
    pub async fn locate(&mut self, key: &str) -> Located {
        if let Some(known) = self.answers.get(key) {
            return known.clone();
        }
        self.locate_again(key).await
    }

    /// A new round for `key` whatever was answered before (4.6: asked again before the fill is
    /// declared lost); its answer replaces the kept one.
    pub async fn locate_again(&mut self, key: &str) -> Located {
        let located = self.locator.locate(key).await;
        self.answers.insert(key.to_owned(), located.clone());
        located
    }
}
