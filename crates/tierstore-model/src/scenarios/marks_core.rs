//! The document as written, with the corrections, its operations, crashes and ghosts.

use super::*;
use crate::marks::{Body, Cfg, Flaw, GhostKind, Init, Op, Phase};
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
    let cfg = if id.starts_with("doc/") || id.starts_with("lit/") {
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

pub(super) fn core(v: &mut Vec<Scenario>) {
    let mut add = |id: &'static str,
                   claims: &'static str,
                   desc: &'static str,
                   cfg: Cfg,
                   expect: Expect,
                   heavy: bool| { push(v, id, claims, desc, cfg, expect, heavy) };
    // The document as written: the counterexamples it has.
    add(
        "doc/fast-put-delete-get",
        "A7",
        "a put whose marker landed before a delete's tombstone cleanup",
        base(
            Init::Empty,
            &[Op::Put(Replay), Op::Delete],
            &[Op::Put(Stream), Op::Get],
        ),
        Fail("A7"),
        false,
    );
    add(
        "doc/create-only-delete",
        "A1",
        "a put's marker bump after a delete resurrects the deleted object",
        base(
            Init::Empty,
            &[Op::PutCo(Replay), Op::Delete],
            &[Op::PutCo(Stream), Op::Get],
        ),
        Fail("A1"),
        false,
    );
    add(
        "doc/recreate-read",
        "A2",
        "a read that missed hot before a recreate serves the deleted cold copy",
        base(
            Init::DeletedCold,
            &[Op::Put(Replay), Op::Get],
            &[Op::Get, Op::List],
        ),
        Fail("A2"),
        false,
    );
    add(
        "doc/cold-only-delete-create",
        "A1",
        "a create-only put lands between a delete's gone lock and its hot DELETE",
        base(
            Init::ColdOnly,
            &[Op::PutCo(Replay), Op::Get],
            &[Op::Delete, Op::Get],
        ),
        Fail("A1"),
        false,
    );
    // The same runs with the corrections.
    add(
        "fixed/fast-put-delete-get",
        "A1 A2 A3 A6 A7",
        "",
        fixed(base(
            Init::Empty,
            &[Op::Put(Replay), Op::Delete],
            &[Op::Put(Stream), Op::Get],
        )),
        Pass,
        false,
    );
    add(
        "fixed/create-only-delete",
        "A1 A5 A21",
        "",
        fixed(base(
            Init::Empty,
            &[Op::PutCo(Replay), Op::Delete],
            &[Op::PutCo(Stream), Op::Get],
        )),
        Pass,
        false,
    );
    add(
        "fixed/recreate-read",
        "A2 A12",
        "",
        fixed(base(
            Init::DeletedCold,
            &[Op::Put(Replay), Op::Get],
            &[Op::Get, Op::List],
        )),
        Pass,
        false,
    );
    add(
        "fixed/cold-only-delete-create",
        "A1 A17 A21",
        "",
        fixed(base(
            Init::ColdOnly,
            &[Op::PutCo(Replay), Op::Get],
            &[Op::Delete, Op::Get],
        )),
        Pass,
        false,
    );
    // Operations the document gets right as written.
    add(
        "overwrite-race",
        "A1 A6 A8",
        "two overwrites of an uncopied key, a get, a list and an eviction",
        with(
            base(
                Init::Lagging,
                &[Op::Put(Stream), Op::Get],
                &[Op::Put(Replay), Op::List],
            ),
            |c| c.evictions = 1,
        ),
        Pass,
        false,
    );
    add(
        "if-match-race",
        "A5",
        "two If-Match puts with one tag, a get and a delete",
        base(
            Init::Both,
            &[Op::PutIm(2), Op::Get],
            &[Op::PutIm(2), Op::Delete],
        ),
        Pass,
        false,
    );
    add(
        "if-match-cold-only",
        "A5",
        "If-Match of an evicted key (resolved from cold)",
        base(Init::ColdOnly, &[Op::PutIm(2)], &[Op::PutIm(2), Op::Get]),
        Pass,
        false,
    );
    add(
        "create-only-recheck/hot-key",
        "A5",
        "a create-only put of a copied key against a delete that completes first: 412 only while the key is there, else 409",
        base(Init::Both, &[Op::PutCo(Replay)], &[Op::Delete, Op::Get]),
        Pass,
        false,
    );
    add(
        "create-only-recheck/cold-only-key",
        "A5 A21",
        "the same for an evicted key",
        base(Init::ColdOnly, &[Op::PutCo(Replay)], &[Op::Delete, Op::Get]),
        Pass,
        false,
    );
    add(
        "create-only-recheck/recreated-key",
        "A5",
        "a create-only put against a delete and a recreate: a gone marker with hot present is a recreate",
        base(
            Init::Both,
            &[Op::PutCo(Replay)],
            &[Op::Delete, Op::Put(Replay), Op::Get],
        ),
        Pass,
        false,
    );
    add(
        "overwrite-vs-delete",
        "A1 A2 A8",
        "an overwrite and a delete of an uncopied key against gets and lists",
        base(
            Init::Lagging,
            &[Op::Put(Replay), Op::Get],
            &[Op::Delete, Op::List],
        ),
        Pass,
        false,
    );
    add(
        "delete-race",
        "A1 A23",
        "two deletes and puts of a copied key",
        base(
            Init::Both,
            &[Op::Delete, Op::Put(Replay)],
            &[Op::Delete, Op::Get],
        ),
        Pass,
        false,
    );
    add(
        "read-pair-after-delete",
        "A13",
        "gets racing a delete, the copier's cold delete and tombstone hold",
        base(Init::Lagging, &[Op::Delete, Op::Get], &[Op::Get, Op::Get]),
        Pass,
        false,
    );
    add(
        "cold-land",
        "A15",
        "two puts at hot Full, a get and a delete",
        base(
            Init::Lagging,
            &[Op::PutCold, Op::Get],
            &[Op::PutCold, Op::Delete],
        ),
        Pass,
        false,
    );
    add(
        "residual/cold-land-vs-create-only",
        "A15",
        "a put at hot Full against a create-only put: both can win, and the cold landing's hot DELETE then removes the other",
        base(
            Init::Empty,
            &[Op::PutCold, Op::Get],
            &[Op::PutCo(Replay), Op::Get],
        ),
        Fail("A5"),
        false,
    );
    add(
        "residual/cold-land-vs-stale-list",
        "A15 A2",
        "a list whose hot page missed before a copy-back reads the cold object a cold landing wrote while hot still holds the older one",
        base(Init::ColdOnly, &[Op::Get, Op::PutCold], &[Op::List]),
        Fail("A2"),
        false,
    );
    add(
        "residual/cold-land-vs-plain-put",
        "A15",
        "a put at hot Full against a plain put: reads see 3, 5, 3",
        base(
            Init::Empty,
            &[Op::PutCold, Op::Get],
            &[Op::Put(Replay), Op::Get],
        ),
        Fail("A1"),
        false,
    );
    add(
        "cold-land-over-deleted",
        "A12 A15",
        "a put at hot Full over a deleted key whose cold copy remains",
        base(
            Init::DeletedCold,
            &[Op::PutCold, Op::Get],
            &[Op::Get, Op::List],
        ),
        Pass,
        false,
    );
    add(
        "lit/cold-land-over-deleted",
        "A12 A15",
        "the same with the lock taking over the gone marker as 5.5 says",
        fixed_without(
            base(
                Init::DeletedCold,
                &[Op::PutCold, Op::Get],
                &[Op::Get, Op::List],
            ),
            8,
        ),
        Fail("A12"),
        false,
    );
    add(
        "cold-only-plain-put",
        "A1 A2",
        "a plain put against a delete of an evicted key",
        base(
            Init::ColdOnly,
            &[Op::Put(Replay), Op::Get],
            &[Op::Delete, Op::Get],
        ),
        Pass,
        false,
    );
    add(
        "copy-back",
        "A17",
        "a copy-back against a put and a delete",
        base(
            Init::ColdOnly,
            &[Op::Get, Op::Get],
            &[Op::Put(Replay), Op::Delete],
        ),
        Pass,
        false,
    );
    add(
        "eviction-vs-create-only",
        "A16 A21",
        "an eviction between the checks and the landing of a create-only put",
        with(
            base(
                Init::Empty,
                &[Op::PutCo(Replay)],
                &[Op::Put(Replay), Op::Get],
            ),
            |c| c.evictions = 1,
        ),
        Pass,
        false,
    );
    add(
        "stream-loses-race",
        "A6 A24",
        "a one-shot stream that loses the creation race",
        base(Init::Empty, &[Op::Put(Stream)], &[Op::Put(Replay), Op::Get]),
        Pass,
        false,
    );
    add(
        "stalled-copier",
        "A23",
        "a delete while the copier is stalled mid-copy",
        with(
            base(Init::HotDirty, &[Op::Delete, Op::Get], &[Op::Put(Replay)]),
            |c| c.stall_leader = true,
        ),
        Pass,
        false,
    );
    add(
        "ambiguous-locks",
        "A20",
        "lock writes whose answers are lost",
        with(
            base(Init::HotDirty, &[Op::Put(Replay)], &[Op::Delete]),
            |c| c.ambiguity = true,
        ),
        Pass,
        false,
    );
    add(
        "old-release-fails-closed",
        "A22",
        "an old-release proxy under active-marks",
        with(
            base(Init::Both, &[Op::Put(Replay), Op::Delete], &[Op::Get]),
            |c| {
                c.scripts[2] = vec![Op::OldPut, Op::OldDelete];
                c.phase = Phase::ActiveMarks;
            },
        ),
        Pass,
        false,
    );
    add(
        "old-release-beside-markers",
        "A22",
        "an old-release proxy running the entry protocol beside markers",
        with(base(Init::Lagging, &[Op::Delete], &[Op::Get]), |c| {
            c.scripts[2] = vec![Op::OldGet];
            c.phase = Phase::ActiveEpoch2;
        }),
        Fail("A2"),
        false,
    );
    // A crash of any actor between any two steps.
    add(
        "crash/fast-put",
        "A4 A7 A18",
        "crash of a proxy or the leader during puts, a delete and gets",
        crashy(
            base(
                Init::Empty,
                &[Op::Put(Replay), Op::Get],
                &[Op::Put(Stream), Op::Delete],
            ),
            None,
        ),
        Pass,
        false,
    );
    add(
        "crash/overwrite-lock",
        "A4 A18",
        "crash holding an OVERWRITE lock",
        crashy(
            base(
                Init::Lagging,
                &[Op::Put(Replay)],
                &[Op::Get, Op::Put(Replay)],
            ),
            None,
        ),
        Pass,
        false,
    );
    add(
        "crash/delete-lock",
        "A4 A18",
        "crash holding a DELETE lock",
        crashy(
            base(Init::Lagging, &[Op::Delete], &[Op::Get, Op::PutCo(Replay)]),
            None,
        ),
        Pass,
        false,
    );
    add(
        "crash/delete-lock-rolled-back",
        "A4 A18",
        "crash holding a DELETE lock, which the leader reaps keeping the gone it found (5.13)",
        with(
            crashy(
                base(Init::Lagging, &[Op::Delete], &[Op::Get, Op::PutCo(Replay)]),
                None,
            ),
            |c| c.reap_delete_keeps_gone = true,
        ),
        Pass,
        false,
    );
    add(
        "crash/cold-land-lock",
        "A4 A18",
        "crash holding a COLDLAND lock",
        crashy(
            base(Init::Lagging, &[Op::PutCold], &[Op::Get, Op::Put(Replay)]),
            None,
        ),
        Pass,
        false,
    );
    add(
        "crash/evict-lock",
        "A4 A18",
        "the leader dies holding an EVICT lock",
        crashy(
            with(base(Init::Both, &[Op::Put(Replay)], &[Op::Get]), |c| {
                c.evictions = 1
            }),
            None,
        ),
        Pass,
        false,
    );
    add(
        "crash/copy-back-lock",
        "A4 A18 A17",
        "the leader dies holding a COPYBACK lock",
        crashy(
            base(Init::ColdOnly, &[Op::Get, Op::Get], &[Op::Delete]),
            None,
        ),
        Pass,
        false,
    );
    // A put whose body streams for longer than `orphan_after` (no size cap, so no put deadline).
    add(
        "doc/slow-put-orphan",
        "A7",
        "the first draft: a slow put's marker is cleared as an orphan before its object lands",
        with(base(Init::Empty, &[Op::Put(Stream)], &[Op::Get]), |c| {
            c.puts_unbounded = true
        }),
        Fail("A7"),
        false,
    );
    add(
        "slow-put/orphan",
        "A7 A4",
        "a slow put re-asserts its marker after the copier cleared it as an orphan",
        with(base(Init::Empty, &[Op::Put(Stream)], &[Op::Get]), |c| {
            c.puts_unbounded = true
        }),
        Pass,
        false,
    );
    add(
        "slow-put/gone-branch",
        "A7 A2",
        "a slow put whose marker was created before a delete lands its object during the tombstone hold",
        with(
            base(Init::Both, &[Op::Delete], &[Op::Put(Stream), Op::Get]),
            |c| c.puts_unbounded = true,
        ),
        Pass,
        false,
    );
    // A live holder whose hot DELETE times out (5.2, 5.6).
    add(
        "timeout/delete-keeps-lock",
        "A8 A1",
        "a delete whose hot DELETE times out keeps its lock until the queued call is gone",
        with(
            base(Init::Both, &[Op::Delete], &[Op::Put(Replay), Op::Get]),
            |c| {
                c.timeouts = true;
                c.ghost = Some(GhostKind::HotDelete);
            },
        ),
        Pass,
        false,
    );
    add(
        "timeout/release-at-once",
        "A8 A1",
        "a delete whose hot DELETE times out releases its lock at once: the queued call deletes a newer put",
        with(
            base(Init::Both, &[Op::Delete], &[Op::Put(Replay), Op::Get]),
            |c| {
                c.timeouts = true;
                c.ghost = Some(GhostKind::HotDelete);
                c.flaw = Flaw::ReleaseOnTimeout;
            },
        ),
        Fail("A8"),
        false,
    );
    // Ghosts: a stalled actor's call lands late.
    add(
        "ghost/hot-delete",
        "A19 (R1)",
        "a stalled delete's hot DELETE lands after a newer put",
        crashy(
            base(Init::Both, &[Op::Delete], &[Op::Put(Replay), Op::Get]),
            Some(GhostKind::HotDelete),
        ),
        Fail("A8"),
        false,
    );
    add(
        "ghost/hot-delete-damage",
        "A19 (R1)",
        "the same with the exclusion assertion off: the acknowledged newer put is deleted",
        crashy(
            with(
                base(Init::Both, &[Op::Delete], &[Op::Put(Replay), Op::Get]),
                |c| c.a8 = false,
            ),
            Some(GhostKind::HotDelete),
        ),
        Fail("A1"),
        false,
    );
    add(
        "ghost/hot-create",
        "A19 (R3')",
        "a stalled put's object lands after its orphan marker was cleared",
        crashy(
            base(Init::Empty, &[Op::Put(Replay)], &[Op::Get]),
            Some(GhostKind::HotCreate),
        ),
        Fail("A7"),
        false,
    );
    add(
        "ghost/cold-write",
        "A10 A19 (R2')",
        "a stalled leader's cold write lands after the tombstone pass",
        crashy(
            base(Init::HotDirty, &[Op::Delete], &[Op::Get]),
            Some(GhostKind::ColdWrite),
        ),
        Fail("A1"),
        false,
    );
    add(
        "ghost/marker-delete",
        "A19 (R4')",
        "a stalled leader's marker DELETE erases a marker written after its CLEARING",
        crashy(
            base(Init::DeletedCold, &[Op::Put(Replay)], &[Op::Delete]),
            Some(GhostKind::MarkerDelete),
        ),
        Fail("A9"),
        false,
    );
    add(
        "ghost/cold-write-overwrite",
        "A10 A19",
        "a stalled leader's cold write over a newer cold object fails on its If-Match",
        crashy(
            base(Init::HotDirty, &[Op::Put(Replay)], &[Op::Get]),
            Some(GhostKind::ColdWrite),
        ),
        Pass,
        false,
    );
    add(
        "ghost/hot-replace",
        "A8 A19",
        "a stalled overwrite's If-Match replace lands late",
        crashy(
            base(Init::Both, &[Op::Put(Replay)], &[Op::Put(Replay), Op::Get]),
            Some(GhostKind::HotReplace),
        ),
        Fail("A7"),
        false,
    );
    add(
        "ghost/marker-write",
        "A19",
        "a stalled marker create or CAS lands late",
        crashy(
            base(Init::HotDirty, &[Op::Put(Replay)], &[Op::Delete, Op::Get]),
            Some(GhostKind::MarkerWrite),
        ),
        Pass,
        false,
    );
}
