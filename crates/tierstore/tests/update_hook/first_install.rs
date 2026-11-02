//! The first install of `packaging/README.md` through the real binary. The host is as step 3
//! leaves it: the record names slot `a` with no update in flight and `a`'s link is in place, but
//! no slot unit runs and no table of the hook is loaded, so the public port is still the old
//! unit's (which the stub does not play: with no redirect, the public port answers nothing here).
//! `restore` points the redirect at the slot before it starts the slot's unit and does not wait
//! for the slot to answer, which is why the README starts the slot and checks its `/readyz`
//! before `restore`, and gives the step that hands the public port back to the old unit.

use std::io;

use tierstore::server::update_hook::fake::{NFT, SYSTEMCTL};
use tierstore::server::update_hook::{FORWARD_TABLE, GUARD_TABLE, Record, unit_name};

use crate::flows::{kinds, public_answers};
use crate::rig::{APPLY, OLD_VERSION, Rig, Setup, start};
use crate::stub::SlotScript;

/// A host as step 3 of the first install leaves it.
fn first_install(name: &str) -> Rig {
    let rig = Rig::new(name, Setup::handover());
    rig.reboot("first-install");
    assert!(rig.running().is_empty());
    assert_eq!(rig.redirect(), None);
    assert!(!rig.has_table(GUARD_TABLE));
    assert_eq!(rig.record(), Record::idle(rig.a));
    rig
}

/// Asserts that when the slot's unit fails to start, `restore` fails with `unit_failed` after it
/// has already pointed the public port at the slot: the calls are the guard, the redirect and
/// then the start, and afterwards the redirect names `a` while nothing runs there, so the public
/// port answers nothing. And that the recovery the README gives, `nft delete table inet
/// kbf_forward_tierstore`, takes the redirect away (the public port is the old unit's again)
/// and leaves the guard. It would catch a README whose order trusts `restore` to start the slot
/// before the public port moves, and a recovery that names a table the hook does not use.
#[test]
fn a_failed_slot_start_leaves_the_public_port_on_the_slot_until_its_redirect_is_deleted() {
    let rig = first_install("hook-first-install-start-fails");
    let a = rig.a;
    rig.world.fail_once(&start(a));
    let mark = rig.calls().len();
    rig.step("restore").fails("unit_failed");
    assert_eq!(rig.calls_since(mark), [APPLY, APPLY, start(a).as_str()]);
    assert_eq!(rig.redirect(), Some(a));
    assert!(rig.running().is_empty());
    assert_eq!(rig.get(rig.public, "/readyz"), None);

    let delete = ["delete", "table", "inet", FORWARD_TABLE];
    let done = rig.world.call(NFT, &delete, &mut io::empty());
    assert_eq!(done.reply.code, 0, "{done:?}");
    assert_eq!(rig.redirect(), None);
    assert!(rig.has_table(GUARD_TABLE));
    assert_eq!(rig.record(), Record::idle(a));
}

/// Asserts the README's order with a slot that is slow to come up: the slot unit started by
/// hand before `restore` runs, its port not answering yet (the stub silent), and the public port
/// still not redirected, so nothing waits on the slot; once `/readyz` on the slot answers,
/// `restore` loads the guard and the redirect and its start finds the unit already running, so
/// the tables are the only things it changes, and the public port answers as slot `a` running
/// the release of its link, read-write. It would catch a `restore` that restarts a running
/// slot, or a README order that moves the public port before the slot answers.
#[test]
fn the_slot_started_and_ready_before_restore_takes_the_public_port_with_no_unanswered_moment() {
    let rig = first_install("hook-first-install-ready-first");
    let a = rig.a;
    rig.script(
        a,
        SlotScript {
            silent: true,
            ..SlotScript::default()
        },
    );
    let started = rig
        .world
        .call(SYSTEMCTL, &["start", "--", &unit_name(a)], &mut io::empty());
    assert_eq!(started.reply.code, 0, "{started:?}");
    assert_eq!(rig.running(), [a]);
    assert_eq!(rig.get(a, "/readyz"), None);
    assert_eq!(rig.redirect(), None);

    rig.script(a, SlotScript::default());
    assert_eq!(
        rig.get(a, "/readyz"),
        Some((200, "ready read-write\n".to_owned()))
    );
    let mark = rig.calls().len();
    let before = rig.effects().len();
    let ran = rig.step("restore");
    ran.ok();
    assert_eq!(ran.detail(), format!("restored slot {a}"));
    assert_eq!(rig.calls_since(mark), [APPLY, APPLY, start(a).as_str()]);
    assert_eq!(
        kinds(&rig.effects()[before..]),
        [format!("set {GUARD_TABLE}"), format!("set {FORWARD_TABLE}")]
    );
    assert_eq!(rig.running(), [a]);
    assert_eq!(rig.redirect(), Some(a));
    public_answers(&rig, a, OLD_VERSION);
}
