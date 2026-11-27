//! `tierstore fs` on macOS as a process: the command line is parsed, the file is checked (a group
//! named by name, and an owner by names, are looked up through the system), and a start of the verb
//! exits 2 and says the file system is the `fs` interface of the node's file. The verb serves
//! nothing, so nothing is mounted and no assertion is about time.

#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::os::unix::fs::MetadataExt as _;
use std::process::Command;

use tierstore_testkit::dir::TestDir;

/// What `/usr/bin/id` says with `flag` of the user of this test, without the newline.
fn id(flag: &str) -> String {
    let out = Command::new("/usr/bin/id").arg(flag).output().unwrap();
    assert!(out.status.success(), "id {flag}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Runs `tierstore fs --config <file> --check-config` over a file whose `fs.client_group` is
/// `group` and `fs.owner` is `owner` and whose other keys are valid, and gives the process's
/// output.
fn check(name: &str, group: &str, owner: &str) -> (std::process::Output, TestDir) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let text = format!(
        "[fs]\nclient_group = \"{group}\"\nowner = \"{owner}\"\ncache_path = {:?}\n",
        dir.path().join("cache"),
    );
    let file = dir.path().join("fs.toml");
    std::fs::write(&file, text).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .arg("fs")
        .arg("--config")
        .arg(&file)
        .arg("--check-config")
        .output()
        .expect("the binary runs");
    (output, dir)
}

/// Asserts that `fs.client_group` given by name is looked up through the system (Open Directory,
/// by `dscacheutil`), not read from `/etc/group`: `--check-config` of a file that names the primary
/// group of the user of the test exits 0 and says nothing.
#[test]
fn fs_takes_its_client_group_by_name_from_the_system() {
    let name = id("-gn");
    let gid = std::fs::metadata(std::env::temp_dir()).unwrap().gid();
    let owner = format!("{}:{gid}", id("-u"));
    let (output, _dir) = check("fs-macos-group", &name, &owner);
    assert_eq!(output.status.code(), Some(0), "{name}: {output:?}");
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "{output:?}"
    );
}

/// Asserts that `fs.owner` given as `<user>:<group>` by the names of the user of this test (their
/// accounts of Open Directory, looked up by `dscacheutil`) is taken: `--check-config` exits 0 and
/// says nothing.
#[test]
fn fs_takes_its_owner_by_names_from_the_system() {
    let owner = format!("{}:{}", id("-un"), id("-gn"));
    let (output, _dir) = check("fs-macos-owner", &id("-g"), &owner);
    assert_eq!(output.status.code(), Some(0), "{owner}: {output:?}");
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "{output:?}"
    );
}

/// Asserts that an `fs.owner` whose user is a name the system does not have is refused: status 2
/// and a line that names `fs.owner` and the name. Catches a name taken as some default user.
#[test]
fn fs_refuses_an_owner_whose_user_the_system_does_not_have() {
    let owner = format!("no-such-user-tierstore:{}", id("-gn"));
    let (output, _dir) = check("fs-macos-owner-absent", &id("-g"), &owner);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let said = String::from_utf8(output.stderr).unwrap();
    assert!(
        said.starts_with("tierstore fs: fs.owner: \"no-such-user-tierstore\" is neither a user id"),
        "{said}"
    );
}

/// Asserts that `tierstore fs --config <file>` with no `attachments` and no `--check-config` is no
/// process of its own: status 2, nothing on stdout, and on stderr the one line that names the
/// `fs` interface of the node's file, whatever the file (it need not exist).
#[test]
fn fs_is_not_a_process_and_names_the_interface() {
    let dir = TestDir::under(std::env::temp_dir(), "fs-macos-not-a-process");
    let output = Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .arg("fs")
        .arg("--config")
        .arg(dir.path().join("fs.toml"))
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let said = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        said,
        "tierstore fs is no longer a process of its own: the file system is the [[interface]] \
         with type = \"fs\" of the node's file\n"
    );
}
