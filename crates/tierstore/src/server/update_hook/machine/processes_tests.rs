//! The processes of the fs units: `systemctl show --property=MainPID` through a recording runner,
//! and `/proc/<pid>/exe` and the service binary under a scratch root.

use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::SERVICE_BINARY;
use super::super::ports::{Family, FileId, Instance, PortError, PortKind, Processes};
use super::testing::{FakeRunner, LIMIT, exit, scratch};
use super::{Call, ProcFiles, SystemdUnits};

const SHOW: Duration = Duration::from_secs(33);

fn units(runner: &FakeRunner) -> SystemdUnits<'_> {
    SystemdUnits::new(runner, LIMIT, LIMIT, SHOW).of(Family::Fs)
}

fn message(error: PortError, kind: PortKind) -> String {
    match error {
        PortError::Failed { kind: got, message } if got == kind => message,
        other => panic!("not a {kind} failure: {other:?}"),
    }
}

/// The file at `path` as the processes port names it.
fn id(path: &Path) -> FileId {
    let meta = fs::metadata(path).unwrap();
    FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    }
}

/// `path` under `root`.
fn at(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// Makes the file at `path` under `root`, and its directories.
fn file(root: &Path, path: &str) -> PathBuf {
    let path = at(root, path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"a binary").unwrap();
    path
}

/// Makes `/proc/<pid>/exe` under `root` a link to `target`.
fn exe_link(root: &Path, pid: u32, target: &Path) {
    let dir = at(root, &format!("/proc/{pid}"));
    fs::create_dir_all(&dir).unwrap();
    symlink(target, dir.join("exe")).unwrap();
}

/// Asserts that the main pid is `systemctl show --property=MainPID --value -- <unit>` of the fs
/// template's instance, within the time limit of a look at the state: a number is that pid, `0`
/// (systemd's word for a unit with no process) is none, and anything else, or a program that
/// fails, is a failure of the units port naming the command, exactly. Catches a pid read from
/// the wrong unit or property, and `0` taken for a process.
#[test]
fn the_main_pid_is_systemds_main_pid_of_the_fs_instance() {
    let runner = FakeRunner::replying(vec![
        exit(0, "4242\n", ""),
        exit(0, "0\n", ""),
        exit(0, "[not set]\n", ""),
        exit(1, "", "Failed to connect to bus\n"),
    ]);
    let units = units(&runner);
    let (_dir, root) = scratch("processes-main-pid");
    let processes = ProcFiles::new(&units, root, &runner, LIMIT);
    assert_eq!(processes.main_pid(Instance::B).unwrap(), Some(4242));
    assert_eq!(processes.main_pid(Instance::A).unwrap(), None);
    let show = "/usr/bin/systemctl show --property=MainPID --value -- tierstore-fs@a.service";
    assert_eq!(
        message(
            processes.main_pid(Instance::A).unwrap_err(),
            PortKind::Units
        ),
        format!("{show}: not a MainPID: \"[not set]\"")
    );
    assert_eq!(
        message(
            processes.main_pid(Instance::A).unwrap_err(),
            PortKind::Units
        ),
        format!("{show}: exited 1: Failed to connect to bus")
    );
    let calls = runner.calls();
    assert_eq!(
        calls[0],
        Call::new(
            "/usr/bin/systemctl",
            &[
                "show",
                "--property=MainPID",
                "--value",
                "--",
                "tierstore-fs@b.service"
            ],
            SHOW
        )
    );
    assert_eq!(calls.len(), 4);
}

/// Asserts that the binary of a process is what its `/proc/<pid>/exe` under the root reads and the
/// file it leads to, and the installed binary the file at the service binary's path: a process
/// that runs the installed file has the same file, one whose link reads `(deleted)` and leads to
/// another file has another. Catches a comparison of paths instead of files, and a look at the
/// link itself (`lstat`) instead of the file it leads to.
#[test]
fn the_binary_of_a_process_is_the_file_its_exe_link_leads_to() {
    let runner = FakeRunner::replying(Vec::new());
    let units = units(&runner);
    let (_dir, root) = scratch("processes-exe");
    let installed = file(&root, SERVICE_BINARY);
    let replaced = file(&root, &format!("/old/{} (deleted)", "tierstore"));
    exe_link(&root, 4101, &installed);
    exe_link(&root, 4102, &replaced);
    let processes = ProcFiles::new(&units, root.clone(), &runner, LIMIT);
    assert_eq!(processes.installed().unwrap(), id(&installed));
    let runs = processes.exe(4101).unwrap();
    assert_eq!(runs.path, installed);
    assert_eq!(runs.file, processes.installed().unwrap());
    let old = processes.exe(4102).unwrap();
    assert_eq!(old.path, replaced);
    assert_eq!(old.file, id(&replaced));
    assert_ne!(old.file, processes.installed().unwrap());
    assert!(runner.calls().is_empty());
}

/// Asserts that a process that is gone (no `/proc/<pid>/exe`), a link that leads nowhere, and an
/// installed binary that is not there are failures of the host port naming the path and the
/// system's words, exactly. Catches an unreadable process taken for one that runs some binary.
#[test]
fn a_process_or_binary_that_cannot_be_read_is_a_failure_naming_it() {
    let runner = FakeRunner::replying(Vec::new());
    let units = units(&runner);
    let (_dir, root) = scratch("processes-unreadable");
    exe_link(&root, 4103, &root.join("nothing"));
    let processes = ProcFiles::new(&units, root.clone(), &runner, LIMIT);
    let gone = at(&root, "/proc/4100/exe");
    assert_eq!(
        message(processes.exe(4100).unwrap_err(), PortKind::Host),
        format!("{}: No such file or directory (os error 2)", gone.display())
    );
    let dangling = at(&root, "/proc/4103/exe");
    assert_eq!(
        message(processes.exe(4103).unwrap_err(), PortKind::Host),
        format!(
            "{}: No such file or directory (os error 2)",
            dangling.display()
        )
    );
    assert_eq!(
        message(processes.installed().unwrap_err(), PortKind::Host),
        format!(
            "{}: No such file or directory (os error 2)",
            at(&root, SERVICE_BINARY).display()
        )
    );
}

/// Asserts that the version of a process is the word after the program's name in what its
/// `/proc/<pid>/exe --version` prints, run through the runner within the port's time limit; and
/// that a binary that prints no version, or only its name, or that exits other than 0, is a
/// failure of the host port naming the command, exactly. Catches the program's name taken for
/// its version, and a failed run taken for an answer.
#[test]
fn the_version_of_a_process_is_what_its_binary_prints_after_its_name() {
    let runner = FakeRunner::replying(vec![
        exit(0, "tierstore 0.4.0+0123456789ab\n", ""),
        exit(0, "\n", ""),
        exit(0, "tierstore\n", ""),
        exit(2, "", "error: unexpected argument '--version'\n"),
    ]);
    let units = units(&runner);
    let (_dir, root) = scratch("processes-version");
    let timeout = Duration::from_secs(44);
    let processes = ProcFiles::new(&units, root.clone(), &runner, timeout);
    let program = at(&root, "/proc/4101/exe").to_string_lossy().into_owned();
    assert_eq!(processes.version(4101).unwrap(), "0.4.0+0123456789ab");
    let command = format!("{program} --version");
    assert_eq!(
        message(processes.version(4101).unwrap_err(), PortKind::Host),
        format!("{command}: no version in \"\"")
    );
    assert_eq!(
        message(processes.version(4101).unwrap_err(), PortKind::Host),
        format!("{command}: no version in \"tierstore\"")
    );
    assert_eq!(
        message(processes.version(4101).unwrap_err(), PortKind::Host),
        format!("{command}: exited 2: error: unexpected argument '--version'")
    );
    assert_eq!(
        runner.calls(),
        vec![Call::new(&program, &["--version"], timeout); 4]
    );
}
