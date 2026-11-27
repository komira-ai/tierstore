//! `tierstore fs` on macOS as a process: a start of the verb exits 2 and says the file system is the
//! `fs` interface of the node's file. The verb serves nothing, so nothing is mounted and no
//! assertion is about time. (The lookup of a group and an owner by name through the system is
//! checked over a node's file in `fs_interface_macos_integration.rs`.)

#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::process::Command;

use tierstore_testkit::dir::TestDir;

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
