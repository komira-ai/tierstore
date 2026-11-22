//! The host over a fixture root, and the capabilities over a recording runner.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::super::ports::{Host, PortError, PortKind};
use super::host::{CONFIG_MAX, DSCL_PROGRAM, MOUNT_PROGRAM, SUDO_PROGRAM};
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
/// `bin` and a release with the binary at its top only (an error that names both places a release
/// may have it) are not run.
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
    assert_eq!(
        message(host.capabilities(release).unwrap_err()),
        format!(
            "{} is not there, nor {}",
            release_dir(&root).join("bin/tierstore").display(),
            release_dir(&root)
                .join("tree/usr/libexec/tierstore/tierstore")
                .display()
        )
    );
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

/// The call that reads the Mac service account's ids from the directory service.
fn dscl_call() -> Call {
    Call::new(
        DSCL_PROGRAM,
        &[
            ".",
            "-read",
            "/Users/_tierstore",
            "UniqueID",
            "PrimaryGroupID",
        ],
        LIMIT,
    )
}

/// The host of a Mac over `root`: the service account is looked up in the directory service.
fn mac_host<'a>(root: &Path, runner: &'a FakeRunner) -> SystemHost<'a> {
    host(root, runner).with_directory_service()
}

/// The call a root hook on a Mac makes to check `config` (a path under the root) as the user
/// `uid`: the release's binary through `sudo -n -u #<uid> --`, within the host's time limit, and
/// started as root (no ids of its own), so that `sudo` gives it the account's groups.
fn mac_check_call(root: &Path, uid: u32, config: &str) -> Call {
    let binary = release_dir(root).join("bin/tierstore");
    let file = root.join(config.trim_start_matches('/'));
    Call::new(
        SUDO_PROGRAM,
        &[
            "-n",
            "-u",
            &format!("#{uid}"),
            "--",
            binary.to_str().unwrap(),
            "--config",
            file.to_str().unwrap(),
            "--check-config",
        ],
        LIMIT,
    )
}

/// Asserts that on a Mac a hook that runs as root asks the directory service for `_tierstore`
/// (`dscl . -read /Users/_tierstore UniqueID PrimaryGroupID`, within the host's time limit) and
/// runs the check as the user id it prints, in either order and with a value on the line after
/// its key, through `sudo -n -u #<uid> --` and not under ids set on the child, and not as the
/// `tierstore` of `etc/passwd` (which the root has here too). Catches the check of a Mac's
/// config hook looked up as `tierstore` in `etc/passwd`, which a Mac does not have, and a check
/// started under the account's ids by the standard library, which on macOS leaves it with the
/// effective group 0 and so unable to enter `/usr/local/tierstore/etc` (root:_tierstore, 0750):
/// every configuration was refused with "Permission denied".
#[test]
fn a_check_by_a_root_mac_hook_runs_as_the_directory_service_account() {
    let (_guard, root) = scratch("host-check-mac-root");
    release_with(&root, 0o755);
    passwd_with_account(&root);
    let config = Path::new("/usr/local/tierstore/etc/tierstore.toml");
    for (printed, ids) in [
        ("PrimaryGroupID: 397\nUniqueID: 398\n", (398, 397)),
        ("UniqueID: 301\nPrimaryGroupID: 302\n", (301, 302)),
        (
            "UniqueIDs: 5\nUniqueID:\n 303\nPrimaryGroupID:\n  304\n",
            (303, 304),
        ),
    ] {
        let runner = FakeRunner::replying(vec![exit(0, printed, ""), exit(0, "", "")]);
        let got = mac_host(&root, &runner)
            .with_hook_user(0)
            .check_config(Path::new(RELEASE), config)
            .unwrap();
        assert_eq!(got, None);
        let calls = runner.calls();
        assert_eq!(calls.len(), 2, "{printed:?}: {calls:?}");
        assert_eq!(calls[0], dscl_call(), "{printed:?}");
        assert_eq!(
            calls[1],
            mac_check_call(&root, ids.0, "/usr/local/tierstore/etc/tierstore.toml"),
            "{printed:?}"
        );
    }
}

