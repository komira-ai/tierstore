//! A Mac's releases of the macOS installer package: the binary in the version's tree that the host
//! asks and checks the node's file with, the copy a root hook runs that check through as
//! `_tierstore`, and the copy itself.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::super::super::ports::{Host, PortError, PortKind};
use super::super::super::{
    CHECK_BINARY, MACOS_PACKAGE_RELEASE_BINARY, PACKAGE_RELEASE_BINARY, SERVICE_BINARY,
};
use super::super::service_binary::ServiceBinary;
use super::super::testing::{FakeRunner, LIMIT, exit, me, scratch};
use super::super::{Call, DSCL_PROGRAM, Output, RunError, Runner, SUDO_PROGRAM, SystemHost};

/// A release as hostkeeper stages it on a Mac.
const RELEASE: &str = "/var/lib/hostkeeper/packages/tierstore/0.3.209";
/// The node's file of a Mac.
const CONFIG: &str = "/usr/local/tierstore/etc/tierstore.toml";
/// What `dscl` prints of `_tierstore`.
const IDS: &str = "UniqueID: 299\nPrimaryGroupID: 299\n";
/// The bytes of the release's binary.
const BYTES: &[u8] = b"#!the binary of the package";

fn mode(path: &Path, bits: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

/// `path` (absolute on the host) under `root`.
fn at(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// The message of a failure of the host port.
fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Host,
            message,
        } => message,
        other => panic!("not a host failure: {other:?}"),
    }
}

/// The host of a Mac over `root`, whose releases are the tests' own user's.
fn mac_host<'a>(root: &Path, runner: &'a dyn Runner) -> SystemHost<'a> {
    SystemHost::new(root.to_path_buf(), runner, LIMIT)
        .with_release_owner(me())
        .with_directory_service()
}

/// A release under `root` with a binary of mode `bits` at `path` below its version directory.
fn release_at(root: &Path, path: &str, bits: u32) -> PathBuf {
    let binary = at(root, RELEASE).join(path);
    let dir = binary.parent().unwrap();
    fs::create_dir_all(dir).unwrap();
    mode(dir, 0o755);
    fs::write(&binary, BYTES).unwrap();
    mode(&binary, bits);
    binary
}

/// A release of the installer package under `root`, its binary of mode `bits`.
fn package_release(root: &Path, bits: u32) -> PathBuf {
    release_at(root, MACOS_PACKAGE_RELEASE_BINARY, bits)
}

/// The arguments of a check of the node's file under `root`.
fn check_args(root: &Path) -> Vec<String> {
    let file = at(root, CONFIG).to_string_lossy().into_owned();
    vec!["--config".to_owned(), file, "--check-config".to_owned()]
}

/// The call of a root hook's check through `sudo` as uid 299, with the binary `program`.
fn sudo_call(root: &Path, program: &Path) -> Call {
    let mut args = vec![
        "-n".to_owned(),
        "-u".to_owned(),
        "#299".to_owned(),
        "--".to_owned(),
        program.to_string_lossy().into_owned(),
    ];
    args.extend(check_args(root));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Call::new(SUDO_PROGRAM, &args, LIMIT)
}

fn dscl_call() -> Call {
    let args = [
        ".",
        "-read",
        "/Users/_tierstore",
        "UniqueID",
        "PrimaryGroupID",
    ];
    Call::new(DSCL_PROGRAM, &args, LIMIT)
}

/// What a stand-in program answers.
type Answer = Result<Output, RunError>;
/// What was at a program while a call ran: its bytes and its mode.
type Seen = Option<(Vec<u8>, u32)>;

/// A runner that answers from a list, keeps every call, notes what was at the program `sudo` was
/// asked to run (its bytes and mode) while the call ran, and does `during` while each call runs.
struct Watching<F: Fn(&Call)> {
    replies: RefCell<VecDeque<Answer>>,
    calls: RefCell<Vec<Call>>,
    seen: RefCell<Vec<Seen>>,
    during: F,
}

