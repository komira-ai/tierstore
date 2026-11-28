//! The steps over the machine's ports, built from the params: which arm runs, which programs it
//! calls, and where it looks for the staged release.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::look::POLL_INTERVAL;
use super::machine::testing::me;
use super::machine::{Call, Output, RunError, Runner, SystemRunner};
use super::system::{
    BOOTOUT_TIMEOUT, LAUNCHCTL_PROGRAM, SYSTEMCTL_PROGRAM, SystemConfig, on_this_machine, run_on,
    system_config,
};
use super::{
    Done, Invocation, LAUNCHD_PLIST, Outcome, Params, Reason, SERVICE_BINARY, STATE_DIR, Step,
};

const STAGED: &str = "/packages/tierstore/versions/0.5.0+0123456789ab";
const LABEL: &str = "com.example.tierstore";
const ALL_STEPS: [Step; 7] = [
    Step::Prepare,
    Step::Cutover,
    Step::Verify,
    Step::Rollback,
    Step::Cleanup,
    Step::Uninstall,
    Step::Restore,
];

/// Answers every call by the program: a release's binary prints no capability, `systemctl show`
/// says inactive, `launchctl print` does not know the job, anything else succeeds. Keeps the
/// calls.
#[derive(Default)]
struct ByProgram {
    calls: RefCell<Vec<Call>>,
}

impl ByProgram {
    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    fn programs(&self) -> Vec<String> {
        self.calls().into_iter().map(|call| call.program).collect()
    }
}

impl Runner for ByProgram {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        self.calls.borrow_mut().push(call.clone());
        // Any `systemctl` answers `show` as one, whichever path the configuration names.
        let systemctl = call.program.ends_with("/systemctl");
        let (code, stdout) = match (call.program.as_str(), call.args.first().map(String::as_str)) {
            (_, Some("show")) if systemctl => (0, "inactive\n"),
            ("/bin/launchctl", Some("print")) => (113, ""),
            _ => (0, ""),
        };
        Ok(Output {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }
}

/// A host under a scratch root.
struct Host {
    _dir: TestDir,
    root: PathBuf,
}

fn new_host(name: &str) -> Host {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let root = dir.path().to_path_buf();
    Host { _dir: dir, root }
}

impl Host {
    fn staged(&self) -> PathBuf {
        self.root.join(STAGED.trim_start_matches('/'))
    }

    /// A staged release with `bin/tierstore`.
    fn release(&self) {
        let bin = self.staged().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let binary = bin.join("tierstore");
        fs::write(&binary, "#!").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn config<'a>(&self, runner: &'a dyn Runner) -> SystemConfig<'a> {
        SystemConfig {
            root: self.root.clone(),
            owner: me(),
            release_owner: me(),
            hook_uid: me(),
            systemctl: SYSTEMCTL_PROGRAM,
            launchctl: LAUNCHCTL_PROGRAM,
            poll: Duration::from_millis(1),
            runner,
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR.trim_start_matches('/'))
    }
}

fn params(extra: &str) -> Params {
    Params::parse(&format!(
        "\"updater.staged_release\" = \"{STAGED}\"\n\"updater.target_version\" = \"0.5.0+0123456789ab\"\n{extra}"
    ))
    .unwrap()
}

fn invocation(step: Step) -> Invocation {
    Invocation {
        step,
        artifact: None,
        params: PathBuf::from("/params.toml"),
    }
}

fn run(host: &Host, runner: &dyn Runner, step: Step, params: &Params) -> Done {
    run_on(&host.config(runner), &invocation(step), params)
}

/// Asserts that a `service_manager` that is neither systemd nor launchd is refused with
/// `service_manager_unknown` by every step, `prepare` included, that no program runs and that
/// the state directory is not made.
#[test]
fn an_unknown_service_manager_is_refused_by_every_step_before_anything_is_touched() {
    let host = new_host("system-unknown");
    host.release();
    let runner = ByProgram::default();
    let params = params("service_manager = \"foo\"\n");
    for step in ALL_STEPS {
        let done = run(&host, &runner, step, &params);
        assert_eq!(done.outcome, Outcome::Failure, "{step}: {done:?}");
        assert_eq!(
            done.report.reason,
            Some(Reason::ServiceManagerUnknown),
            "{step}"
        );
        assert!(done.report.detail.unwrap().contains("\"foo\""));
    }
    assert!(runner.calls().is_empty());
    assert!(!host.state_dir().exists());
}

