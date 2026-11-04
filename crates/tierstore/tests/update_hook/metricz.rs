//! The probe through the public port, fed what the real server says: `/metricz` of a
//! `tierstore` process started with `slot` in its file (and one started without it), served verbatim by
//! the stub as the new slot's answer, and read by the real hook's `cutover`.

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use tierstore::server::build_info::{COMMIT, VERSION, gauge};
use tierstore::server::update_hook::{Marker, build_of};
use tierstore_testkit::reaped::{first_line, spawn};

use crate::rig::{HOOK, Rig, Setup};
use crate::stub::SlotScript;

/// Starts the real server (memory tiers, a scratch key file) with `server.slot` set when a slot is
/// given, and returns the body of its `/metricz`.
fn served_metricz(rig: &Rig, slot: Option<u16>) -> String {
    let key = rig.root.join("server-key");
    fs::write(&key, "AKIDHOOK\nhook-secret\n").unwrap();
    fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
    let slot_line = slot.map_or_else(String::new, |slot| format!("slot = {slot}\n"));
    let config = rig.root.join("server.toml");
    let text = format!(
        "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\n\
         [server]\nproxy_id = \"hook-probe\"\n{slot_line}bucket = \"tier-bucket\"\naccess_key_file = {:?}\n\n\
         [hot_store]\nkind = \"memory\"\nbudget = 1048576\n\n[cold_store]\nkind = \"memory\"\n",
        key.display().to_string()
    );
    fs::write(&config, text).unwrap();
    let mut command = Command::new(HOOK);
    command
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = spawn(&mut command, 3).unwrap();
    let (line, _reader) =
        first_line(child.stdout.take().unwrap(), Duration::from_secs(30)).unwrap();
    let port: u16 = line
        .split_whitespace()
        .find_map(|word| word.strip_prefix("listen="))
        .and_then(|addr| addr.rsplit(':').next())
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("no listen= in {line:?}"));
    let (status, body) = rig.get(port, "/metricz").expect("an answer");
    assert_eq!(status, 200);
    body
}

/// Asserts that the body of `/metricz` the server itself serves when started with `server.slot`
/// carries the server's build gauge with that slot, and that the real hook's probe, given that
/// body through the public port, finds the new slot and the staged version in it (`cutover`
/// succeeds and records `swapped`); and that the body of a server started without `server.slot`
/// fails the same probe (`probe_failed`), after which the redirect is back on the old slot. It
/// would catch a probe that reads a label the server does not render, which a stub written by
/// hand would not.
#[test]
fn the_probe_reads_the_slot_from_the_body_the_server_renders() {
    let rig = Rig::new("hook-conformance-metricz", Setup::handover());
    let b = rig.b;
    rig.set_target(VERSION);
    let body = served_metricz(&rig, Some(b));
    assert!(body.contains(&gauge(VERSION, COMMIT, Some(b))), "{body}");
    assert_eq!(build_of(&body).and_then(|build| build.slot), Some(b));
    rig.script(
        b,
        SlotScript {
            metricz: Some(body),
            ..SlotScript::default()
        },
    );
    rig.step("prepare").ok();
    rig.step("cutover").ok();
    assert_eq!(rig.redirect(), Some(b));
    assert!(rig.record().markers.contains(&Marker::Swapped));

    let rig = Rig::new("hook-conformance-metricz-no-slot", Setup::handover());
    let (a2, b2) = (rig.a, rig.b);
    rig.set_target(VERSION);
    let body = served_metricz(&rig, None);
    assert!(body.contains(&gauge(VERSION, COMMIT, None)), "{body}");
    assert!(!body.contains("slot="), "{body}");
    rig.script(
        b2,
        SlotScript {
            metricz: Some(body),
            ..SlotScript::default()
        },
    );
    rig.step("prepare").ok();
    rig.step("cutover").fails("probe_failed");
    assert_eq!(rig.redirect(), Some(a2));
    assert!(rig.record().markers.is_empty());
}
