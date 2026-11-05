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

/// The password file of a scratch root that has the service account.
fn passwd_with_account(root: &Path) {
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(
        root.join("etc/passwd"),
        "root:x:0:0:root:/root:/bin/sh\n\
         broken:x:nine:9::/:/bin/false\n\
         short:x:5\n\
         tierstore:x:991:992:tierstore:/var/lib/tierstore:/usr/sbin/nologin\n",
    )
    .unwrap();
}

fn check_args(root: &Path) -> Vec<String> {
    vec![
        "--config".to_owned(),
        root.join("etc/tierstore/config.toml")
            .to_string_lossy()
            .into_owned(),
        "--check-config".to_owned(),
    ]
}

/// Asserts (T29, H01) that a check of the configuration is the call
/// `<release>/bin/tierstore --config <file under the root> --check-config`, with no input and the
/// host's time limit, run as the hook itself when the hook is not root, and that exit 0 is
/// `None`: the release accepts it.
#[test]
fn a_check_runs_the_binary_with_config_and_check_config_as_the_hook() {
    let (_guard, root) = scratch("host-check");
    release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    let got = host(&root, &runner)
        .with_hook_user(1000)
        .check_config(Path::new(RELEASE), Path::new("/etc/tierstore/config.toml"))
        .unwrap();
    assert_eq!(got, None);
    let binary = release_dir(&root).join("bin/tierstore");
    let call = runner.calls().remove(0);
    assert_eq!(call.program, binary.to_str().unwrap());
    assert_eq!(call.args, check_args(&root));
    assert_eq!(call.stdin, None);
    assert_eq!(call.timeout, LIMIT);
    assert_eq!(call.run_as, None);
}

/// Asserts (T29, H01) that exit 2 is a refusal and its standard error is the line returned,
/// trimmed; exit 1, a signal, a program that cannot be run and one that does not end are
/// `host_failed`-class errors that say what happened, not refusals.
#[test]
fn a_check_exiting_2_refuses_and_any_other_end_is_an_error() {
    let (_guard, root) = scratch("host-check-ends");
    release_with(&root, 0o755);
    let config = Path::new("/etc/tierstore/config.toml");
    let line = "tierstore: config file x: server.handoff_socket: a build node does not hand off";
    let runner = FakeRunner::replying(vec![
        exit(2, "", &format!("{line}\n")),
        exit(1, "", "boom"),
        signalled(),
        Err(RunError::Spawn("no such file".to_owned())),
        Err(RunError::Timeout(LIMIT)),
    ]);
    let host = host(&root, &runner).with_hook_user(1000);
    let refused = host.check_config(Path::new(RELEASE), config).unwrap();
    assert_eq!(refused, Some(line.to_owned()));
    let exited = message(host.check_config(Path::new(RELEASE), config).unwrap_err());
    assert!(exited.contains("exited 1: boom"), "{exited}");
    let killed = message(host.check_config(Path::new(RELEASE), config).unwrap_err());
    assert!(killed.contains("ended by a signal"), "{killed}");
    let spawn = message(host.check_config(Path::new(RELEASE), config).unwrap_err());
    assert!(spawn.contains("cannot run: no such file"), "{spawn}");
    let timeout = message(host.check_config(Path::new(RELEASE), config).unwrap_err());
    assert!(timeout.contains("killed"), "{timeout}");
}

/// Asserts (T29) that a hook that runs as root has the check run as the user and group ids of the
/// service account in `etc/passwd` under the root (the fields of its line, not those of a line
/// that is short or not numeric), and that a hook that runs as another user than root runs it as
/// itself even when the file has the account. Catches a check run as root, which refuses the key
/// files the service account owns.
#[test]
fn a_check_by_a_root_hook_runs_as_the_service_account() {
    let (_guard, root) = scratch("host-check-root");
    release_with(&root, 0o755);
    passwd_with_account(&root);
    let config = Path::new("/etc/tierstore/config.toml");
    let runner = FakeRunner::replying(vec![exit(0, "", ""), exit(0, "", "")]);
    let host = host(&root, &runner);
    host.with_hook_user(0)
        .check_config(Path::new(RELEASE), config)
        .unwrap();
    assert_eq!(runner.calls()[0].run_as, Some((991, 992)));
    assert_eq!(runner.calls()[0].args, check_args(&root));
}

/// Asserts (T29) that when the hook runs as root and the service account is not in the password
/// file, is on a line that is not numeric or short, or the file is absent, the check is an error
/// that names the file and the account, and no program is run.
#[test]
fn a_check_by_a_root_hook_without_the_account_is_an_error() {
    let (_guard, root) = scratch("host-check-noaccount");
    release_with(&root, 0o755);
    let config = Path::new("/etc/tierstore/config.toml");
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner).with_hook_user(0);
    let text = message(host.check_config(Path::new(RELEASE), config).unwrap_err());
    assert!(
        text.contains("etc/passwd has no account tierstore"),
        "{text}"
    );
    fs::create_dir_all(root.join("etc")).unwrap();
    for body in [
        "root:x:0:0::/:/bin/sh\n",
        "tierstore:x:nine:9::/:/bin/false\n",
        "tierstore:x:9:nine::/:/bin/false\n",
        "tierstore:x:9\n",
        "tierstore\n",
    ] {
        fs::write(root.join("etc/passwd"), body).unwrap();
        let text = message(host.check_config(Path::new(RELEASE), config).unwrap_err());
        assert!(
            text.contains("has no account tierstore"),
            "{body:?}: {text}"
        );
    }
    assert!(runner.calls().is_empty());
}

/// Asserts (T29) that a password file that cannot be read as text is an error naming it.
#[test]
fn a_check_by_a_root_hook_with_an_unreadable_password_file_is_an_error() {
    let (_guard, root) = scratch("host-check-badpasswd");
    release_with(&root, 0o755);
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(root.join("etc/passwd"), [0xff, 0xfe]).unwrap();
    let runner = FakeRunner::replying(vec![]);
    let text = message(
        host(&root, &runner)
            .with_hook_user(0)
            .check_config(Path::new(RELEASE), Path::new("/c"))
            .unwrap_err(),
    );
    assert!(text.contains("etc/passwd"), "{text}");
    assert!(text.contains("UTF-8"), "{text}");
}

/// Asserts (T29) that a check is refused, without running anything, for a release whose binary
/// is not one to run (missing, writable by others), as the capabilities are.
#[test]
fn a_check_of_a_binary_that_is_not_one_to_run_is_an_error() {
    let (_guard, root) = scratch("host-check-binary");
    let config = Path::new("/etc/tierstore/config.toml");
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner).with_hook_user(1000);
    let missing = message(host.check_config(Path::new(RELEASE), config).unwrap_err());
    assert!(missing.contains("bin"), "{missing}");
    release_with(&root, 0o777);
    let open = message(host.check_config(Path::new(RELEASE), config).unwrap_err());
    assert!(open.contains("written by a group or others"), "{open}");
    assert!(runner.calls().is_empty());
}

/// Asserts that the account the check runs as is the `User=` of the unit file the package
/// ships. Catches a unit file whose account is renamed and a hook that still looks up the old one.
#[test]
fn the_service_account_is_the_user_of_the_unit_file() {
    const UNIT: &str =
        include_str!("../../../../../../packaging/hostkeeper/units/tierstore@.service");
    let user = UNIT
        .lines()
        .find_map(|line| line.strip_prefix("User="))
        .expect("the unit file has a User=");
    assert_eq!(user, super::super::SERVICE_ACCOUNT);
}