impl<F: Fn(&Call)> Watching<F> {
    fn new(replies: Vec<Answer>, during: F) -> Watching<F> {
        Watching {
            replies: RefCell::new(replies.into()),
            calls: RefCell::new(Vec::new()),
            seen: RefCell::new(Vec::new()),
            during,
        }
    }
}

impl<F: Fn(&Call)> Runner for Watching<F> {
    fn run(&self, call: &Call) -> Answer {
        self.calls.borrow_mut().push(call.clone());
        let seen = (call.program == SUDO_PROGRAM)
            .then(|| call.args.get(4))
            .flatten()
            .and_then(|program| {
                let path = Path::new(program);
                Some((fs::read(path).ok()?, mode_of(path)))
            });
        self.seen.borrow_mut().push(seen);
        (self.during)(call);
        self.replies.borrow_mut().pop_front().expect("a reply")
    }
}

/// The plist the macOS installer package installs, whose program is the package's binary.
const PACKAGE_PLIST: &str = include_str!("../../../../../../packaging/macos-pkg/tierstore.plist");

/// Asserts that a release of the installer package is found where the package puts its binary:
/// `tree` and the program of the package's plist (xtask's tests hold the payload to the same
/// path), and that the copy a check runs is beside the copy of the archive package's units.
#[test]
fn the_macos_package_binary_is_the_path_the_installer_package_installs() {
    let program = PACKAGE_PLIST
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .and_then(|rest| rest.split("<string>").nth(1))
        .and_then(|rest| rest.split("</string>").next())
        .expect("the plist names its program");
    assert_eq!(MACOS_PACKAGE_RELEASE_BINARY, format!("tree{program}"));
    assert_eq!(
        Path::new(CHECK_BINARY).parent(),
        Path::new(SERVICE_BINARY).parent()
    );
}

/// Asserts that a Mac asks a release of the installer package, which has nothing at
/// `bin/tierstore`, its capabilities with the binary of its tree, and that a hook that is not
/// root checks the node's file with that binary itself. Catches a hook that fails `prepare` of
/// every update of a Mac from the package with `host_failed`.
#[test]
fn a_mac_asks_a_release_of_the_installer_package_with_the_binary_of_its_tree() {
    let (_guard, root) = scratch("host-mac-package");
    let binary = package_release(&root, 0o755);
    let runner = FakeRunner::replying(vec![exit(0, "handoff-exec-v1\n", ""), exit(0, "", "")]);
    let host = mac_host(&root, &runner).with_hook_user(1000);
    let release = Path::new(RELEASE);
    assert_eq!(
        host.capabilities(release).unwrap(),
        vec!["handoff-exec-v1".to_owned()]
    );
    assert_eq!(host.check_config(release, Path::new(CONFIG)).unwrap(), None);
    let program = binary.to_str().unwrap();
    let args = check_args(&root);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    assert_eq!(
        runner.calls(),
        vec![
            Call::new(program, &["--capabilities"], LIMIT),
            Call::new(program, &args, LIMIT),
        ]
    );
}

/// Asserts that a Mac's release with neither binary is refused by both asks, the message naming
/// exactly the two paths a Mac's release may have it at, and that nothing is run.
#[test]
fn a_mac_release_with_neither_binary_is_refused_naming_the_two_paths_of_a_mac() {
    let (_guard, root) = scratch("host-mac-neither");
    fs::create_dir_all(at(&root, RELEASE).join("tree")).unwrap();
    let runner = FakeRunner::replying(Vec::new());
    let host = mac_host(&root, &runner).with_hook_user(0);
    let release = Path::new(RELEASE);
    let said = format!(
        "{} is not there, nor {}",
        at(&root, RELEASE).join("bin/tierstore").display(),
        at(&root, RELEASE)
            .join("tree/Library/Application Support/tierstore/bin/tierstore")
            .display()
    );
    assert_eq!(message(host.capabilities(release).unwrap_err()), said);
    let refused = host.check_config(release, Path::new(CONFIG)).unwrap_err();
    assert_eq!(message(refused), said);
    assert!(runner.calls().is_empty());
}

