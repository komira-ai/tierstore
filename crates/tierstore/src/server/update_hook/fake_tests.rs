//! The stand-in programs of the conformance test: the tests that use them are only as good as
//! they are, so each answer the hook reads is checked here against what the real program says,
//! with the hook's own ruleset text and its own reading of a listing.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

use super::fake::{Done, NFT, RELEASE, Reply, SYSTEMCTL, WORLD, World, release, run};
use super::{GUARD_TABLE, guard_ruleset, redirect_ruleset, redirect_target};

/// A scratch world.
fn world(name: &str) -> (TestDir, World, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().join(WORLD);
    fs::create_dir(&path).unwrap();
    (dir, World::new(path.clone()), path)
}

fn nft(world: &World, args: &[&str], input: &str) -> Reply {
    world.call(NFT, args, &mut input.as_bytes()).reply
}

fn systemctl(world: &World, args: &[&str]) -> Reply {
    world.call(SYSTEMCTL, args, &mut io::empty()).reply
}

/// A reader that fails.
struct Broken;

impl Read for Broken {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("broken"))
    }
}

/// Asserts what the fake `nft` answers: an absent table is exit 1 with ENOENT's text on standard
/// error, for a listing and a deletion; the hook's own rulesets load, the listing gives the
/// redirect's port back through the hook's own reading, a second load of the same text changes
/// nothing, a flush and reload replaces the target; a ruleset with a line it does not know, a
/// flush or a rule of an absent table, or an input that cannot be read changes nothing (one
/// transaction); a deletion removes the table. It would catch a fake that answers what the real
/// `nft` never does, which would let the hook pass against it.
#[test]
fn the_fake_nft_answers_as_nft_does() {
    let (_dir, world, _) = world("hook-fake-nft");
    let listed = nft(
        &world,
        &["list", "table", "inet", "kbf_forward_tierstore"],
        "",
    );
    assert_eq!(listed.code, 1);
    assert!(listed.stderr.contains("No such file or directory"));
    assert!(listed.stdout.is_empty());
    let deleted = nft(&world, &["delete", "table", "inet", GUARD_TABLE], "");
    assert_eq!(deleted.code, 1);
    assert!(deleted.stderr.contains("No such file or directory"));
    assert_eq!(world.redirect(), None);

    for port in [9301, 9301, 9302] {
        let applied = nft(&world, &["-f", "-"], &redirect_ruleset(9300, port));
        assert_eq!(applied.code, 0, "{applied:?}");
        let listed = nft(
            &world,
            &["list", "table", "inet", "kbf_forward_tierstore"],
            "",
        );
        assert_eq!(listed.code, 0);
        assert!(
            listed
                .stdout
                .starts_with("table inet kbf_forward_tierstore {"),
            "{listed:?}"
        );
        assert_eq!(redirect_target(&listed.stdout), Some(port));
        assert_eq!(world.redirect(), Some(port));
    }
    let guard = nft(&world, &["-f", "-"], &guard_ruleset(9300, [9301, 9302]));
    assert_eq!(guard.code, 0);
    let effects = world.lines("effects");
    assert_eq!(effects.len(), 3, "{effects:#?}");
    assert!(effects[0].starts_with("set kbf_forward_tierstore ") && effects[0].ends_with(":9301"));
    assert!(effects[1].ends_with(":9302"), "{effects:#?}");
    assert!(effects[2].starts_with("set kbf_guard_tierstore "));
    assert_eq!(world.tables().len(), 2);

    let bad = format!("{}bounce\n", redirect_ruleset(9300, 9301));
    assert_eq!(nft(&world, &["-f", "-"], &bad).code, 1);
    assert_eq!(world.redirect(), Some(9302));
    let flush = nft(&world, &["-f", "-"], "flush table inet nothing_here\n");
    assert!(flush.stderr.contains("No such file or directory"));
    let rule = nft(
        &world,
        &["-f", "-"],
        "add rule inet nothing_here pre tcp dport 1 accept\n",
    );
    assert!(rule.stderr.contains("No such file or directory"));
    let unread = world.call(NFT, &["-f", "-"], &mut Broken).reply;
    assert_eq!(unread.code, 1);
    assert!(!world.tables().contains_key("nothing_here"));

    let deleted = nft(&world, &["delete", "table", "inet", GUARD_TABLE], "");
    assert_eq!(deleted.code, 0);
    assert!(!world.tables().contains_key(GUARD_TABLE));
    assert_eq!(
        world.lines("effects").last().unwrap(),
        "delete kbf_guard_tierstore"
    );
    assert_eq!(nft(&world, &["monitor"], "").code, 1);
    assert_eq!(world.lines("calls").len(), 15);
    assert_eq!(
        world.lines("calls")[0],
        "nft list table inet kbf_forward_tierstore"
    );
}

