//! The real runner, over ordinary programs of the host (`cat`, `ls`, `echo`, `head`, `sleep`).

use std::time::Duration;

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
