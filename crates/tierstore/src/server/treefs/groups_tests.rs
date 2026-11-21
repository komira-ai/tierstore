//! What the lookup of a group name reads from the system's tool, for both tools, with no system:
//! the outputs are the ones the tools print (the `dscacheutil` ones as macOS prints them for a
//! group of the directory service and for a name it does not have).

use std::os::unix::process::ExitStatusExt as _;
use std::process::{ExitStatus, Output};

use super::{Found, Tool, answer};

/// An output with the exit status `code` and the two texts.
fn said(code: i32, stdout: &str, stderr: &str) -> std::io::Result<Output> {
    Ok(Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    })
}

/// What `dscacheutil` said, read.
fn dscacheutil(code: i32, stdout: &str, stderr: &str) -> Found {
    answer(Tool::Dscacheutil, "dscacheutil", said(code, stdout, stderr))
}

/// What `getent` said, read.
fn getent(code: i32, stdout: &str, stderr: &str) -> Found {
    answer(Tool::Getent, "getent", said(code, stdout, stderr))
}

/// Asserts the command of each tool: the group database queried by name, the name last.
#[test]
fn each_tool_asks_for_a_group_by_name() {
    let command = |tool: Tool| [&[tool.program()][..], tool.arguments()].concat().join(" ");
    assert_eq!(
        command(Tool::Dscacheutil),
        "/usr/bin/dscacheutil -q group -a name"
    );
    assert_eq!(command(Tool::Getent), "/usr/bin/getent group");
}

/// Asserts that a group `dscacheutil` prints is its `gid` line, with or without a `users` line,
/// and that the empty output it gives for a name it does not have is no group.
#[test]
fn dscacheutil_gives_the_gid_of_a_group_and_nothing_for_an_absent_one() {
    let service_group = "name: _tierstore\npassword: *\ngid: 394\n\n";
    assert_eq!(dscacheutil(0, service_group, ""), Ok(Some(394)));
    let with_users = "name: staff\npassword: *\ngid: 20\nusers: root \n\n";
    assert_eq!(dscacheutil(0, with_users, ""), Ok(Some(20)));
    assert_eq!(dscacheutil(0, "", ""), Ok(None));
    assert_eq!(dscacheutil(0, "\n", ""), Ok(None));
}

/// Asserts that a group `getent` prints is the third field of its line, a group with members
/// included, and that its status 2 (no such key) is no group.
#[test]
fn getent_gives_the_gid_of_a_group_and_nothing_for_an_absent_one() {
    assert_eq!(getent(0, "tierstore:x:997:\n", ""), Ok(Some(997)));
    assert_eq!(getent(0, "sudo:x:27:alice,bob\n", ""), Ok(Some(27)));
    assert_eq!(getent(2, "", ""), Ok(None));
}

/// Asserts each failure, by its exact words: a tool that could not be run, one that failed (for
/// `dscacheutil`, status 2 is a failure, not an absent name), an output with no group id in it,
/// and a group id that is not a number.
#[test]
fn every_failure_of_the_lookup_says_what_the_tool_did() {
    let gone = answer(
        Tool::Getent,
        "/usr/bin/getent",
        Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
    );
    assert_eq!(
        gone,
        Err("/usr/bin/getent could not be run: entity not found".to_owned())
    );
    assert_eq!(
        getent(1, "", "getent: bad database\n"),
        Err("getent failed (exit status: 1): getent: bad database".to_owned())
    );
    assert_eq!(
        dscacheutil(2, "", "no such query\n"),
        Err("dscacheutil failed (exit status: 2): no such query".to_owned())
    );
    assert_eq!(
        dscacheutil(0, "name: odd\npassword: *\n", ""),
        Err("dscacheutil said no group id: name: odd\npassword: *".to_owned())
    );
    assert_eq!(
        getent(0, "odd\n", ""),
        Err("getent said no group id: odd".to_owned())
    );
    assert_eq!(
        getent(0, "odd:x:notanumber:\n", ""),
        Err("getent said the group id \"notanumber\", which is not a number".to_owned())
    );
}