/// Asserts that on a Mac the check run through `sudo` by a root hook is read as the release's
/// own answer: exit 0 accepts, exit 2 refuses with its standard error as the line, and any other
/// end (exit 1, which is also how `sudo` itself fails, as when it may not run as the account, a
/// signal, a `sudo` that cannot be started or does not end) is an error naming the whole call,
/// not a refusal.
#[test]
fn a_check_through_sudo_on_a_mac_refuses_on_2_and_fails_on_any_other_end() {
    let (_guard, root) = scratch("host-check-mac-sudo-ends");
    release_with(&root, 0o755);
    let config = Path::new("/usr/local/tierstore/etc/tierstore.toml");
    let ids = || exit(0, "UniqueID: 394\nPrimaryGroupID: 394\n", "");
    let line = "tierstore: reading the config file x: Permission denied (os error 13)";
    let call = mac_check_call(&root, 394, "/usr/local/tierstore/etc/tierstore.toml");
    let said = call.describe();
    let runner = FakeRunner::replying(vec![
        ids(),
        exit(0, "", ""),
        ids(),
        exit(2, "", &format!("{line}\n")),
        ids(),
        exit(1, "", "sudo: a password is required\n"),
        ids(),
        signalled(),
        ids(),
        Err(RunError::Spawn("no such file".to_owned())),
        ids(),
        Err(RunError::Timeout(LIMIT)),
    ]);
    let host = mac_host(&root, &runner).with_hook_user(0);
    let check = || host.check_config(Path::new(RELEASE), config);
    assert_eq!(check().unwrap(), None);
    assert_eq!(check().unwrap(), Some(line.to_owned()));
    let failed = [
        "exited 1: sudo: a password is required",
        "ended by a signal",
        "cannot run: no such file",
        "killed",
    ];
    for how in failed {
        let text = message(check().unwrap_err());
        assert!(text.contains(&said), "{how}: {text}");
        assert!(text.contains(how), "{how}: {text}");
    }
    let calls = runner.calls();
    assert_eq!(calls.len(), 12);
    for pair in calls.chunks(2) {
        assert_eq!(pair, [dscl_call(), call.clone()]);
    }
}

/// Asserts that on a Mac a hook that is not root runs the check as itself and asks the
/// directory service nothing.
#[test]
fn a_check_by_a_mac_hook_that_is_not_root_runs_as_the_hook() {
    let (_guard, root) = scratch("host-check-mac-user");
    release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    mac_host(&root, &runner)
        .with_hook_user(1000)
        .check_config(Path::new(RELEASE), Path::new("/c"))
        .unwrap();
    let calls = runner.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].run_as, None);
    assert_ne!(calls[0].program, DSCL_PROGRAM);
}

/// Asserts that on a Mac a root hook whose directory service has no `_tierstore` (`dscl` exits
/// non-zero), prints no numeric ids for it (an id missing, empty or not a number), is ended by
/// a signal, cannot be started or does not end, fails the check with an error that names the
/// account and the `dscl` call that looked for it, and runs no release.
#[test]
fn a_check_by_a_root_mac_hook_without_the_account_is_an_error() {
    let (_guard, root) = scratch("host-check-mac-noaccount");
    release_with(&root, 0o755);
    let config = Path::new("/usr/local/tierstore/etc/tierstore.toml");
    let call = "/usr/bin/dscl . -read /Users/_tierstore UniqueID PrimaryGroupID";
    let lead = "the directory service has no account _tierstore";
    let unparsed = "no numeric UniqueID and PrimaryGroupID";
    let cases = [
        (
            exit(56, "", "<dscl_cmd> DS Error: -14136 (eDSRecordNotFound)\n"),
            "exited 56: <dscl_cmd> DS Error: -14136 (eDSRecordNotFound)".to_owned(),
        ),
        (exit(0, "PrimaryGroupID: 397\n", ""), unparsed.to_owned()),
        (exit(0, "UniqueID: 398\n", ""), unparsed.to_owned()),
        (
            exit(0, "UniqueID: x\nPrimaryGroupID: 397\n", ""),
            unparsed.to_owned(),
        ),
        (
            exit(0, "UniqueID: 398\nPrimaryGroupID: -1\n", ""),
            unparsed.to_owned(),
        ),
        (
            exit(0, "PrimaryGroupID: 397\nUniqueID:\n", ""),
            unparsed.to_owned(),
        ),
        (exit(0, "", ""), unparsed.to_owned()),
        (signalled(), "ended by a signal".to_owned()),
        (
            Err(RunError::Spawn("no such file".to_owned())),
            "cannot run: no such file".to_owned(),
        ),
        (
            Err(RunError::Timeout(LIMIT)),
            format!("no end after {LIMIT:?}, killed"),
        ),
    ];
    for (reply, how) in cases {
        let runner = FakeRunner::replying(vec![reply]);
        let text = message(
            mac_host(&root, &runner)
                .with_hook_user(0)
                .check_config(Path::new(RELEASE), config)
                .unwrap_err(),
        );
        assert_eq!(text, format!("{lead} ({call}: {how})"));
        assert_eq!(runner.calls(), vec![dscl_call()]);
    }
}

