//! The bounds and variants of one run.

use crate::lin::{Eff, Effs};

/// Number of client actors: proxies 0 and 1, and the old-release proxy 2.
pub const NC: usize = 3;
/// The version id of the object hot holds in the initial states that have one.
pub const H0: u8 = 2;
/// The version id of an older object cold holds in the initial states that have one.
pub const C0: u8 = 1;
/// A body a one-shot stream lost: whatever a protocol writes from it is not any client's bytes.
pub const CORRUPT: u8 = 7;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Body {
    /// Held bytes, a staged file: can be sent again.
    Replay,
    /// A one-shot stream: consumed by the first store call that reads it.
    Stream,
}

/// A client operation of a proxy's script.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Op {
    /// A plain put (S3 `PutObject`, last writer wins).
    Put(Body),
    /// A create-only put (`If-None-Match: *`).
    PutCo(Body),
    /// A put with `If-Match: <tag>`; the tag is a version id.
    PutIm(u8),
    /// A plain put of a hot tier at `Full`: lands in cold under the lock.
    PutCold,
    Delete,
    Get,
    List,
    /// The previous release: put, get and delete by the entry protocol, which ignores markers.
    OldPut,
    OldGet,
    OldDelete,
}

impl Op {
    /// True for the operations that can change the key.
    pub fn mutates(self) -> bool {
        !matches!(self, Op::Get | Op::List | Op::OldGet)
    }
}

/// What the stores hold before the scripts start.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Init {
    /// Nothing anywhere.
    Empty,
    /// An acknowledged put not copied yet: hot has `H0`, a `DIRTY` marker.
    HotDirty,
    /// The same, with an older object `C0` in cold: a read that wrongly trusts cold sees it.
    Lagging,
    /// Copied and unmarked: both tiers hold `H0`.
    Both,
    /// Evicted: cold holds `H0`, hot nothing, no marker.
    ColdOnly,
    /// Deleted, cold not yet cleaned: cold holds `C0`, hot nothing, a `DIRTY` marker saying `gone`.
    DeletedCold,
}

/// The wrong variants of the protocol: each must be rejected by the property named next to it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Flaw {
    None,
    /// A7: the fast path writes the object and no marker.
    NoMarkerOnPut,
    /// A9/A10: the copier deletes the marker without the CAS to `CLEARING`.
    ClearWithoutCas,
    /// A16: the evictor deletes hot without taking the lock.
    EvictNoLock,
    /// A13: the leader clears a `gone` marker right after the cold delete, with no hold.
    NoHold,
    /// A14: a delete's `gone` marker is written after the hot DELETE.
    DeleteGoneLate,
    /// A15: a cold landing without the lock.
    ColdLandNoLock,
    /// A17: the copy-back lands with a plain PUT instead of a create-only one.
    CopyBackPlainPut,
    /// A23/A2: a delete leaves no `gone` marker behind.
    DeleteNoMarker,
    /// A2/A9: a hot-miss read ignores the marker.
    ReadIgnoresMarker,
    /// A6: a writer that meets a lock answers 503 at once.
    ImmediateLock503,
    /// A24: a stream that lost the creation race goes on as an overwrite of the bytes it lost.
    StreamRaceContinues,
    /// A20: an ambiguous lock write is assumed to have landed.
    AssumeLockOnUnknown,
    /// A23: a delete waits while the copier is mid-copy.
    DeleteWaitsForCopier,
    /// A21: no `evict_min_age`.
    NoEvictMinAge,
    /// A5: `If-Match` compares once, before the lock, and not again under it.
    IfMatchNoRecheck,
    /// A8/A10: the overwrite replaces with a plain PUT, the copier writes cold with a plain PUT.
    PlainReplace,
    /// A10: the copier's cold write is a plain PUT.
    CopierPlainColdPut,
    /// A7/A4: the leader clears an orphan marker without waiting for the put to finish.
    NoOrphanGuard,
    /// A8/A1: the overwrite takes no lock.
    OverwriteNoLock,
    /// A4/A18: the leader never reaps a dead owner's lock or finishes its `CLEARING`.
    NoReap,
    /// A11: a writer that meets a dead owner's lock waits for ever (no `lock_wait`).
    WaitForever,
    /// A11/A12: a read waits while a lock is held.
    ReadWaitsOnLock,
    /// A12: the fast path clears `gone` before its object has landed.
    GoneClearedFirst,
    /// A17: the copy-back takes no lock.
    CopyBackNoLock,
    /// A21: a create-only put does not ask cold.
    CreateOnlyIgnoresCold,
    /// A8/A1: a holder whose hot DELETE timed out releases its lock at once.
    ReleaseOnTimeout,
}

