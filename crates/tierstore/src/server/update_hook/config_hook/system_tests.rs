//! The config hook over the machine's ports: which binary checks which file, which record is
//! written, and when the guard is reloaded.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::super::machine::testing::me;
use super::super::machine::{Call, Output, RunError, Runner};
use super::super::system::{
    LAUNCHCTL_PROGRAM, SYSTEMCTL_PROGRAM, SystemConfig, config_run_as, config_run_on,
};
use super::super::{Invocation, Outcome, Params, Reason, STATE_DIR, Step};
use super::{Defaults, LINUX_CONFIG_FILE, parse_params};

const STAGED: &str = "/packages/tierstore-config-g/0.4.0";
const BINARY: &str = "/usr/libexec/tierstore/tierstore";

/// Answers `systemctl show` with `inactive`, `launchctl print` with "not loaded", a `start` with
/// a failure (so no step goes on to look at a port), and every other call with success and no
/// output; keeps the calls.
#[derive(Default)]
struct Calls {
    calls: RefCell<Vec<Call>>,
}

impl Calls {
    fn lines(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .map(|call| format!("{} {}", call.program, call.args.join(" ")))
            .collect()
    }
}

impl Runner for Calls {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        self.calls.borrow_mut().push(call.clone());
        let (code, stdout) = match call.args.first().map(String::as_str) {
            Some("show") => (0, "inactive\n"),
            Some("print") => (113, ""),
            Some("start") => (1, ""),
            _ => (0, ""),
        };
        Ok(Output {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }
}

/// A node under a scratch root: the service's binary, the installed configuration, and a
/// staged config package holding a new one.
struct Node {
    _dir: TestDir,
    root: PathBuf,
}

fn node(name: &str) -> Node {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let root = dir.path().to_path_buf();
    let node = Node { _dir: dir, root };
    let binary = node.at(BINARY);
    let bin = binary.parent().unwrap();
    fs::create_dir_all(bin).unwrap();
    fs::set_permissions(bin, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(&binary, "#!").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    node.write(
        LINUX_CONFIG_FILE,
        "[server]\nhandoff_socket = \"/run/h.sock\"\n",
    );
    node.write(
        &format!("{STAGED}/tree{LINUX_CONFIG_FILE}"),
        "[server]\nhandoff_socket = \"/run/h.sock\"\n",
    );
    node
}

impl Node {
    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.at(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
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

    fn state(&self, file: &str) -> PathBuf {
        self.at(STATE_DIR).join(file)
    }

    fn run(&self, runner: &dyn Runner, step: Step, params: &Params) -> super::super::Done {
        let invocation = Invocation {
            step,
            artifact: None,
            params: PathBuf::from("/params.toml"),
        };
        config_run_on(&self.config(runner), Path::new(BINARY), &invocation, params)
    }
}

fn params(extra: &str) -> Params {
    let defaults = Defaults {
        config_file: LINUX_CONFIG_FILE,
        service_manager: super::super::ServiceManager::Systemd,
    };
    parse_params(
        &format!(
            "\"updater.staged_release\" = \"{STAGED}\"\n\"updater.target_version\" = \"0.4.0\"\n{extra}"
        ),
        &defaults,
    )
    .unwrap()
}

/// Asserts that `prepare` has the service's own binary check the staged copy of the file, under
/// the package's tree, and writes the config hook's record, never the release's.
#[test]
fn prepare_checks_the_staged_file_with_the_serving_binary_and_keeps_its_own_record() {
    let node = node("config-hook-system-prepare");
    let calls = Calls::default();
    let done = node.run(&calls, Step::Prepare, &params(""));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let binary = node.at(BINARY).display().to_string();
    let staged = node.at(&format!("{STAGED}/tree{LINUX_CONFIG_FILE}"));
    let check = format!("{binary} --config {} --check-config", staged.display());
    assert!(calls.lines().contains(&check), "{:?}", calls.lines());
    assert!(
        calls
            .lines()
            .iter()
            .all(|line| !line.contains("/packages/") || line.starts_with(&binary)),
        "only the serving binary runs: {:?}",
        calls.lines()
    );
    assert!(node.state("config-state.json").is_file());
    assert!(!node.state("state.json").exists());
}

/// Asserts that `cutover` reloads the guard when the node has `guard.nft`, before it starts an
/// instance.
#[test]
fn cutover_reloads_the_guard_of_a_node_that_has_one() {
    let node = node("config-hook-system-guard");
    node.write("/etc/tierstore/guard.nft", "table inet tierstore_guard\n");
    let calls = Calls::default();
    let prepared = node.run(&calls, Step::Prepare, &params(""));
    assert_eq!(prepared.outcome, Outcome::Success, "{prepared:?}");
    calls.calls.borrow_mut().clear();
    // The start fails here, which is enough to see the order of the calls.
    let done = node.run(&calls, Step::Cutover, &params(""));
    assert_eq!(done.report.reason, Some(Reason::UnitFailed), "{done:?}");
    let lines = calls.lines();
    let reload = format!("{SYSTEMCTL_PROGRAM} reload-or-restart -- tierstore-guard.service");
    let start = lines
        .iter()
        .position(|line| line.contains(" start -- tierstore@"));
    assert_eq!(lines.first(), Some(&reload), "{lines:?}");
    assert!(start.is_some(), "{lines:?}");
}

/// Asserts that a node without `guard.nft` has no guard to reload.
#[test]
fn cutover_reloads_no_guard_on_a_node_without_its_file() {
    let node = node("config-hook-system-no-guard");
    let calls = Calls::default();
    let _ = node.run(&calls, Step::Prepare, &params(""));
    calls.calls.borrow_mut().clear();
    let _ = node.run(&calls, Step::Cutover, &params(""));
    assert!(
        calls
            .lines()
            .iter()
            .all(|line| !line.contains("tierstore-guard")),
        "{:?}",
        calls.lines()
    );
}

/// Asserts that on a Mac `prepare` runs no `systemctl` and writes the config hook's record.
#[test]
fn a_mac_prepares_through_launchd() {
    let node = node("config-hook-system-mac");
    let calls = Calls::default();
    let done = node.run(
        &calls,
        Step::Prepare,
        &params("service_manager = \"launchd\"\nlaunchd_label = \"tierstore\"\n"),
    );
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(
        calls
            .lines()
            .iter()
            .all(|line| !line.starts_with(SYSTEMCTL_PROGRAM)),
        "{:?}",
        calls.lines()
    );
    assert!(node.state("config-state.json").is_file());
}

/// Asserts that a service manager the hook does not know refuses the step and touches nothing.
#[test]
fn an_unknown_service_manager_is_refused() {
    let node = node("config-hook-system-unknown");
    let calls = Calls::default();
    let done = node.run(
        &calls,
        Step::Prepare,
        &params("service_manager = \"runit\"\n"),
    );
    assert_eq!(done.report.reason, Some(Reason::ServiceManagerUnknown));
    assert!(calls.lines().is_empty());
    assert!(!node.at(STATE_DIR).exists());
}

/// Asserts that a binary whose own path cannot be read fails the step as the host's fault.
#[test]
fn a_binary_whose_path_cannot_be_read_fails_the_step() {
    let node = node("config-hook-system-exe");
    let calls = Calls::default();
    let invocation = Invocation {
        step: Step::Verify,
        artifact: None,
        params: PathBuf::from("/params.toml"),
    };
    let done = config_run_as(
        &node.config(&calls),
        Err(std::io::Error::other("gone")),
        &invocation,
        &params(""),
    );
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::HostFailed));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the path of this binary cannot be read: gone")
    );
    assert!(calls.lines().is_empty());
}