/// Asserts that a params file with no `service_manager` runs the systemd arm: a first-install
/// `prepare` on a root that has no `/etc` at all (so no declaration file anywhere) succeeds, asks
/// the staged binary for its capabilities and `systemctl` for the state of the units, calls no
/// `launchctl`, and leaves its record in the state directory under the root.
#[test]
fn an_absent_service_manager_is_systemd_and_prepare_needs_no_declaration_file() {
    let host = new_host("system-systemd");
    host.release();
    assert!(!host.root.join("etc").exists());
    let runner = ByProgram::default();
    let done = run(&host, &runner, Step::Prepare, &params(""));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let programs = runner.programs();
    assert!(
        programs.iter().any(|p| p == "/usr/bin/systemctl"),
        "{programs:?}"
    );
    assert!(
        programs.iter().all(|p| p != "/bin/launchctl"),
        "{programs:?}"
    );
    assert!(host.state_dir().join("state.json").is_file());
    assert!(!host.root.join("etc").exists());
}

/// Asserts that `service_manager = "systemd"` runs the same arm as an absent key, and that the
/// `systemctl` the configuration names is the program called.
#[test]
fn the_systemctl_of_the_configuration_is_the_program_called() {
    let host = new_host("system-systemctl");
    host.release();
    let runner = ByProgram::default();
    let config = SystemConfig {
        systemctl: "/opt/fake/systemctl",
        ..host.config(&runner)
    };
    let params = params("service_manager = \"systemd\"\n");
    let done = run_on(&config, &invocation(Step::Prepare), &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(runner.programs().iter().any(|p| p == "/opt/fake/systemctl"));
}

/// Asserts that launchd params run the launchd arm: a `restore` of a job the domain does not
/// have asks `launchctl` about `system/<label from the params>`, puts the staged release's binary
/// at `SERVICE_BINARY` under the root and bootstraps the plist, every call goes to `launchctl`
/// and names that label or the plist, and `systemctl` is never called.
#[test]
fn launchd_params_issue_launchctl_calls_for_their_label_only() {
    let host = new_host("system-launchd");
    host.release();
    let runner = ByProgram::default();
    let params = params(&format!(
        "service_manager = \"launchd\"\nlaunchd_label = \"{LABEL}\"\nconfig_file = \"/etc/tierstore.toml\"\n"
    ));
    let done = run(&host, &runner, Step::Restore, &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let calls = runner.calls();
    assert!(!calls.is_empty());
    for call in &calls {
        assert_eq!(call.program, "/bin/launchctl", "{call:?}");
    }
    let target = format!("system/{LABEL}");
    assert_eq!(calls[0].args, ["print", target.as_str()]);
    assert!(
        calls
            .iter()
            .any(|call| call.args == ["bootstrap", "system", LAUNCHD_PLIST])
    );
    let copy = host.root.join(SERVICE_BINARY.trim_start_matches('/'));
    assert_eq!(fs::read_to_string(copy).unwrap(), "#!");
}

/// Asserts that the `launchctl` the configuration names is the program called by the launchd
/// arm.
#[test]
fn the_launchctl_of_the_configuration_is_the_program_called() {
    let host = new_host("system-launchctl");
    let runner = ByProgram::default();
    let config = SystemConfig {
        launchctl: "/opt/fake/launchctl",
        ..host.config(&runner)
    };
    let params = params(&format!(
        "service_manager = \"launchd\"\nlaunchd_label = \"{LABEL}\"\n"
    ));
    let done = run_on(&config, &invocation(Step::Restore), &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(runner.programs().iter().all(|p| p == "/opt/fake/launchctl"));
}

/// Asserts that the staged binary is resolved under `bin/`: the capabilities call runs
/// `<staged>/bin/tierstore --capabilities` (under the root), and a release whose binary is at its
/// top only is refused as `host_failed` with no call to it.
#[test]
fn the_staged_binary_is_resolved_under_bin() {
    let host = new_host("system-bin");
    host.release();
    let runner = ByProgram::default();
    let done = run(&host, &runner, Step::Prepare, &params(""));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let binary = host.staged().join("bin/tierstore");
    let asked: Vec<Call> = runner
        .calls()
        .into_iter()
        .filter(|call| call.program == binary.to_str().unwrap())
        .collect();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].args, ["--capabilities"]);
    assert_eq!(asked[0].timeout, super::system::CAPABILITIES_TIMEOUT);

    let top_only = new_host("system-top");
    fs::create_dir_all(top_only.staged()).unwrap();
    fs::write(top_only.staged().join("tierstore"), "#!").unwrap();
    let runner = ByProgram::default();
    let done = run(&top_only, &runner, Step::Prepare, &params(""));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::HostFailed), "{done:?}");
    only_the_looks_at_the_old_layout(&runner.calls());
}