/// Which stalled calls may land late (the ghost mode of Appendix A).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum GhostKind {
    /// A hot DELETE (R1).
    HotDelete,
    /// A hot create (R3', the late landing of an unacknowledged put).
    HotCreate,
    /// An `If-Match` replace of hot.
    HotReplace,
    /// A cold write or cold DELETE (R2').
    ColdWrite,
    /// A marker create or CAS.
    MarkerWrite,
    /// A marker DELETE (R4').
    MarkerDelete,
    /// Any of them.
    Any,
}

/// The state of the format gate, for the old-release proxy of the rollout scenarios.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Phase {
    /// `{phase: active-marks}`: the old release fails closed.
    ActiveMarks,
    /// `{phase: active, epoch >= 2}`: the old release runs the entry protocol beside the markers.
    ActiveEpoch2,
}

/// One run: scripts, bounds, enabled actors and the variant.
#[derive(Clone, Debug)]
pub struct Cfg {
    pub init: Init,
    pub scripts: [Vec<Op>; NC],
    /// The leader's copier (with reaper and copy-back).
    pub copier: bool,
    /// How many evictions the evictor may start.
    pub evictions: u8,
    pub sweep: bool,
    pub crash_clients: bool,
    pub crash_leader: bool,
    pub max_crashes: u8,
    /// Ghost mode: a crashed actor's pending call may land later.
    pub ghost: Option<GhostKind>,
    pub flaw: Flaw,
    /// Marker writes of the lock may come back as an unknown outcome.
    pub ambiguity: bool,
    pub phase: Phase,
    /// The leader may stall (without dying) before it has written `CLEARING`.
    pub stall_leader: bool,
    /// Fix, A12: a put that finds a `DIRTY` marker HEADs hot before it bumps `gone`.
    pub head_before_bump: bool,
    /// Fix, 5.13: a reaped overwrite or cold-landing lock keeps its `gone`.
    pub reap_keeps_gone: bool,
    /// Fix, 5.1 row 2: a put that finds `DIRTY` bumps it keeping its `gone` (the copier clears it
    /// after it replaced the cold copy), so no reader sees the deleted cold copy.
    pub bump_keeps_gone: bool,
    /// Fix, 5.6: a delete takes its lock with the `gone` it found (false on a create), HEADs hot
    /// under the lock, and only then writes `gone=true` into the lock (one more CAS); the hot
    /// DELETE follows only if the HEAD found an object. Written at the start, `gone` takes
    /// effect at once for a key only cold has, and the hot DELETE would then remove an object
    /// created after the delete's own effect.
    pub delete_decides_gone: bool,
    /// Fix, 5.9: after the tombstone hold the copier HEADs hot again, because a put whose marker
    /// landed before a delete may land its object during the hold.
    pub hold_rechecks_hot: bool,
    /// Fix, 5.9: a `gone` marker whose key was recreated is also held after the copier replaced
    /// the cold object, before it is cleared.
    pub recreate_hold: bool,
    /// Fix, 5.7 and 5.8: a read or list that finds `gone` HEADs hot before it answers "absent",
    /// because a put may have landed since its first hot miss.
    pub gone_rechecks_hot: bool,
    /// Fix, 5.10: `evict_min_age` counts from the copier's cold write, not from the hot object's
    /// creation, so a hot-miss pair or a create-only wave 1 that read cold before the copy has
    /// ended (the proxy discards it) by the time the eviction deletes hot.
    pub evict_age_from_copy: bool,
    /// Fix, 5.9: after its cold HEAD the copier reads the marker again and gives up if it moved,
    /// so it cannot copy an older hot object over a cold object a cold landing just wrote.
    pub copier_rechecks_marker: bool,
    /// Fix, 5.5: a put at hot `Full` that finds a `gone` marker is 503 `SlowDown` until the
    /// copier has cleared the tombstone (nothing in 5.5 can clear `gone` safely before cold holds
    /// the new object and no read of the old one can still be open).
    pub coldland_503_on_gone: bool,
    /// 5.5 read the other way: the cold-landing lock keeps `gone` of the marker it took over.
    pub coldland_keeps_gone: bool,
    /// The world of 5.1 step 5: a put's object write may outlast `orphan_after` and the
    /// tombstone hold (there is no size cap, so no put deadline short of the transfer). The
    /// guards that held the copier back while an object write was pending are gone.
    pub puts_unbounded: bool,
    /// Fix, 5.1 step 5 (used when `puts_unbounded`): a put whose object landed re-asserts its
    /// marker (GET; create, CAS or wait) before it acknowledges, because the copier may have
    /// cleared the marker as an orphan while the body was still streaming.
    pub put_reasserts_marker: bool,
    /// The world of 5.6 step 2 and 5.10: a live holder's hot DELETE may time out and still be
    /// queued at the store (the proxy answers 503 while the call lands later).
    pub timeouts: bool,
    /// Fix, 5.2: a holder whose hot DELETE ended unknown keeps its lock until the call can no
    /// longer land (a margin longer than the store's request lifetime), then releases.
    pub timeout_keeps_lock: bool,
    /// Assertion A8 on or off (off: the run shows the damage rather than the call).
    pub a8: bool,
    pub max_states: usize,
}

