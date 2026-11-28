//! The volume of a Mac in an update of the release: `prepare` refuses a node of the two-process
//! layout (a mount provider, no `fs` interface) and stops nothing; a node switched to one process
//! hands over with its mount kept; and `verify` requires the mount of the `fs` interface listed
//! and answering, or fails and the update goes back.

use std::path::Path;

use super::fake::{
    CONFIG, FS_LABEL, FS_MOUNT, Fake, LABEL, NEW, NEW_VERSION, OLD, OLD_VERSION, Volume,
    assert_failed, assert_ok, in_flight, params_with, path,
};
use crate::server::update_hook::{
    Done, HANDOFF_EXEC_CAPABILITY, Host, Marker, Params, Reason, Record, Step,
};

use Step::{Cleanup, Cutover, Prepare, Restore, Rollback, Verify};

/// The release whose update failed on the farm Macs after their switch: its manifest named the fs
/// job, its `cutover` failed at the bootstrap of the disabled `tierstore-fs.plist` after the job
/// served it, and so did its `rollback`, after the job served `OLD` again.
const HALTED: &str = "/var/lib/hostkeeper/packages/tierstore/0.3.203+000000000203";
/// Its version.
const HALTED_VERSION: &str = "0.3.203+000000000203";

/// The params of the update to `NEW` as the manifests before this one wrote them: they name the fs
/// job.
fn labelled() -> Params {
    params_with(
        NEW,
        NEW_VERSION,
        Some((OLD, OLD_VERSION)),
        &format!("fs_launchd_label = \"{FS_LABEL}\"\n"),
    )
}

/// Whether any call so far named the fs job, its plist or its file.
fn touched_fs_job(fake: &Fake) -> bool {
    named_any(&fake.log(), &["tierstore-fs", "fs.toml"])
}

/// Whether any of `lines` holds one of `words`.
fn named_any(lines: &[String], words: &[&str]) -> bool {
    lines
        .iter()
        .any(|line| words.iter().any(|word| line.contains(word)))
}

/// A Mac's file of the two-process layout: a mount provider under the mount `tierstore fs`
/// served, the fill socket that process read through, and no `fs` interface.
const TWO_PROCESS: &str = "[server]\n\n\
    [[interface]]\ntype = \"mount-provider\"\n\
    params = { socket = \"/usr/local/tierstore/run/provider.sock\", mountpoint = \"/private/var/tierstore/fs\" }\n\n\
    [local_cache]\nfill_socket = \"/usr/local/tierstore/run/fill.sock\"\n";

/// The same Mac's file switched to one process: the `fs` interface on the provider's mount point,
/// and no fill socket.
const ONE_PROCESS: &str = "[server]\n\n\
    [[interface]]\ntype = \"fs\"\n\
    params = { mountpoint = \"/private/var/tierstore/fs\" }\n\n\
    [[interface]]\ntype = \"mount-provider\"\n\
    params = { socket = \"/usr/local/tierstore/run/provider.sock\", mountpoint = \"/private/var/tierstore/fs\" }\n";

/// The detail of the refusal of a node of the two-process layout.
fn two_process_refusal() -> String {
    format!(
        "{CONFIG} has a mount-provider interface and no fs interface: the two-process layout, \
         whose mount tierstore fs served as a job of its own. No release since 0.3.196 runs that \
         process, so after this update nothing would serve the provider's mount. Switch the node \
         to one process first (an [[interface]] with type = \"fs\" in its file, through its \
         config package); nothing was stopped"
    )
}

fn kill() -> String {
    format!("launchd.kill SIGUSR2 {LABEL}")
}

fn loaded() -> String {
    format!("launchd.loaded {LABEL}")
}

fn detail(done: &Done) -> String {
    done.report.detail.clone().unwrap_or_default()
}

/// A Mac switched to one process, on which both releases list `handoff-exec-v1`: the job runs
/// `OLD` and serves the volume at [`FS_MOUNT`].
fn switched() -> Fake {
    let fake = Fake::new();
    fake.set_config(CONFIG, Some(ONE_PROCESS));
    fake.set_capabilities(OLD, &[HANDOFF_EXEC_CAPABILITY]);
    fake.set_capabilities(NEW, &[HANDOFF_EXEC_CAPABILITY]);
    fake
}

