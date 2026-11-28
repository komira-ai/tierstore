//! The mount of a build node's `fs` interface, made by the one process under systemd: `tierstore
//! --config <file>` whose file has an `[[interface]]` of type `fs`, started as a transient unit with
//! the service options of the unit that runs it
//! (`packaging/hostkeeper/units/tierstore@.service`: `CAP_SYS_ADMIN` as its one capability, and
//! none of the options that give a service a mount namespace of its own). The packaged unit is
//! `Type=simple`; the transient unit is made `Type=notify`, so the
//! start returns once the process said `READY=1`; the test, outside the unit, then finds the mount
//! in its own `/proc/self/mountinfo`: the mount reached the host's mount namespace, where the job
//! runtime sees it. A stop ends the unit with success.
//!
//! An integration target of real OS adapters (systemd, `sudo -n`, `/dev/fuse`), so it is ignored in
//! a plain run and in the pull request's checks: it is run by hand on a Linux host of the farm,
//! `cargo test -p tierstore --test fs_unit_integration -- --ignored`, and says why it skips when one
//! of the three is missing. Everything it makes is in a scratch directory and one transient unit of
//! its own name, stopped and detached at the end; no assertion is about time.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tierstore_testkit::dir::TestDir;

/// The packaged unit whose service options the test runs the process with.
const UNIT: &str = "tierstore@.service";

/// The options of [`UNIT`] the transient unit does not take: the command, the
/// account (the test's own user runs it, so that it can read the scratch directory), the runtime
/// directories under `/run` and the state directory under `/var/lib` (the test's paths are in its
/// scratch directory), the restarts (one start is the test), and the type, which the test sets
/// itself.
const LEFT_OUT: [&str; 10] = [
    "ExecStart",
    "Type",
    "StateDirectory",
    "StateDirectoryMode",
    "User",
    "RuntimeDirectory",
    "RuntimeDirectoryPreserve",
    "Restart",
    "RestartSec",
    "RestartPreventExitStatus",
];

/// Runs `program` with `args` through `sudo -n`.
fn sudo(program: &str, args: &[String]) -> Output {
    Command::new("/usr/bin/sudo")
        .arg("-n")
        .arg(program)
        .args(args)
        .output()
        .unwrap()
}

/// Why this machine cannot run the test, if it cannot.
fn missing() -> Option<&'static str> {
    if !Path::new("/run/systemd/system").is_dir() {
        return Some("systemd is not the service manager here");
    }
    if !Path::new("/dev/fuse").exists() {
        return Some("there is no /dev/fuse");
    }
    (!sudo("/usr/bin/true", &[]).status.success()).then_some("sudo -n is not allowed")
}

/// The `[Service]` options of the packaged [`UNIT`], one `key=value` each, continuation lines
/// joined, but those of [`LEFT_OUT`].
fn service_options() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/hostkeeper/units")
        .join(UNIT);
    let text = std::fs::read_to_string(path).unwrap();
    let joined = text.replace("\\\n", " ");
    let service = joined
        .split("[Service]")
        .nth(1)
        .and_then(|rest| rest.split("\n[").next())
        .expect("a [Service] section");
    service
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter(|line| {
            let key = line.split('=').next().unwrap_or_default();
            !LEFT_OUT.contains(&key)
        })
        .map(str::to_owned)
        .collect()
}

/// The transient unit and the scratch directory: the unit is stopped, and the mount it left to a
/// store that a stop drops is detached, when this is dropped.
struct Started {
    unit: String,
    mountpoint: PathBuf,
}

impl Drop for Started {
    fn drop(&mut self) {
        let _ = sudo(
            "/usr/bin/systemctl",
            &["stop".to_owned(), self.unit.clone()],
        );
        let _ = sudo(
            "/usr/bin/systemctl",
            &["reset-failed".to_owned(), self.unit.clone()],
        );
        let _ = sudo(
            "/usr/bin/umount",
            &["-l".to_owned(), self.mountpoint.display().to_string()],
        );
    }
}

