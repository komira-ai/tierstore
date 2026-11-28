//! What the rig asks of the operating system, of this crate's `fake-s3` program as the process
//! it starts and stops: the start line, `/proc/<pid>/status`, the signals, the logs; and the
//! size of the test's own file system. Its name ends in `_integration`: it reads `/proc`.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tierstore_blackbox::os::{self, OsError, Process};
use tierstore_testkit::dir::TestDir;

/// The time a program of the tests is given for its start line, and to end after SIGTERM: bounds
/// that only a hung program reaches.
const WAIT: Duration = Duration::from_secs(60);

/// A scratch directory with room for a socket, and the socket's path.
fn scratch(name: &str) -> (TestDir, PathBuf) {
    let temp = std::env::temp_dir();
    TestDir::for_socket(
        &[temp.as_path(), Path::new(support::SCRATCH)],
        name,
        "c.sock",
    )
}

fn fake(control: &Path) -> Command {
    let mut command = Command::new(support::FAKE_S3);
    command.arg("--control").arg(control);
    command
}

/// Sends `stop` to the fake at `control`, and waits for `process` to end.
fn stop(control: &Path, process: &mut Process) {
    let mut stream = UnixStream::connect(control).unwrap();
    stream.write_all(b"stop\n").unwrap();
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).unwrap();
    assert_eq!(answer, "ok\n");
    while process.alive() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Asserts that a program counts as started at its start line, whose address it names; that it
/// runs with a pid and a resident memory read from /proc; that its standard output is in its log
/// as it came; and that SIGTERM ends it.
#[test]
fn a_program_starts_at_its_start_line_and_ends_at_sigterm() {
    let (dir, control) = scratch("os-start");
    let mut process = Process::start(fake(&control), "fake-s3", dir.path(), WAIT).unwrap();
    assert!(
        process.line().starts_with("fake-s3 listen=127.0.0.1:"),
        "{}",
        process.line()
    );
    assert!(
        process
            .line()
            .contains(&format!("listen={} ", process.listen()))
    );
    assert!(process.pid() > 0);
    assert!(process.alive());
    let resident = process.resident_bytes().unwrap();
    assert!(resident > 0, "{resident}");
    let out = std::fs::read_to_string(dir.path().join("fake-s3.out")).unwrap();
    assert_eq!(out, format!("{}\n", process.line()));
    let ended = process.terminate(WAIT).unwrap();
    assert_eq!(ended.signal(), Some(15));
}

/// Asserts that SIGKILL ends a program, and that a program the rig drops is killed with it.
#[test]
fn a_program_is_killed_and_none_outlives_the_rig() {
    let (dir, control) = scratch("os-kill");
    let process = Process::start(fake(&control), "fake-s3", dir.path(), WAIT).unwrap();
    assert_eq!(process.kill().unwrap().signal(), Some(9));

    std::fs::remove_file(&control).unwrap();
    let dropped = Process::start(fake(&control), "fake-s3", dir.path(), WAIT).unwrap();
    let proc = PathBuf::from(format!("/proc/{}", dropped.pid()));
    assert!(proc.exists());
    drop(dropped);
    assert!(!proc.exists());
}

/// Asserts that a program that ended has no resident memory to read and takes no SIGTERM, each
/// failure naming what was asked.
#[test]
fn a_program_that_ended_has_no_memory_and_takes_no_signal() {
    let (dir, control) = scratch("os-ended");
    let mut process = Process::start(fake(&control), "fake-s3", dir.path(), WAIT).unwrap();
    stop(&control, &mut process);
    let pid = process.pid();
    let unread = process.resident_bytes().unwrap_err().to_string();
    assert!(
        unread.starts_with(&format!("/proc/{pid}/status: ")),
        "{unread}"
    );
    let refused = process.terminate(WAIT).unwrap_err().to_string();
    assert!(refused.starts_with("SIGTERM to fake-s3: "), "{refused}");
}