impl Cfg {
    pub fn new(init: Init) -> Cfg {
        Cfg {
            init,
            scripts: [Vec::new(), Vec::new(), Vec::new()],
            copier: true,
            evictions: 0,
            sweep: true,
            crash_clients: false,
            crash_leader: false,
            max_crashes: 0,
            ghost: None,
            flaw: Flaw::None,
            ambiguity: false,
            phase: Phase::ActiveMarks,
            stall_leader: false,
            head_before_bump: false,
            bump_keeps_gone: false,
            delete_decides_gone: false,
            hold_rechecks_hot: false,
            recreate_hold: false,
            gone_rechecks_hot: false,
            evict_age_from_copy: false,
            copier_rechecks_marker: false,
            coldland_503_on_gone: false,
            reap_keeps_gone: false,
            coldland_keeps_gone: false,
            puts_unbounded: false,
            put_reasserts_marker: false,
            timeouts: false,
            timeout_keeps_lock: false,
            a8: true,
            max_states: 30_000_000,
        }
    }

    /// The global index of the `j`th operation of client `c`.
    pub(crate) fn gidx(&self, c: usize, j: usize) -> u8 {
        let before: usize = self.scripts[..c].iter().map(Vec::len).sum();
        (before + j) as u8
    }

    /// The version id operation `g` writes: 3 + the number of mutating operations before it.
    pub(crate) fn value(&self, g: u8) -> u8 {
        let mut n = 0u8;
        let mut i = 0u8;
        for s in &self.scripts {
            for op in s {
                if i == g {
                    return 3 + n;
                }
                if op.mutates() {
                    n += 1;
                }
                i += 1;
            }
        }
        3 + n
    }

    pub(crate) fn effs(&self) -> Effs {
        let mut e: Effs = [None; 8];
        for c in 0..NC {
            for (j, op) in self.scripts[c].iter().enumerate() {
                let g = self.gidx(c, j);
                let v = self.value(g);
                e[usize::from(g)] = match *op {
                    Op::Put(_) | Op::PutCold => Some(Eff::Put(v)),
                    Op::PutCo(_) | Op::OldPut => Some(Eff::PutCo(v)),
                    Op::PutIm(t) => Some(Eff::PutIm(v, t)),
                    Op::Delete | Op::OldDelete => Some(Eff::Del),
                    Op::Get | Op::List | Op::OldGet => None,
                };
            }
        }
        e
    }

    /// True when the run has no crash, ghost or ambiguity: every plain put must then be a 200.
    pub(crate) fn clean(&self) -> bool {
        self.max_crashes == 0 && self.ghost.is_none() && !self.ambiguity && !self.stall_leader
    }
}