/// Asserts that `prepare` of the release refuses a Mac whose file has a mount provider and no `fs`
/// interface with `mode_unsupported` and a detail that names the switch to one process: for an
/// update and a first install, whether the releases list `handoff-exec-v1` or not, whether the
/// old fs job is loaded or not, and whether the params name it or not. Nothing is recorded,
/// started or stopped: no launchd call, no release asked or made to check the file, the job and
/// the fs job as they were. hostkeeper's `rollback` after the refused `prepare` then finds
/// nothing to undo and calls launchd not at all. Catches an update that boots the fs job out
/// (unmounting) or in again (a binary that exits 2) and leaves the provider's mount served by no
/// process while the update reports success.
#[test]
fn a_release_update_of_a_two_process_mac_is_refused_and_stops_nothing() {
    for first in [false, true] {
        for capable in [false, true] {
            for (fs_loaded, fs_named) in [(false, false), (true, false), (true, true)] {
                let case = format!("first {first}, capable {capable}, fs job {fs_loaded}");
                let fake = if first {
                    Fake::first_install()
                } else {
                    Fake::new()
                };
                if fs_named {
                    fake.with_fs(fs_loaded);
                } else if fs_loaded {
                    fake.with_fs(true);
                    fake.name_fs_job(false);
                }
                fake.set_config(CONFIG, Some(TWO_PROCESS));
                if capable {
                    fake.set_capabilities(OLD, &[HANDOFF_EXEC_CAPABILITY]);
                    fake.set_capabilities(NEW, &[HANDOFF_EXEC_CAPABILITY]);
                }
                let before = fake.snapshot();
                let done = fake.run(Prepare);
                assert_failed(&done, Reason::ModeUnsupported);
                assert_eq!(detail(&done), two_process_refusal(), "{case}");
                assert!(done.report.reversible, "{case}");
                assert_eq!(fake.snapshot(), before, "{case}");
                assert!(fake.launchd_calls().is_empty(), "{case}");
                assert!(
                    !named_any(
                        &fake.log(),
                        &[
                            "host.capabilities",
                            "host.check_config",
                            "host.install_binary"
                        ]
                    ),
                    "{case}: {:?}",
                    fake.log()
                );
                assert_ok(&fake.host_run(Rollback));
                assert!(fake.launchd_calls().is_empty(), "{case}");
                assert_eq!(fake.snapshot(), before, "{case}");
            }
        }
    }
}

/// Asserts that the refusal is the update hook's alone: the config hook, which drives the same
/// arm for a package that carries no binary and whose switch is the way off the two-process
/// layout, prepares the same node and records its update. Catches a refusal that would also stop
/// the switch, leaving such a Mac with no way to one process.
#[test]
fn the_config_hook_prepares_a_two_process_mac() {
    let fake = Fake::new();
    fake.set_config(CONFIG, Some(TWO_PROCESS));
    assert_ok(&fake.run_for_config(Prepare));
    assert_eq!(fake.record(), in_flight(false));
}

/// Asserts that a file with a mount provider beside an `fs` interface (a Mac switched to one
/// process), and a file with neither, are not refused: `prepare` records the update. Catches a
/// refusal that keys on the provider alone.
#[test]
fn a_mac_with_an_fs_interface_or_no_provider_is_not_refused() {
    for text in [ONE_PROCESS, "[server]\n"] {
        let fake = switched();
        fake.set_config(CONFIG, Some(text));
        assert_ok(&fake.run(Prepare));
        assert_eq!(fake.record(), in_flight(false), "{text}");
    }
}

