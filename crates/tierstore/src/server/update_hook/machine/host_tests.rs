//! The host over a fixture root, and the capabilities over a recording runner.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use std::time::Duration;
use tierstore_testkit::dir::TestDir;

use super::super::ports::{Conntrack, Host, PortError, PortKind};
use super::testing::{FakeRunner, LIMIT, exit, scratch, signalled};
use super::{Call, RunError, SystemHost};

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn host<'a>(root: &Path, runner: &'a FakeRunner) -> SystemHost<'a> {
    SystemHost::new(root.to_path_buf(), runner, LIMIT)
        .with_release_owner(me())
        .with_declaration_owner(me())
}

fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Host,
            message,
        } => message,
        other => panic!("not a host failure: {other:?}"),
    }
}

const CONNTRACK: &str = "proc/sys/net/netfilter";

fn conntrack_root(root: &Path) {
    write(
        root,
        &format!("{CONNTRACK}/nf_conntrack_tcp_timeout_established"),
        "432000\n",
    );
    write(root, &format!("{CONNTRACK}/nf_conntrack_count"), "1234\n");
    write(root, &format!("{CONNTRACK}/nf_conntrack_max"), "262144\n");
}

/// Asserts that the declaration is read whole from `etc/updater.d/tierstore.toml` under the
/// root, and that a root without it is a failure of the host port that names the path.
#[test]
fn the_declaration_is_read_from_its_place_under_the_root() {
    let (_guard, root) = scratch("host-declaration");
    let runner = FakeRunner::replying(vec![]);
    let text = message(host(&root, &runner).declaration().unwrap_err());
    assert!(text.contains("etc/updater.d/tierstore.toml"), "{text}");
    write(
        &root,
        "etc/updater.d/tierstore.toml",
        "state_dir = \"/x\"\n",
    );
    assert_eq!(
        host(&root, &runner).declaration().unwrap(),
        "state_dir = \"/x\"\n"
    );
}

/// Asserts that the boot id is the content of `proc/sys/kernel/random/boot_id` without its
/// newline, and that a missing file is a failure.
#[test]
fn the_boot_id_is_read_and_trimmed() {
    let (_guard, root) = scratch("host-boot");
    let runner = FakeRunner::replying(vec![]);
    assert!(host(&root, &runner).boot_id().is_err());
    write(&root, "proc/sys/kernel/random/boot_id", "0b1c-22\n");
    assert_eq!(host(&root, &runner).boot_id().unwrap(), "0b1c-22");
}

/// Asserts that the three connection-tracking values are read, each from its own file. It would
/// catch two values swapped.
#[test]
fn the_conntrack_values_are_read_from_their_files() {
    let (_guard, root) = scratch("host-conntrack");
    conntrack_root(&root);
    let runner = FakeRunner::replying(vec![]);
    assert_eq!(
        host(&root, &runner).conntrack().unwrap(),
        Conntrack {
            tcp_timeout_established_secs: 432_000,
            count: 1234,
            max: 262_144,
        }
    );
}

/// Asserts that a value that is not a number, and a file that is missing, are failures that name
/// the value (a host without the conntrack module has no files).
#[test]
fn a_conntrack_value_that_is_not_a_number_or_is_missing_is_a_failure() {
    let (_guard, root) = scratch("host-conntrack-bad");
    let runner = FakeRunner::replying(vec![]);
    assert!(host(&root, &runner).conntrack().is_err());
    conntrack_root(&root);
    write(&root, &format!("{CONNTRACK}/nf_conntrack_count"), "many\n");
    let text = message(host(&root, &runner).conntrack().unwrap_err());
    assert!(
        text.starts_with("nf_conntrack_count is not a number"),
        "{text}"
    );
}

/// A release directory with a binary of mode `bits`.
fn release_with(bits: u32) -> (TestDir, PathBuf) {
    let (guard, root) = scratch("host-release");
    let binary = root.join("tierstore");
    fs::write(&binary, "#!").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(bits)).unwrap();
    (guard, root)
}

fn release() -> (TestDir, PathBuf) {
    release_with(0o755)
}

