//! The steps run over the machine's ports, built from a declaration.

use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

use super::machine::SystemRunner;
use super::machine::testing::{FakeRunner, SERVICE, exit, my_group};
use super::machine::{Account, STATE_FILE, StateFiles};
use super::system::{Config, NFT_TIMEOUT, START_TIMEOUT, run_on, system_config};
use super::{
    DECLARATION_PATH, Done, FORWARD_TABLE, GUARD_TABLE, HostArgs, Invocation, Machine, Outcome,
    ROOT_UID, Reason, Record, StateDir, Step,
};

/// A host under a scratch root: the declaration where the verb reads it, a state directory the
/// declaration names, and a staged release.
struct Scratch {
    _dir: TestDir,
    root: PathBuf,
    state: PathBuf,
}

/// The state directory the declaration names: a path under the host's root, which is the scratch
/// directory (a scratch under `/tmp` could not be declared, `/tmp` being a reserved path).
const DECLARED_STATE: &str = "/var/lib/updater/hooks/tierstore";

fn declaration_text() -> String {
    format!(
        "uid = \"{SERVICE}\"\nstate_dir = \"{DECLARED_STATE}\"\nhealth_ports = [9301, 9302]\n\n[hook]\nunits = [\"tierstore@9301.service\", \"tierstore@9302.service\"]\n",
    )
}

fn scratch(name: &str) -> Scratch {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let root = dir.path().to_path_buf();
    let state = root.join(DECLARED_STATE.trim_start_matches('/'));
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o750)).unwrap();
    let group = root.join("etc/group");
    std::fs::create_dir_all(group.parent().unwrap()).unwrap();
    std::fs::write(&group, format!("hook:x:{}:{SERVICE}\n", my_group())).unwrap();
    let declaration = root.join(DECLARATION_PATH.trim_start_matches('/'));
    std::fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    std::fs::write(&declaration, declaration_text()).unwrap();
    std::fs::set_permissions(&declaration, std::fs::Permissions::from_mode(0o644)).unwrap();
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
            group: my_group(),
            release_owner: self.owner(),
            declaration_owner: self.owner(),
            nft: PathBuf::from("/usr/sbin/nft"),
            systemctl: PathBuf::from("/usr/bin/systemctl"),
            runner,
        }
    }

    fn record(&self, record: &Record) {
        let file = self.state.join(STATE_FILE);
        std::fs::write(&file, record.encode()).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
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
    let account = Account {
        owner: host.owner(),
        group: my_group(),
        service: SERVICE.to_owned(),
        group_file: host.root.join("etc/group"),
    };
    let _held = StateFiles::new(host.state.clone(), account).lock().unwrap();
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

/// Asserts that the machine's configuration expects the state directory to be owned by the user
/// the hook runs as (its effective user id, since the hook has an account of its own and does
/// not run as root), a staged release and the declaration to be owned by root, the files of the
/// host to be under `/`, and the host's own `nft` and `systemctl`, when the command line names
/// none of them. It would catch a wiring that expects root to own the state directory, or one
/// whose defaults are not the host's.
#[test]
fn the_system_config_expects_the_hooks_own_user_to_own_the_state_directory() {
    let config = system_config(&SystemRunner, &HostArgs::default());
    assert_eq!(config.owner, rustix::process::geteuid().as_raw());
    assert_eq!(config.group, rustix::process::getegid().as_raw());
    assert_eq!(config.release_owner, ROOT_UID);
    assert_eq!(config.declaration_owner, 0);
    assert_eq!(ROOT_UID, 0);
    assert_eq!(config.root, Path::new("/"));
    assert_eq!(config.nft, Path::new("/usr/sbin/nft"));
    assert_eq!(config.systemctl, Path::new("/usr/bin/systemctl"));
}

/// Asserts that the flags of the host reach the machine's configuration: the root, the user that
/// must own the declaration and a staged release, and the two programs, which the ports then
/// call. It would catch a flag that is parsed and dropped, which would leave a conformance test
/// running the host's own `nft`.
#[test]
fn the_flags_of_the_host_reach_the_configuration_and_the_programs() {
    let host = scratch("tierstore-hook-system-flags");
    host.record(&Record::idle(9301));
    host.link(9301);
    let words = [
        "restore",
        "--root",
        host.root.to_str().unwrap(),
        "--root-uid",
        &host.owner().to_string(),
        "--nft",
        "/opt/fake/nft",
        "--systemctl",
        "/opt/fake/systemctl",
    ]
    .map(std::ffi::OsString::from);
    let invocation = Invocation::parse(words).unwrap();
    let runner = FakeRunner::replying(vec![ok(), ok(), ok()]);
    let config = system_config(&runner, &invocation.host);
    assert_eq!(config.root, host.root);
    assert_eq!(
        (config.release_owner, config.declaration_owner),
        (host.owner(), host.owner())
    );
    let done = run_on(&config, &invocation, None);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let programs: Vec<String> = runner.calls().into_iter().map(|c| c.program).collect();
    assert_eq!(
        programs,
        ["/opt/fake/nft", "/opt/fake/nft", "/opt/fake/systemctl"]
    );
}

fn declaration_file(host: &Scratch) -> PathBuf {
    host.root.join(DECLARATION_PATH.trim_start_matches('/'))
}

/// Asserts that a declaration that is a symbolic link (even to a good file), one a group can
/// write, one others can write, one owned by another user and a directory are each refused as `host_failed`
/// ("not trusted"), with no program run and no lock file made. It would catch a declaration read
/// that a non-root account could have written or replaced.
#[test]
fn a_declaration_that_another_account_could_have_written_is_refused() {
    for case in ["link", "group", "others", "owner", "directory"] {
        let host = scratch(&format!("tierstore-hook-system-untrusted-{case}"));
        let path = declaration_file(&host);
        let mut config_owner = host.owner();
        match case {
            "link" => {
                let real = host.root.join("real.toml");
                std::fs::rename(&path, &real).unwrap();
                symlink(&real, &path).unwrap();
            }
            "group" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
            }
            "others" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o646)).unwrap();
            }
            "directory" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            _ => config_owner += 1,
        }
        let runner = FakeRunner::replying(vec![]);
        let config = Config {
            declaration_owner: config_owner,
            ..host.config(&runner)
        };
        let invocation = Invocation::parse([std::ffi::OsString::from("restore")]).unwrap();
        let done = run_on(&config, &invocation, None);
        assert_eq!(
            done.report.reason,
            Some(Reason::HostFailed),
            "{case}: {done:?}"
        );
        let detail = done.report.detail.unwrap();
        assert!(detail.contains("not trusted"), "{case}: {detail}");
        assert!(runner.calls().is_empty(), "{case}");
        assert!(!host.state.join("lock").exists(), "{case}");
    }
}

