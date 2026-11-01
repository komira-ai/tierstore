//! `nft` through a recording runner: the argument vector and the standard input of every call,
//! and what each outcome of the program becomes.

use std::time::Duration;

use super::super::ports::{Nft, PortError, PortKind};
use super::testing::{FakeRunner, LIMIT, exit, signalled};
use super::{Call, NftTool, RunError};

const ABSENT: &str = "Error: No such file or directory\nlist table inet kbf_x\n          ^^^^^^^\n";

fn tool(runner: &FakeRunner) -> NftTool<'_> {
    NftTool::new(runner, LIMIT)
}

fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Nft,
            message,
        } => message,
        other => panic!("not an nft failure: {other:?}"),
    }
}

/// Asserts that `apply` runs `nft -f -` with the ruleset text, byte for byte, on standard
/// input and nothing in the arguments, under the time limit it was given. It would catch a
/// ruleset passed as an argument or through a shell, or written to a file.
#[test]
fn apply_runs_nft_with_the_ruleset_on_stdin() {
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    tool(&runner).apply("add table inet t\n").unwrap();
    let want = Call::new("/usr/sbin/nft", &["-f", "-"], LIMIT).with_stdin(b"add table inet t\n");
    assert_eq!(runner.calls(), vec![want]);
}

/// Asserts that a non-zero exit of `nft -f -` is a failure of the nft port that carries the
/// command, the status and the program's own words. It would catch a failed transaction that is
/// taken for applied.
#[test]
fn apply_fails_on_a_non_zero_exit_with_the_programs_words() {
    let runner = FakeRunner::replying(vec![exit(1, "", "Error: syntax error\n")]);
    let text = message(tool(&runner).apply("x").unwrap_err());
    assert_eq!(text, "/usr/sbin/nft -f -: exited 1: Error: syntax error");
}

/// Asserts that a program a signal ended is a failure that says so.
#[test]
fn apply_fails_when_a_signal_ends_the_program() {
    let runner = FakeRunner::replying(vec![signalled()]);
    let text = message(tool(&runner).apply("x").unwrap_err());
    assert_eq!(text, "/usr/sbin/nft -f -: ended by a signal: ");
}

/// Asserts that a call that runs past its time, and one that cannot be started, are failures
/// of the nft port that name the command.
#[test]
fn apply_fails_on_a_timeout_and_on_a_program_that_cannot_start() {
    let runner = FakeRunner::replying(vec![
        Err(RunError::Timeout(Duration::from_secs(7))),
        Err(RunError::Spawn(
            "No such file or directory (os error 2)".to_owned(),
        )),
    ]);
    let text = message(tool(&runner).apply("x").unwrap_err());
    assert_eq!(text, "/usr/sbin/nft -f -: no end after 7s, killed");
    let text = message(tool(&runner).apply("x").unwrap_err());
    assert_eq!(
        text,
        "/usr/sbin/nft -f -: cannot run: No such file or directory (os error 2)"
    );
}

/// Asserts that `list_table` asks `nft list table inet <name>` and returns standard output.
#[test]
fn list_table_returns_the_listing() {
    let runner = FakeRunner::replying(vec![exit(0, "table inet t {\n}\n", "")]);
    let got = tool(&runner).list_table("t").unwrap();
    assert_eq!(got.as_deref(), Some("table inet t {\n}\n"));
    let want = Call::new("/usr/sbin/nft", &["list", "table", "inet", "t"], LIMIT);
    assert_eq!(runner.calls(), vec![want]);
}

/// Asserts that a table that is not there, which is a non-zero exit whose standard error holds
/// ENOENT's text, is `None`. It would catch an absent table that is reported as a failure
/// (every first `prepare` would defer) or any failure taken for absence.
#[test]
fn list_table_says_none_for_a_table_that_is_not_there() {
    let runner = FakeRunner::replying(vec![exit(1, "", ABSENT)]);
    assert_eq!(tool(&runner).list_table("kbf_x").unwrap(), None);
}

