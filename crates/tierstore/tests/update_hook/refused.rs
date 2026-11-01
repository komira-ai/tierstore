//! What the real binary refuses, and what it does not take for success: a forged or foreign
//! record, a refused declaration, a declaration the default owner would not trust, a unit start
//! that systemd skipped, and the params key that forces a restart.

use std::os::unix::fs::{PermissionsExt, symlink};

use tierstore::server::update_hook::{Mode, Record};

use crate::flows::kinds;
use crate::rig::{APPLY, DECLARED_STATE, LIST_GUARD, Rig, Setup, start, uid};
use crate::stub::SlotScript;

/// Every step and the boot entry.
const ALL: [&str; 6] = [
    "prepare", "cutover", "verify", "rollback", "cleanup", "restore",
];

/// Asserts that a record in the state directory that the hook could not have written (a port
/// outside the two declared, an unknown mode or marker, equal `serving` and `previous`, a newer
/// schema, a reason for the mode that names the other mode, text that is not JSON) makes every
/// step and `restore` exit 1 with `state_invalid` and the class `environment`; that a record the
/// service could have written (one a group may write, or a link) is refused as `state_io`; and
/// that in all of it the fake `nft` and `systemctl` record no call and the fakes are unchanged. It
/// would catch a hook that takes a port, a unit or a mode from a record it should not trust.
#[test]
fn a_forged_or_foreign_record_is_refused_with_no_call() {
    let rig = Rig::new("hook-conformance-forged", Setup::handover());
    let (a, b) = (rig.a, rig.b);
    let before = rig.machine();
    let forged = [
        r#"{"schema":1,"serving":9999,"previous":null,"mode":"handover","markers":[]}"#.to_owned(),
        format!(r#"{{"schema":1,"serving":{b},"previous":{a},"mode":"bounce","markers":[]}}"#),
        format!(
            r#"{{"schema":1,"serving":{b},"previous":{a},"mode":"handover","markers":["done"]}}"#
        ),
        format!(r#"{{"schema":1,"serving":{a},"previous":{a},"mode":"handover","markers":[]}}"#),
        format!(r#"{{"schema":2,"serving":{a},"previous":null,"mode":"handover","markers":[]}}"#),
        format!(
            r#"{{"schema":1,"serving":{b},"previous":{a},"mode":"handover","markers":[],"mode_reason":"declared_restart"}}"#
        ),
        "not json".to_owned(),
    ];
    for text in &forged {
        rig.set_record_bytes(text.as_bytes(), 0o600);
        for step in ALL {
            let ran = rig.step(step);
            assert_eq!(ran.code, Some(1), "{step} over {text}: {ran:?}");
            assert_eq!(ran.line["reason"], "state_invalid", "{step} over {text}");
            assert_eq!(ran.line["class"], "environment", "{step} over {text}");
        }
    }
    let good = Record::idle(a).encode();
    rig.set_record_bytes(&good, 0o664);
    for step in ALL {
        let ran = rig.step(step);
        ran.fails("state_io");
        assert!(
            ran.detail().contains("written by a group or others"),
            "{ran:?}"
        );
    }
    let elsewhere = rig.root.join("record.json");
    std::fs::write(&elsewhere, &good).unwrap();
    std::fs::remove_file(rig.state.join("state.json")).unwrap();
    symlink(&elsewhere, rig.state.join("state.json")).unwrap();
    for step in ALL {
        rig.step(step).fails("state_io");
    }
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    assert_eq!(rig.machine(), before);
}

/// Asserts that a declaration the hook refuses (a relative `state_dir`, a reserved one, one
/// health port) is exit 1 with `state_invalid` and the reason in the detail, with no call to the
/// fakes and no lock file made; and that without `--root-uid` a declaration owned by anyone but
/// root is not trusted (`host_failed`). It would catch a hook that reads a state directory from
/// a declaration it refused, or trusts the declaration's owner by default.
#[test]
fn a_refused_declaration_is_exit_1_with_no_call() {
    let rig = Rig::new("hook-conformance-declaration", Setup::handover());
    let (a, b) = (rig.a, rig.b);
    let good = rig.declaration_text(DECLARED_STATE);
    let one_port = good.replace(&format!("[{a}, {b}]"), &format!("[{a}]"));
    let cases = [
        (
            rig.declaration_text("var/lib/updater/hooks/tierstore"),
            "state_dir must be an absolute path",
        ),
        (rig.declaration_text("/etc/tierstore"), "is a reserved path"),
        (
            rig.declaration_text("/var/lib/tierstore/hook"),
            "is a reserved path",
        ),
        (one_port, "health_ports must be two different ports"),
    ];
    for (text, why) in &cases {
        rig.set_declaration(text);
        for step in ["prepare", "restore"] {
            let ran = rig.step(step);
            ran.fails("state_invalid");
            assert!(ran.detail().contains(why), "{why}: {ran:?}");
        }
    }
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    assert!(!rig.state.join("lock").exists());
    rig.set_declaration(&good);
    if uid() != 0 {
        let ran = rig.hook_trusting(None, &["restore"]);
        ran.fails("host_failed");
        assert!(ran.detail().contains("is not trusted"), "{ran:?}");
    }
    let ran = rig.hook_trusting(Some(uid() + 1), &["restore"]);
    ran.fails("host_failed");
    assert!(ran.detail().contains("is not trusted"), "{ran:?}");
    assert!(rig.calls().is_empty());
    rig.step("restore").ok();
}

/// Asserts that a staged release whose binary a group may write is refused by `prepare`
/// (`host_failed`, the binary named) before any call to the fakes, whatever `--root-uid` says:
/// the flag names who must own the files, it does not loosen what they must be. It would catch
/// a hook that runs a binary the service could have replaced.
#[test]
fn a_release_binary_others_could_write_is_refused() {
    let rig = Rig::new("hook-conformance-release-mode", Setup::handover());
    let binary = rig.new.join("tierstore");
    let copy = rig.new.join("copy");
    std::fs::copy(&binary, &copy).unwrap();
    std::fs::rename(&copy, &binary).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o775)).unwrap();
    let ran = rig.step("prepare");
    ran.fails("host_failed");
    assert!(
        ran.detail().contains("can be written by a group or others"),
        "{ran:?}"
    );
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    assert_eq!(rig.record(), Record::idle(rig.a));
}

/// Asserts that a start of the idle slot whose `ConditionPathExists=` fails, which `systemctl`
/// answers with exit 0 and no running unit (as systemd does), is not taken for success:
/// `prepare` polls `/readyz`, gets no answer, and fails with `not_ready` (class `release`), not
/// with a unit error; the `cutover` after it fails the same way, and the redirect never leaves
/// the old slot. It would catch a `prepare` that trusts the exit status of the start.
#[test]
fn a_start_whose_condition_fails_is_not_taken_for_a_running_instance() {
    let rig = Rig::new("hook-conformance-condition", Setup::handover());
    let (a, b) = (rig.a, rig.b);
    rig.world
        .set_condition(&format!("{}/misdeclared/%i", rig.root.display()));
    let ran = rig.step("prepare");
    ran.fails("not_ready");
    assert_eq!(ran.line["class"], "release");
    assert_eq!(rig.calls(), [LIST_GUARD, APPLY, start(b).as_str()]);
    assert_eq!(rig.running(), [a]);
    assert_eq!(rig.redirect(), Some(a));
    rig.step("cutover").fails("not_ready");
    assert_eq!(rig.redirect(), Some(a));
    assert_eq!(rig.running(), [a]);
    assert_eq!(kinds(&rig.effects()), ["set kbf_guard_tierstore"]);
}

/// Asserts, through the real binary, that `mode = "restart"` in the params makes `prepare` choose
/// a restart though both releases list `handover-v1` (the record and the detail say why, and no
/// unit is started), that the same host without the key chooses a handover, and that any other
/// value or type is refused before any step runs, with a message naming the key. It would catch
/// a declaration that cannot keep a `dir:` hot tier out of a handover.
#[test]
fn a_declared_restart_is_chosen_though_both_releases_can_hand_over() {
    let rig = Rig::new("hook-conformance-declared", Setup::handover());
    let (a, b) = (rig.a, rig.b);
    let ran = rig.step_with("prepare", "mode = \"restart\"\n");
    ran.ok();
    assert_eq!(
        ran.detail(),
        format!("prepared slot {b} beside {a}, mode restart (declared_restart)")
    );
    assert_eq!(rig.calls(), [LIST_GUARD, APPLY]);
    assert_eq!(rig.running(), [a]);
    assert_eq!(rig.record().mode, Mode::Restart);

    let rig = Rig::new("hook-conformance-capable", Setup::handover());
    let (a, b) = (rig.a, rig.b);
    for extra in [
        "mode = \"handover\"\n",
        "mode = \"Restart\"\n",
        "mode = 1\n",
        "mode = [\"restart\"]\n",
    ] {
        let ran = rig.step_with("prepare", extra);
        ran.fails("bad_params");
        assert!(
            ran.stderr.contains("params key \"mode\""),
            "{extra}: {ran:?}"
        );
    }
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    assert_eq!(rig.record(), Record::idle(a));
    let ran = rig.step("prepare");
    ran.ok();
    assert_eq!(
        ran.detail(),
        format!("prepared slot {b} beside {a}, mode handover (handover_capable)")
    );
    assert_eq!(rig.calls(), [LIST_GUARD, APPLY, start(b).as_str()]);
    assert_eq!(rig.running(), [a, b]);
}

/// Asserts that a new instance that says `ready read-write` while the old one runs (two writers
/// on one cache) makes the real `prepare` fail with `unexpected_mode`, class `release`. It would
/// catch a `prepare` that reads only the status of `/readyz`, not the mode.
#[test]
fn a_new_instance_that_says_read_write_beside_the_old_one_fails_prepare() {
    let rig = Rig::new("hook-conformance-two-writers", Setup::handover());
    rig.script(
        rig.b,
        SlotScript {
            readyz: Some("ready read-write\n".to_owned()),
            ..SlotScript::default()
        },
    );
    let ran = rig.step("prepare");
    ran.fails("unexpected_mode");
    assert_eq!(ran.line["class"], "release");
}