/// Asserts that the account a Mac's check runs as is the `UserName` and `GroupName` of the
/// launchd plist the package ships. Catches a plist whose account is renamed and a hook that
/// still looks up the old one.
#[test]
fn the_mac_service_account_is_the_user_of_the_plist() {
    const PLIST: &str =
        include_str!("../../../../../../packaging/hostkeeper/units/tierstore.plist");
    for key in ["UserName", "GroupName"] {
        let after = PLIST
            .split_once(&format!("<key>{key}</key>"))
            .map(|(_, after)| after.trim_start())
            .expect("the plist has the key");
        let value = after
            .strip_prefix("<string>")
            .and_then(|rest| rest.split_once("</string>"))
            .map(|(value, _)| value)
            .expect("a string value");
        assert_eq!(value, super::super::MACOS_SERVICE_ACCOUNT, "{key}");
    }
}

/// A release under `root` laid out as hostkeeper stages the Debian or RPM package: the package's
/// files in `tree/`, the binary at `tree/usr/libexec/tierstore/tierstore` of mode `bits`.
fn package_release_with(root: &Path, bits: u32) -> PathBuf {
    let dir = release_dir(root).join("tree/usr/libexec/tierstore");
    fs::create_dir_all(&dir).unwrap();
    mode(&dir, 0o755);
    let binary = dir.join("tierstore");
    fs::write(&binary, "#!").unwrap();
    mode(&binary, bits);
    binary
}

/// Asserts that a release of the Debian or RPM package, which has no `bin/tierstore` and has the
/// binary at `tree/usr/libexec/tierstore/tierstore` (the path the package installs, under the
/// tree hostkeeper extracts it to), is asked its capabilities and checks the configuration with
/// that binary. Catches a hook that fails `prepare` of every update from the package with
/// `host_failed` because it looks for `bin/tierstore` only.
#[test]
fn a_release_of_the_package_is_asked_with_the_binary_of_its_tree() {
    let (_guard, root) = scratch("host-package");
    let binary = package_release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![exit(0, "handoff-socket-v1\n", ""), exit(0, "", "")]);
    let host = host(&root, &runner).with_hook_user(1000);
    let release = Path::new(RELEASE);
    assert_eq!(
        host.capabilities(release).unwrap(),
        vec!["handoff-socket-v1".to_owned()]
    );
    assert_eq!(
        host.check_config(release, Path::new("/etc/tierstore/config.toml"))
            .unwrap(),
        None
    );
    let calls = runner.calls();
    let program = binary.to_str().unwrap();
    assert_eq!(
        calls,
        vec![
            Call::new(program, &["--capabilities"], LIMIT),
            Call::new(
                program,
                &check_args(&root)
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                LIMIT
            ),
        ]
    );
}

