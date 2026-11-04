//! The first install of `packaging/README.md` through the real binary. The host is as step 3
//! leaves it: the record names slot `a` with no update in flight and `a`'s link is in place, but
//! no slot unit runs and no table of the hook is loaded, so the public port is still the old
//! unit's (which the stub does not play: with no redirect, the public port answers nothing here).
//! `restore` points the redirect at the slot before it starts the slot's unit and does not wait
//! for the slot to answer, which is why the README loads the guard, starts the slot and checks
//! its `/readyz` before `restore`, and gives the step that hands the public port back to the old
//! unit.

use std::fs;
use std::io;
use std::path::Path;

use tierstore::server::update_hook::fake::{NFT, SYSTEMCTL};
use tierstore::server::update_hook::{
    FORWARD_TABLE, GUARD_TABLE, Record, guard_ruleset, unit_name,
};

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
/// tierstore_forward`, takes the redirect away (the public port is the old unit's again)
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

/// The README's first install.
fn readme() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/README.md");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The guard table the README has loaded before any slot runs: the lines of its `nft` block,
/// each without the list item's indentation, up to the line of the closing fence.
fn readme_guard() -> String {
    let readme = readme();
    let mut lines = readme.lines().map(str::trim);
    lines
        .by_ref()
        .find(|line| *line == "```nft")
        .expect("the README's nft block");
    let block: Vec<&str> = lines.take_while(|line| *line != "```").collect();
    block.iter().map(|line| format!("{line}\n")).collect()
}

/// Asserts that the guard the README loads by hand is the hook's own guard for the public port
/// 9300 and the declaration's slots 9301 and 9302, byte for byte: a table of the `inet` family
/// (IPv4 and IPv6 alike, which `tests/handover/guard.rs` proves in the real kernel for this
/// ruleset) whose last rule drops the slot ports, with loopback and connections redirected from
/// 9300 let through. It would catch a README guard that drifts from the hook's, names other
/// ports, or is limited to one family.
#[test]
fn the_readmes_guard_is_the_hooks_guard_for_the_declared_ports() {
    let text = readme_guard();
    assert_eq!(text, guard_ruleset(9300, [9301, 9302]));
    assert!(text.lines().all(|line| line.contains(" inet ")), "{text}");
    assert_eq!(
        text.lines().last(),
        Some("add rule inet tierstore_guard in tcp dport { 9301, 9302 } drop")
    );
    assert!(!text.contains(" ip ") && !text.contains(" ip6 "), "{text}");
}

/// Asserts the README's order, with a slot that is slow to come up, through the real hook. The
/// guard is loaded by hand first, then the slot unit is started, so the guard's change comes
/// before the start's. While the slot does not answer yet (the stub silent), the public port is
/// not redirected, so nothing waits on the slot. Once `/readyz` on the slot answers, `restore`
/// finds the guard as it would load it and the unit already running, so the redirect is the only
/// thing it changes, and the guard is the same table before and after. The public port then
/// answers as slot `a`, running the release of its link, read-write. It would catch a guard
/// that `restore` replaces (the README's text not the hook's), a `restore` that restarts a
/// running slot, or an order that moves the public port before the slot answers.
#[test]
fn the_guard_then_the_slot_ready_before_restore_takes_the_public_port_with_no_unanswered_moment() {
    let rig = first_install("hook-first-install-ready-first");
    let a = rig.a;
    let guard = guard_ruleset(rig.public, [a, rig.b]);
    let loaded = rig.world.call(NFT, &["-f", "-"], &mut guard.as_bytes());
    assert_eq!(loaded.reply.code, 0, "{loaded:?}");
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
    assert_eq!(
        kinds(&rig.effects()),
        [
            format!("set {GUARD_TABLE}"),
            format!("start {}", unit_name(a))
        ]
    );
    let guarded = rig.machine().tables[GUARD_TABLE].clone();
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
        [format!("set {FORWARD_TABLE}")]
    );
    assert_eq!(rig.machine().tables[GUARD_TABLE], guarded);
    assert_eq!(rig.running(), [a]);
    assert_eq!(rig.redirect(), Some(a));
    public_answers(&rig, a, OLD_VERSION);
}
