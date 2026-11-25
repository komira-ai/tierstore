//! The check that the mount reaches the host, over files a test makes: a directory with the
//! namespace links and the tables of mounts of the two sides.

use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

use super::{HostView, device_at};
use crate::server::treefs::start::StartError;
use crate::server::treefs::testing::{Logged, logged};

/// The mount point of the tests; nothing is made there.
const MOUNTPOINT: &str = "/run/tierstore/fs";

/// A line of a table of mounts: a mount of this file system at the mount point, on `device`.
fn ours(device: &str) -> String {
    format!(
        "36 24 {device} / {MOUNTPOINT} rw,nosuid,nodev,relatime shared:19 - fuse.tierstore-fs tierstore-fs rw\n"
    )
}

/// The root mount, which every table has.
const ROOT: &str = "24 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n";

/// The two sides of a check, in a scratch directory.
struct Sides {
    dir: TestDir,
}

impl Sides {
    fn new(name: &str) -> Self {
        Self {
            dir: TestDir::under(std::env::temp_dir(), name),
        }
    }

    /// Makes a link named `name` that reads `target`, as `/proc/PID/ns/mnt` does.
    fn link(&self, name: &str, target: &str) {
        std::os::unix::fs::symlink(target, self.dir.path().join(name)).unwrap();
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.dir.path().join(name), text).unwrap();
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The view of the host: `own` and `host` are the links, `table` the table of process 1.
    fn view(&self) -> HostView {
        HostView {
            own_ns: self.path("own-ns"),
            host_ns: self.path("host-ns"),
            host_mountinfo: self.path("host-mountinfo"),
        }
    }

    /// The check of the mount at the mount point, whose table of this process's mounts is
    /// `own-mountinfo`.
    fn check(&self) -> Result<(), StartError> {
        self.view()
            .check(Path::new(MOUNTPOINT), &self.path("own-mountinfo"))
    }
}

/// The text of the refusal for these names of the namespaces.
fn refusal(own: &str, host: &str) -> String {
    format!(
        "fs.mountpoint {MOUNTPOINT}: the mount is not in the host's mount namespace: this process is in \
         {own}, process 1 is in {host}, and the table of mounts of process 1 does not show the mount; \
         a unit option such as ReadWritePaths=, ProtectSystem= or PrivateTmp=, for example in a drop-in \
         of the host's, gives the service a mount namespace of its own, from which a mount does not \
         reach the host or the job runtime; remove the option and restart the unit \
         (docs/design/one-service.md section 2.2, docs/install.md)"
    )
}

/// Asserts that a process in the namespace of process 1 is ready without a look at the table of
/// mounts of process 1 (none exists here). Catches a check that asks for more than the namespace
/// when the namespaces are one.
#[test]
fn the_same_mount_namespace_as_process_1_is_the_host() {
    let sides = Sides::new("tsfs-host-same");
    sides.link("own-ns", "mnt:[4026531841]");
    sides.link("host-ns", "mnt:[4026531841]");
    sides.check().unwrap();
}

/// Asserts that a different namespace of this process, with a table of process 1 that has no
/// mount at the mount point, is refused with the full text, naming both namespaces and the cause.
/// This is a service with `ReadWritePaths=` in a drop-in: its mount is made in its own namespace.
#[test]
fn a_mount_namespace_of_its_own_is_refused_naming_both_namespaces_and_the_cause() {
    let sides = Sides::new("tsfs-host-own");
    sides.link("own-ns", "mnt:[4026532500]");
    sides.link("host-ns", "mnt:[4026531841]");
    sides.write("own-mountinfo", &format!("{ROOT}{}", ours("0:45")));
    sides.write("host-mountinfo", ROOT);
    let error = sides.check().unwrap_err();
    assert!(matches!(error, StartError::HostNamespace { .. }));
    assert_eq!(
        error.to_string(),
        refusal("mnt:[4026532500]", "mnt:[4026531841]")
    );
    assert_eq!(error.status(), 1);
}

/// Asserts that the namespaces' names do not decide when the host's table shows the mount with the
/// device of this process's mount: the mount is where the host sees it.
#[test]
fn a_mount_the_table_of_process_1_shows_is_ready_whatever_the_namespaces_are_called() {
    let sides = Sides::new("tsfs-host-shown");
    sides.link("own-ns", "mnt:[4026532500]");
    sides.link("host-ns", "mnt:[4026531841]");
    sides.write("own-mountinfo", &format!("{ROOT}{}", ours("0:45")));
    sides.write("host-mountinfo", &format!("{ROOT}{}", ours("0:45")));
    sides.check().unwrap();
}

/// Asserts the case of the packaged unit: the account of `User=tierstore` cannot read the link of
/// process 1, which root owns, so the table of process 1 decides. Shown there with the device of
/// this process's mount, the mount is ready (the link here is missing, as the kernel's refusal is
/// not one a test can make as root); not shown, it is refused and the text says the host's
/// namespace could not be read, and why.
#[test]
fn an_unreadable_namespace_of_process_1_is_judged_by_its_table_of_mounts() {
    let sides = Sides::new("tsfs-host-denied");
    sides.link("own-ns", "mnt:[4026532500]");
    sides.write("own-mountinfo", &format!("{ROOT}{}", ours("0:45")));
    sides.write("host-mountinfo", &format!("{ROOT}{}", ours("0:45")));
    sides.check().unwrap();

    sides.write("host-mountinfo", ROOT);
    assert_eq!(
        sides.check().unwrap_err().to_string(),
        refusal("mnt:[4026532500]", "unreadable (entity not found)")
    );
}