/// Asserts that a declaration that is a regular file owned by the configured user, mode 0644, is
/// read and the step runs. It would catch a check that refuses every declaration.
#[test]
fn a_declaration_owned_by_the_configured_user_with_mode_0644_is_read() {
    let host = scratch("tierstore-hook-system-trusted");
    assert_eq!(
        std::fs::metadata(declaration_file(&host)).unwrap().mode() & 0o777,
        0o644
    );
    let runner = FakeRunner::replying(vec![]);
    let done = host.run(Step::Restore, &runner);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}

/// Runs `restore` on `host` configured by `change` and returns what it said and the programs it
/// ran.
fn refused_by(host: &Scratch, change: impl FnOnce(&mut Config<'_>)) -> (Done, usize) {
    let runner = FakeRunner::replying(vec![]);
    let mut config = host.config(&runner);
    change(&mut config);
    let invocation = Invocation::parse([std::ffi::OsString::from("restore")]).unwrap();
    let done = run_on(&config, &invocation, None);
    (done, runner.calls().len())
}

/// Asserts that a state directory that is not owned by the hook's group, one that is not mode
/// 0750 and one whose group does not list the service account are each refused as `state_io`
/// with the state port's words, and no program runs. It would catch a wiring that leaves the
/// group, the mode or the membership unchecked.
#[test]
fn a_state_directory_the_service_cannot_reach_or_could_write_is_refused() {
    let host = scratch("tierstore-hook-system-dir");
    host.record(&Record::idle(9301));
    host.link(9301);
    let (done, calls) = refused_by(&host, |config| config.group = my_group() + 1);
    assert_eq!(done.report.reason, Some(Reason::StateIo), "{done:?}");
    assert!(done.report.detail.unwrap().contains("belongs to group"));
    assert_eq!(calls, 0);
    std::fs::set_permissions(&host.state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (done, calls) = refused_by(&host, |_| {});
    assert_eq!(done.report.reason, Some(Reason::StateIo), "{done:?}");
    assert!(done.report.detail.unwrap().contains("not 0750"));
    assert_eq!(calls, 0);
    std::fs::set_permissions(&host.state, std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::write(host.root.join("etc/group"), "hook:x:1:other\n").unwrap();
    let (done, calls) = refused_by(&host, |_| {});
    assert_eq!(done.report.reason, Some(Reason::StateIo), "{done:?}");
    assert!(
        done.report
            .detail
            .unwrap()
            .contains("is not a member of group")
    );
    assert_eq!(calls, 0);
}

/// Asserts that a declaration with no `uid` (no service account to look for in the group) is
/// refused as `state_invalid` before anything is made. It would catch a wiring that goes on
/// without knowing who must be able to reach the directory.
#[test]
fn a_declaration_without_a_service_account_is_refused() {
    let host = scratch("tierstore-hook-system-nouid");
    let path = declaration_file(&host);
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace(&format!("uid = \"{SERVICE}\"\n"), "")).unwrap();
    let (done, calls) = refused_by(&host, |_| {});
    assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{done:?}");
    assert!(done.report.detail.unwrap().contains("uid"));
    assert_eq!(calls, 0);
    assert!(!host.state.join("lock").exists());
}

/// Asserts that a record that a group or others can write (a forged one) is refused as `state_io`
/// by a step, and no program runs. It would catch a step that acts on a record the service could
/// have written.
#[test]
fn a_record_the_service_could_have_written_stops_the_step() {
    let host = scratch("tierstore-hook-system-forged");
    host.record(&Record::idle(9301));
    host.link(9301);
    let file = host.state.join(STATE_FILE);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o660)).unwrap();
    let (done, calls) = refused_by(&host, |_| {});
    assert_eq!(done.report.reason, Some(Reason::StateIo), "{done:?}");
    assert!(
        done.report
            .detail
            .unwrap()
            .contains("can be written by a group or others")
    );
    assert_eq!(calls, 0);
}