/// Asserts that `calls` are the two looks `prepare` takes at the units of the old layout before it
/// asks any release (`systemctl show` of `tierstore-fs@a` and `@b`), and nothing else: no release's
/// binary is run.
fn only_the_looks_at_the_old_layout(calls: &[Call]) {
    let looked: Vec<(&str, Vec<&str>)> = calls
        .iter()
        .map(|call| {
            (
                call.program.as_str(),
                call.args.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let show = |unit| {
        (
            "/usr/bin/systemctl",
            vec!["show", "--property=ActiveState", "--value", "--", unit],
        )
    };
    assert_eq!(
        looked,
        [
            show("tierstore-fs@a.service"),
            show("tierstore-fs@b.service")
        ]
    );
}

/// Asserts that a staged binary of another owner than the configured release owner is refused as
/// `host_failed` and not run.
#[test]
fn a_staged_binary_of_another_owner_is_refused() {
    let host = new_host("system-owner");
    host.release();
    let runner = ByProgram::default();
    let config = SystemConfig {
        release_owner: me() + 1,
        ..host.config(&runner)
    };
    let done = run_on(&config, &invocation(Step::Prepare), &params(""));
    assert_eq!(done.report.reason, Some(Reason::HostFailed), "{done:?}");
    only_the_looks_at_the_old_layout(&runner.calls());
}

/// Asserts that a state directory owned by a user other than the configured owner is refused
/// with the state port's reason, and that no program runs.
#[test]
fn a_state_directory_of_another_owner_is_refused() {
    let host = new_host("system-state-owner");
    host.release();
    let runner = ByProgram::default();
    let config = SystemConfig {
        owner: me() + 1,
        ..host.config(&runner)
    };
    let done = run_on(&config, &invocation(Step::Prepare), &params(""));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateIo), "{done:?}");
    assert!(done.report.detail.unwrap().contains("owned by user"));
    assert!(runner.calls().is_empty());
}

/// Asserts that a second step that finds the lock of the state directory held is `locked`, and
/// runs no program.
#[test]
fn a_held_lock_is_locked() {
    use super::StateDir;
    use super::machine::StateFiles;

    let host = new_host("system-locked");
    host.release();
    let held = StateFiles::new(host.state_dir(), me());
    let _lock = held.lock().unwrap();
    let runner = ByProgram::default();
    let done = run(&host, &runner, Step::Prepare, &params(""));
    assert_eq!(done.report.reason, Some(Reason::Locked), "{done:?}");
    assert!(runner.calls().is_empty());
}

/// Asserts the configuration of this machine: files under `/`, the state directory owned by the
/// user the hook runs as, a staged release owned by root, the host's own programs and the
/// looks' own interval.
#[test]
fn the_system_configuration_is_the_hosts_own() {
    let config = system_config(&SystemRunner);
    assert_eq!(config.root, Path::new("/"));
    assert_eq!(config.owner, rustix::process::geteuid().as_raw());
    assert_eq!(config.release_owner, 0);
    assert_eq!(config.systemctl, "/usr/bin/systemctl");
    assert_eq!(config.launchctl, "/bin/launchctl");
    assert_eq!(config.poll, POLL_INTERVAL);
}

/// Asserts that the verb's machine refuses an unknown service manager without touching the
/// host: the step of `on_this_machine` answers `service_manager_unknown`.
#[test]
fn the_machine_of_the_verb_refuses_an_unknown_service_manager() {
    let params = params("service_manager = \"foo\"\n");
    let done = on_this_machine(&invocation(Step::Prepare), &params);
    assert_eq!(
        done.report.reason,
        Some(Reason::ServiceManagerUnknown),
        "{done:?}"
    );
    let machine = super::Machine::system();
    let done = (machine.step)(&invocation(Step::Restore), &params);
    assert_eq!(
        done.report.reason,
        Some(Reason::ServiceManagerUnknown),
        "{done:?}"
    );
}

/// Asserts that the systemd arm of a hook on this machine looks at the units of `tierstore-fs`
/// through `systemctl show` and at the markers of `run-fs-<letter>` in the markers' directory: a
/// `prepare` on a host with the marker `run-fs-b` and both units inactive is refused with
/// `mode_unsupported`, naming the marker, with no `systemctl` call that starts or stops anything
/// and the marker left where it was. Catches a hook whose fs ports were left out of its wiring,
/// and one that changes what it only looks at.
#[test]
fn the_systemd_arm_is_wired_to_the_fs_units_and_markers_for_looking_only() {
    let host = new_host("system-fs-wiring");
    let markers = host.root.join("var/lib/tierstore");
    fs::create_dir_all(&markers).unwrap();
    fs::write(markers.join("run-fs-b"), "").unwrap();
    let runner = ByProgram::default();
    let done = run(&host, &runner, Step::Prepare, &params(""));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::ModeUnsupported));
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            "this node still runs the two-process layout (the marker run-fs-b is present): \
             switch this node to the fs interface with release M first"
        )
    );
    let calls = runner.calls();
    for unit in ["tierstore-fs@a.service", "tierstore-fs@b.service"] {
        assert!(
            calls
                .iter()
                .any(|call| call.args.first().map(String::as_str) == Some("show")
                    && call.args.last().map(String::as_str) == Some(unit)),
            "{unit}: {calls:?}"
        );
    }
    assert!(
        calls
            .iter()
            .all(|call| call.args.first().map(String::as_str) == Some("show")),
        "{calls:?}"
    );
    assert!(markers.join("run-fs-b").exists(), "the fs marker stays");
}