/// A slot directory with a link of 9301 to a release, the release's binary, and the world's
/// unit binary pointing into it.
fn slots(world: &World, base: &Path) -> PathBuf {
    let slots = base.join("slots");
    let release = base.join("release");
    fs::create_dir_all(&slots).unwrap();
    fs::create_dir_all(&release).unwrap();
    fs::write(release.join(RELEASE), "").unwrap();
    world.set_exec(&format!("{}/%i/{RELEASE}", slots.display()));
    release
}

/// Asserts what the fake `systemctl` answers: a start whose condition path is missing exits 0
/// and leaves the unit inactive (systemd skips it); a start whose binary resolves makes the unit
/// active with the release it runs, once (a second start changes nothing), and a later start of
/// another unit comes after it; a stop makes it inactive, and a stop of an inactive unit is exit
/// 0 and no change; a unit whose binary does not resolve fails to start; another unit name and
/// another verb are refused. It would catch a fake that reports a skipped start as a running
/// unit.
#[test]
fn the_fake_systemctl_answers_as_systemd_does() {
    let (dir, world, _) = world("hook-fake-systemctl");
    let release = slots(&world, dir.path());
    let start = |unit: &str| systemctl(&world, &["start", "--", unit]);
    let stop = |unit: &str| systemctl(&world, &["stop", "--", unit]);

    assert_eq!(start("tierstore@9301.service").code, 0);
    assert!(world.units().is_empty(), "no link: skipped");
    symlink(&release, dir.path().join("slots/9301")).unwrap();
    world.set_condition("/nonexistent/%i");
    assert_eq!(start("tierstore@9301.service").code, 0);
    assert!(world.units().is_empty(), "the condition fails: skipped");
    world.set_condition("/");
    assert_eq!(start("tierstore@9301.service").code, 0);
    assert_eq!(start("tierstore@9301.service").code, 0);
    let units = world.units();
    assert_eq!(units.len(), 1);
    assert_eq!(units[&9301].release, fs::canonicalize(&release).unwrap());
    assert_eq!(units[&9301].seq, 1);
    symlink(&release, dir.path().join("slots/9302")).unwrap();
    assert_eq!(start("tierstore@9302.service").code, 0);
    assert_eq!(world.units()[&9302].seq, 2);
    assert_eq!(stop("tierstore@9301.service").code, 0);
    assert_eq!(stop("tierstore@9301.service").code, 0);
    assert_eq!(world.units().keys().copied().collect::<Vec<_>>(), [9302]);
    assert_eq!(
        world.lines("effects"),
        [
            "start tierstore@9301.service",
            "start tierstore@9302.service",
            "stop tierstore@9301.service"
        ]
    );
    let broken = start("tierstore@9303.service");
    assert_eq!(broken.code, 1);
    assert!(broken.stderr.contains("failed"));
    assert_eq!(start("sshd.service").code, 5);
    assert_eq!(systemctl(&world, &["mask", "x"]).code, 1);
    fs::write(dir.path().join(WORLD).join("units/stray"), "x").unwrap();
    fs::write(
        dir.path().join(WORLD).join("units/tierstore@9304.service"),
        "x",
    )
    .unwrap();
    assert_eq!(world.units().keys().copied().collect::<Vec<_>>(), [9302]);
    world.reboot();
    assert!(world.units().is_empty() && world.tables().is_empty());
}

