//! The real binary when a program it runs fails (an `nft` or `systemctl` call the fake answers
//! with exit 1 and no change), and when another step holds the lock of the state directory.

use tierstore::server::update_hook::{GUARD_TABLE, Record};

use crate::rig::{APPLY, Host, LIST_GUARD, Rig, Setup, start, stop};

/// Asserts that a guard table `nft` will not load defers `prepare` (exit 75, `host_not_ready`,
/// class `environment`, the retry delay) with nothing recorded, linked or started, and that the
/// `prepare` the updater retries goes through. It would catch a `prepare` that starts an
/// instance whose private port nothing guards.
#[test]
fn a_guard_nft_refuses_defers_prepare_and_a_retry_goes_through() {
    let rig = Rig::new("hook-conformance-guard-fails", Setup::handover());
    rig.world.fail_once(APPLY);
    let ran = rig.step("prepare");
    assert_eq!(ran.code, Some(75), "{ran:?}");
    assert_eq!(ran.line["reason"], "host_not_ready");
    assert_eq!(ran.line["class"], "environment");
    assert_eq!(ran.line["retry_after_secs"], 300);
    assert_eq!(rig.calls(), [LIST_GUARD, APPLY]);
    assert_eq!(rig.record(), Record::idle(rig.a));
    assert!(!rig.has_table(GUARD_TABLE));
    assert_eq!(rig.link(rig.b), None);
    assert!(rig.effects().is_empty());
    rig.step("prepare").ok();
    assert_eq!(rig.running(), [rig.a, rig.b]);
}

/// Asserts that a start of the idle unit that `systemctl` fails is `unit_failed` (class
/// `environment`) with the update on record and only the old unit running, and that `prepare`
/// run again starts it. It would catch a failure to start taken for a start, or a re-run that
/// cannot pick up the update it recorded.
#[test]
fn a_start_systemctl_refuses_fails_prepare_and_a_rerun_recovers() {
    let rig = Rig::new("hook-conformance-start-fails", Setup::handover());
    let (a, b) = (rig.a, rig.b);
    rig.world.fail_once(&start(b));
    let ran = rig.step("prepare");
    ran.fails("unit_failed");
    assert_eq!(ran.line["class"], "environment");
    assert!(
        ran.detail().contains("a failure the test asked for"),
        "{ran:?}"
    );
    let record = rig.record();
    assert_eq!((record.serving, record.previous), (b, Some(a)));
    assert_eq!(rig.running(), [a]);
    assert_eq!(rig.link(b), Some(rig.new.clone()));
    rig.step("prepare").ok();
    assert_eq!(rig.running(), [a, b]);
}

/// Asserts that a redirect `nft` will not apply fails `cutover` with `nft_failed`, puts the old
/// slot back in front (its start a no-op, the redirect applied again to it), records no swap,
/// and that the `cutover` run again moves the redirect. It would catch a failed swap that leaves
/// the public port on neither slot.
#[test]
fn a_redirect_nft_refuses_fails_cutover_and_leaves_the_old_slot() {
    let rig = Rig::new("hook-conformance-redirect-fails", Setup::handover());
    let (a, b) = (rig.a, rig.b);
    rig.step("prepare").ok();
    rig.world.fail_once(APPLY);
    let mark = rig.calls().len();
    rig.step("cutover").fails("nft_failed");
    assert_eq!(
        rig.calls_since(mark),
        [APPLY, start(a).as_str(), APPLY],
        "the swap, then the old slot put back"
    );
    assert_eq!(rig.redirect(), Some(a));
    assert!(rig.record().markers.is_empty());
    rig.step("cutover").ok();
    assert_eq!(rig.redirect(), Some(b));
}

/// Asserts that a stop of the old unit that `systemctl` fails during `cleanup` is `unit_failed`
/// with `"reversible":false` (the commit is on record), with the old slot's link already gone,
/// and that `cleanup` run again goes forward and finishes. It would catch a failed retirement
/// that reports itself reversible, or one a re-run cannot finish.
#[test]
fn a_stop_systemctl_refuses_in_cleanup_is_irreversible_and_finishes_when_run_again() {
    let rig = Rig::new("hook-conformance-stop-fails", Setup::handover());
    let (a, b) = (rig.a, rig.b);
    for step in ["prepare", "cutover", "verify"] {
        rig.step(step).ok();
    }
    rig.world.fail_once(&stop(a));
    let ran = rig.step("cleanup");
    ran.fails("unit_failed");
    assert_eq!(ran.line["reversible"], false);
    let record = rig.record();
    assert!(record.committed());
    assert_eq!(record.previous, Some(a));
    assert_eq!(rig.link(a), None);
    assert_eq!(rig.running(), [a, b]);
    rig.step("rollback").fails("committed");
    rig.step("cleanup").ok();
    assert_eq!(rig.running(), [b]);
    assert_eq!(rig.record().previous, None);
}

/// Asserts that while one hook process holds the lock of the state directory (held in the
/// middle of `prepare`, after its start of the idle unit, by the fake's handshake), a second
/// process for `verify`, `rollback`, `cleanup` or `restore` exits 1 with `locked` and the class
/// `environment`, makes no call and leaves the record byte for byte as it was; and that the
/// first finishes once released. It would catch a hook that acts on the record while another
/// step is changing it.
#[test]
fn a_second_hook_finds_the_lock_held_and_does_nothing() {
    let rig = Rig::new("hook-conformance-locked", Setup::handover());
    let (a, b) = (rig.a, rig.b);
    rig.world.hold_after(3);
    let holder = rig.spawn_step("prepare");
    rig.world.wait_held();
    let calls = rig.calls();
    assert_eq!(calls.last().map(String::as_str), Some(start(b).as_str()));
    let record = rig.record_bytes();
    for step in ["verify", "rollback", "cleanup", "restore"] {
        let ran = rig.step(step);
        ran.fails("locked");
        assert_eq!(ran.line["class"], "environment");
    }
    assert_eq!(rig.calls(), calls);
    assert_eq!(rig.record_bytes(), record);
    rig.world.release();
    Host::finish(holder).ok();
    assert_eq!(rig.running(), [a, b]);
}
