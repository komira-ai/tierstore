//! The steps run over the machine's ports, built from a declaration.

use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

use super::machine::testing::{FakeRunner, exit};
use super::machine::{STATE_FILE, StateFiles};
use super::system::{Config, NFT_TIMEOUT, START_TIMEOUT, run_on};
use super::{
    DECLARATION_PATH, Done, FORWARD_TABLE, GUARD_TABLE, Invocation, Machine, Outcome, Reason,
    Record, StateDir, Step,
};

/// A host under a scratch root: the declaration where the verb reads it, a state directory the
/// declaration names, and a staged release.
struct Scratch {
    _dir: TestDir,
    root: PathBuf,
    state: PathBuf,
}

fn declaration_text(state: &Path) -> String {
    format!(
        "state_dir = \"{}\"\nhealth_ports = [9301, 9302]\n\n[hook]\nunits = [\"tierstore@9301.service\", \"tierstore@9302.service\"]\n",
        state.display()
    )
}

fn scratch(name: &str) -> Scratch {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let root = dir.path().to_path_buf();
    let state = root.join("var-state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let declaration = root.join(DECLARATION_PATH.trim_start_matches('/'));
    std::fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    std::fs::write(&declaration, declaration_text(&state)).unwrap();
    Scratch {
        _dir: dir,
        root,
        state,
    }
}

impl Scratch {
    fn owner(&self) -> u32 {
        std::fs::metadata(&self.state).unwrap().uid()
    }

    fn config<'a>(&self, runner: &'a FakeRunner) -> Config<'a> {
        Config {
            root: self.root.clone(),
            owner: self.owner(),
            runner,
        }
    }

    fn record(&self, record: &Record) {
        std::fs::write(self.state.join(STATE_FILE), record.encode()).unwrap();
    }

    fn link(&self, port: u16) {
        let slots = self.state.join("slots");
        std::fs::create_dir_all(&slots).unwrap();
        symlink("/releases/old", slots.join(port.to_string())).unwrap();
    }

    fn run(&self, step: Step, runner: &FakeRunner) -> Done {
        let invocation =
            Invocation::parse([step.to_string()].into_iter().map(std::ffi::OsString::from))
                .unwrap();
        run_on(&self.config(runner), &invocation, None)
    }
}

fn ok() -> Result<super::machine::Output, super::machine::RunError> {
    exit(0, "", "")
}

/// Asserts that `restore` on a host with a declaration, a record and a link runs the real step:
/// it applies the guard and the redirect through `nft -f -` with the ruleset on standard input,
/// starts the serving slot's unit through `systemctl`, each with its own time limit, leaves the
/// record as it was, and takes the lock file of the state directory the declaration names. It
/// would catch a machine that still answers `not_implemented`, or builds a port with the wrong
/// program, arguments or limit.
#[test]
fn restore_runs_the_step_over_the_machine_ports() {
    let host = scratch("tierstore-hook-system-restore");
    host.record(&Record::idle(9301));
    host.link(9301);
    let before = std::fs::read(host.state.join(STATE_FILE)).unwrap();
    let runner = FakeRunner::replying(vec![ok(), ok(), ok()]);
    let done = host.run(Step::Restore, &runner);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    let calls = runner.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0].program, "/usr/sbin/nft");
    assert_eq!(calls[0].args, ["-f", "-"]);
    let guard = String::from_utf8(calls[0].stdin.clone().unwrap()).unwrap();
    assert!(guard.contains(GUARD_TABLE), "{guard}");
    assert_eq!(calls[0].timeout, NFT_TIMEOUT);
    let forward = String::from_utf8(calls[1].stdin.clone().unwrap()).unwrap();
    assert!(
        forward.contains(FORWARD_TABLE) && forward.contains("9301"),
        "{forward}"
    );
    assert_eq!(calls[1].timeout, NFT_TIMEOUT);
    assert_eq!(calls[2].program, "/usr/bin/systemctl");
    assert_eq!(calls[2].args, ["start", "--", "tierstore@9301.service"]);
    assert_eq!(calls[2].timeout, START_TIMEOUT);
    assert_eq!(std::fs::read(host.state.join(STATE_FILE)).unwrap(), before);
    assert!(host.state.join("lock").exists());
}

