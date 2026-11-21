//! The check of the configuration from a staged package that only root may enter: the binary
//! the hook opens and the child executes, the checks held to what was opened, the staged copy of
//! a config package handed over by the hook, and the failures that name what was run, as
//! whom, and how it failed.

#[cfg(target_os = "linux")]
use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::super::super::ports::{Host, PortError, PortKind};
use super::super::RunError;
use super::super::testing::{FakeRunner, LIMIT, OWN_DESCRIPTOR, exit, me, scratch};
#[cfg(target_os = "linux")]
use super::super::{Call, Output, Runner};
use super::{CONFIG_MAX, SystemHost};

const RELEASE: &str = "/packages/tierstore/0.3.156";
const CONFIG: &str = "/etc/tierstore/config.toml";
/// A config package's staged release, which only root may enter.
const STAGED: &str = "/packages/tierstore-config/7";

fn mode(path: &Path, bits: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
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

/// A release under `root` as hostkeeper stages the Debian or RPM package; its binary.
fn package_release(root: &Path) -> PathBuf {
    let dir = root
        .join(RELEASE.trim_start_matches('/'))
        .join("tree/usr/libexec/tierstore");
    fs::create_dir_all(&dir).unwrap();
    mode(&dir, 0o755);
    let binary = dir.join("tierstore");
    fs::write(&binary, "#!").unwrap();
    mode(&binary, 0o755);
    binary
}

/// `etc/passwd` under `root`, with the service account as 991:992.
fn passwd(root: &Path) {
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(
        root.join("etc/passwd"),
        "tierstore:x:991:992::/var/lib/tierstore:/usr/sbin/nologin\n",
    )
    .unwrap();
}

#[cfg(target_os = "linux")]
/// What a call was, as the runner was handed it: the program as given, and the file it named
/// while the call ran (what a `/proc/self/fd/<n>` was open on).
struct Seen {
    program: String,
    file: Option<PathBuf>,
}

/// Records each call as it was handed over, and answers every one with exit 0.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Raw {
    seen: RefCell<Vec<Seen>>,
}

