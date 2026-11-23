//! A first install over a copy installed by hand, run as hostkeeper runs it: the verb
//! `update-hook <step> --params <file>` reads the command line and a params file on disk, and the
//! machine runs the launchd arm's own steps, with the label the params name, over the fake ports.
//! What hostkeeper reads (the exit status and the last line) and what the host is then (the jobs,
//! the mount, the record) are asserted together.

use std::ffi::OsString;
use std::process::ExitCode;

use tierstore_testkit::dir::TestDir;

use super::fake::{
    CONFIG, FS_LABEL, FS_MOUNT, Fake, LABEL, NEW, NEW_VERSION, OLD, in_flight, path,
};
use crate::server::update_hook::launchd::run_step;
use crate::server::update_hook::{
    Done, Invocation, Machine, Marker, Params, Record, ServiceManager, run,
};

/// The params file hostkeeper writes for the first install of `NEW`, naming the fs job when `fs`
/// says so.
fn params_text(fs: bool) -> String {
    let fs_line = if fs {
        format!("fs_launchd_label = \"{FS_LABEL}\"\n")
    } else {
        String::new()
    };
    format!(
        "service_manager = \"launchd\"\n\
         launchd_label = \"{LABEL}\"\n\
         {fs_line}\
         config_file = \"{CONFIG}\"\n\
         \"updater.staged_release\" = \"{NEW}\"\n\
         \"updater.target_version\" = \"{NEW_VERSION}\"\n"
    )
}

/// What one run of the verb gave hostkeeper.
struct Ran {
    status: String,
    line: serde_json::Value,
    err: String,
}

/// A host whose first install is run through the verb, with its params file on disk.
struct Host {
    fake: Fake,
    /// The params name the fs job.
    fs: bool,
    _dir: TestDir,
    params: String,
}

impl Host {
    fn new(name: &str, fs: bool) -> Host {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let file = dir.path().join("params.toml");
        std::fs::write(&file, params_text(fs)).expect("the params file is written");
        let params = file.to_str().expect("a UTF-8 path").to_owned();
        let fake = Fake::first_install();
        if fs {
            fake.with_fs(false);
        }
        Host {
            fake,
            fs,
            _dir: dir,
            params,
        }
    }