/// Asserts that an update of a Mac switched to one process, whose params name no fs job (the
/// package's manifest names none), is the hand-over: `prepare` has the staged release check the
/// file, `cutover` sends `SIGUSR2` with no bootout or bootstrap of either job and the volume stays
/// listed throughout, `verify` lists the mount at the fs interface's mount point and looks `trees`
/// up in it, then (the file has a mount provider) runs the mount check, before it records
/// `verified`, and `cleanup` commits. The fs job is never named.
/// Catches an update that restarts the job (which unmounts under the running jobs) and one that
/// verifies without looking at the volume.
#[test]
fn a_switched_mac_hands_over_and_verify_proves_the_volume() {
    let fake = switched();
    let volume = Path::new(FS_MOUNT);
    assert_eq!(fake.mounted(volume), Ok(true));
    fake.advance(&[Prepare]);
    assert!(
        fake.log()
            .contains(&format!("host.check_config {NEW} {CONFIG}"))
    );
    fake.advance(&[Cutover]);
    assert_eq!(fake.launchd_calls(), [loaded(), kill()]);
    assert_eq!(fake.snapshot().job, path(NEW));
    assert_eq!(fake.mounted(volume), Ok(true));
    let before = fake.log().len();
    fake.advance(&[Verify]);
    let looks: Vec<String> = fake.log()[before..]
        .iter()
        .filter(|line| line.starts_with("host."))
        .cloned()
        .collect();
    assert_eq!(
        looks,
        [
            format!("host.config {CONFIG}"),
            format!("host.mounted {FS_MOUNT}"),
            format!("host.is_dir {FS_MOUNT}/trees"),
            // Then the mount check reads the file for the provider it drives.
            format!("host.config {CONFIG}"),
        ]
    );
    assert!(!fake.provider.calls().is_empty());
    assert!(fake.record().has(Marker::Verified));
    fake.advance(&[Cleanup]);
    assert!(fake.record().committed());
    assert!(!named_any(&fake.launchd_calls(), &[FS_LABEL, "bootout"]));
    assert!(fake.strays().is_empty());
}

/// Asserts that `verify` of a Mac whose file has an `fs` interface fails with `not_ready` and
/// records nothing when the volume is not served: nothing is mounted at the mount point, a mount
/// of another file system is there (no `trees` in it), or the mount's server is gone (the look-up
/// fails). Each detail is exact. The report stays reversible, and hostkeeper's `rollback` hands
/// the job back to the previous release by `SIGUSR2` and leaves the record at rest. Catches an
/// update reported verified while nothing serves the provider's mount, and a look-up failure
/// taken for a served mount.
#[test]
fn verify_fails_and_the_update_goes_back_when_the_volume_is_not_served() {
    let not_dir = format!(
        "{FS_MOUNT} is mounted and {FS_MOUNT}/trees is not a directory: the mount is not \
         tierstore's file system"
    );
    let cases = [
        (
            Volume::NotMounted,
            format!(
                "{FS_MOUNT} is not mounted: nothing serves the volume of the fs interface, under \
                 which the mount provider answers"
            ),
        ),
        (Volume::Foreign, not_dir),
        (
            Volume::Dead,
            format!(
                "{FS_MOUNT} is mounted and a look-up in it failed (host: stat {FS_MOUNT}/trees: \
                 Input/output error (os error 5)): its server does not answer"
            ),
        ),
    ];
    for (volume, want) in cases {
        let fake = switched();
        fake.advance(&[Prepare, Cutover]);
        fake.set_volume(volume);
        let done = fake.host_run(Verify);
        assert_failed(&done, Reason::NotReady);
        assert_eq!(detail(&done), want, "{volume:?}");
        assert!(done.report.reversible, "{volume:?}");
        assert!(!fake.record().has(Marker::Verified), "{volume:?}");
        let before = fake.launchd_calls().len();
        assert_ok(&fake.host_run(Rollback));
        assert_eq!(fake.launchd_calls()[before..], [loaded(), kill()]);
        assert_eq!(fake.snapshot().job, path(OLD));
        assert_eq!(fake.record(), Record::idle(None));
    }
}

/// Asserts what `verify` does not require: a node whose file has no `fs` interface is verified
/// with no look at a mount; the config hook's drive of the arm, after its own update, does not
/// look at the volume (its switch checks the mount itself); and a `verify` after the commit only
/// observes the version. Asserts too that a file gone before `verify` fails it with
/// `config_missing`. Catches a proof of a volume a node does not have, and a verify that passes
/// without knowing which volume to look for.
#[test]
fn verify_looks_for_a_volume_only_where_the_release_update_has_one() {
    let plain = Fake::new();
    plain.advance(&[Prepare, Cutover, Verify]);
    assert!(!named_any(&plain.log(), &["host.mounted", "host.is_dir"]));

    let config = switched();
    for step in [Prepare, Cutover] {
        assert_ok(&config.run_for_config(step));
    }
    config.set_volume(Volume::NotMounted);
    assert_ok(&config.run_for_config(Verify));

    let committed = switched();
    committed.advance(&[Prepare, Cutover, Verify, Cleanup]);
    committed.set_volume(Volume::NotMounted);
    assert_ok(&committed.host_run(Verify));

    let gone = switched();
    gone.advance(&[Prepare, Cutover]);
    gone.set_config(CONFIG, None);
    let done = gone.host_run(Verify);
    assert_failed(&done, Reason::ConfigMissing);
    assert_eq!(
        detail(&done),
        format!("there is no configuration file at {CONFIG}: the volume to look for is not known")
    );
}