/// Asserts that a mount at the mount point in the host's table is not this process's mount when
/// its device differs (a dead mount of an earlier process, which the host still shows), and that a
/// mount of another kind there is not it either.
#[test]
fn a_mount_of_the_host_at_the_mount_point_that_is_not_ours_is_refused() {
    let sides = Sides::new("tsfs-host-other");
    sides.link("own-ns", "mnt:[4026532500]");
    sides.write("own-mountinfo", &format!("{ROOT}{}", ours("0:45")));
    sides.write("host-mountinfo", &format!("{ROOT}{}", ours("0:51")));
    assert_eq!(
        sides.check().unwrap_err().to_string(),
        refusal("mnt:[4026532500]", "unreadable (entity not found)")
    );
    sides.write(
        "host-mountinfo",
        &format!("{ROOT}41 24 0:45 / {MOUNTPOINT} rw - tmpfs tmpfs rw\n"),
    );
    assert!(sides.check().is_err());
}

/// Asserts that when this process's own table does not show the mount, the host's table showing a
/// mount of this file system at the mount point is enough: there is no device to compare.
#[test]
fn a_host_mount_is_enough_when_our_own_table_cannot_be_read() {
    let sides = Sides::new("tsfs-host-noown");
    sides.link("own-ns", "mnt:[4026532500]");
    sides.write("host-mountinfo", &format!("{ROOT}{}", ours("0:45")));
    sides.check().unwrap();
}

/// Asserts that two namespaces that differ, with no table of process 1 to read, are refused, and
/// that a start that can read neither the namespace nor the table of process 1 (a `/proc` that
/// hides it) goes on and says why it cannot tell: a node that works is not stopped for a guess.
#[test]
fn what_cannot_be_read_refuses_only_where_the_namespaces_are_known_to_differ() {
    let sides = Sides::new("tsfs-host-blind");
    sides.link("own-ns", "mnt:[4026532500]");
    sides.link("host-ns", "mnt:[4026531841]");
    assert_eq!(
        sides.check().unwrap_err().to_string(),
        refusal("mnt:[4026532500]", "mnt:[4026531841]")
    );

    let blind = Sides::new("tsfs-host-hidden");
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    blind.check().unwrap();
    // The same where only this process's own namespace can be read.
    let partly = Sides::new("tsfs-host-partly");
    partly.link("own-ns", "mnt:[4026532500]");
    partly.check().unwrap();
    drop(guard);
    assert!(
        log.text().contains(
            "tierstore fs: cannot tell whether the mount at /run/tierstore/fs is in the host's \
             mount namespace: neither the namespace nor the table of mounts of process 1 can be read"
        ),
        "{}",
        log.text()
    );
}

/// Asserts that the device number is that of the last FUSE mount at the path, and that lines that
/// are short, or of another kind of file system, or at another path, give none.
#[test]
fn the_device_of_a_mount_is_the_last_fuse_mount_at_the_path() {
    fn at(text: &str) -> Option<&str> {
        device_at(text, Path::new(MOUNTPOINT))
    }
    assert_eq!(at(&format!("{ROOT}{}", ours("0:45"))), Some("0:45"));
    assert_eq!(
        at(&format!("{}{}", ours("0:45"), ours("0:46"))),
        Some("0:46")
    );
    assert_eq!(at(ROOT), None);
    assert_eq!(at("short\nnot - enough\n"), None);
    assert_eq!(at("1 2 0:3 / /x - fuse a b\n"), None);
    assert_eq!(
        at(&format!("1 2 0:3 / {MOUNTPOINT} rw - fuse a b\n")),
        Some("0:3")
    );
    assert_eq!(at(&format!("1 2 0:3 / {MOUNTPOINT} rw - nfs4 a b\n")), None);
    assert_eq!(at(&format!("1 2 0:3 /{MOUNTPOINT} rw")), None);
}

#[test]
fn a_mount_point_the_kernel_escapes_is_found_by_the_bytes_it_names() {
    let at =
        |text: &str, mountpoint: &str| device_at(text, Path::new(mountpoint)).map(str::to_owned);
    let line = |path: &str| format!("1 2 0:7 / {path} rw - fuse.tierstore-fs x rw\n");
    assert_eq!(
        at(&line("/run/my\\040fs"), "/run/my fs"),
        Some("0:7".to_owned())
    );
    assert_eq!(
        at(&line("/run/a\\134b"), "/run/a\\b"),
        Some("0:7".to_owned())
    );
    assert_eq!(
        at(&line("/run/t\\011n\\012"), "/run/t\tn\n"),
        Some("0:7".to_owned())
    );
    // Not an escape: too few digits, a non-octal digit, the escape itself unmatched.
    assert_eq!(
        at(&line("/run/a\\04"), "/run/a\\04"),
        Some("0:7".to_owned())
    );
    assert_eq!(
        at(&line("/run/a\\089"), "/run/a\\089"),
        Some("0:7".to_owned())
    );
    assert_eq!(at(&line("/run/my\\040fs"), "/run/my\\040fs"), None);
}
