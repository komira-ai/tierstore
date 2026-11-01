//! The real runner, over ordinary programs of the host (`cat`, `ls`, `echo`, `head`, `sleep`).

use std::io::{self, Read};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};

use super::runner::{Group, MAX_OUTPUT, collect, drain};
use super::{Call, Output, RunError, Runner, SystemRunner};

const LIMIT: Duration = Duration::from_secs(30);

/// Asserts that the bytes given as input arrive on the program's standard input and come back
/// on its standard output, with exit status 0.
#[test]
fn standard_input_reaches_the_program() {
    let call = Call::new("cat", &[], LIMIT).with_stdin(b"hello\nworld");
    let output = SystemRunner.run(&call).unwrap();
    assert_eq!(output.code, Some(0));
    assert_eq!(output.stdout, b"hello\nworld");
}

/// Asserts that a call with no input gives the program an input that is at its end, not one that
/// never ends: `cat` returns.
#[test]
fn no_input_is_an_empty_input() {
    let output = SystemRunner.run(&Call::new("cat", &[], LIMIT)).unwrap();
    assert_eq!(output.code, Some(0));
    assert!(output.stdout.is_empty());
}

/// Asserts that the exit status and standard error of a program that fails come back.
#[test]
fn the_exit_status_and_standard_error_come_back() {
    let call = Call::new("ls", &["/this/path/does/not/exist"], LIMIT);
    let output = SystemRunner.run(&call).unwrap();
    assert_eq!(output.code, Some(2));
    assert!(output.stderr_text().contains("No such file or directory"));
}

/// Asserts that arguments are passed as they are: the characters a shell would expand or split
/// on reach the program unchanged. It would catch a runner that builds a command line.
#[test]
fn arguments_go_to_the_program_without_a_shell() {
    let call = Call::new("echo", &["a  b;$HOME `x` *"], LIMIT);
    let output = SystemRunner.run(&call).unwrap();
    assert_eq!(output.stdout, b"a  b;$HOME `x` *\n");
}

/// Asserts that output larger than a pipe holds is read while the program runs: a runner that
/// waits for the exit before it reads would hang here.
#[test]
fn output_larger_than_a_pipe_is_read_in_full() {
    let call = Call::new("head", &["-c", "1000000", "/dev/zero"], LIMIT);
    let output = SystemRunner.run(&call).unwrap();
    assert_eq!(output.stdout.len(), 1_000_000);
}

/// Asserts that a program that outlives its time is killed and reported as a timeout that names
/// the limit given.
#[test]
fn a_program_that_runs_too_long_is_killed() {
    let limit = Duration::from_millis(100);
    let call = Call::new("sleep", &["30"], limit);
    assert_eq!(SystemRunner.run(&call), Err(RunError::Timeout(limit)));
}