/// Asserts that the capabilities are the lines of `<release>/tierstore --capabilities`,
/// trimmed, with empty lines dropped, from a call with that argument vector and no input. It
/// would catch the wrong binary path or a flag passed through a shell.
#[test]
fn capabilities_are_the_lines_of_the_releases_binary() {
    let (_guard, release) = release();
    let runner = FakeRunner::replying(vec![exit(0, "handover-v1\n\n  other \n", "")]);
    let got = host(Path::new("/"), &runner)
        .capabilities(&release)
        .unwrap();
    assert_eq!(got, vec!["handover-v1".to_owned(), "other".to_owned()]);
    let binary = release.join("tierstore");
    let want = Call::new(binary.to_str().unwrap(), &["--capabilities"], LIMIT);
    assert_eq!(runner.calls(), vec![want]);
}

/// Asserts that a build with no capabilities, which prints nothing and exits 0, has none.
#[test]
fn a_build_with_none_has_none() {
    let (_guard, release) = release();
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    assert!(
        host(Path::new("/"), &runner)
            .capabilities(&release)
            .unwrap()
            .is_empty()
    );
}

/// Asserts that a binary that does not know the flag is a release with no capabilities, and
/// that this is detected as exit status 2 (the usage error of the argument parser) and nothing
/// else: its standard output is not read.
#[test]
fn a_binary_that_does_not_know_the_flag_has_no_capabilities() {
    let (_guard, release) = release();
    let runner = FakeRunner::replying(vec![exit(2, "ignored\n", "error: unexpected argument\n")]);
    assert!(
        host(Path::new("/"), &runner)
            .capabilities(&release)
            .unwrap()
            .is_empty()
    );
}

/// Asserts that any other failure is an error and not an empty list (which would send an
/// update down the restart path because a release could not be asked): another exit status, a
/// signal, a timeout, a binary that cannot be started.
#[test]
fn a_binary_that_cannot_be_asked_is_an_error() {
    let (_guard, release) = release();
    let runner = FakeRunner::replying(vec![
        exit(1, "", "boom\n"),
        signalled(),
        Err(RunError::Timeout(Duration::from_secs(7))),
        Err(RunError::Spawn(
            "No such file or directory (os error 2)".to_owned(),
        )),
    ]);
    let host = host(Path::new("/"), &runner);
    let text = message(host.capabilities(&release).unwrap_err());
    assert_eq!(
        text,
        format!(
            "{}/tierstore --capabilities: exited 1: boom",
            release.display()
        )
    );
    for _ in 0..3 {
        assert!(host.capabilities(&release).is_err());
    }
}

/// Asserts that a binary that a group or others can write is not run: no call is made.
#[test]
fn a_binary_that_others_can_write_is_not_run() {
    for bits in [0o775, 0o757] {
        let (_guard, release) = release_with(bits);
        let runner = FakeRunner::replying(vec![]);
        let text = message(
            host(Path::new("/"), &runner)
                .capabilities(&release)
                .unwrap_err(),
        );
        assert!(
            text.ends_with("can be written by a group or others"),
            "{text}"
        );
        assert!(runner.calls().is_empty());
    }
}

/// Asserts that a binary of another owner than the release owner is not run (the default owner
/// is root, which these tests are not).
#[test]
fn a_binary_of_another_owner_is_not_run() {
    let (_guard, release) = release();
    let runner = FakeRunner::replying(vec![]);
    let strange = SystemHost::new(PathBuf::from("/"), &runner, LIMIT).with_release_owner(me() + 1);
    let text = message(strange.capabilities(&release).unwrap_err());
    assert!(
        text.contains(&format!("is owned by user {}, not {}", me(), me() + 1)),
        "{text}"
    );
    assert!(runner.calls().is_empty());
}

/// Asserts that a link in place of the binary, a directory, and no binary at all are not run.
#[test]
fn a_binary_that_is_a_link_a_directory_or_missing_is_not_run() {
    let (_guard, release) = release();
    let runner = FakeRunner::replying(vec![]);
    let host = host(Path::new("/"), &runner);
    let binary = release.join("tierstore");
    fs::remove_file(&binary).unwrap();
    assert!(message(host.capabilities(&release).unwrap_err()).contains("No such file"));
    symlink("/bin/true", &binary).unwrap();
    assert!(message(host.capabilities(&release).unwrap_err()).ends_with("is not a regular file"));
    fs::remove_file(&binary).unwrap();
    fs::create_dir(&binary).unwrap();
    assert!(message(host.capabilities(&release).unwrap_err()).ends_with("is not a regular file"));
    assert!(runner.calls().is_empty());
}
