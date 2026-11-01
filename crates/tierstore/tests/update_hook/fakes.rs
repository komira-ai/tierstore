//! The stand-in programs themselves (fake/main.rs): the conformance tests are only as good as
//! the fakes, so each answer the hook reads is checked here against what the real program says,
//! with the hook's own ruleset text and its own reading of a listing.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use tierstore::server::update_hook::{
    GUARD_TABLE, PUBLIC_PORT, guard_ruleset, redirect_ruleset, redirect_target,
};
use tierstore_testkit::dir::TestDir;

use crate::rig::place_fake;
use crate::world;

/// A directory with the fakes in it, named as the hook calls them, and their world.
struct Tools {
    _dir: TestDir,
    path: PathBuf,
    world: PathBuf,
}

fn tools(name: &str) -> Tools {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().to_path_buf();
    for program in ["nft", "systemctl", "tierstore"] {
        place_fake(&path.join(program));
    }
    let world = path.join("state");
    fs::create_dir(&world).unwrap();
    Tools {
        _dir: dir,
        path,
        world,
    }
}

impl Tools {
    fn run(&self, program: &str, args: &[&str], input: &str) -> Output {
        let mut child = Command::new(self.path.join(program))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Asserts what the fake `nft` answers: an absent table is exit 1 with ENOENT's text on standard
/// error, for a listing and a deletion; the hook's own rulesets load, the listing gives the
/// redirect's port back through the hook's own reading, a second load of the same text changes
/// nothing, a flush and reload replaces the target; a ruleset with a line it does not know
/// changes nothing (one transaction); a deletion removes the table. It would catch a fake that
/// answers what the real `nft` never does, which would let the hook pass against it.
#[test]
fn the_fake_nft_answers_as_nft_does() {
    let tools = tools("hook-fake-nft");
    let listed = tools.run(
        "nft",
        &["list", "table", "inet", "kbf_forward_tierstore"],
        "",
    );
    assert_eq!(listed.status.code(), Some(1));
    assert!(text(&listed.stderr).contains("No such file or directory"));
    assert!(listed.stdout.is_empty());
    let deleted = tools.run("nft", &["delete", "table", "inet", GUARD_TABLE], "");
    assert_eq!(deleted.status.code(), Some(1));
    assert!(text(&deleted.stderr).contains("No such file or directory"));

    for port in [9301, 9301, 9302] {
        let applied = tools.run("nft", &["-f", "-"], &redirect_ruleset(PUBLIC_PORT, port));
        assert!(applied.status.success(), "{applied:?}");
        let listed = tools.run(
            "nft",
            &["list", "table", "inet", "kbf_forward_tierstore"],
            "",
        );
        assert!(listed.status.success());
        let listing = text(&listed.stdout);
        assert!(
            listing.starts_with("table inet kbf_forward_tierstore {"),
            "{listing}"
        );
        assert_eq!(redirect_target(&listing), Some(port), "{listing}");
    }
    let guard = tools.run(
        "nft",
        &["-f", "-"],
        &guard_ruleset(PUBLIC_PORT, [9301, 9302]),
    );
    assert!(guard.status.success());
    let effects = world::lines(&tools.world, "effects");
    assert_eq!(effects.len(), 3, "{effects:#?}");
    assert!(effects[0].starts_with("set kbf_forward_tierstore ") && effects[0].ends_with(":9301"));
    assert!(effects[1].ends_with(":9302"), "{effects:#?}");
    assert!(effects[2].starts_with("set kbf_guard_tierstore "));

    let bad = format!("{}bounce\n", redirect_ruleset(PUBLIC_PORT, 9301));
    let refused = tools.run("nft", &["-f", "-"], &bad);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(world::redirect(&tools.world), Some(9302));
    let flush_absent = tools.run("nft", &["-f", "-"], "flush table inet nothing_here\n");
    assert_eq!(flush_absent.status.code(), Some(1));
    assert!(text(&flush_absent.stderr).contains("No such file or directory"));

    let deleted = tools.run("nft", &["delete", "table", "inet", GUARD_TABLE], "");
    assert!(deleted.status.success());
    assert!(!world::tables(&tools.world).contains_key(GUARD_TABLE));
    assert_eq!(tools.run("nft", &["monitor"], "").status.code(), Some(1));
    assert_eq!(world::lines(&tools.world, "calls").len(), 13);
}

/// Asserts what the fake `systemctl` answers: a start whose condition path is missing exits 0
/// and leaves the unit inactive (systemd skips it); a start whose binary resolves makes the unit
/// active with the release it runs, once (a second start changes nothing); a stop makes it
/// inactive, and a stop of an inactive unit is exit 0 and no change; a unit whose binary does not
/// resolve fails to start. It would catch a fake that reports a skipped start as a running unit.
#[test]
fn the_fake_systemctl_answers_as_systemd_does() {
    let tools = tools("hook-fake-systemctl");
    let slots = tools.path.join("slots");
    let release = tools.path.join("release");
    fs::create_dir_all(&slots).unwrap();
    fs::create_dir_all(&release).unwrap();
    place_fake(&release.join("tierstore"));
    fs::write(
        tools.world.join("exec"),
        format!("{}/%i/tierstore", slots.display()),
    )
    .unwrap();
    let start = |unit: &str| tools.run("systemctl", &["start", "--", unit], "");
    let stop = |unit: &str| tools.run("systemctl", &["stop", "--", unit], "");

    assert!(start("tierstore@9301.service").status.success());
    assert!(world::units(&tools.world).is_empty(), "no link: skipped");

    std::os::unix::fs::symlink(&release, slots.join("9301")).unwrap();
    fs::write(tools.world.join("condition"), "/nonexistent/%i").unwrap();
    assert!(start("tierstore@9301.service").status.success());
    assert!(
        world::units(&tools.world).is_empty(),
        "condition fails: skipped"
    );
    fs::remove_file(tools.world.join("condition")).unwrap();

    assert!(start("tierstore@9301.service").status.success());
    assert!(start("tierstore@9301.service").status.success());
    let units = world::units(&tools.world);
    assert_eq!(units.len(), 1);
    assert_eq!(
        units[&9301].release,
        fs::canonicalize(&release).unwrap(),
        "the release the link named at the start"
    );
    assert!(stop("tierstore@9301.service").status.success());
    assert!(stop("tierstore@9301.service").status.success());
    assert!(world::units(&tools.world).is_empty());
    assert_eq!(
        world::lines(&tools.world, "effects"),
        [
            "start tierstore@9301.service",
            "stop tierstore@9301.service"
        ]
    );

    fs::write(tools.world.join("condition"), "/").unwrap();
    let broken = start("tierstore@9302.service");
    assert_eq!(broken.status.code(), Some(1));
    assert!(text(&broken.stderr).contains("failed"));
    let unknown = start("sshd.service");
    assert_eq!(unknown.status.code(), Some(5));
    assert_eq!(
        tools.run("systemctl", &["mask", "x"], "").status.code(),
        Some(1)
    );
}

/// Asserts what the fake release answers: `--capabilities` prints the file beside it, nothing
/// when there is none, and any other argument is the usage error (exit 2). It would catch a
/// release that lists a capability the test did not give it.
#[test]
fn the_fake_release_prints_its_capabilities() {
    let tools = tools("hook-fake-release");
    let caps = tools.run("tierstore", &["--capabilities"], "");
    assert!(caps.status.success());
    assert!(caps.stdout.is_empty());
    fs::write(tools.path.join("capabilities"), "handover-v1\n").unwrap();
    let caps = tools.run("tierstore", &["--capabilities"], "");
    assert_eq!(text(&caps.stdout), "handover-v1\n");
    assert_eq!(
        tools.run("tierstore", &["--version"], "").status.code(),
        Some(2)
    );
    let other = tools.path.join("other");
    place_fake(&other);
    let unknown = Command::new(&other).output().unwrap();
    assert_eq!(unknown.status.code(), Some(2));
    assert!(world::lines(&tools.world, "calls").is_empty());
}