/// Asserts that a program that does not exist is a spawn error that says why.
#[test]
fn a_program_that_does_not_exist_cannot_be_run() {
    let call = Call::new("/this/program/does/not/exist", &[], LIMIT);
    match SystemRunner.run(&call) {
        Err(RunError::Spawn(why)) => assert!(why.contains("No such file or directory"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// Asserts the words of a call as a message shows them, and that standard error is cut at 300
/// characters and trimmed.
#[test]
fn a_call_and_its_stderr_read_as_words() {
    let call = Call::new("nft", &["-f", "-"], LIMIT);
    assert_eq!(call.describe(), "nft -f -");
    let long = format!("  {}\n", "e".repeat(500));
    let output = Output {
        code: Some(1),
        stdout: Vec::new(),
        stderr: long.into_bytes(),
    };
    assert_eq!(output.stderr_text(), "e".repeat(300));
}

/// Asserts that the child has an environment of its own: nothing of the test's, only the fixed
/// `PATH` and `LC_ALL=C` (so nft's and systemctl's words do not depend on the host's locale).
#[test]
fn the_child_gets_a_clean_environment() {
    let output = SystemRunner
        .run(&Call::new("/usr/bin/env", &[], LIMIT))
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, ["LC_ALL=C", "PATH=/usr/sbin:/usr/bin:/sbin:/bin"]);
}

/// Asserts that only the first mebibyte of an output is kept, and that the rest is still read:
/// `head` writes three million bytes and exits 0, which it could not if nothing read them.
#[test]
fn output_over_a_mebibyte_is_cut_and_the_program_still_ends() {
    let call = Call::new("head", &["-c", "3000000", "/dev/zero"], LIMIT);
    let output = SystemRunner.run(&call).unwrap();
    assert_eq!(output.code, Some(0));
    assert_eq!(output.stdout.len(), MAX_OUTPUT);
}

/// Asserts that a program that writes without end is stopped by its time limit: `yes` is a
/// timeout, not a program that fills the memory.
#[test]
fn a_program_that_prints_without_end_is_timed_out() {
    let limit = Duration::from_millis(300);
    let call = Call::new("yes", &[], limit);
    assert_eq!(SystemRunner.run(&call), Err(RunError::Timeout(limit)));
}

fn boxed<R: Read + Send + 'static>(reader: R) -> Option<Box<dyn Read + Send>> {
    Some(Box::new(reader))
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

/// Asserts that a stream of three mebibytes comes back as its first mebibyte, whole.
#[test]
fn a_drained_stream_keeps_its_first_mebibyte() {
    let reader = io::repeat(7).take(3 << 20);
    let kept = collect(&drain(boxed(reader)), soon()).unwrap();
    assert_eq!(kept.len(), MAX_OUTPUT);
    assert!(kept.iter().all(|byte| *byte == 7));
}

struct Panics;

impl Read for Panics {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        panic!("the reader fails");
    }
}

/// Asserts that a reader that panics is an error and not empty output (which the capabilities
/// would read as a release with none).
#[test]
fn a_drain_that_panics_is_an_error_not_empty_output() {
    let error = collect(&drain(boxed(Panics)), soon()).unwrap_err();
    assert_eq!(error.to_string(), "the output reader failed");
}

struct Blocks(mpsc::Receiver<()>);

impl Read for Blocks {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        let _ = self.0.recv();
        Ok(0)
    }
}

/// Asserts that a stream that does not reach its end is waited for only until the deadline,
/// and that the call then reports that it was not drained.
#[test]
fn a_stream_that_does_not_end_is_waited_for_only_until_the_deadline() {
    let (release, wait) = mpsc::channel();
    let drained = drain(boxed(Blocks(wait)));
    let error = collect(&drained, Instant::now() + Duration::from_millis(100)).unwrap_err();
    assert_eq!(error.to_string(), "the output was not drained in time");
    drop(release);
}

/// Whether no process of the number `pid` is left, asked up to 5000 times (a process that was
/// killed stays as a zombie until whoever adopted it reaps it). No clock is compared: the
/// answer is a count of asks.
fn is_gone(pid: u32) -> bool {
    let pid = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    std::iter::repeat_with(|| {
        let gone = test_kill_process(pid) == Err(Errno::SRCH);
        std::thread::sleep(Duration::from_millis(1));
        gone
    })
    .take(5000)
    .any(|gone| gone)
}

const HELPER: &str = "server::update_hook::machine::runner_tests::";

/// The program of the first test below: it starts a sleeper that never ends, which keeps the
/// pipes it was given, says its number on standard output, and ends at once. It does nothing in
/// a run of the tests (it is ignored); the test below runs this binary with it selected.
#[test]
#[ignore = "a helper process of a_helper_left_running_by_the_program_is_killed_with_it"]
#[allow(clippy::zombie_processes)]
fn leaves_a_sleeper_holding_the_pipes() {
    let sleeper = Command::new("sleep").arg("infinity").spawn().unwrap();
    println!("sleeper {}", sleeper.id());
}

/// Asserts that a program that has ended but left a process of its group holding its output
/// pipes (it never ends) does not hold up the call: the group is killed, the answer is the
/// program's own, and the sleeper, whose number the program printed, is gone. An implementation
/// that waits for the pipes of a sleeper that never ends does not return at all.
#[test]
fn a_helper_left_running_by_the_program_is_killed_with_it() {
    let this = std::env::current_exe().unwrap();
    let name = format!("{HELPER}leaves_a_sleeper_holding_the_pipes");
    let args = ["--exact", name.as_str(), "--ignored", "--nocapture"];
    let call = Call::new(this.to_str().unwrap(), &args, LIMIT);
    let output = SystemRunner.run(&call).unwrap();
    assert_eq!(output.code, Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    let (_, rest) = text.split_once("sleeper ").unwrap();
    let pid = rest.split_whitespace().next().unwrap().parse().unwrap();
    assert!(is_gone(pid));
}

/// Where the helper below writes the number of its child.
fn pid_file() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/tierstore-runner-test-{}", std::process::id()))
}

/// The program of the second test below: it starts a sleeper that never ends, writes its number
/// to a file named for the process that runs it, and then waits for ever. Ignored, like the one
/// above.
#[test]
#[ignore = "a helper process of a_timeout_kills_the_program_and_the_child_that_holds_the_pipes"]
#[allow(clippy::zombie_processes)]
fn waits_for_ever_with_a_sleeper_holding_the_pipes() {
    let sleeper = Command::new("sleep").arg("infinity").spawn().unwrap();
    let parent = std::os::unix::process::parent_id();
    let file = format!("/tmp/tierstore-runner-test-{parent}");
    std::fs::write(file, sleeper.id().to_string()).unwrap();
    loop {
        std::thread::park();
    }
}

/// Asserts that a program that runs past its limit while its child, which never ends, holds the
/// output pipes, is reported as a timeout, and that the child is gone afterwards. An
/// implementation that kills only the program and then waits for the pipes does not return.
#[test]
fn a_timeout_kills_the_program_and_the_child_that_holds_the_pipes() {
    let this = std::env::current_exe().unwrap();
    let name = format!("{HELPER}waits_for_ever_with_a_sleeper_holding_the_pipes");
    let args = ["--exact", name.as_str(), "--ignored"];
    let limit = Duration::from_secs(3);
    let call = Call::new(this.to_str().unwrap(), &args, limit);
    assert_eq!(SystemRunner.run(&call), Err(RunError::Timeout(limit)));
    let text = std::fs::read_to_string(pid_file()).unwrap();
    std::fs::remove_file(pid_file()).unwrap();
    assert!(is_gone(text.parse().unwrap()));
}

/// Asserts that dropping the guard of a child kills it and reaps it: no process of its number
/// is left, a zombie included. The runner leaves every path through it, an error's too.
#[test]
fn dropping_the_group_kills_and_reaps_the_child() {
    let mut command = Command::new("sleep");
    command.arg("30");
    let group = Group::spawn(&mut command).unwrap();
    let pid = Pid::from_child(&group.child);
    drop(group);
    assert_eq!(test_kill_process(pid), Err(Errno::SRCH));
}
