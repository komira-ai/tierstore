//! What the lookup of a group or user name reads from the system's tool, for both tools and both
//! databases, with no system: the outputs are the ones the tools print (the `dscacheutil` ones as
//! macOS prints them for an account of the directory service and for a name it does not have).

use std::os::unix::process::ExitStatusExt as _;
use std::process::{ExitStatus, Output};

use super::{Database, Found, Tool, answer, lookup, spawn};

/// An output with the exit status `code` and the two texts.
fn said(code: i32, stdout: &str, stderr: &str) -> std::io::Result<Output> {
    Ok(Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    })
}

/// What `dscacheutil` said of a group, read.
fn dscacheutil(code: i32, stdout: &str, stderr: &str) -> Found {
    answer(
        Tool::Dscacheutil,
        Database::Group,
        "dscacheutil",
        said(code, stdout, stderr),
    )
}

/// What `getent` said of a group, read.
fn getent(code: i32, stdout: &str, stderr: &str) -> Found {
    answer(
        Tool::Getent,
        Database::Group,
        "getent",
        said(code, stdout, stderr),
    )
}

/// What `dscacheutil` said of a user, read.
fn dscacheutil_user(code: i32, stdout: &str, stderr: &str) -> Found {
    answer(
        Tool::Dscacheutil,
        Database::User,
        "dscacheutil",
        said(code, stdout, stderr),
    )
}

/// What `getent` said of a user, read.
fn getent_user(code: i32, stdout: &str, stderr: &str) -> Found {
    answer(
        Tool::Getent,
        Database::User,
        "getent",
        said(code, stdout, stderr),
    )
}

/// Asserts the command of each tool for each database: the database queried by name, the name
/// last, and the word that names what the database holds.
#[test]
fn each_tool_asks_for_a_group_or_a_user_by_name() {
    let command = |tool: Tool, database: Database| {
        [&[tool.program()][..], tool.arguments(database)]
            .concat()
            .join(" ")
    };
    assert_eq!(
        command(Tool::Dscacheutil, Database::Group),
        "/usr/bin/dscacheutil -q group -a name"
    );
    assert_eq!(
        command(Tool::Dscacheutil, Database::User),
        "/usr/bin/dscacheutil -q user -a name"
    );
    assert_eq!(
        command(Tool::Getent, Database::Group),
        "/usr/bin/getent group"
    );
    assert_eq!(
        command(Tool::Getent, Database::User),
        "/usr/bin/getent passwd"
    );
    assert_eq!(Database::Group.noun(), "group");
    assert_eq!(Database::User.noun(), "user");
}

/// Asserts that a lookup runs the tool of the system with the name last, and reads its output by
/// that tool's rules: `dscacheutil` on macOS, `getent` on Linux and any other system.
#[test]
fn a_lookup_runs_the_tool_of_its_system() {
    let ran = std::cell::RefCell::new(Vec::new());
    let run = |program: &str, arguments: &[&str]| {
        ran.borrow_mut()
            .push([&[program][..], arguments].concat().join(" "));
        match program {
            "/usr/bin/dscacheutil" => said(
                0,
                "name: _tierstore\npassword: *\nuid: 394\ngid: 394\n\n",
                "",
            ),
            _ => said(2, "", ""),
        }
    };
    let group = Database::Group;
    assert_eq!(lookup("macos", group, "_tierstore", &run), Ok(Some(394)));
    assert_eq!(lookup("linux", group, "_tierstore", &run), Ok(None));
    assert_eq!(lookup("freebsd", group, "builders", &run), Ok(None));
    assert_eq!(
        lookup("macos", Database::User, "_tierstore", &run),
        Ok(Some(394))
    );
    assert_eq!(lookup("linux", Database::User, "alice", &run), Ok(None));
    assert_eq!(
        ran.into_inner(),
        [
            "/usr/bin/dscacheutil -q group -a name _tierstore",
            "/usr/bin/getent group _tierstore",
            "/usr/bin/getent group builders",
            "/usr/bin/dscacheutil -q user -a name _tierstore",
            "/usr/bin/getent passwd alice",
        ]
    );
}

/// Asserts the runner of the lookup: a program's status and both outputs, with no standard input
/// (a `cat` of it ends at once), and a program that is not there.
#[test]
fn the_runner_gives_what_a_program_said() {
    let ran = spawn("/bin/sh", &["-c", "cat; echo out; echo err >&2; exit 2"]).unwrap();
    assert_eq!(ran.status.code(), Some(2));
    assert_eq!(
        (&ran.stdout[..], &ran.stderr[..]),
        (&b"out\n"[..], &b"err\n"[..])
    );
    let missing = spawn("/nonexistent/program", &[]).unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
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
        Database::Group,
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

/// Asserts that a user `dscacheutil` prints is its `uid` line (not the `gid` line before or after
/// it), and that the empty output it gives for a name it does not have is no user. Catches a user
/// id read from the `gid` line.
#[test]
fn dscacheutil_gives_the_uid_of_a_user_and_nothing_for_an_absent_one() {
    let account = "name: _tierstore\npassword: *\nuid: 394\ngid: 395\ndir: /var/empty\n\
                   shell: /usr/bin/false\n\n";
    assert_eq!(dscacheutil_user(0, account, ""), Ok(Some(394)));
    let gid_first = "name: odd\ngid: 20\nuid: 501\n";
    assert_eq!(dscacheutil_user(0, gid_first, ""), Ok(Some(501)));
    assert_eq!(dscacheutil_user(0, "", ""), Ok(None));
    assert_eq!(dscacheutil_user(0, "\n", ""), Ok(None));
}

/// Asserts that a user `getent passwd` prints is the third field of its line, and that its status
/// 2 (no such key) is no user. Catches a user id read from the fourth field, the group id.
#[test]
fn getent_gives_the_uid_of_a_user_and_nothing_for_an_absent_one() {
    let line = "alice:x:1000:1001:Alice:/home/alice:/bin/sh\n";
    assert_eq!(getent_user(0, line, ""), Ok(Some(1000)));
    assert_eq!(getent_user(2, "", ""), Ok(None));
}

/// Asserts each failure of a user lookup by its exact words: they name a user id, not a group id.
#[test]
fn every_failure_of_a_user_lookup_says_what_the_tool_did() {
    assert_eq!(
        getent_user(1, "", "getent: bad database\n"),
        Err("getent failed (exit status: 1): getent: bad database".to_owned())
    );
    assert_eq!(
        dscacheutil_user(2, "", "no such query\n"),
        Err("dscacheutil failed (exit status: 2): no such query".to_owned())
    );
    assert_eq!(
        dscacheutil_user(0, "name: odd\ngid: 20\n", ""),
        Err("dscacheutil said no user id: name: odd\ngid: 20".to_owned())
    );
    assert_eq!(
        getent_user(0, "odd\n", ""),
        Err("getent said no user id: odd".to_owned())
    );
    assert_eq!(
        getent_user(0, "odd:x:notanumber:1:\n", ""),
        Err("getent said the user id \"notanumber\", which is not a number".to_owned())
    );
}