/// Asserts that with a declaration and no record `restore` reports that there is nothing to
/// restore, runs no program, and still takes the lock in the declared state directory. It would
/// catch a state port built over another directory than the declaration's.
#[test]
fn restore_with_no_record_runs_no_program() {
    let host = scratch("tierstore-hook-system-norecord");
    let runner = FakeRunner::replying(vec![]);
    let done = host.run(Step::Restore, &runner);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(runner.calls().is_empty());
    assert!(host.state.join("lock").exists());
}

/// Asserts that a slot with no link under `<state_dir>/slots` is refused as `no_slot` before any
/// program runs. It would catch slot links read from somewhere other than the declared state
/// directory.
#[test]
fn restore_reads_the_links_of_the_declared_state_directory() {
    let host = scratch("tierstore-hook-system-nolink");
    host.record(&Record::idle(9301));
    host.link(9302);
    let runner = FakeRunner::replying(vec![]);
    let done = host.run(Step::Restore, &runner);
    assert_eq!(done.report.reason, Some(Reason::NoSlot), "{done:?}");
    assert!(runner.calls().is_empty());
}

/// Asserts that a state directory owned by a user other than the configured owner is refused
/// as `state_io` and no program runs. It would catch an owner that is not passed to the state
/// port.
#[test]
fn a_state_directory_of_another_owner_is_refused() {
    let host = scratch("tierstore-hook-system-owner");
    let runner = FakeRunner::replying(vec![]);
    let config = Config {
        owner: host.owner() + 1,
        ..host.config(&runner)
    };
    let invocation = Invocation::parse([std::ffi::OsString::from("restore")]).unwrap();
    let done = run_on(&config, &invocation, None);
    assert_eq!(done.report.reason, Some(Reason::StateIo), "{done:?}");
    assert!(done.report.detail.unwrap().contains("owned by user"));
    assert!(runner.calls().is_empty());
}

/// Asserts that a second step that finds the lock held exits 1 with `locked`, and runs no
/// program. It would catch a machine that takes no lock.
#[test]
fn a_held_lock_is_locked() {
    let host = scratch("tierstore-hook-system-locked");
    host.record(&Record::idle(9301));
    host.link(9301);
    let _held = StateFiles::new(host.state.clone(), host.owner())
        .lock()
        .unwrap();
    let runner = FakeRunner::replying(vec![]);
    let done = host.run(Step::Restore, &runner);
    assert_eq!(done.report.reason, Some(Reason::Locked), "{done:?}");
    assert!(runner.calls().is_empty());
}

/// Asserts that a host with no declaration, and one whose declaration is refused, stop before
/// any program runs or any file is made: `host_failed` for the first, `state_invalid` for the
/// second. It would catch a machine that guesses a state directory.
#[test]
fn a_missing_or_refused_declaration_stops_the_step() {
    let host = scratch("tierstore-hook-system-nodecl");
    let runner = FakeRunner::replying(vec![]);
    let path = host.root.join(DECLARATION_PATH.trim_start_matches('/'));
    std::fs::write(
        &path,
        "state_dir = \"/x\"\nhealth_ports = [9301]\n[hook]\nunits = []\n",
    )
    .unwrap();
    let refused = host.run(Step::Restore, &runner);
    assert_eq!(
        refused.report.reason,
        Some(Reason::StateInvalid),
        "{refused:?}"
    );
    std::fs::remove_file(&path).unwrap();
    let missing = host.run(Step::Restore, &runner);
    assert_eq!(
        missing.report.reason,
        Some(Reason::HostFailed),
        "{missing:?}"
    );
    assert!(runner.calls().is_empty());
    assert!(!host.state.join("lock").exists());
}

/// Asserts that the verb on this machine runs the step: with no declaration at the declared
/// path on the build host, `restore` says the host could not be asked, not `not_implemented`.
/// It would catch `Machine::system` left answering every step `not_implemented`.
#[test]
fn the_system_machine_reads_the_declaration_of_the_host() {
    if Path::new(DECLARATION_PATH).exists() {
        return;
    }
    let invocation = Invocation::parse([std::ffi::OsString::from("restore")]).unwrap();
    let done = (Machine::system().step)(&invocation, None);
    assert_eq!(done.report.reason, Some(Reason::HostFailed), "{done:?}");
    assert!(done.report.detail.unwrap().contains("tierstore.toml"));
}
