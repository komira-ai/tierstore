//! The host over a fixture root, and the capabilities over a recording runner.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::super::ports::{Host, PortError, PortKind};
use super::host::CONFIG_MAX;
use super::testing::{FakeRunner, LIMIT, exit, me, scratch, signalled};
use super::{Call, RunError, SystemHost};

const RELEASE: &str = "/packages/tierstore/versions/0.5.0";

fn host<'a>(root: &Path, runner: &'a FakeRunner) -> SystemHost<'a> {
    SystemHost::new(root.to_path_buf(), runner, LIMIT).with_release_owner(me())
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

fn mode(path: &Path, bits: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
}

/// Where `RELEASE` is under `root`.
fn release_dir(root: &Path) -> PathBuf {
    root.join(RELEASE.trim_start_matches('/'))
}

/// A release under `root` with `bin/tierstore` of mode `bits`.
fn release_with(root: &Path, bits: u32) {
    let bin = release_dir(root).join("bin");
    fs::create_dir_all(&bin).unwrap();
    mode(&bin, 0o755);
    let binary = bin.join("tierstore");
    fs::write(&binary, "#!").unwrap();
    mode(&binary, bits);
}

/// Asserts that the configuration file is read whole from its path under the root, whether the
/// path is given as an absolute path or not.
#[test]
fn the_config_is_read_from_its_place_under_the_root() {
    let (_guard, root) = scratch("host-config");
    fs::create_dir_all(root.join("etc/tierstore")).unwrap();
    fs::write(root.join("etc/tierstore/config.toml"), "[server]\nx = 1\n").unwrap();
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner);
    let want = Some("[server]\nx = 1\n".to_owned());
    assert_eq!(
        host.config(Path::new("/etc/tierstore/config.toml"))
            .unwrap(),
        want
    );
    assert_eq!(
        host.config(Path::new("etc/tierstore/config.toml")).unwrap(),
        want
    );
}

/// Asserts that a configuration file that is not there is `None` and not an error.
#[test]
fn a_config_that_is_absent_is_none() {
    let (_guard, root) = scratch("host-config-absent");
    let runner = FakeRunner::replying(vec![]);
    assert_eq!(
        host(&root, &runner)
            .config(Path::new("/etc/none.toml"))
            .unwrap(),
        None
    );
}

/// Asserts that a configuration file that cannot be read (a directory), one that cannot be
/// opened (its parent is a file), one that is not UTF-8 and one larger than the cap are errors
/// that name the path; one of exactly the cap is read.
#[test]
fn a_config_that_cannot_be_read_is_an_error() {
    let (_guard, root) = scratch("host-config-bad");
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner);
    fs::create_dir(root.join("dir")).unwrap();
    let text = message(host.config(Path::new("/dir")).unwrap_err());
    assert!(
        text.starts_with(&format!("read {}", root.join("dir").display())),
        "{text}"
    );
    fs::write(root.join("binary"), [0xff, 0xfe]).unwrap();
    assert!(message(host.config(Path::new("/binary")).unwrap_err()).contains("UTF-8"));
    let text = message(host.config(Path::new("/binary/config.toml")).unwrap_err());
    assert!(
        text.starts_with(&format!(
            "read {}",
            root.join("binary/config.toml").display()
        )),
        "{text}"
    );
    let cap = usize::try_from(CONFIG_MAX).unwrap();
    fs::write(root.join("exact"), "a".repeat(cap)).unwrap();
    assert_eq!(
        host.config(Path::new("/exact")).unwrap().unwrap().len(),
        cap
    );
    fs::write(root.join("large"), "a".repeat(cap + 1)).unwrap();
    let text = message(host.config(Path::new("/large")).unwrap_err());
    assert!(
        text.ends_with(&format!("is larger than {CONFIG_MAX} bytes")),
        "{text}"
    );
}

/// Asserts that the capabilities are the lines of `<release>/bin/tierstore --capabilities`,
/// trimmed, with empty lines dropped, from a call with that argument vector and no input, and
/// that the program is the binary under `bin/` of the release.
#[test]
fn capabilities_are_the_lines_of_the_releases_binary_under_bin() {
    let (_guard, root) = scratch("host-caps");
    release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![exit(0, "handoff-socket-v1\n\n  other \n", "")]);
    let got = host(&root, &runner)
        .capabilities(Path::new(RELEASE))
        .unwrap();
    assert_eq!(
        got,
        vec!["handoff-socket-v1".to_owned(), "other".to_owned()]
    );
    let binary = release_dir(&root).join("bin/tierstore");
    let want = Call::new(binary.to_str().unwrap(), &["--capabilities"], LIMIT);
    assert_eq!(runner.calls(), vec![want]);
}

/// Asserts that a build with no capabilities, which prints nothing and exits 0, has none.
#[test]
fn a_build_with_none_has_none() {
    let (_guard, root) = scratch("host-caps-none");
    release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    assert!(
        host(&root, &runner)
            .capabilities(Path::new(RELEASE))
            .unwrap()
            .is_empty()
    );
}