    /// Runs `update-hook <words> --params <file>` with the launchd arm over the fake, after a
    /// check that the params read from the file name the launchd job (and the fs job) whose label
    /// the step is given, as the machine takes it from them.
    fn ran(&self, words: &[&str]) -> Ran {
        let step = |invocation: &Invocation, params: &Params| -> Done {
            assert_eq!(
                params.service_manager,
                ServiceManager::Launchd {
                    label: LABEL.to_owned(),
                    fs_label: self.fs.then(|| FS_LABEL.to_owned()),
                }
            );
            run_step(invocation.step, params, LABEL, &self.fake.ports())
        };
        let machine = Machine { step: &step };
        let mut args: Vec<OsString> = ["tierstore", "update-hook"]
            .iter()
            .chain(words)
            .map(OsString::from)
            .collect();
        args.extend([OsString::from("--params"), OsString::from(&self.params)]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = run(&args, &mut out, &mut err, &machine);
        let out = String::from_utf8(out).expect("UTF-8");
        assert_eq!(out.lines().count(), 1, "{out}");
        Ran {
            status: format!("{status:?}"),
            line: serde_json::from_str(out.trim_end()).expect("one JSON object"),
            err: String::from_utf8(err).expect("UTF-8"),
        }
    }

    /// Runs `words` and asserts that it exits 0 with a report that names no reason.
    fn ok(&self, words: &[&str]) -> Ran {
        let ran = self.ran(words);
        assert_eq!(ran.status, status(0), "{words:?}: {}", ran.line);
        assert!(ran.line.get("reason").is_none(), "{words:?}: {}", ran.line);
        assert_eq!(ran.err, "");
        ran
    }
}

fn status(code: u8) -> String {
    format!("{:?}", ExitCode::from(code))
}

/// The record of a finished first install of `NEW`.
fn installed() -> Record {
    Record {
        to: path(NEW),
        version: Some(NEW_VERSION.to_owned()),
        markers: [Marker::Swapped, Marker::Verified, Marker::Stopping].into(),
        ..Record::idle(None)
    }
}

/// Asserts that the first install of a host whose label a copy installed by hand keeps loaded
/// with nothing running (a stopped copy), run step by step through the verb with the params file
/// hostkeeper writes, exits 0 at `prepare`, `cutover`, `verify` and `cleanup`; that `cutover`
/// reports `<label> serves <version>`, reversible, and leaves the job on the staged release and
/// the record with `swapped`; and that `cleanup` reports irreversible and leaves the record of a
/// finished install. Catches a verb whose cutover fails on a loaded copy and records a failure on
/// a host that then runs the release.
#[test]
fn the_verb_installs_over_a_stopped_loaded_copy() {
    let host = Host::new("tierstore-hook-takeover-stopped", false);
    host.fake.set_job_idle();
    host.ok(&["prepare", NEW]);
    assert_eq!(host.fake.record(), in_flight(true));
    let cutover = host.ok(&["cutover"]);
    assert_eq!(
        cutover.line["detail"],
        format!("{LABEL} serves {NEW_VERSION}")
    );
    assert_eq!(cutover.line["reversible"], true);
    assert_eq!(host.fake.snapshot().job, path(NEW));
    assert_eq!(host.fake.record(), in_flight(true).with(Marker::Swapped));
    host.ok(&["verify"]);
    let cleanup = host.ok(&["cleanup"]);
    assert_eq!(cleanup.line["reversible"], false);
    assert_eq!(host.fake.record(), installed());
}

/// Asserts that the first install of a host where a copy installed by hand runs the old version
/// with its fs job loaded and its mount up, run through the verb, exits 0 at every step and ends
/// with the staged release serving, the fs job loaded, the mount listed and the record of a
/// finished install, the fs job having been booted out (and the mount gone) before the main job
/// was. Catches a verb that fails the takeover, or that stops the main job beside a mount.
#[test]
fn the_verb_installs_over_a_running_copy_with_its_mount_up() {
    let host = Host::new("tierstore-hook-takeover-running", true);
    host.fake.with_fs(true);
    host.fake.set_job(Some(OLD));
    host.ok(&["prepare", NEW]);
    host.ok(&["cutover"]);
    let calls = host.fake.launchd_calls();
    let fs_out = calls
        .iter()
        .position(|call| *call == format!("launchd.bootout {FS_LABEL}"));
    let main_out = calls
        .iter()
        .position(|call| *call == format!("launchd.bootout {LABEL}"));
    assert!(
        matches!((fs_out, main_out), (Some(fs), Some(main)) if fs < main),
        "{calls:?}"
    );
    host.ok(&["verify"]);
    host.ok(&["cleanup"]);
    let end = host.fake.snapshot();
    assert_eq!((end.job, end.fs_job, end.mounted), (path(NEW), true, true));
    assert_eq!(host.fake.record(), installed());
}

/// Asserts that a takeover whose mount stays after its fs job is booted out exits 1 at `cutover`
/// and at the `rollback` hostkeeper runs next, both with `not_ready`, reversible, and the detail
/// naming the mount; the copy serves on with its fs job and mount, and the record stays the update
/// in flight. And that a takeover whose staged release never answers exits 1 at `cutover`
/// (`not_ready`) and at `rollback` with `no_previous_release` and the detail `nothing to go back
/// to: <label> is stopped`, which is what the host is: no job, no fs job, no mount. Catches a
/// recorded outcome that is not the host's state.
#[test]
fn the_verb_reports_a_failed_takeover_as_the_host_is() {
    let held = Host::new("tierstore-hook-takeover-held", true);
    held.fake.with_fs(true);
    held.fake.set_job(Some(OLD));
    held.fake.keep_mount(true);
    held.ok(&["prepare", NEW]);
    let mounted = format!(
        "{FS_MOUNT} is still mounted after {FS_LABEL} stopped: {LABEL} is left as it is, and \
         {FS_LABEL} is loaded again"
    );
    for step in ["cutover", "rollback"] {
        let ran = held.ran(&[step]);
        assert_eq!(ran.status, status(1), "{step}");
        assert_eq!(ran.line["reason"], "not_ready", "{step}");
        assert_eq!(ran.line["reversible"], true, "{step}");
        assert_eq!(ran.line["detail"], mounted.as_str(), "{step}");
        let end = held.fake.snapshot();
        assert_eq!(
            (end.job, end.fs_job, end.mounted),
            (path(OLD), true, true),
            "{step}"
        );
        assert_eq!(held.fake.record(), in_flight(true), "{step}");
    }

    let mute = Host::new("tierstore-hook-takeover-mute", true);
    mute.fake.with_fs(true);
    mute.fake.set_job(Some(OLD));
    mute.fake.mute(NEW);
    mute.ok(&["prepare", NEW]);
    let cutover = mute.ran(&["cutover"]);
    assert_eq!(cutover.status, status(1));
    assert_eq!(cutover.line["reason"], "not_ready");
    let rollback = mute.ran(&["rollback"]);
    assert_eq!(rollback.status, status(1));
    assert_eq!(rollback.line["reason"], "no_previous_release");
    assert_eq!(
        rollback.line["detail"],
        format!("nothing to go back to: {LABEL} is stopped")
    );
    let end = mute.fake.snapshot();
    assert_eq!((end.job, end.fs_job, end.mounted), (None, false, false));
    assert_eq!(mute.fake.record(), in_flight(true));
}