/// Asserts that only a non-zero exit counts as absence: a zero exit whose standard error happens
/// to hold the words is a listing.
#[test]
fn list_table_reads_a_zero_exit_as_a_listing_whatever_it_says_on_stderr() {
    let runner = FakeRunner::replying(vec![exit(0, "listing", ABSENT)]);
    assert_eq!(
        tool(&runner).list_table("t").unwrap().as_deref(),
        Some("listing")
    );
}

/// Asserts that a failure that is not an absent table (no permission), a timeout and a program
/// that cannot start are errors, not `None`.
#[test]
fn list_table_fails_on_any_other_failure() {
    let runner = FakeRunner::replying(vec![
        exit(1, "", "Error: Operation not permitted\n"),
        Err(RunError::Timeout(LIMIT)),
    ]);
    let text = message(tool(&runner).list_table("t").unwrap_err());
    assert_eq!(
        text,
        "/usr/sbin/nft list table inet t: exited 1: Error: Operation not permitted"
    );
    assert!(tool(&runner).list_table("t").is_err());
}

/// Asserts that `delete_table` runs `nft delete table inet <name>`.
#[test]
fn delete_table_runs_nft_delete() {
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    tool(&runner).delete_table("t").unwrap();
    let want = Call::new("/usr/sbin/nft", &["delete", "table", "inet", "t"], LIMIT);
    assert_eq!(runner.calls(), vec![want]);
}

/// Asserts that deleting a table that is not there is fine, which is what makes a rollback that
/// runs twice end the same.
#[test]
fn delete_table_of_an_absent_table_is_ok() {
    let runner = FakeRunner::replying(vec![exit(1, "", ABSENT)]);
    tool(&runner).delete_table("kbf_x").unwrap();
}

/// Asserts that another failure of the delete, and a timeout, are errors.
#[test]
fn delete_table_fails_on_any_other_failure() {
    let runner = FakeRunner::replying(vec![
        exit(1, "", "Error: Operation not permitted\n"),
        Err(RunError::Timeout(LIMIT)),
    ]);
    let text = message(tool(&runner).delete_table("t").unwrap_err());
    assert_eq!(
        text,
        "/usr/sbin/nft delete table inet t: exited 1: Error: Operation not permitted"
    );
    assert!(tool(&runner).delete_table("t").is_err());
}

/// Asserts that a table name outside letters, digits and underscores (or empty, longer than 64
/// characters, or starting with a digit) is refused by the listing and by the delete before any
/// program runs: no call is made.
#[test]
fn a_table_name_outside_the_alphabet_is_refused_before_any_call() {
    let runner = FakeRunner::replying(vec![]);
    let long = "t".repeat(65);
    for name in ["", "1t", "a b", "a;b", "-x", "t\n", "a.b", long.as_str()] {
        let text = message(tool(&runner).list_table(name).unwrap_err());
        assert!(text.ends_with("is not a table name"), "{text}");
        assert!(tool(&runner).delete_table(name).is_err());
    }
    assert!(runner.calls().is_empty());
}

/// Asserts that names that follow the rule are passed on: one starting with an underscore, one
/// with digits, and one of 64 characters.
#[test]
fn a_table_name_inside_the_alphabet_is_passed_on() {
    let runner = FakeRunner::replying(vec![exit(0, "", ""), exit(0, "", ""), exit(0, "", "")]);
    let longest = "t".repeat(64);
    for name in ["_a1", "kbf_forward_tierstore", longest.as_str()] {
        assert!(tool(&runner).list_table(name).is_ok());
    }
    assert_eq!(runner.calls().len(), 3);
}

/// Asserts that the program can be named by the caller, and is that program in every call.
#[test]
fn the_program_can_be_named() {
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    tool(&runner).with_program("/opt/nft").apply("x").unwrap();
    assert_eq!(runner.calls()[0].program, "/opt/nft");
}