/// The file of a build node in `dir` with an `fs` interface whose mount point is `mountpoint`.
fn node_file(dir: &Path, mountpoint: &Path) -> PathBuf {
    let (uid, gid) = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    let key = dir.join("access.key");
    std::fs::write(&key, "AKIDINTEGRATION\nsecretintegration\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let text = format!(
        r#"[[interface]]
type = "fs"
[interface.params]
mountpoint   = {mountpoint:?}
runtime_dir  = {runtime:?}
client_group = "{gid}"
owner        = "{uid}:{gid}"

[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "integration"
bucket          = "cas"
access_key_file = {key:?}

[local_cache]
path   = {cache:?}
budget = "1MiB"

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#,
        mountpoint = mountpoint.display().to_string(),
        runtime = dir.join("run").display().to_string(),
        key = key.display().to_string(),
        cache = dir.join("cache").display().to_string(),
    );
    let file = dir.join("config.toml");
    std::fs::write(&file, text).unwrap();
    file
}

/// Whether this process's table of mounts, which is the host's, shows a mount of the file system
/// (type `fuse.tierstore-fs`) at `mountpoint`.
fn shown_here(mountpoint: &Path) -> bool {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    info.lines().any(|line| {
        let Some((before, after)) = line.split_once(" - ") else {
            return false;
        };
        let at = before.split(' ').nth(4).map(Path::new);
        at == Some(mountpoint) && after.starts_with("fuse.tierstore-fs ")
    })
}

/// Asserts that the one process, under the service options of the unit that makes the mount, says
/// `READY=1` with its mount made in the host's mount namespace, and that a stop ends the unit with
/// success.
#[test]
#[ignore = "real OS adapters (systemd, sudo -n, /dev/fuse): run by hand on a Linux host of the farm with --ignored"]
fn the_mount_reaches_the_host_namespace_from_the_unit() {
    if let Some(why) = missing() {
        // A skip says why, on the test's stderr.
        let _ = writeln!(std::io::stderr(), "skipped: {why}");
        return;
    }
    let dir = TestDir::under(std::env::temp_dir(), "tsfs-unit");
    let mountpoint = dir.path().join("mnt");
    let file = node_file(dir.path(), &mountpoint);
    let unit = format!("tierstore-fs-integration-{}", std::process::id());
    let started = Started {
        unit: unit.clone(),
        mountpoint: mountpoint.clone(),
    };
    let mut args = vec![
        "--unit".to_owned(),
        unit.clone(),
        "--quiet".to_owned(),
        "-p".to_owned(),
        format!("User={}", rustix::process::geteuid().as_raw()),
        "-p".to_owned(),
        format!("Group={}", rustix::process::getegid().as_raw()),
    ];
    for option in ["Type=notify", "NotifyAccess=main"]
        .map(str::to_owned)
        .into_iter()
        .chain(service_options())
    {
        args.push("-p".to_owned());
        args.push(option);
    }
    args.extend([
        env!("CARGO_BIN_EXE_tierstore").to_owned(),
        "--config".to_owned(),
        file.display().to_string(),
    ]);
    // A notify unit: the start returns once the process said READY=1, or fails.
    let ran = sudo("/usr/bin/systemd-run", &args);
    assert!(
        ran.status.success(),
        "systemd-run: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(
        shown_here(&mountpoint),
        "no mount at {}",
        mountpoint.display()
    );
    let stopped = sudo("/usr/bin/systemctl", &["stop".to_owned(), unit.clone()]);
    assert!(stopped.status.success());
    // A unit whose process ended in failure stays loaded as failed; one that ended with success
    // is not failed (and a transient one is gone).
    let failed = sudo("/usr/bin/systemctl", &["is-failed".to_owned(), unit]);
    assert!(
        !failed.status.success(),
        "{}",
        String::from_utf8_lossy(&failed.stdout)
    );
    drop(started);
}
