//! The first start of a hot tier with no format object (`docs/design/tierstore-design.md`,
//! 13.1): a tier with no format object is new, and the node writes it with a create-only put.

use super::*;
use crate::firststart::{Create, Flaw, Fmt, FsCfg};
use Expect::{Fail, Pass};

pub(super) fn first_start(v: &mut Vec<Scenario>) {
    let mut add =
        |id: &'static str, claims: &'static str, desc: &'static str, cfg: FsCfg, expect: Expect| {
            v.push(Scenario {
                id,
                claims,
                desc,
                sys: Sys::FirstStart(cfg),
                expect,
                heavy: false,
                liveness: true,
            });
        };
    let one = |nodes| FsCfg::new(nodes);
    let tweak = |mut c: FsCfg, f: &dyn Fn(&mut FsCfg)| {
        f(&mut c);
        c
    };

    add(
        "first-start/one-node",
        "FS2",
        "one node over a tier with no format object writes it and is writable",
        one(1),
        Pass,
    );
    add(
        "first-start/two-nodes-race",
        "FS2 FS4",
        "two nodes start at once: one create-only put wins, the other reads the object and follows it",
        one(2),
        Pass,
    );
    add(
        "first-start/two-nodes-race-with-a-pause",
        "FS2 FS4",
        "two nodes start at once and the operator writes a pause over the record as soon as it exists: no start replaces it",
        tweak(one(2), &|c| {
            c.operator = true;
            c.crashes = 1;
        }),
        Pass,
    );
    add(
        "first-start/crash-at-any-step",
        "FS2 FS4",
        "either node crashes at any step, registered or not, and restarts: the tier still gets one object and every node follows it",
        tweak(one(2), &|c| c.crashes = 2),
        Pass,
    );
    add(
        "first-start/crash-between-put-and-answer",
        "FS2",
        "a node crashes after the put took effect and before it read the answer: it restarts, reads the object and follows it",
        tweak(one(1), &|c| c.crashes = 2),
        Pass,
    );
    add(
        "first-start/ambiguous-answer",
        "FS2 FS4",
        "a put is answered ambiguously (a 409 that took effect, or one that did not because another put was in flight): it is not retried, and the node reads the object at every beat and follows it",
        tweak(one(2), &|c| {
            c.ambiguous = true;
            c.crashes = 1;
        }),
        Pass,
    );
    add(
        "first-start/restart-after-activation",
        "FS2 FS4",
        "nodes restart over a tier whose object exists: they read it and write nothing",
        tweak(one(2), &|c| {
            c.init_format = Fmt::Written;
            c.crashes = 2;
        }),
        Pass,
    );
    add(
        "first-start/restart-over-a-pause",
        "FS4",
        "nodes start over a record someone else wrote (a pause): they read it and write nothing",
        tweak(one(2), &|c| {
            c.init_format = Fmt::Changed;
            c.crashes = 1;
            c.must_activate = false;
        }),
        Pass,
    );

    add(
        "negative/first-start-create-not-atomic",
        "FS4",
        "wrong variant: a create that is a check then an unconditional write replaces a pause the operator wrote between the check and the write of a slow node, after a peer's record",
        tweak(one(2), &|c| {
            c.create = Create::CheckThenWrite;
            c.operator = true;
            c.must_activate = false;
        }),
        Fail("FS4"),
    );
    add(
        "negative/first-start-ambiguous-answer-not-followed",
        "FS2",
        "wrong variant: a node that has an ambiguous answer reads the object once and never again stays 503",
        tweak(one(1), &|c| {
            c.ambiguous = true;
            c.flaw = Flaw::NoFollow;
        }),
        Fail("FS2"),
    );

    add(
        "residual/first-start-every-create-answered-409",
        "FS2",
        "every create is answered 409 and none lands (no peer's put is in flight, so the answer is false): all nodes stay 503 until a restart; the node reads at every beat but writes nothing more",
        tweak(one(2), &|c| {
            c.ambiguous = true;
            c.lost_always = true;
        }),
        Fail("FS2"),
    );
    add(
        "residual/first-start-old-journal-left",
        "FS1",
        "a journal of a release before the format object is there and there is no format object: the tier is taken for new, and the format sits beside a journal nobody applies (unsupported; the release only ran on the farm, which is past it)",
        tweak(one(1), &|c| {
            c.init_journal = true;
            c.must_activate = false;
        }),
        Fail("FS1"),
    );
    add(
        "residual/first-start-old-release-running",
        "FS1",
        "a node of a release before the format object starts and writes a journal at any point of the start of two nodes, one of which may crash (unsupported)",
        tweak(one(2), &|c| {
            c.old = true;
            c.old_starts = 1;
            c.crashes = 1;
            c.must_activate = false;
        }),
        Fail("FS1"),
    );
}