/// Asserts that when a release has both, `bin/tierstore` is the one run, and that a release with a
/// `bin` whose `tierstore` is not a binary to run (a link, a directory, nothing) is refused rather
/// than passed over for the package's binary. Catches a lookup that prefers the tree, or that falls through
/// to it past a link an archive release should not have.
#[test]
fn the_archive_binary_is_taken_first_and_never_passed_over() {
    let (_guard, root) = scratch("host-package-both");
    release_with(&root, 0o755);
    package_release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    let host = host(&root, &runner);
    let release = Path::new(RELEASE);
    host.capabilities(release).unwrap();
    let archive = release_dir(&root).join("bin/tierstore");
    assert_eq!(runner.calls()[0].program, archive.to_str().unwrap());
    fs::remove_file(&archive).unwrap();
    symlink("/bin/true", &archive).unwrap();
    assert!(message(host.capabilities(release).unwrap_err()).ends_with("is not a regular file"));
    fs::remove_file(&archive).unwrap();
    fs::create_dir(&archive).unwrap();
    assert!(message(host.capabilities(release).unwrap_err()).ends_with("is not a regular file"));
    fs::remove_dir(&archive).unwrap();
    assert!(message(host.capabilities(release).unwrap_err()).contains("No such file"));
    assert_eq!(runner.calls().len(), 1);
}

/// Asserts that the package's binary is held to the checks of the archive's: one that a group or
/// others can write, one in a directory they can write, a link, and one of another owner are not
/// run.
#[test]
fn the_package_binary_that_is_not_one_to_run_is_not_run() {
    for bits in [0o775, 0o757] {
        let (_guard, root) = scratch("host-package-writable");
        package_release_with(&root, bits);
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
    let (_guard, root) = scratch("host-package-kinds");
    let binary = package_release_with(&root, 0o755);
    let runner = FakeRunner::replying(vec![]);
    let release = Path::new(RELEASE);
    mode(binary.parent().unwrap(), 0o777);
    assert!(
        message(host(&root, &runner).capabilities(release).unwrap_err())
            .ends_with("can be written by a group or others")
    );
    mode(binary.parent().unwrap(), 0o755);
    let strange = SystemHost::new(root.clone(), &runner, LIMIT).with_release_owner(me() + 1);
    assert!(
        message(strange.capabilities(release).unwrap_err()).contains(&format!(
            "is owned by user {}, not {}",
            me(),
            me() + 1
        ))
    );
    fs::remove_file(&binary).unwrap();
    symlink("/bin/true", &binary).unwrap();
    assert!(
        message(host(&root, &runner).capabilities(release).unwrap_err())
            .ends_with("is not a regular file")
    );
    assert!(runner.calls().is_empty());
}

/// Asserts that a host given one binary asks that binary for every release, the capabilities
/// and the check alike, and checks it as a release's binary: one that others can write is not
/// run.
#[test]
fn a_host_given_one_binary_asks_it_for_every_release() {
    let (_guard, root) = scratch("host-one-binary");
    let bin = root.join("usr/libexec/tierstore");
    fs::create_dir_all(&bin).unwrap();
    mode(&bin, 0o755);
    let binary = bin.join("tierstore");
    fs::write(&binary, "#!").unwrap();
    mode(&binary, 0o755);
    let runner = FakeRunner::replying(vec![exit(0, "handoff-socket-v1\n", ""), exit(0, "", "")]);
    let one = host(&root, &runner).with_binary(PathBuf::from("/usr/libexec/tierstore/tierstore"));
    assert_eq!(
        one.capabilities(Path::new("/packages/config/1")).unwrap(),
        vec!["handoff-socket-v1".to_owned()]
    );
    assert_eq!(
        one.check_config(
            Path::new("/elsewhere"),
            Path::new("/etc/tierstore/config.toml")
        )
        .unwrap(),
        None
    );
    let programs: Vec<String> = runner.calls().into_iter().map(|c| c.program).collect();
    let want = binary.to_str().unwrap().to_owned();
    assert_eq!(programs, vec![want.clone(), want]);
    mode(&binary, 0o777);
    let runner = FakeRunner::replying(vec![]);
    let one = host(&root, &runner).with_binary(PathBuf::from("/usr/libexec/tierstore/tierstore"));
    let text = message(one.capabilities(Path::new("/r")).unwrap_err());
    assert!(text.contains("tierstore"), "{text}");
    assert!(runner.calls().is_empty());
    mode(&binary, 0o755);
    mode(&bin, 0o777);
    let text = message(one.capabilities(Path::new("/r")).unwrap_err());
    assert!(text.contains("usr/libexec/tierstore"), "{text}");
}

const LISTING: &str = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
127.0.0.1:/ on /private/var/tierstore/fs (nfs, nodev, nosuid, mounted by build)\n\
map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)\n";

