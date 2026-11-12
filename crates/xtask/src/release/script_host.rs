//! A host for one run of a package's install script, for the tests of the Debian maintainer
//! scripts and the RPM scriptlets: a scratch root, and stand-ins of the programs the scripts call.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::testdir::TestDir;

/// An empty directory of its own for one run, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-script-unit"), name)
}

/// A file of `dir` holding `bytes`.
fn put(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write the file");
    path
}

/// The commands a stand-in records: its name and its arguments, as one line of `$STUB_LOG`.
const STUB: &str = "#!/bin/sh\nprintf '%s\\n' \"${0##*/} $*\" >> \"$STUB_LOG\"\n";

/// `getent`'s stand-in: it records itself and finds the group when `$STUB_GROUP` exists.
const STUB_GETENT: &str =
    "#!/bin/sh\nprintf '%s\\n' \"${0##*/} $*\" >> \"$STUB_LOG\"\n[ -e \"$STUB_GROUP\" ]\n";

/// A host for one run of a maintainer script: the root dpkg installs into (`$DPKG_ROOT`), with
/// systemd running or not and the group `tierstore` there or not, and stand-ins of the programs
/// the scripts call (`systemd-sysusers`, `getent`, `chgrp`, `systemctl`) first on `PATH`, which
/// record each call. `mkdir` and the shell are the system's.
pub(crate) struct Host {
    dir: TestDir,
}

impl Host {
    pub(crate) fn new(name: &str, systemd: bool, group: bool) -> Host {
        let dir = scratch(name);
        let stubs = dir.join("stubs");
        std::fs::create_dir_all(&stubs).expect("stubs");
        for (program, text) in [
            ("systemd-sysusers", STUB),
            ("getent", STUB_GETENT),
            ("chgrp", STUB),
            ("systemctl", STUB),
        ] {
            let path = put(&stubs, program, text.as_bytes());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("a stand-in that runs");
        }
        std::fs::create_dir_all(dir.join("root/etc")).expect("the root and its etc");
        if systemd {
            std::fs::create_dir_all(dir.join("root/run/systemd/system")).expect("systemd");
        }
        if group {
            put(&dir, "group", b"");
        }
        Host { dir }
    }

    /// The root the script installs into.
    pub(crate) fn root(&self) -> PathBuf {
        self.dir.join("root")
    }

    /// Runs `script` with `args`: whether it exited 0, and the calls it made, in order.
    pub(crate) fn run(&self, script: &str, args: &[&str]) -> (bool, Vec<String>) {
        let path = put(&self.dir, "script", script.as_bytes());
        let log = self.dir.join("log");
        let _ = std::fs::remove_file(&log);
        let status = std::process::Command::new("sh")
            .arg(&path)
            .args(args)
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.join("stubs").display()),
            )
            .env("DPKG_ROOT", self.root())
            .env("STUB_LOG", &log)
            .env("STUB_GROUP", self.dir.join("group"))
            .status()
            .expect("sh");
        let calls = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        (status.success(), calls)
    }

    /// The mode of `etc/tierstore` below the root, if anything is there (not following a link).
    pub(crate) fn config_dir_mode(&self) -> Option<u32> {
        self.root()
            .join("etc/tierstore")
            .symlink_metadata()
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }
}

pub(crate) const SYSUSERS_CALL: &str = "systemd-sysusers /usr/lib/sysusers.d/tierstore.conf";
pub(crate) const GETENT_CALL: &str = "getent group tierstore";
pub(crate) const RELOAD_CALL: &str = "systemctl daemon-reload";
