//! Today's entry protocol under write-once.

use super::*;
use crate::entries::{BCfg, BFlaw, BOp};
use Expect::{Fail, Pass};

pub(super) fn baseline(v: &mut Vec<Scenario>) {
    // Today's entry protocol under write-once, as a sanity check of the model and its properties.
    let mut addb =
        |id: &'static str, claims: &'static str, desc: &'static str, cfg: BCfg, expect: Expect| {
            v.push(Scenario {
                id,
                claims,
                desc,
                sys: Sys::Entries(cfg),
                expect,
                heavy: false,
                liveness: true,
            });
        };
    let bwith = |mut c: BCfg, f: &dyn Fn(&mut BCfg)| {
        f(&mut c);
        c
    };
    addb(
        "baseline/put-delete-get",
        "A1 A2 A3 A5",
        "write-once puts, deletes and gets against the copier, a copy-back and an eviction",
        bwith(
            BCfg::new(&[BOp::Put, BOp::Delete], &[BOp::Put, BOp::Get]),
            &|c| {
                c.evictions = 1;
                c.copy_back = true;
            },
        ),
        Pass,
    );
    addb(
        "baseline/delete-waits-for-copy",
        "A1 A2 A23",
        "a delete against a put and the copy of it",
        BCfg::new(&[BOp::Put, BOp::Get], &[BOp::Delete, BOp::Get]),
        Pass,
    );
    addb(
        "baseline/cold-only",
        "A5",
        "puts, deletes and gets of an evicted key",
        bwith(
            BCfg::new(&[BOp::Put, BOp::Get], &[BOp::Delete, BOp::Put]),
            &|c| {
                c.cold_only = true;
                c.copy_back = true;
            },
        ),
        Pass,
    );
    addb(
        "baseline/crash",
        "A4 A7 A18",
        "a crash of any actor between any two steps",
        bwith(
            BCfg::new(&[BOp::Put, BOp::Get], &[BOp::Delete, BOp::Put]),
            &|c| {
                c.crash_clients = true;
                c.crash_leader = true;
                c.max_crashes = 1;
                c.evictions = 1;
                c.copy_back = true;
            },
        ),
        Pass,
    );
    addb(
        "baseline/two-client-crashes",
        "A4 A7 A18",
        "both proxies crash, at any step, one after the other",
        bwith(BCfg::new(&[BOp::Put, BOp::Get], &[BOp::Delete]), &|c| {
            c.crash_clients = true;
            c.max_crashes = 2;
        }),
        Pass,
    );
    addb(
        "baseline/leader-crash",
        "A4 A18",
        "the leader dies between any two steps and a successor takes over",
        bwith(BCfg::new(&[BOp::Put], &[BOp::Delete, BOp::Get]), &|c| {
            c.crash_leader = true;
            c.max_crashes = 1;
        }),
        Pass,
    );
    addb(
        "baseline-negative/delete-no-wait",
        "A2",
        "a delete that does not wait for the copy it amended",
        bwith(BCfg::new(&[BOp::Put], &[BOp::Delete, BOp::Get]), &|c| {
            c.flaw = BFlaw::DeleteNoWait
        }),
        Fail("A1"),
    );
    addb(
        "baseline-negative/put-no-check",
        "A5",
        "a put that does not check the tiers under its entry",
        bwith(BCfg::new(&[BOp::Put], &[BOp::Put]), &|c| {
            c.flaw = BFlaw::PutNoCheck;
            c.cold_only = true;
        }),
        Fail("A5"),
    );
    addb(
        "baseline-negative/ack-before-landing",
        "A1",
        "a put acknowledged before its object landed",
        bwith(BCfg::new(&[BOp::Put, BOp::Get], &[BOp::Get]), &|c| {
            c.flaw = BFlaw::AckBeforeLanding;
            c.crash_clients = true;
            c.max_crashes = 1;
        }),
        Fail("A1"),
    );
    addb(
        "baseline-negative/copy-without-held",
        "A1 A2",
        "the leader copies without taking the entry first",
        bwith(BCfg::new(&[BOp::Put], &[BOp::Delete, BOp::Get]), &|c| {
            c.flaw = BFlaw::CopyNoHeld
        }),
        Fail("A1"),
    );
}