/// Asserts the scripts of a call: one scripted to fail is answered with exit 1 and changes
/// nothing, once, and the same call then succeeds; `kill-after` asks for the parent's death after
/// exactly the n-th call from when it was set, and no longer once cleared; the logs clear. It
/// would catch a script that fails a call twice, or kills the hook a call early.
#[test]
fn a_call_fails_or_kills_as_the_test_scripts_it() {
    let (_dir, world, _) = world("hook-fake-scripts");
    let apply = redirect_ruleset(9300, 9301);
    world.fail_once("nft list table inet other");
    world.fail_once("nft -f -");
    let failed = world.call(NFT, &["-f", "-"], &mut apply.as_bytes());
    assert_eq!(failed.reply.code, 1);
    assert!(failed.reply.stderr.contains("a failure the test asked for"));
    assert_eq!(world.redirect(), None);
    assert_eq!(world.lines("fail"), ["nft list table inet other"]);
    let applied = world.call(NFT, &["-f", "-"], &mut apply.as_bytes());
    assert_eq!(
        applied,
        Done {
            reply: Reply {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            kill_parent: false,
        }
    );
    world.kill_after(Some(2));
    let listing = ["list", "table", "inet", "kbf_forward_tierstore"];
    assert!(!world.call(NFT, &listing, &mut io::empty()).kill_parent);
    assert!(world.call(NFT, &listing, &mut io::empty()).kill_parent);
    assert!(!world.call(NFT, &listing, &mut io::empty()).kill_parent);
    world.kill_after(Some(1));
    world.kill_after(None);
    world.kill_after(None);
    assert!(!world.call(NFT, &listing, &mut io::empty()).kill_parent);
    world.clear_logs();
    assert!(world.lines("calls").is_empty() && world.lines("effects").is_empty());
}

/// Asserts the handshake that holds a hook in the middle of a step: the call after which it
/// holds is done and logged, the test sees it held, and the call returns only when the test
/// releases it. It would catch a hold that returns at once, which would let a test of the lock
/// race the hook.
#[test]
fn a_call_is_held_until_the_test_releases_it() {
    let (dir, world, path) = world("hook-fake-hold");
    slots(&world, dir.path());
    world.hold_after(1);
    let held = std::thread::spawn(move || {
        World::new(path).call(
            SYSTEMCTL,
            &["stop", "--", "tierstore@9301.service"],
            &mut io::empty(),
        )
    });
    world.wait_held();
    assert_eq!(
        world.lines("calls"),
        ["systemctl stop -- tierstore@9301.service"]
    );
    world.release();
    assert_eq!(held.join().unwrap().reply.code, 0);
}

/// Asserts the program a run is by its name: `nft` and `systemctl` act on the world beside
/// them, `tierstore` prints the capabilities beside it (none when there is no file) and refuses
/// another argument with the usage status, and another name is refused. It would catch a
/// release that lists a capability the test did not give it.
#[test]
fn a_run_is_the_program_its_name_says() {
    let (dir, _world, path) = world("hook-fake-run");
    let at = |name: &str| dir.path().join(name);
    let none = run(
        &at(RELEASE),
        &["--capabilities".to_owned()],
        &mut io::empty(),
    );
    assert_eq!(none.reply, release(dir.path(), &["--capabilities"]));
    assert!(none.reply.stdout.is_empty() && !none.kill_parent);
    fs::write(at("capabilities"), "handover-v1\n").unwrap();
    let caps = run(
        &at(RELEASE),
        &["--capabilities".to_owned()],
        &mut io::empty(),
    );
    assert_eq!(caps.reply.stdout, "handover-v1\n");
    assert_eq!(
        run(&at(RELEASE), &["--version".to_owned()], &mut io::empty())
            .reply
            .code,
        2
    );
    let other = run(&at("other"), &[], &mut io::empty());
    assert_eq!(other.reply.code, 2);
    assert!(
        other
            .reply
            .stderr
            .contains("not a program this stands in for")
    );
    assert_eq!(run(Path::new("/"), &[], &mut io::empty()).reply.code, 2);
    let listing: Vec<String> = ["list", "table", "inet", "x"].map(str::to_owned).to_vec();
    assert_eq!(run(&at(NFT), &listing, &mut io::empty()).reply.code, 1);
    let stop: Vec<String> = ["stop", "--", "tierstore@9301.service"]
        .map(str::to_owned)
        .to_vec();
    assert_eq!(run(&at(SYSTEMCTL), &stop, &mut io::empty()).reply.code, 0);
    assert_eq!(
        World::new(path).lines("calls"),
        [
            "nft list table inet x",
            "systemctl stop -- tierstore@9301.service"
        ]
    );
}
