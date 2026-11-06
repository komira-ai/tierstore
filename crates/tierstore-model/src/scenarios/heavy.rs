//! Larger bounds: three operations per proxy, two crashes.

use super::*;
use crate::marks::{Body, Cfg, GhostKind, Init, Op};
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
    // The heavy runs are of the corrected protocol.
    let cfg = fixed(cfg);
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

pub(super) fn heavy(v: &mut Vec<Scenario>) {
    let mut add = |id: &'static str,
                   claims: &'static str,
                   desc: &'static str,
                   cfg: Cfg,
                   expect: Expect,
                   heavy: bool| { push(v, id, claims, desc, cfg, expect, heavy) };
    // Larger bounds: three operations per proxy.
    add(
        "heavy/puts-delete-reads",
        "A1 A2 A3 A5 A6 A7",
        "four writes and two reads from an empty key, with an eviction",
        with(
            base(
                Init::Empty,
                &[Op::Put(Replay), Op::Delete, Op::Get],
                &[Op::PutCo(Stream), Op::Get, Op::Put(Replay)],
            ),
            |c| c.evictions = 1,
        ),
        Pass,
        true,
    );
    add(
        "heavy/overwrite-if-match",
        "A1 A5 A6 A8",
        "overwrites, If-Match puts, a delete and lists of an uncopied key",
        base(
            Init::Lagging,
            &[Op::Put(Stream), Op::Delete, Op::List],
            &[Op::PutIm(2), Op::Put(Replay), Op::Get],
        ),
        Pass,
        true,
    );
    add(
        "heavy/crash-puts-delete",
        "A4 A7 A18",
        "a crash of any actor during puts, a delete and reads",
        crashy(
            base(
                Init::Empty,
                &[Op::Put(Replay), Op::Delete, Op::Get],
                &[Op::PutCo(Stream), Op::Get],
            ),
            None,
        ),
        Pass,
        true,
    );
    add(
        "heavy/cold-only-copy-back",
        "A17 A21",
        "an evicted key: create-only, delete, put, reads and copy-backs",
        base(
            Init::ColdOnly,
            &[Op::PutCo(Replay), Op::Delete, Op::Get],
            &[Op::Get, Op::Put(Replay), Op::Get],
        ),
        Pass,
        true,
    );
    add(
        "heavy/two-crashes",
        "A4 A7 A18",
        "two crashes of any actors during overwrites, a delete and reads",
        with(
            crashy(
                base(
                    Init::Lagging,
                    &[Op::Put(Replay), Op::Delete, Op::Get],
                    &[Op::Put(Stream), Op::List],
                ),
                None,
            ),
            |c| c.max_crashes = 2,
        ),
        Pass,
        true,
    );
    add(
        "heavy/ambiguity",
        "A20",
        "lost answers of lock writes during puts, a delete and reads",
        with(
            base(
                Init::HotDirty,
                &[Op::Put(Replay), Op::Delete, Op::Get],
                &[Op::Put(Stream), Op::Get],
            ),
            |c| c.ambiguity = true,
        ),
        Pass,
        true,
    );
    add(
        "heavy/cold-land-mix",
        "A12 A15 A16",
        "puts at Full among overwrites, a delete and reads of a deleted key",
        with(
            base(
                Init::DeletedCold,
                &[Op::PutCold, Op::Delete, Op::List],
                &[Op::PutCold, Op::List, Op::Delete],
            ),
            |c| c.evictions = 1,
        ),
        Pass,
        true,
    );
    add(
        "heavy/ghost-any",
        "A19",
        "any stalled call landing late among puts and a delete",
        crashy(
            base(
                Init::Both,
                &[Op::Put(Replay), Op::Get],
                &[Op::Delete, Op::Get],
            ),
            Some(GhostKind::Any),
        ),
        Fail("A8"),
        true,
    );

    add(
        "heavy/crashes-and-ambiguity",
        "A4 A7 A18 A20",
        "a crash and lost lock answers during puts, a delete, an If-Match put and reads of an uncopied key",
        with(
            crashy(
                base(
                    Init::HotDirty,
                    &[Op::Put(Replay), Op::Delete, Op::Get],
                    &[Op::PutIm(2), Op::Put(Stream), Op::List],
                ),
                None,
            ),
            |c| c.ambiguity = true,
        ),
        Pass,
        true,
    );
    add(
        "heavy/everything-with-eviction",
        "A1 A2 A5 A6 A8 A16 A17 A21",
        "every operation kind against the copier, a copy-back and two evictions",
        with(
            base(
                Init::Both,
                &[Op::PutCo(Replay), Op::Delete, Op::Get],
                &[Op::Put(Stream), Op::PutIm(2), Op::Get],
            ),
            |c| c.evictions = 2,
        ),
        Pass,
        true,
    );
}