#[cfg(target_os = "linux")]
impl Runner for Raw {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        self.seen.borrow_mut().push(Seen {
            program: call.program.clone(),
            file: fs::read_link(&call.program).ok(),
        });
        Ok(Output {
            code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}

/// Asserts that on Linux the check is the binary the hook opened, executed by the child as
/// `/proc/self/fd/<n>` with `n` at least 3 (never one of the standard streams the child is
/// given), under the service account's ids, while `--capabilities` (run as the hook, root) is
/// still started by its path. Catches a check that starts the staged binary by its path as the
/// account, which cannot search a package only root may enter.
#[cfg(target_os = "linux")]
#[test]
fn a_check_executes_the_binary_the_hook_opened() {
    let (_guard, root) = scratch("host-root-only-fd");
    let binary = package_release(&root);
    passwd(&root);
    let raw = Raw::default();
    let host = SystemHost::new(root.clone(), &raw, LIMIT)
        .with_release_owner(me())
        .with_hook_user(0);
    host.capabilities(Path::new(RELEASE)).unwrap();
    assert_eq!(
        host.check_config(Path::new(RELEASE), Path::new(CONFIG)),
        Ok(None)
    );
    let seen = raw.seen.borrow();
    assert_eq!(seen[0].program, binary.to_string_lossy());
    let fd: u32 = seen[1]
        .program
        .strip_prefix("/proc/self/fd/")
        .expect("a descriptor of the hook's")
        .parse()
        .unwrap();
    assert!(fd >= 3, "{fd}");
    assert_eq!(seen[1].file.as_deref(), Some(binary.as_path()));
}

/// Asserts that what the hook opens is held to the checks of the path: a link is not followed,
/// and a file of another owner, or one a group or others can write, is refused, each naming the
/// file, so that a binary swapped after the path was checked is not run. A file that is not
/// there names it too.
#[cfg(target_os = "linux")]
#[test]
fn the_opened_binary_is_held_to_the_checks_of_its_path() {
    let (_guard, root) = scratch("host-root-only-open");
    let binary = package_release(&root);
    let runner = FakeRunner::replying(vec![]);
    let host = SystemHost::new(root.clone(), &runner, LIMIT).with_release_owner(me());
    let Ok(opened) = host.open_binary(&binary) else {
        panic!("the binary opens");
    };
    assert_eq!(fs::read_link(&opened.exec).unwrap(), binary);
    drop(opened);
    let shown = binary.display();
    mode(&binary, 0o775);
    let error = host.open_binary(&binary).err().unwrap();
    assert_eq!(
        error.0,
        format!("{shown} can be written by a group or others")
    );
    mode(&binary, 0o755);
    let other = SystemHost::new(root.clone(), &runner, LIMIT).with_release_owner(me() + 1);
    let error = other.open_binary(&binary).err().unwrap();
    assert_eq!(
        error.0,
        format!("{shown} is owned by user {}, not {}", me(), me() + 1)
    );
    let link = root.join("link");
    std::os::unix::fs::symlink(&binary, &link).unwrap();
    let error = host.open_binary(&link).err().unwrap();
    assert!(
        error.0.starts_with(&format!("{}: ", link.display())),
        "{}",
        error.0
    );
    let gone = root.join("gone");
    let error = host.open_binary(&gone).err().unwrap();
    assert!(
        error.0.starts_with(&format!("{}: ", gone.display())),
        "{}",
        error.0
    );
    assert!(runner.calls().is_empty());
}

/// The staged copy of the configuration of a config package under `root`, with `text`.
fn staged_copy(root: &Path, text: &str) -> PathBuf {
    let file = root
        .join(STAGED.trim_start_matches('/'))
        .join("tree/etc/tierstore/config.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, text).unwrap();
    file
}

/// Asserts that a configuration file under the root-only directory (a config package's staged
/// copy) is read by the hook and handed to the check: on Linux in a file in memory the child
/// inherits (`--config /proc/self/fd/<n>`), elsewhere on its standard input (`--config
/// /dev/stdin`), and that a failure then names the binary, the file by its own path, the user
/// and the end. Catches a check that names the staged copy by its path, which the service
/// account cannot read.
#[test]
fn a_staged_copy_is_handed_to_the_check_by_the_hook() {
    let (_guard, root) = scratch("host-root-only-handed");
    let binary = package_release(&root);
    let staged = staged_copy(&root, "[server]\nproxy_id = \"p\"\n");
    let runner = FakeRunner::replying(vec![exit(0, "", ""), exit(1, "", "boom")]);
    let host = SystemHost::new(root.clone(), &runner, LIMIT)
        .with_release_owner(me())
        .with_hook_user(1000)
        .with_root_only(PathBuf::from(STAGED));
    let config = Path::new(STAGED).join("tree/etc/tierstore/config.toml");
    assert_eq!(host.check_config(Path::new(RELEASE), &config), Ok(None));
    let call = runner.calls().remove(0);
    assert_eq!(call.program, binary.to_string_lossy());
    assert_eq!(call.args.len(), 3, "{:?}", call.args);
    assert_eq!(call.args[0], "--config");
    if cfg!(target_os = "linux") {
        assert!(call.args[1].starts_with(OWN_DESCRIPTOR), "{:?}", call.args);
        assert_eq!(call.stdin, None);
    } else {
        assert_eq!(call.args[1], "/dev/stdin");
    }
    assert_eq!(call.args[2], "--check-config");
    assert_eq!(
        runner.handed()[0].as_deref(),
        Some(&b"[server]\nproxy_id = \"p\"\n"[..])
    );
    let text = message(host.check_config(Path::new(RELEASE), &config).unwrap_err());
    let ran = if cfg!(target_os = "linux") {
        format!(
            "{} --config {} --check-config (executed from the file the hook opened)",
            binary.display(),
            staged.display()
        )
    } else {
        format!("{} --config /dev/stdin --check-config", binary.display())
    };
    assert_eq!(
        text,
        format!(
            "{ran}, {} read by the hook and handed over, as the hook's own user (uid 1000): \
             exited 1: boom",
            staged.display()
        )
    );
}

/// Asserts that the file in memory the hook hands a configuration over in is read, through the
/// path the check is given, by a child that runs under other user and group ids than the hook's
/// (`nobody` when the test runs as root, else the test's own), and holds the text whole. Catches
/// a hand-over the account cannot open (standard input reopened as `/dev/stdin` is a pipe that
/// root made, mode 0600: `Permission denied`), and one closed before the child starts.
#[cfg(target_os = "linux")]
#[test]
fn the_handed_over_file_is_read_by_a_child_under_the_accounts_ids() {
    let text = "[server]\nproxy_id = \"p\"\n";
    let Ok(handed) = super::handed_over(text) else {
        panic!("the configuration is handed over");
    };
    let ids = if me() == 0 {
        (65534, 65534)
    } else {
        (me(), rustix::process::getegid().as_raw())
    };
    let call = Call::new("cat", &[handed.path.as_str()], LIMIT).running_as(ids.0, ids.1);
    let out = super::super::SystemRunner.run(&call).unwrap();
    assert_eq!(
        (out.code, String::from_utf8_lossy(&out.stdout).into_owned()),
        (Some(0), text.to_owned())
    );
}

/// Asserts that a staged copy that is not there, or is larger than [`CONFIG_MAX`], is an error
/// naming it, and that nothing is run.
#[test]
fn a_staged_copy_missing_or_too_large_is_an_error_and_nothing_runs() {
    let (_guard, root) = scratch("host-root-only-stdin-bad");
    package_release(&root);
    let runner = FakeRunner::replying(vec![]);
    let host = SystemHost::new(root.clone(), &runner, LIMIT)
        .with_release_owner(me())
        .with_hook_user(1000)
        .with_root_only(PathBuf::from(STAGED));
    let config = Path::new(STAGED).join("tree/etc/tierstore/config.toml");
    let file = root
        .join(STAGED.trim_start_matches('/'))
        .join("tree/etc/tierstore/config.toml");
    let missing = message(host.check_config(Path::new(RELEASE), &config).unwrap_err());
    assert_eq!(missing, format!("{} is not there", file.display()));
    let big = "#".repeat(usize::try_from(CONFIG_MAX).unwrap() + 1);
    staged_copy(&root, &big);
    let large = message(host.check_config(Path::new(RELEASE), &config).unwrap_err());
    assert_eq!(
        large,
        format!("{}: is larger than {CONFIG_MAX} bytes", file.display())
    );
    assert!(runner.calls().is_empty());
}

/// Asserts that a check by a root hook that cannot be run, or ends any way but accepting or
/// refusing, fails with a message that names the binary by its path, the configuration, the
/// account and its ids, and the error, whole. Catches a `host_failed` with no text that names the
/// cause.
#[test]
fn a_failed_check_names_what_ran_as_whom_and_the_error() {
    let (_guard, root) = scratch("host-root-only-says");
    let binary = package_release(&root);
    passwd(&root);
    let runner = FakeRunner::replying(vec![
        Err(RunError::Spawn(
            "Permission denied (os error 13)".to_owned(),
        )),
        exit(126, "", "denied"),
    ]);
    let host = SystemHost::new(root.clone(), &runner, LIMIT)
        .with_release_owner(me())
        .with_hook_user(0);
    let opened = if cfg!(target_os = "linux") {
        " (executed from the file the hook opened)"
    } else {
        ""
    };
    let ran = format!(
        "{} --config {} --check-config{opened}, as tierstore (uid 991, gid 992)",
        binary.display(),
        root.join("etc/tierstore/config.toml").display()
    );
    let spawn = host
        .check_config(Path::new(RELEASE), Path::new(CONFIG))
        .unwrap_err();
    assert_eq!(
        spawn.to_string(),
        format!("host: {ran}: cannot run: Permission denied (os error 13)")
    );
    let exited = host
        .check_config(Path::new(RELEASE), Path::new(CONFIG))
        .unwrap_err();
    assert_eq!(
        exited.to_string(),
        format!("host: {ran}: exited 126: denied")
    );
}