/// The `ExitTimeOut` of the launchd job at `path` from the repository's root.
fn exit_timeout_of(path: &str) -> Duration {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    let text = fs::read_to_string(&file).unwrap();
    let (_, after) = text.split_once("<key>ExitTimeOut</key>").unwrap();
    let (_, value) = after.split_once("<integer>").unwrap();
    let (number, _) = value.split_once("</integer>").unwrap();
    Duration::from_secs(number.trim().parse().unwrap())
}

/// Asserts that the hook waits for `launchctl bootout` of the job longer than launchd lets the
/// job's process take to exit (`ExitTimeOut` of each plist the job is loaded from, the archive
/// package's and the installer package's): the wait is above its plists' 210 s. No package ships
/// the plist of the fs job any more. Catches a wait left at the 120 s of the 90 s plist, with which
/// the hook gives up on a bootout while launchd still lets the process drain and unmount, and
/// fails the step of an update that was going well.
#[test]
fn the_hook_waits_for_each_bootout_longer_than_its_plists_let_the_job_exit() {
    for dir in ["packaging/hostkeeper/units", "packaging/macos-pkg"] {
        let exit = exit_timeout_of(&format!("{dir}/tierstore.plist"));
        assert!(
            BOOTOUT_TIMEOUT > exit,
            "{dir}/tierstore.plist: {BOOTOUT_TIMEOUT:?} is not above {exit:?}"
        );
    }
    assert_eq!(
        exit_timeout_of("packaging/macos-pkg/tierstore.plist"),
        Duration::from_secs(210)
    );
}