/// Asserts that the mount list is read by running `mount` with no argument, and that a line with
/// ` on <path> (` is a mount at the path, while a path that is only a prefix of a mounted one, a
/// path the listing names as a source and a listing without it are not. Catches a check that
/// takes a neighbour's mount for the file service's, or one that reads the wrong command.
#[test]
fn the_mount_list_says_whether_a_path_is_mounted() {
    let (_guard, root) = scratch("host-mounted");
    let runner = FakeRunner::replying(vec![
        exit(0, LISTING, ""),
        exit(0, LISTING, ""),
        exit(0, LISTING, ""),
        exit(0, "", ""),
    ]);
    let host = host(&root, &runner);
    assert_eq!(
        host.mounted(Path::new("/private/var/tierstore/fs")),
        Ok(true)
    );
    assert_eq!(host.mounted(Path::new("/private/var/tierstore")), Ok(false));
    assert_eq!(host.mounted(Path::new("/System/Volumes/Data")), Ok(false));
    assert_eq!(
        host.mounted(Path::new("/private/var/tierstore/fs")),
        Ok(false)
    );
    assert_eq!(runner.calls()[0], Call::new(MOUNT_PROGRAM, &[], LIMIT));
    assert_eq!(MOUNT_PROGRAM, "/sbin/mount");
}

/// Asserts that a mount is found under the path the listing prints when the path given resolves
/// to it through a link (a Mac's `/var` is a link to `/private/var`, and `mount` prints the
/// resolved path). Catches a check that misses the file service's mount because its configuration
/// names the path by the link.
#[test]
fn a_mount_is_found_under_the_path_a_link_resolves_to() {
    let (_guard, root) = scratch("host-mounted-link");
    fs::create_dir_all(root.join("private/var/tierstore/fs")).unwrap();
    symlink("private/var", root.join("var")).unwrap();
    let real = fs::canonicalize(root.join("var/tierstore/fs")).unwrap();
    let listing = format!("127.0.0.1:/ on {} (nfs, nodev)\n", real.display());
    let runner = FakeRunner::replying(vec![exit(0, &listing, "")]);
    assert_eq!(
        host(&root, &runner).mounted(Path::new("/var/tierstore/fs")),
        Ok(true)
    );
}

/// Asserts that a mount list that cannot be had is an error naming the command and how it failed,
/// for a program that cannot be run, one that exits with a status and one that a signal ends, and
/// is not taken for "nothing is mounted". Catches a check that reads a failed `mount` as a mount
/// that is gone.
#[test]
fn a_mount_list_that_cannot_be_had_is_an_error() {
    let (_guard, root) = scratch("host-mounted-fails");
    let runner = FakeRunner::replying(vec![
        Err(RunError::Spawn("no such file".to_owned())),
        exit(1, "", "mount: broken"),
        signalled(),
    ]);
    let host = host(&root, &runner);
    let path = Path::new("/private/var/tierstore/fs");
    let spawn = message(host.mounted(path).unwrap_err());
    assert_eq!(spawn, "/sbin/mount: cannot run: no such file");
    let exited = message(host.mounted(path).unwrap_err());
    assert_eq!(exited, "/sbin/mount: exited 1: mount: broken");
    let ended = message(host.mounted(path).unwrap_err());
    assert_eq!(ended, "/sbin/mount: ended by a signal: ");
}