/// Asserts that a failure to read the node's file, the mount list or the look-up in the mount
/// during `verify` fails the step (`host_failed` for the file and the list, `not_ready` for the
/// look-up) without `verified`, reversible, and that `verify` run again once the host answers
/// records `verified`. Catches a verify that takes a host it could not ask for a served volume.
#[test]
fn verify_that_cannot_look_at_the_volume_fails_and_succeeds_once_it_can() {
    for (call, reason) in [
        ("host.config", Reason::HostFailed),
        ("host.mounted", Reason::HostFailed),
        ("host.is_dir", Reason::NotReady),
    ] {
        let fake = switched();
        fake.advance(&[Prepare, Cutover]);
        fake.fail_nth(call, 1);
        let done = fake.host_run(Verify);
        assert_failed(&done, reason);
        assert!(done.report.reversible, "{call}");
        assert!(!fake.record().has(Marker::Verified), "{call}");
        fake.heal();
        assert_ok(&fake.host_run(Verify));
        assert!(fake.record().has(Marker::Verified), "{call}");
    }
}

/// Asserts the update of a Mac in the state the switch to one process leaves (its file has the
/// `fs` interface, `tierstore-fs` is not loaded and is disabled), with the params of this
/// release's manifest and with params that name the fs job: `prepare`, `cutover` (the look and
/// `SIGUSR2` alone), `verify` and `cleanup` succeed; a `rollback` after a failed `verify` hands
/// back by `SIGUSR2`; and the boot step after a reboot loads the job alone. No call names the fs
/// job, its plist or `fs.toml`; the fs job stays disabled and unloaded. Catches the update that
/// failed on every switched Mac: a hook that bootstraps the disabled `tierstore-fs.plist`
/// (`Bootstrap failed: 5`) after the job served, in `cutover`, `rollback` or the boot step.
#[test]
fn a_switched_mac_is_updated_without_touching_its_disabled_fs_job() {
    for named in [false, true] {
        let params = if named {
            labelled()
        } else {
            switched().params()
        };
        let fake = switched();
        fake.set_disabled(FS_LABEL);
        for step in [Prepare, Cutover, Verify, Cleanup] {
            assert_ok(&fake.run_with(step, &params, LABEL));
        }
        assert_eq!(
            fake.launchd_calls(),
            [loaded(), kill(), loaded(), loaded()],
            "named {named}"
        );
        assert!(fake.record().committed(), "named {named}");
        fake.reboot();
        assert_ok(&fake.run_with(Restore, &params, LABEL));
        assert_eq!(fake.snapshot().job, path(NEW), "named {named}");
        assert_eq!(fake.mounted(Path::new(FS_MOUNT)), Ok(true));

        let back = switched();
        back.set_disabled(FS_LABEL);
        for step in [Prepare, Cutover] {
            assert_ok(&back.run_with(step, &params, LABEL));
        }
        back.set_volume(Volume::NotMounted);
        assert_failed(&back.run_with(Verify, &params, LABEL), Reason::NotReady);
        back.set_volume(Volume::Served);
        assert_ok(&back.run_with(Rollback, &params, LABEL));
        assert_eq!(back.snapshot().job, path(OLD), "named {named}");

        for host in [&fake, &back] {
            assert!(!touched_fs_job(host), "named {named}: {:?}", host.log());
            assert_eq!(host.disabled(), [FS_LABEL]);
            assert!(!host.snapshot().fs_job);
            assert!(host.strays().is_empty());
        }
    }
}

