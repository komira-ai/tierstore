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
    LAUNCHCTL_PROGRAM, SYSTEMCTL_PROGRAM, SystemConfig, on_this_machine, run_on, system_config,
};
use super::{Done, Invocation, LAUNCHD_PLIST, Outcome, Params, Reason, STATE_DIR, Step};

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
/// have asks `launchctl` about `system/<label from the params>` and bootstraps the plist, every
/// call goes to `launchctl` and names that label or the plist, and `systemctl` is never called.
#[test]
fn launchd_params_issue_launchctl_calls_for_their_label_only() {
    let host = new_host("system-launchd");
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
    assert!(runner.calls().is_empty(), "{:?}", runner.calls());
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
    assert!(runner.calls().is_empty());
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