/// Asserts that each arm looks at its own package's path only: a Linux host does not take the
/// binary at the macOS package's path, nor a Mac the one at the Debian package's, and each
/// refusal names the path the arm looks at.
#[test]
fn each_arm_takes_only_its_own_package_path() {
    let (_guard, root) = scratch("host-arm-paths");
    release_at(&root, MACOS_PACKAGE_RELEASE_BINARY, 0o755);
    let runner = FakeRunner::replying(Vec::new());
    let linux = SystemHost::new(root.clone(), &runner, LIMIT).with_release_owner(me());
    let release = Path::new(RELEASE);
    assert_eq!(
        message(linux.capabilities(release).unwrap_err()),
        format!(
            "{} is not there, nor {}",
            at(&root, RELEASE).join("bin/tierstore").display(),
            at(&root, RELEASE).join(PACKAGE_RELEASE_BINARY).display()
        )
    );

    let (_guard, root) = scratch("host-arm-paths-mac");
    release_at(&root, PACKAGE_RELEASE_BINARY, 0o755);
    let mac = mac_host(&root, &runner);
    assert_eq!(
        message(mac.capabilities(release).unwrap_err()),
        format!(
            "{} is not there, nor {}",
            at(&root, RELEASE).join("bin/tierstore").display(),
            at(&root, RELEASE)
                .join(MACOS_PACKAGE_RELEASE_BINARY)
                .display()
        )
    );
    assert!(runner.calls().is_empty());
}

/// Asserts that the binary at the macOS package's path is held to the checks of every release's
/// binary: a link, a file of another owner, one a group can write, and one in a `bin` others can
/// write are refused with nothing run; and that anything at the archive's `bin` is taken, not
/// passed over for the package's binary.
#[test]
fn a_mac_package_binary_that_is_not_one_to_run_is_not_run() {
    let release = Path::new(RELEASE);
    let runner = FakeRunner::replying(Vec::new());

    let (_guard, root) = scratch("host-mac-link");
    let binary = package_release(&root, 0o755);
    let target = binary.with_file_name("real");
    fs::rename(&binary, &target).unwrap();
    symlink(&target, &binary).unwrap();
    let said = message(mac_host(&root, &runner).capabilities(release).unwrap_err());
    assert_eq!(said, format!("{} is not a regular file", binary.display()));

    let (_guard, root) = scratch("host-mac-owner");
    let binary = package_release(&root, 0o755);
    let host = mac_host(&root, &runner).with_release_owner(me() + 1);
    let bin = binary.parent().unwrap();
    assert_eq!(
        message(host.capabilities(release).unwrap_err()),
        format!(
            "{} is owned by user {}, not {}",
            bin.display(),
            me(),
            me() + 1
        )
    );

    let (_guard, root) = scratch("host-mac-group");
    let binary = package_release(&root, 0o775);
    assert_eq!(
        message(mac_host(&root, &runner).capabilities(release).unwrap_err()),
        format!("{} can be written by a group or others", binary.display())
    );

    let (_guard, root) = scratch("host-mac-bin");
    let binary = package_release(&root, 0o755);
    let bin = binary.parent().unwrap();
    mode(bin, 0o757);
    assert_eq!(
        message(mac_host(&root, &runner).capabilities(release).unwrap_err()),
        format!("{} can be written by a group or others", bin.display())
    );

    let (_guard, root) = scratch("host-mac-archive-bin");
    package_release(&root, 0o755);
    let archive_bin = at(&root, RELEASE).join("bin");
    fs::create_dir(&archive_bin).unwrap();
    mode(&archive_bin, 0o755);
    let said = message(mac_host(&root, &runner).capabilities(release).unwrap_err());
    assert!(
        said.starts_with(&format!("{}: ", archive_bin.join("tierstore").display())),
        "{said}"
    );
    assert!(runner.calls().is_empty());
}