/// Asserts what the next update does on a switched Mac whose earlier update was left in flight by
/// a `cutover` and a `rollback` that both failed at the bootstrap of the fs job: the record names
/// the update to `HALTED` from `OLD`, with no marker (the step failed before `swapped`) or with
/// `swapped`, and the job serves `OLD`. The update to `NEW` from `OLD` prepares over the record
/// with no marker, and rolls the one with `swapped` back first with its own target (the job
/// already serves `OLD`: nothing is stopped); then it hands over, verifies and commits, and no
/// call names the fs job. Catches a halted record that refuses every later release, and a
/// rollback of it that touches the job the switch disabled.
#[test]
fn the_update_after_a_halted_one_goes_over_its_record() {
    for swapped in [false, true] {
        let fake = switched();
        fake.set_disabled(FS_LABEL);
        let mut halted = Record {
            updating: true,
            from: path(OLD),
            to: path(HALTED),
            version: Some(HALTED_VERSION.to_owned()),
            ..Record::idle(None)
        };
        if swapped {
            halted = halted.with(Marker::Swapped);
        }
        fake.set_record(&halted);
        let prepared = fake.run(Prepare);
        assert_ok(&prepared);
        assert_eq!(fake.record(), in_flight(false), "swapped {swapped}");
        assert_eq!(
            detail(&prepared).starts_with("the record is of the update to "),
            swapped,
            "{prepared:?}"
        );
        fake.advance(&[Cutover, Verify, Cleanup]);
        assert!(fake.record().committed());
        assert_eq!(fake.snapshot().job, path(NEW));
        let calls = fake.launchd_calls();
        assert!(calls.contains(&kill()), "{calls:?}");
        assert!(
            !calls.iter().any(|call| call.contains("bootout")),
            "{calls:?}"
        );
        assert!(!touched_fs_job(&fake), "{:?}", fake.log());
        assert_eq!(fake.disabled(), [FS_LABEL]);
    }
}

/// Asserts that the boot step of a release whose params name the fs job leaves it alone on a Mac
/// whose file has an `fs` interface (the job loaded already, as launchd loads it at boot, or not
/// loaded), and still loads it on a Mac whose file has none, as before. Catches the boot step
/// that failed on the switched Macs (`boot_step_failed`, `Bootstrap failed: 5`) and a guard that
/// would also stop the fs job of a node that still needs it.
#[test]
fn the_boot_step_drives_the_fs_job_only_where_the_file_has_no_fs_interface() {
    for loaded_at_boot in [false, true] {
        let fake = switched();
        fake.set_disabled(FS_LABEL);
        if !loaded_at_boot {
            fake.reboot();
        }
        assert_ok(&fake.run_with(Restore, &labelled(), LABEL));
        assert!(!touched_fs_job(&fake), "{:?}", fake.log());
        assert!(
            fake.snapshot().job.is_some(),
            "loaded at boot {loaded_at_boot}"
        );
    }
    let two = Fake::new();
    two.with_fs(false);
    assert_ok(&two.run(Restore));
    assert!(two.snapshot().fs_job);
}

/// Asserts that params that name the fs job and no `config_file` (a boot step's params carry the
/// manifest's, which name one; these do not) leave the fs job driven as before: with no file to
/// read, the node is not taken for one that serves its own mount, so `restore` boots the fs job
/// in. Catches a guard that drops the fs job of a node whose file it never read.
#[test]
fn params_with_no_config_file_still_drive_the_fs_job() {
    let fake = Fake::new();
    fake.with_fs(false);
    let params = Params::parse(&format!(
        "service_manager = \"launchd\"\n\
         launchd_label = \"{LABEL}\"\n\
         fs_launchd_label = \"{FS_LABEL}\"\n\
         \"updater.staged_release\" = \"{OLD}\"\n\
         \"updater.target_version\" = \"{OLD_VERSION}\"\n"
    ))
    .expect("params");
    assert_ok(&fake.run_with(Restore, &params, LABEL));
    assert!(fake.snapshot().fs_job);
    assert!(
        !fake
            .log()
            .iter()
            .any(|line| line.starts_with("host.config"))
    );
}

/// Asserts the fake's look-up at its edges: a path whose directory is not a listed mount finds
/// nothing, and a gone server fails only the look-ups in the main job's mount, not in the fs
/// job's. Catches a fake that answers a look-up off any mount, which would let a verify pass with
/// nothing mounted.
#[test]
fn the_fakes_look_up_answers_only_within_a_listed_mount() {
    let fake = Fake::new();
    assert_eq!(fake.is_dir(Path::new("/elsewhere/trees")), Ok(false));
    fake.with_fs(true);
    fake.set_volume(Volume::Dead);
    let trees = Path::new(FS_MOUNT).join("trees");
    assert_eq!(fake.is_dir(&trees), Ok(true));
}
