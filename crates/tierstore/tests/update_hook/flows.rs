//! The steps in order through the real binary: a handover, a restart, a rollback, and the boot
//! entry `restore`. Each step's calls to the fakes, what the fakes hold after it, and the record
//! after it are asserted.

use std::collections::{BTreeMap, BTreeSet};

use tierstore::server::update_hook::{GUARD_TABLE, Marker, Mode, build_of};

use crate::rig::{
    APPLY, BOOT, DELETE_GUARD, LIST_FORWARD, LIST_GUARD, NEW_VERSION, OLD_VERSION, Rig, Setup,
    start, stop,
};
use crate::stub::SlotScript;

/// The first two words of each effect: what changed, without the text of a table.
pub fn kinds(effects: &[String]) -> Vec<String> {
    effects
        .iter()
        .map(|effect| {
            effect
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// Asserts that the public port is answered, by the build gauge of `/metricz`, by the instance
/// on `slot` running `version`, and that it says `read-write`.
pub fn public_answers(rig: &Rig, slot: u16, version: &str) {
    let (status, body) = rig
        .get(rig.public, "/metricz")
        .expect("an answer on the public port");
    assert_eq!(status, 200);
    let build = build_of(&body).expect("a build gauge");
    assert_eq!(build.slot, Some(slot), "{body}");
    assert_eq!(build.version, version, "{body}");
    assert_eq!(
        rig.get(rig.public, "/readyz"),
        Some((200, "ready read-write\n".to_owned()))
    );
}

/// Asserts that the real hook runs `prepare`, `cutover`, `verify` and `cleanup` against the fakes
/// with the calls the design names, in order, and that what the fakes hold and the record after
/// each step are those of the design: `prepare` loads the guard (after looking for one) and
/// starts the idle slot beside the running one, recording the update; `cutover` applies the
/// redirect in one `nft -f -`; `verify` lists the redirect, finds the old instance still holding
/// the lock and reports the promotion pending; `cleanup` looks again and stops the old unit, after
/// which every report is irreversible and the new instance alone answers, read-write, through the
/// public port. It would catch a hook that runs against the real tools as it never ran against
/// the in-memory fakes: another argument vector, another order, a table text the listing does not
/// give back, a probe the server's gauge does not satisfy.
#[test]
fn a_handover_runs_its_steps_against_the_fakes_and_ends_on_the_new_slot() {
    let rig = Rig::new("hook-conformance-handover", Setup::handover());
    let (a, b) = (rig.a, rig.b);

    let ran = rig.step("prepare");
    ran.ok();
    assert_eq!(
        ran.detail(),
        format!("prepared slot {b} beside {a}, mode handover (handover_capable)")
    );
    assert_eq!(ran.line["reversible"], true);
    assert_eq!(rig.calls(), [LIST_GUARD, APPLY, start(b).as_str()]);
    let record = rig.record();
    assert_eq!(
        (record.serving, record.previous, record.mode),
        (b, Some(a), Mode::Handover)
    );
    assert!(record.markers.is_empty() && record.guard_placed);
    let machine = rig.machine();
    assert_eq!(
        machine.units,
        BTreeMap::from([(a, rig.old.clone()), (b, rig.new.clone())])
    );
    assert!(rig.has_table(GUARD_TABLE));
    assert_eq!(rig.redirect(), Some(a));
    assert_eq!(rig.link(b), Some(rig.new.clone()));

    let mark = rig.calls().len();
    rig.step("cutover").ok();
    assert_eq!(rig.calls_since(mark), [APPLY]);
    assert_eq!(rig.redirect(), Some(b));
    assert_eq!(rig.record().markers, BTreeSet::from([Marker::Swapped]));

    let mark = rig.calls().len();
    let ran = rig.step("verify");
    ran.ok();
    assert_eq!(ran.line["promotion_pending"], true);
    assert_eq!(
        ran.detail(),
        format!("{b} serves read-only: the promotion is pending")
    );
    assert_eq!(rig.calls_since(mark), [LIST_FORWARD]);
    let record = rig.record();
    assert_eq!(
        record.markers,
        BTreeSet::from([Marker::Swapped, Marker::Verified])
    );
    assert_eq!(record.boot_id.as_deref(), Some(BOOT));
    assert_eq!(rig.running(), [a, b]);

    let mark = rig.calls().len();
    let ran = rig.step("cleanup");
    ran.ok();
    assert_eq!(ran.line["reversible"], false);
    assert_eq!(
        rig.calls_since(mark),
        [LIST_GUARD, LIST_FORWARD, stop(a).as_str()]
    );
    let record = rig.record();
    assert_eq!((record.serving, record.previous), (b, None));
    assert_eq!(
        record.markers,
        BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping])
    );
    assert!(!record.guard_placed);
    assert_eq!(rig.running(), [b]);
    assert_eq!(rig.link(a), None);

    let mark = rig.calls().len();
    let ran = rig.step("verify");
    ran.ok();
    assert_eq!(ran.line["promotion_pending"], false);
    assert_eq!(ran.line["reversible"], false);
    assert!(rig.calls_since(mark).is_empty());

    public_answers(&rig, b, NEW_VERSION);
    assert_eq!(
        kinds(&rig.effects()),
        [
            "set kbf_guard_tierstore",
            format!("start tierstore@{b}.service").as_str(),
            "set kbf_forward_tierstore",
            format!("stop tierstore@{a}.service").as_str(),
        ]
    );
}

/// Asserts the restart through the real binary, when the running release lacks `handover-v1`:
/// `prepare` loads the guard and starts nothing; `cutover` stops the old unit, starts the new one
/// and only then applies the redirect; `verify` sees the new instance read-write at once (no
/// promotion pending); `cleanup`'s stop of the stopped unit changes nothing. It would catch two
/// instances of a release without the lock running side by side.
#[test]
fn a_restart_stops_the_old_unit_before_the_new_one_starts() {
    let rig = Rig::new("hook-conformance-restart", Setup::restart());
    let (a, b) = (rig.a, rig.b);
    let ran = rig.step("prepare");
    ran.ok();
    assert_eq!(
        ran.detail(),
        format!("prepared slot {b} beside {a}, mode restart (not_handover_capable)")
    );
    assert_eq!(rig.calls(), [LIST_GUARD, APPLY]);
    assert_eq!(rig.running(), [a]);
    assert_eq!(rig.record().mode, Mode::Restart);

    let mark = rig.calls().len();
    rig.step("cutover").ok();
    assert_eq!(
        rig.calls_since(mark),
        [stop(a).as_str(), start(b).as_str(), APPLY]
    );
    assert_eq!(rig.running(), [b]);

    let mark = rig.calls().len();
    let ran = rig.step("verify");
    ran.ok();
    assert_eq!(ran.line["promotion_pending"], false);
    assert_eq!(rig.calls_since(mark), [LIST_FORWARD]);

    let mark = rig.calls().len();
    rig.step("cleanup").ok();
    assert_eq!(
        rig.calls_since(mark),
        [LIST_GUARD, LIST_FORWARD, stop(a).as_str()]
    );
    public_answers(&rig, b, NEW_VERSION);
    assert_eq!(
        kinds(&rig.effects()),
        [
            "set kbf_guard_tierstore",
            format!("stop tierstore@{a}.service").as_str(),
            format!("start tierstore@{b}.service").as_str(),
            "set kbf_forward_tierstore",
        ]
    );
}

/// Asserts that a `verify` that finds the old instance silent fails with `previous_down`, and
/// that the `rollback` the updater then runs, through the real binary, leaves the fakes exactly as
/// they were before the update: the old unit running (its start a no-op), the redirect naming it,
/// the new unit stopped, the guard table this update loaded removed, the new slot's link gone and
/// the record back to no update in flight; and that a second `rollback` changes nothing. It would
/// catch a rollback that leaves the new instance running, the guard behind, or the redirect on
/// the new slot.
#[test]
fn a_rollback_leaves_the_fakes_as_they_were_before_the_update() {
    let rig = Rig::new("hook-conformance-rollback", Setup::handover());
    let (a, b) = (rig.a, rig.b);
    let before = rig.machine();
    rig.step("prepare").ok();
    rig.step("cutover").ok();
    rig.script(
        a,
        SlotScript {
            silent: true,
            ..SlotScript::default()
        },
    );
    rig.step("verify").fails("previous_down");
    assert!(!rig.record().markers.contains(&Marker::Verified));
    rig.script(a, SlotScript::default());
    let mark = rig.calls().len();
    rig.step("rollback").ok();
    assert_eq!(
        rig.calls_since(mark),
        [start(a).as_str(), APPLY, stop(b).as_str(), DELETE_GUARD]
    );
    assert_eq!(rig.machine(), before);
    assert_eq!(rig.link(b), None);
    assert_eq!(rig.link(a), Some(rig.old.clone()));
    let record = rig.record();
    assert_eq!((record.serving, record.previous), (a, None));
    assert!(record.markers.is_empty() && !record.guard_placed);
    public_answers(&rig, a, OLD_VERSION);

    let effects = rig.effects().len();
    let ran = rig.step("rollback");
    ran.ok();
    assert_eq!(ran.detail(), "no update in flight: nothing to roll back");
    assert_eq!(rig.effects().len(), effects);
    assert_eq!(rig.machine(), before);
}

/// Asserts the boot entry through the real binary. A reboot between `cutover` and the commit
/// (every unit stopped, every table gone, a new boot id) is an abort: `restore` loads the guard,
/// points the redirect at the old slot and starts only that one, changes nothing when run again,
/// and the `rollback` the updater then runs leaves the fakes as they were before the update. A
/// reboot after `cleanup` starts the new slot alone. It would catch a boot that starts both
/// slots, or the slot the record does not name.
#[test]
fn restore_starts_the_slot_the_record_names_after_a_reboot() {
    let rig = Rig::new("hook-conformance-restore", Setup::handover());
    let a = rig.a;
    let before = rig.machine();
    rig.step("prepare").ok();
    rig.step("cutover").ok();
    let record = rig.record();
    rig.reboot("boot-two");
    assert!(rig.running().is_empty());
    let mark = rig.calls().len();
    let ran = rig.step("restore");
    ran.ok();
    assert_eq!(ran.detail(), format!("restored slot {a}"));
    assert_eq!(rig.calls_since(mark), [APPLY, APPLY, start(a).as_str()]);
    assert_eq!(rig.running(), [a]);
    assert_eq!(rig.redirect(), Some(a));
    assert!(rig.has_table(GUARD_TABLE));
    assert_eq!(rig.record(), record);
    let effects = rig.effects().len();
    rig.step("restore").ok();
    assert_eq!(rig.effects().len(), effects);
    rig.step("rollback").ok();
    assert_eq!(rig.machine(), before);

    let rig = Rig::new("hook-conformance-restore-after", Setup::handover());
    for step in ["prepare", "cutover", "verify", "cleanup"] {
        rig.step(step).ok();
    }
    rig.reboot("boot-two");
    rig.step("restore").ok();
    assert_eq!(rig.running(), [rig.b]);
    assert_eq!(rig.redirect(), Some(rig.b));
    public_answers(&rig, rig.b, NEW_VERSION);
}
