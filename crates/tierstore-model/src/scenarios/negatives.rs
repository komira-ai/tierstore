//! Each property's negative: a wrong variant of the protocol the checker must reject.

use super::*;
use crate::marks::{Body, Cfg, Flaw, GhostKind, Init, Op};
use Body::{Replay, Stream};
use Expect::{Fail, Pass};

fn push(
    v: &mut Vec<Scenario>,
    id: &'static str,
    claims: &'static str,
    desc: &'static str,
    cfg: Cfg,
    expect: Expect,
    heavy: bool,
) {
    // Every run but the document-as-written ones applies the corrections.
    let cfg = if id.starts_with("lit/") {
        cfg
    } else {
        fixed(cfg)
    };
    let liveness = cfg.copier;
    v.push(Scenario {
        id,
        claims,
        desc,
        sys: Sys::Marks(cfg),
        expect,
        heavy,
        liveness,
    });
}

pub(super) fn negatives(v: &mut Vec<Scenario>) {
    let mut add = |id: &'static str,
                   claims: &'static str,
                   desc: &'static str,
                   cfg: Cfg,
                   expect: Expect,
                   heavy: bool| { push(v, id, claims, desc, cfg, expect, heavy) };
    let flaw = |c: Cfg, f: Flaw| with(c, |c| c.flaw = f);
    // Each property's negative: a wrong variant that must be rejected.
    add(
        "negative/no-marker-on-put",
        "A7",
        "the fast path writes no marker",
        flaw(
            base(Init::Empty, &[Op::Put(Replay)], &[Op::Get]),
            Flaw::NoMarkerOnPut,
        ),
        Fail("A7"),
        false,
    );
    add(
        "negative/clear-without-cas",
        "A9 A10",
        "the copier deletes the marker without the CAS",
        flaw(
            base(Init::HotDirty, &[Op::Put(Replay)], &[Op::Get]),
            Flaw::ClearWithoutCas,
        ),
        Fail("A9"),
        false,
    );
    add(
        "negative/evict-without-lock",
        "A16",
        "the evictor deletes hot without the lock (A8 off, to show the loss)",
        with(
            flaw(
                with(base(Init::Both, &[Op::Put(Replay)], &[Op::Get]), |c| {
                    c.evictions = 1
                }),
                Flaw::EvictNoLock,
            ),
            |c| c.a8 = false,
        ),
        Fail("A1"),
        false,
    );
    add(
        "negative/evict-without-lock-a8",
        "A8 A16",
        "the same, with the exclusion assertion on",
        flaw(
            with(base(Init::Both, &[Op::Put(Replay)], &[Op::Get]), |c| {
                c.evictions = 1
            }),
            Flaw::EvictNoLock,
        ),
        Fail("A8"),
        false,
    );
    add(
        "negative/no-hold",
        "A13",
        "the parallel read pair with no tombstone hold",
        flaw(base(Init::Lagging, &[Op::Delete], &[Op::Get]), Flaw::NoHold),
        Fail("A2"),
        false,
    );
    add(
        "negative/delete-gone-late",
        "A14",
        "the gone flag written after the hot DELETE",
        flaw(
            base(Init::Lagging, &[Op::Delete], &[Op::Get, Op::Get]),
            Flaw::DeleteGoneLate,
        ),
        Fail("A1"),
        false,
    );
    add(
        "negative/cold-land-without-lock",
        "A15",
        "a cold landing without the lock",
        flaw(
            base(Init::Both, &[Op::PutCold], &[Op::PutCo(Replay), Op::Get]),
            Flaw::ColdLandNoLock,
        ),
        Fail("A8"),
        false,
    );
    add(
        "negative/copy-back-plain-put",
        "A17",
        "the copy-back lands with a plain PUT",
        flaw(
            base(Init::ColdOnly, &[Op::Get], &[Op::Put(Replay)]),
            Flaw::CopyBackPlainPut,
        ),
        Fail("A1"),
        false,
    );
    add(
        "negative/delete-leaves-no-marker",
        "A23",
        "a delete that leaves no gone marker behind",
        flaw(
            base(Init::Lagging, &[Op::Delete], &[Op::Get, Op::Get]),
            Flaw::DeleteNoMarker,
        ),
        Fail("A1"),
        false,
    );
    add(
        "negative/read-ignores-marker",
        "A2 A9",
        "a hot-miss read that ignores the marker",
        flaw(
            base(Init::DeletedCold, &[Op::Get], &[Op::Get]),
            Flaw::ReadIgnoresMarker,
        ),
        Fail("A2"),
        false,
    );
    add(
        "negative/immediate-503",
        "A6",
        "a writer that meets a live lock answers 503 at once",
        flaw(
            base(Init::HotDirty, &[Op::Put(Replay)], &[Op::Put(Replay)]),
            Flaw::ImmediateLock503,
        ),
        Fail("A6"),
        false,
    );
    add(
        "negative/stream-continues",
        "A24",
        "a lost-race stream goes on as an overwrite of bytes it no longer has",
        flaw(
            base(Init::Empty, &[Op::Put(Stream)], &[Op::Put(Replay)]),
            Flaw::StreamRaceContinues,
        ),
        Fail("A24"),
        false,
    );
    add(
        "negative/assume-lock-on-unknown",
        "A20",
        "an ambiguous lock write assumed to have landed",
        flaw(
            with(
                base(Init::HotDirty, &[Op::Put(Replay)], &[Op::Delete]),
                |c| c.ambiguity = true,
            ),
            Flaw::AssumeLockOnUnknown,
        ),
        Fail("A8"),
        false,
    );
    add(
        "negative/delete-waits-for-copier",
        "A23",
        "a delete that waits while the copier is mid-copy",
        flaw(
            with(base(Init::HotDirty, &[Op::Delete], &[Op::Get]), |c| {
                c.stall_leader = true
            }),
            Flaw::DeleteWaitsForCopier,
        ),
        Fail("A23"),
        false,
    );
    add(
        "negative/no-evict-min-age",
        "A21",
        "no evict_min_age: an eviction between the checks and the landing",
        flaw(
            with(
                base(
                    Init::Empty,
                    &[Op::PutCo(Replay)],
                    &[Op::Put(Replay), Op::Get],
                ),
                |c| c.evictions = 1,
            ),
            Flaw::NoEvictMinAge,
        ),
        Fail("A5"),
        false,
    );
    add(
        "lit/eviction-age-from-landing",
        "A21",
        "evict_min_age counted from the object's creation, not from the copy",
        fixed_without(
            with(
                base(
                    Init::Empty,
                    &[Op::PutCo(Replay)],
                    &[Op::Put(Replay), Op::Get],
                ),
                |c| c.evictions = 1,
            ),
            6,
        ),
        Fail("A5"),
        false,
    );
    add(
        "lit/reap-overwrite-gone",
        "A12 A18",
        "an OVERWRITE lock reaped to DIRTY(gone=false) over a deleted key whose cold copy remains",
        fixed_without(
            crashy(base(Init::Both, &[Op::Put(Replay)], &[Op::Delete]), None),
            2,
        ),
        Fail("A12"),
        false,
    );
    add(
        "negative/if-match-no-recheck",
        "A5",
        "If-Match compares once, before the lock",
        flaw(
            base(Init::Both, &[Op::PutIm(2)], &[Op::PutIm(2)]),
            Flaw::IfMatchNoRecheck,
        ),
        Fail("A5"),
        false,
    );
    add(
        "negative/overwrite-without-lock",
        "A8",
        "an overwrite that takes no lock",
        flaw(
            base(Init::Both, &[Op::Put(Replay)], &[Op::Get]),
            Flaw::OverwriteNoLock,
        ),
        Fail("A8"),
        false,
    );
    add(
        "negative/plain-replace-ghost",
        "A8",
        "a stalled holder's plain replace",
        flaw(
            crashy(
                base(Init::Both, &[Op::Put(Replay)], &[Op::Put(Replay), Op::Get]),
                Some(GhostKind::HotReplace),
            ),
            Flaw::PlainReplace,
        ),
        Fail("A8"),
        false,
    );
    add(
        "negative/plain-replace-ghost-damage",
        "A8",
        "a stalled holder's plain replace puts its bytes over a newer acknowledged version",
        flaw(
            with(
                crashy(
                    base(Init::Both, &[Op::Put(Replay)], &[Op::Put(Replay), Op::Get]),
                    Some(GhostKind::HotReplace),
                ),
                |c| c.a8 = false,
            ),
            Flaw::PlainReplace,
        ),
        Fail("A7"),
        false,
    );
    add(
        "negative/copier-plain-cold-put",
        "A10",
        "the copier's cold write is a plain PUT",
        flaw(
            crashy(
                base(Init::HotDirty, &[Op::Put(Replay)], &[Op::Get]),
                Some(GhostKind::ColdWrite),
            ),
            Flaw::CopierPlainColdPut,
        ),
        Fail("A7"),
        false,
    );
    add(
        "negative/no-orphan-guard",
        "A7",
        "an orphan marker cleared while the put is still in flight",
        flaw(
            base(Init::Empty, &[Op::Put(Replay)], &[Op::Get]),
            Flaw::NoOrphanGuard,
        ),
        Fail("A7"),
        false,
    );
    add(
        "reads-never-wait",
        "A11",
        "reads finish while a dead owner's lock is held and no leader reaps it",
        with(
            crashy(
                base(Init::Lagging, &[Op::Delete], &[Op::Get, Op::List, Op::Get]),
                None,
            ),
            |c| {
                c.copier = false;
                c.crash_leader = false;
            },
        ),
        Pass,
        false,
    );
    add(
        "negative/read-waits-on-lock",
        "A11",
        "a read that waits for a lock",
        flaw(
            with(
                crashy(
                    base(Init::Lagging, &[Op::Delete], &[Op::Get, Op::List, Op::Get]),
                    None,
                ),
                |c| {
                    c.copier = false;
                    c.crash_leader = false;
                },
            ),
            Flaw::ReadWaitsOnLock,
        ),
        Fail("A11"),
        false,
    );
    add(
        "lock-wait-ends",
        "A11",
        "a writer meets a dead owner's lock: 503 after lock_wait",
        with(
            crashy(
                base(Init::Lagging, &[Op::Delete], &[Op::Put(Replay), Op::Get]),
                None,
            ),
            |c| {
                c.copier = false;
                c.crash_leader = false;
            },
        ),
        Pass,
        false,
    );
    add(
        "negative/wait-forever",
        "A11",
        "a writer with no lock_wait behind a dead owner",
        flaw(
            with(
                crashy(
                    base(Init::Lagging, &[Op::Delete], &[Op::Put(Replay), Op::Get]),
                    None,
                ),
                |c| {
                    c.copier = false;
                    c.crash_leader = false;
                },
            ),
            Flaw::WaitForever,
        ),
        Fail("A11"),
        false,
    );
    add(
        "negative/no-reap",
        "A4 A18",
        "the leader never reaps a dead owner's lock",
        flaw(
            crashy(
                base(Init::Lagging, &[Op::Delete], &[Op::Get, Op::PutCo(Replay)]),
                None,
            ),
            Flaw::NoReap,
        ),
        Fail("A4"),
        false,
    );
    add(
        "negative/gone-cleared-first",
        "A12",
        "the fast path clears gone before its object has landed",
        flaw(
            base(Init::DeletedCold, &[Op::Put(Replay)], &[Op::Get]),
            Flaw::GoneClearedFirst,
        ),
        Fail("A12"),
        false,
    );
    add(
        "negative/copy-back-without-lock",
        "A17",
        "a copy-back that takes no lock lands over a delete",
        flaw(
            base(Init::ColdOnly, &[Op::Get], &[Op::Delete, Op::Get]),
            Flaw::CopyBackNoLock,
        ),
        Fail("A1"),
        false,
    );
    add(
        "negative/create-only-ignores-cold",
        "A21",
        "a create-only put that does not ask cold",
        flaw(
            base(Init::ColdOnly, &[Op::PutCo(Replay)], &[Op::Get]),
            Flaw::CreateOnlyIgnoresCold,
        ),
        Fail("A5"),
        false,
    );
    add(
        "negative/create-only-no-recheck",
        "A5",
        "a create-only put that answers 412 from what wave 1 read while a delete completes",
        flaw(
            base(Init::Both, &[Op::PutCo(Replay)], &[Op::Delete, Op::Get]),
            Flaw::CreateOnlyNoRecheck,
        ),
        Fail("A5"),
        false,
    );
    add(
        "negative/create-only-no-recheck-cold-only",
        "A5",
        "the same for a key only cold has, deleted while the put reads",
        flaw(
            base(Init::ColdOnly, &[Op::PutCo(Replay)], &[Op::Delete, Op::Get]),
            Flaw::CreateOnlyNoRecheck,
        ),
        Fail("A5"),
        false,
    );
    add(
        "rollout/old-release-put-delete-beside-markers",
        "A22",
        "an old-release put and delete running the entry protocol beside markers",
        with(base(Init::Both, &[Op::Put(Replay)], &[Op::Get]), |c| {
            c.scripts[2] = vec![Op::OldPut, Op::OldDelete];
            c.phase = crate::marks::Phase::ActiveEpoch2;
        }),
        Fail("A1"),
        false,
    );
    add(
        "rollout/old-release-put-beside-markers",
        "A22",
        "an old-release put of an absent key beside a delete",
        with(base(Init::Empty, &[Op::Delete], &[Op::Get]), |c| {
            c.scripts[2] = vec![Op::OldPut];
            c.phase = crate::marks::Phase::ActiveEpoch2;
        }),
        Fail("A7"),
        false,
    );
    add(
        "negative/overwrite-without-lock-damage",
        "A8 A1",
        "an overwrite without the lock, A8 off: the damage a replace over a moved object does",
        with(
            flaw(
                base(
                    Init::HotDirty,
                    &[Op::Put(Stream)],
                    &[Op::Put(Replay), Op::Delete],
                ),
                Flaw::OverwriteNoLock,
            ),
            |c| c.a8 = false,
        ),
        Pass,
        false,
    );
    add(
        "head-before-bump",
        "A12",
        "a put that finds DIRTY HEADs hot before it bumps gone",
        with(
            base(
                Init::DeletedCold,
                &[Op::Put(Replay), Op::Get],
                &[Op::Delete, Op::Put(Replay)],
            ),
            |c| c.head_before_bump = true,
        ),
        Pass,
        false,
    );
    add(
        "crash/leader-only",
        "A4 A18",
        "only the leader dies, between any two steps, during puts, a delete and gets",
        with(
            base(
                Init::Empty,
                &[Op::Put(Replay), Op::Get],
                &[Op::Put(Stream), Op::Delete],
            ),
            |c| {
                c.crash_leader = true;
                c.max_crashes = 1;
            },
        ),
        Pass,
        false,
    );
    add(
        "if-match-cold-resolve",
        "A5",
        "an If-Match put of an evicted key whose cold copy is replaced and evicted again while it waits",
        with(
            base(Init::ColdOnly, &[Op::PutIm(2)], &[Op::Put(Replay), Op::Get]),
            |c| c.evictions = 2,
        ),
        Pass,
        false,
    );
}