/// Asserts that a root hook on a Mac checks the node's file of a release of the installer package
/// with a copy at `CHECK_BINARY` (the binary's bytes, mode 0755, there while `sudo` runs it as
/// `_tierstore`) and removes the copy once the check has answered. Catches a check that has
/// `sudo` run the binary inside the version directory, which hostkeeper keeps root's alone:
/// `sudo` fails with "Permission denied" and `prepare` with `host_failed`.
#[test]
fn a_root_mac_check_of_a_package_release_runs_a_copy_and_removes_it() {
    let (_guard, root) = scratch("host-mac-copy");
    package_release(&root, 0o755);
    let runner = Watching::new(vec![exit(0, IDS, ""), exit(0, "", "")], |_: &Call| {});
    let host = mac_host(&root, &runner).with_hook_user(0);
    let got = host.check_config(Path::new(RELEASE), Path::new(CONFIG));
    assert_eq!(got, Ok(None));
    let copy = at(&root, CHECK_BINARY);
    assert_eq!(
        *runner.calls.borrow(),
        vec![dscl_call(), sudo_call(&root, &copy)]
    );
    assert_eq!(
        *runner.seen.borrow(),
        vec![None, Some((BYTES.to_vec(), 0o755))]
    );
    assert!(!copy.exists());
    assert!(!copy.with_file_name(".tierstore-check.new").exists());
    assert_eq!(mode_of(copy.parent().unwrap()), 0o755);
    assert!(!at(&root, SERVICE_BINARY).exists());
}

/// Asserts that the check through the copy is read as the release's own answer, exit 2 a
/// refusal with its line, and any other end an error naming what was run (the copy, and the
/// binary it is a copy of) and as whom; and that the copy is gone after each.
#[test]
fn a_root_mac_check_through_a_copy_says_what_it_ran_and_removes_the_copy_on_every_end() {
    let (_guard, root) = scratch("host-mac-copy-ends");
    let binary = package_release(&root, 0o755);
    let copy = at(&root, CHECK_BINARY);
    let ran = format!(
        "{} (a copy of {}), as _tierstore (uid 299, gid 299)",
        sudo_call(&root, &copy).describe(),
        binary.display()
    );
    let ends: [(Answer, Result<Option<String>, String>); 3] = [
        (
            exit(2, "", "the file is refused\n"),
            Ok(Some("the file is refused".to_owned())),
        ),
        (
            exit(1, "", "sudo: a password is required\n"),
            Err(format!("{ran}: exited 1: sudo: a password is required")),
        ),
        (
            Err(RunError::Spawn("no such program".to_owned())),
            Err(format!("{ran}: cannot run: no such program")),
        ),
    ];
    for (end, want) in ends {
        let runner = Watching::new(vec![exit(0, IDS, ""), end], |_: &Call| {});
        let host = mac_host(&root, &runner).with_hook_user(0);
        let got = host
            .check_config(Path::new(RELEASE), Path::new(CONFIG))
            .map_err(message);
        assert_eq!(got, want);
        assert!(!copy.exists());
    }
}

/// Asserts that a copy that cannot be made (a directory above it that others can write) fails
/// the check with an error naming the binary, the copy and why, and that `sudo` is not run.
#[test]
fn a_copy_that_cannot_be_made_is_an_error_and_nothing_is_run() {
    let (_guard, root) = scratch("host-mac-copy-unsafe");
    let binary = package_release(&root, 0o755);
    let local = at(&root, "/usr/local");
    fs::create_dir_all(&local).unwrap();
    mode(&local, 0o777);
    let runner = Watching::new(vec![exit(0, IDS, "")], |_: &Call| {});
    let host = mac_host(&root, &runner).with_hook_user(0);
    let got = host.check_config(Path::new(RELEASE), Path::new(CONFIG));
    assert_eq!(
        got.map_err(message),
        Err(format!(
            "the copy of {} at {CHECK_BINARY} cannot be made: {} can be written by others",
            binary.display(),
            local.display()
        ))
    );
    assert_eq!(*runner.calls.borrow(), vec![dscl_call()]);
}