/// Asserts that a program that ends before its start line is an error naming how it ended, with
/// its standard error in its log; that one that cannot be started, or whose log cannot be
/// opened, says so.
#[test]
fn a_program_that_does_not_start_says_why() {
    let (dir, _) = scratch("os-unstarted");
    let control = dir.path().join("no-such-dir").join("c.sock");
    let ended = Process::start(fake(&control), "fake-s3", dir.path(), WAIT).unwrap_err();
    assert!(matches!(&ended, OsError::Ended { .. }), "{ended}");
    let said = format!(
        "fake-s3 ended before it printed its start line (exit status: 1); see fake-s3.err: \
         fake-s3: the control socket {}: ",
        control.display()
    );
    assert!(ended.to_string().starts_with(&said), "{ended}");
    let err = std::fs::read_to_string(dir.path().join("fake-s3.err")).unwrap();
    assert!(
        err.starts_with(&format!(
            "fake-s3: the control socket {}: ",
            control.display()
        )),
        "{err}"
    );

    let missing = Command::new(dir.path().join("no-such-program"));
    let refused = Process::start(missing, "none", dir.path(), WAIT)
        .unwrap_err()
        .to_string();
    assert!(
        refused.starts_with("none could not be started: "),
        "{refused}"
    );

    let logs = dir.path().join("no-logs");
    let refused = Process::start(fake(&control), "fake-s3", &logs, WAIT)
        .unwrap_err()
        .to_string();
    assert!(
        refused.starts_with(&format!("{}: ", logs.join("fake-s3.out").display())),
        "{refused}"
    );
}

/// Asserts that the size of the test's file system is read, that of a path that is not there is
/// an error naming it, and that a fill that cannot make its file says so (a fill of a real disk
/// is the case `disk-full`, on a disk of its own).
#[test]
fn the_size_of_a_disk_is_read_and_a_fill_says_why_it_cannot() {
    let (dir, _) = scratch("os-disk");
    assert!(os::disk_bytes(dir.path()).unwrap() > 0);
    let missing = dir.path().join("none");
    let refused = os::disk_bytes(&missing).unwrap_err().to_string();
    assert!(
        refused.starts_with(&format!("statvfs of {}: ", missing.display())),
        "{refused}"
    );
    let ballast = missing.join("ballast");
    let refused = os::fill(&ballast).unwrap_err().to_string();
    assert!(
        refused.starts_with(&format!("filling {}: ", ballast.display())),
        "{refused}"
    );
}

/// Asserts that only a line with a `listen=` word is a start line: a program that prints other
/// lines and ends (the driver's `--help`) ended before its start line, with its status, and its
/// lines are in its log.
#[test]
fn a_line_without_an_address_is_no_start_line() {
    let (dir, _) = scratch("os-no-line");
    let mut help = Command::new(support::DRIVER);
    help.arg("--help");
    let ended = Process::start(help, "driver", dir.path(), WAIT).unwrap_err();
    assert_eq!(
        ended.to_string(),
        "driver ended before it printed its start line (exit status: 0); see driver.err"
    );
    let out = std::fs::read_to_string(dir.path().join("driver.out")).unwrap();
    assert!(out.contains("--small-disk"), "{out}");
}

/// Asserts that a program given no time for its start line, or none to end after SIGTERM, is
/// dealt with all the same: started, or killed and named; and ended, by the SIGTERM or by the
/// SIGKILL that follows it. Which of the two comes first is the machine's, not the test's.
#[test]
fn a_program_given_no_time_is_killed_and_named() {
    let (dir, control) = scratch("os-no-time");
    match Process::start(fake(&control), "fake-s3", dir.path(), Duration::ZERO) {
        Ok(started) => drop(started),
        Err(refused) => assert_eq!(
            refused.to_string(),
            "fake-s3 printed no start line within 0ns, and was killed; see fake-s3.err"
        ),
    }
    std::fs::remove_file(&control).ok();
    let process = Process::start(fake(&control), "fake-s3", dir.path(), WAIT).unwrap();
    let ended = process.terminate(Duration::ZERO).unwrap();
    assert!([Some(9), Some(15)].contains(&ended.signal()), "{ended:?}");
}