/// Asserts that a binary that does not know the flag is a release with no capabilities, which is
/// detected as exit status 2 and nothing else: its standard output is not read.
#[test]
fn a_binary_that_does_not_know_the_flag_has_no_capabilities() {
    let (_guard, root) = scratch("host-caps-old");
    release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![exit(2, "ignored\n", "error: unexpected argument\n")]);
    assert!(
        host(&root, &runner)
            .capabilities(Path::new(RELEASE))
            .unwrap()
            .is_empty()
    );
}

/// Asserts that any other failure is an error and not an empty list (which would send an update
/// down the restart path because a release could not be asked): another exit status, a signal,
/// a timeout, a binary that cannot be started.
#[test]
fn a_binary_that_cannot_be_asked_is_an_error() {
    let (_guard, root) = scratch("host-caps-fail");
    release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![
        exit(1, "", "boom\n"),
        signalled(),
        Err(RunError::Timeout(std::time::Duration::from_secs(7))),
        Err(RunError::Spawn(
            "No such file or directory (os error 2)".to_owned(),
        )),
    ]);
    let host = host(&root, &runner);
    let text = message(host.capabilities(Path::new(RELEASE)).unwrap_err());
    assert_eq!(
        text,
        format!(
            "{}/bin/tierstore --capabilities: exited 1: boom",
            release_dir(&root).display()
        )
    );
    for _ in 0..3 {
        assert!(host.capabilities(Path::new(RELEASE)).is_err());
    }
}

/// Asserts that a binary that a group or others can write is not run: no call is made.
#[test]
fn a_binary_that_others_can_write_is_not_run() {
    for bits in [0o775, 0o757] {
        let (_guard, root) = scratch("host-caps-writable");
        release_with(&root, bits);
        let runner = FakeRunner::replying(vec![]);
        let text = message(
            host(&root, &runner)
                .capabilities(Path::new(RELEASE))
                .unwrap_err(),
        );
        assert!(
            text.ends_with("can be written by a group or others"),
            "{text}"
        );
        assert!(runner.calls().is_empty());
    }
}

/// Asserts that a `bin` directory that a group or others can write is not run from.
#[test]
fn a_bin_directory_that_others_can_write_is_not_run_from() {
    let (_guard, root) = scratch("host-caps-bin-writable");
    release_with(&root, 0o755);
    mode(&release_dir(&root).join("bin"), 0o775);
    let runner = FakeRunner::replying(vec![]);
    let text = message(
        host(&root, &runner)
            .capabilities(Path::new(RELEASE))
            .unwrap_err(),
    );
    assert!(
        text.ends_with("can be written by a group or others"),
        "{text}"
    );
    assert!(runner.calls().is_empty());
}

/// Asserts that a binary, or a `bin` directory, of another owner than the release owner is not
/// run (the default owner is root, which these tests are not).
#[test]
fn a_binary_of_another_owner_is_not_run() {
    let (_guard, root) = scratch("host-caps-owner");
    release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![]);
    let strange = SystemHost::new(root.clone(), &runner, LIMIT).with_release_owner(me() + 1);
    let text = message(strange.capabilities(Path::new(RELEASE)).unwrap_err());
    assert!(
        text.contains(&format!("is owned by user {}, not {}", me(), me() + 1)),
        "{text}"
    );
    assert!(runner.calls().is_empty());
}

/// Asserts that a link in place of the binary, a directory, no binary at all, a link in place of
/// `bin` and a release with the binary at its top only are not run.
#[test]
fn a_binary_that_is_a_link_a_directory_missing_or_not_under_bin_is_not_run() {
    let (_guard, root) = scratch("host-caps-kinds");
    release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner);
    let release = Path::new(RELEASE);
    let bin = release_dir(&root).join("bin");
    let binary = bin.join("tierstore");
    fs::remove_file(&binary).unwrap();
    assert!(message(host.capabilities(release).unwrap_err()).contains("No such file"));
    symlink("/bin/true", &binary).unwrap();
    assert!(message(host.capabilities(release).unwrap_err()).ends_with("is not a regular file"));
    fs::remove_file(&binary).unwrap();
    fs::create_dir(&binary).unwrap();
    assert!(message(host.capabilities(release).unwrap_err()).ends_with("is not a regular file"));
    fs::remove_dir(&binary).unwrap();
    fs::remove_dir(&bin).unwrap();
    fs::write(release_dir(&root).join("tierstore"), "#!").unwrap();
    assert!(message(host.capabilities(release).unwrap_err()).contains("No such file"));
    symlink(root.as_path(), &bin).unwrap();
    assert!(message(host.capabilities(release).unwrap_err()).ends_with("is not a directory"));
    assert!(runner.calls().is_empty());
}
