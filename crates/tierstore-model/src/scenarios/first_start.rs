//! The first start of a hot tier with no format object (`docs/design/tierstore-design.md`,
//! 13.1): the proof of an empty tier, the create-only put of the format object, and what stays
//! out of reach of both.

use super::*;
use crate::firststart::{Create, Flaw, FsCfg, OldAt, Target};
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
    let marks = |nodes| FsCfg::new(Target::Marks, nodes);
    let dir = |nodes| FsCfg::new(Target::Entries, nodes);
    let tweak = |mut c: FsCfg, f: &dyn Fn(&mut FsCfg)| {
        f(&mut c);
        c
    };

    add(
        "first-start/one-node-s3",
        "FS1 FS2 FS3",
        "one node over a new s3 tier with an empty marks bucket writes active-marks and is writable",
        marks(1),
        Pass,
    );
    add(
        "first-start/one-node-dir",
        "FS1 FS2",
        "one node over a new dir tier writes active at epoch 2 and is writable",
        dir(1),
        Pass,
    );
    add(
        "first-start/two-nodes-race-s3",
        "FS1 FS2 FS3",
        "two nodes start at once over a new s3 tier: one create wins, the other follows the object, and the markers the first writes do not stop the second",
        marks(2),
        Pass,
    );
    add(
        "first-start/two-nodes-race-dir",
        "FS1 FS2",
        "two nodes (two processes of one hand-off) start at once over a new dir tier",
        dir(2),
        Pass,
    );
    add(
        "first-start/two-nodes-race-create-not-atomic",
        "FS1 FS2",
        "two nodes over a dir tier whose create is a check then a write: both write the same record, so no harm",
        tweak(dir(2), &|c| c.create = Create::CheckThenWrite),
        Pass,
    );
    add(
        "first-start/crash-between-registration-and-put",
        "FS1 FS2",
        "either node crashes at any step, registered or not, and restarts: the tier still activates once",
        tweak(marks(2), &|c| c.crashes = 2),
        Pass,
    );
    add(
        "first-start/crash-between-put-and-answer",
        "FS1 FS2",
        "a node crashes after the put took effect and before it read the answer: it restarts, reads the object and follows it",
        tweak(dir(1), &|c| c.crashes = 2),
        Pass,
    );
    add(
        "first-start/restart-after-activation",
        "FS1 FS2",
        "nodes restart over a tier that is active: they read the object, run no proof and write nothing",
        tweak(marks(2), &|c| {
            c.init_format = true;
            c.crashes = 2;
        }),
        Pass,
    );
    add(
        "first-start/old-release-writing",
        "FS1",
        "a node of an earlier release is running and writes journals, while two nodes of this release start and may crash: none activates, whatever the order",
        tweak(marks(2), &|c| {
            c.old = true;
            c.old_up = true;
            c.must_activate = false;
            c.crashes = 1;
        }),
        Pass,
    );
    add(
        "first-start/old-release-writing-dir",
        "FS1",
        "as above over a dir tier, where the proof lists no marks bucket, and the old node may die: its registration and its journal each stop the proof",
        tweak(dir(2), &|c| {
            c.old = true;
            c.old_up = true;
            c.must_activate = false;
        }),
        Pass,
    );
    add(
        "first-start/journal-left",
        "FS1",
        "a journal object of an earlier release is there and no node of it runs: nothing activates",
        tweak(dir(2), &|c| {
            c.init_journal = true;
            c.must_activate = false;
        }),
        Pass,
    );
    add(
        "first-start/entries-left",
        "FS1",
        "an entry object is there and there is no format object: nothing activates",
        tweak(marks(2), &|c| {
            c.init_entries = true;
            c.must_activate = false;
        }),
        Pass,
    );
    add(
        "first-start/marks-bucket-used",
        "FS1",
        "the marks bucket holds a marker and there is no format object: nothing activates",
        tweak(marks(2), &|c| {
            c.bucket_used = true;
            c.must_activate = false;
        }),
        Pass,
    );
    add(
        "first-start/marks-bucket-missing",
        "FS3",
        "the marks bucket does not exist: nothing activates, at either epoch",
        tweak(marks(2), &|c| {
            c.bucket_exists = false;
            c.must_activate = false;
        }),
        Pass,
    );

    add(
        "negative/first-start-no-journal-check",
        "FS1",
        "wrong variant: a proof that does not list _journal/ activates over a journal",
        tweak(dir(1), &|c| {
            c.init_journal = true;
            c.flaw = Flaw::NoJournalCheck;
            c.must_activate = false;
        }),
        Fail("FS1"),
    );
    add(
        "negative/first-start-no-registration-check",
        "FS1",
        "wrong variant: a proof that does not list the registrations activates beside a running old node, which then writes a journal",
        tweak(marks(1), &|c| {
            c.old = true;
            c.old_up = true;
            c.flaw = Flaw::NoRegistrationCheck;
            c.must_activate = false;
        }),
        Fail("FS1"),
    );
    add(
        "negative/first-start-no-entries-check",
        "FS1",
        "wrong variant: a proof that does not list _entries/ activates over an entry",
        tweak(marks(1), &|c| {
            c.init_entries = true;
            c.flaw = Flaw::NoEntriesCheck;
            c.must_activate = false;
        }),
        Fail("FS1"),
    );
    add(
        "negative/first-start-no-marks-check-used",
        "FS1",
        "wrong variant: a proof that does not list the marks bucket activates over a marker",
        tweak(marks(1), &|c| {
            c.bucket_used = true;
            c.flaw = Flaw::NoMarksCheck;
            c.must_activate = false;
        }),
        Fail("FS1"),
    );
    add(
        "negative/first-start-no-marks-check-missing",
        "FS3",
        "wrong variant: a proof that does not list the marks bucket writes epoch 3 over a bucket that is not there",
        tweak(marks(1), &|c| {
            c.bucket_exists = false;
            c.flaw = Flaw::NoMarksCheck;
            c.must_activate = false;
        }),
        Fail("FS3"),
    );

    add(
        "negative/first-start-counts-its-own-registration",
        "FS2",
        "wrong variant: a proof that counts the node's own registration refuses on a new tier",
        tweak(marks(1), &|c| c.flaw = Flaw::CountsOwnRegistration),
        Fail("FS2"),
    );
    add(
        "negative/first-start-counts-its-peers-registrations",
        "FS2",
        "wrong variant: a proof that counts the registrations of peers that start now: two nodes that start at once block each other",
        tweak(marks(2), &|c| c.flaw = Flaw::CountsPeerRegistrations),
        Fail("FS2"),
    );

    add(
        "residual/first-start-old-starts-after-the-proof",
        "FS1",
        "an old node that starts and writes after the last list of the proof and before the put is not seen; the credential fence is what closes it",
        tweak(marks(1), &|c| {
            c.old = true;
            c.old_starts = 1;
            c.old_at = OldAt::BeforeFormat;
            c.must_activate = false;
        }),
        Fail("FS1"),
    );

    add(
        "residual/first-start-old-starts-after-the-activation",
        "FS1",
        "an old node that starts once the object exists writes journals: it never reads the object; the credential fence is what stops it",
        tweak(marks(1), &|c| {
            c.old = true;
            c.old_starts = 1;
            c.old_at = OldAt::AfterFormat;
            c.must_activate = false;
        }),
        Fail("FS1"),
    );

    add(
        "residual/first-start-old-starts-at-any-time",
        "FS1",
        "an old node that starts at any point of a start of two nodes, one of which may crash, writes a journal the format object then sits beside",
        tweak(dir(2), &|c| {
            c.old = true;
            c.old_starts = 1;
            c.crashes = 1;
            c.must_activate = false;
        }),
        Fail("FS1"),
    );
}