/// Asserts that a copy that cannot be removed after the check (its directory became writable by
/// others while the check ran) is an error naming what was run and why, also when the check
/// accepted the file; and that a run that failed is reported before the copy that stays.
#[test]
fn a_copy_that_cannot_be_removed_is_an_error_after_the_check() {
    let (_guard, root) = scratch("host-mac-copy-stays");
    let binary = package_release(&root, 0o755);
    let copy = at(&root, CHECK_BINARY);
    let dir = copy.parent().unwrap().to_path_buf();
    let opened = |call: &Call| {
        if call.program == SUDO_PROGRAM {
            mode(&dir, 0o757);
        }
    };
    let ran = format!(
        "{} (a copy of {}), as _tierstore (uid 299, gid 299)",
        sudo_call(&root, &copy).describe(),
        binary.display()
    );
    let runner = Watching::new(vec![exit(0, IDS, ""), exit(0, "", "")], opened);
    let host = mac_host(&root, &runner).with_hook_user(0);
    let got = host.check_config(Path::new(RELEASE), Path::new(CONFIG));
    assert_eq!(
        got.map_err(message),
        Err(format!(
            "{ran}: the copy at {CHECK_BINARY} cannot be removed: {} can be written by others",
            dir.display()
        ))
    );
    mode(&dir, 0o755);

    let runner = Watching::new(
        vec![exit(0, IDS, ""), Err(RunError::Spawn("gone".to_owned()))],
        opened,
    );
    let host = mac_host(&root, &runner).with_hook_user(0);
    let got = host.check_config(Path::new(RELEASE), Path::new(CONFIG));
    assert_eq!(
        got.map_err(message),
        Err(format!("{ran}: cannot run: gone"))
    );
}

/// Asserts that a root hook on a Mac checks an archive release with that release's own binary,
/// by its path inside the release as before, and makes no copy.
#[test]
fn a_root_mac_check_of_an_archive_release_runs_its_own_binary_and_makes_no_copy() {
    let (_guard, root) = scratch("host-mac-archive");
    let binary = release_at(&root, "bin/tierstore", 0o755);
    let runner = Watching::new(vec![exit(0, IDS, ""), exit(0, "", "")], |_: &Call| {});
    let host = mac_host(&root, &runner).with_hook_user(0);
    assert_eq!(
        host.check_config(Path::new(RELEASE), Path::new(CONFIG)),
        Ok(None)
    );
    assert_eq!(
        *runner.calls.borrow(),
        vec![dscl_call(), sudo_call(&root, &binary)]
    );
    assert!(!at(&root, "/usr/local/libexec").exists());
}

/// Asserts that the copy of a check is a file of its own beside the copy the units run: it is
/// written at `CHECK_BINARY` through its own temporary file (a stale one is replaced), leaves the
/// units' copy as it is, and its removal takes it and its temporary file and nothing else.
#[test]
fn the_copy_of_a_check_is_its_own_file_beside_the_units_copy() {
    let (_guard, root) = scratch("host-check-copy-file");
    let units = ServiceBinary::new(&root, me());
    let mut source = fs::File::open(package_release(&root, 0o755)).unwrap();
    assert!(units.install(&mut source).is_ok());
    let check = ServiceBinary::for_check(&root, me());
    assert_eq!(check.path(), at(&root, CHECK_BINARY));
    let stale = check.path().with_file_name(".tierstore-check.new");
    fs::write(&stale, "a crash's").unwrap();
    let mut source = fs::File::open(at(&root, RELEASE).join(MACOS_PACKAGE_RELEASE_BINARY)).unwrap();
    assert!(check.install(&mut source).is_ok());
    assert_eq!(fs::read(check.path()).unwrap(), BYTES);
    assert_eq!(mode_of(&check.path()), 0o755);
    assert!(!stale.exists());
    fs::write(&stale, "a crash's").unwrap();
    assert!(check.remove().is_ok());
    assert!(!check.path().exists());
    assert!(!stale.exists());
    assert_eq!(fs::read(units.path()).unwrap(), BYTES);
}
