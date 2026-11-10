//! The mount and unmount commands, over a runner that runs none, and the runner that runs them.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::MetadataExt as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{Mounter, NfsError, Ran, Runner, System};
use crate::server::treefs::locked;
use crate::server::treefs::nfs::config::NfsSettings;
use crate::server::treefs::nfs::fixtures::loopback;
use tierstore_testkit::dir::TestDir;

const LOOP: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

fn said(success: bool, stdout: &str, stderr: &str) -> Ran {
    Ran {
        success,
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    }
}

/// A runner that records the commands and the pauses, and answers `id -u` with `user_id`, a mount
/// with the next of `mounts` (success when there are none left), and everything else with success.
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    pauses: Mutex<Vec<Duration>>,
    user_id: Ran,
    mounts: Mutex<VecDeque<Ran>>,
    umount: Ran,
    refuse_to_run: bool,
}

impl Fake {
    fn new(user_id: u32) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            pauses: Mutex::new(Vec::new()),
            user_id: said(true, &format!("{user_id}\n"), ""),
            mounts: Mutex::new(VecDeque::new()),
            umount: said(true, "", ""),
            refuse_to_run: false,
        })
    }

    fn with(user_id: Ran, mounts: Vec<Ran>, umount: Ran) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            pauses: Mutex::new(Vec::new()),
            user_id,
            mounts: Mutex::new(mounts.into()),
            umount,
            refuse_to_run: false,
        })
    }

    fn refusing() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            pauses: Mutex::new(Vec::new()),
            user_id: said(true, "0", ""),
            mounts: Mutex::new(VecDeque::new()),
            umount: said(true, "", ""),
            refuse_to_run: true,
        })
    }

    fn calls(&self) -> Vec<Vec<String>> {
        locked(&self.calls).clone()
    }
}

impl Runner for Fake {
    fn run(&self, argv: &[String]) -> std::io::Result<Ran> {
        if self.refuse_to_run {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        }
        locked(&self.calls).push(argv.to_vec());
        Ok(match argv[0].as_str() {
            "/usr/bin/id" => self.user_id.clone(),
            "/sbin/mount_nfs" => locked(&self.mounts)
                .pop_front()
                .unwrap_or_else(|| said(true, "", "")),
            "/sbin/umount" => self.umount.clone(),
            _ => said(true, "", ""),
        })
    }

    fn pause(&self, how_long: Duration) {
        locked(&self.pauses).push(how_long);
    }
}

fn mountpoint(name: &str) -> (TestDir, std::path::PathBuf, u32) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().join("mnt");
    std::fs::create_dir_all(&path).unwrap();
    let owner = std::fs::metadata(&path).unwrap().uid();
    (dir, path, owner)
}

fn mounter(runner: Arc<Fake>, nfs: &NfsSettings, path: &std::path::Path, euid: u32) -> Mounter {
    Mounter::new(runner, nfs, path.to_owned(), euid)
}

fn mount_line(path: &std::path::Path, port: u16) -> Vec<String> {
    vec![
        "/sbin/mount_nfs".to_owned(),
        "-o".to_owned(),
        format!("vers=3,tcp,ro,rdirplus,locallocks,actimeo=3600,hard,port={port},mountport={port}"),
        "127.0.0.1:/".to_owned(),
        path.display().to_string(),
    ]
}

/// Asserts the command that mounts: the options are exactly the documented set with the port twice
/// (the server serves MOUNT and NFS on one), the export is the loopback address and `/`, and the
/// mount point is last; and that a mount runs that command and nothing else when no user is named.
#[test]
fn mount_options_are_exactly_the_documented_set() {
    let (_dir, path, owner) = mountpoint("nfs-mount-options");
    let runner = Fake::new(0);
    let mounter = mounter(Arc::clone(&runner), &loopback(), &path, owner);
    assert_eq!(mounter.mount_command(LOOP, 12049), mount_line(&path, 12049));
    mounter.mount(LOOP, 4321).unwrap();
    assert_eq!(runner.calls(), [mount_line(&path, 4321)]);
}

/// Asserts that a named user is looked up by `id -u` and the mount runs as it: directly where the
/// process is that user, through `sudo -n -u` where the process is root, and not at all where it is
/// someone else; and that the mount point must be the user's.
#[test]
fn mount_runs_as_the_configured_user() {
    let (_dir, path, owner) = mountpoint("nfs-mount-user");
    let nfs = NfsSettings {
        mount_user: Some("builder".to_owned()),
        ..loopback()
    };
    let runner = Fake::new(owner);
    mounter(Arc::clone(&runner), &nfs, &path, owner)
        .mount(LOOP, 7)
        .unwrap();
    let id = vec![
        "/usr/bin/id".to_owned(),
        "-u".to_owned(),
        "builder".to_owned(),
    ];
    assert_eq!(runner.calls(), [id, mount_line(&path, 7)]);

    // The commands of a root process run through sudo as the user.
    let root = mounter(Fake::new(501), &nfs, &path, 0);
    let plain = vec!["/sbin/umount".to_owned(), path.display().to_string()];
    let through = root.as_user(501, plain.clone()).unwrap();
    assert_eq!(
        through,
        ["/usr/bin/sudo", "-n", "-u", "builder", "--"]
            .iter()
            .map(|word| (*word).to_owned())
            .chain(plain.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        mounter(Fake::new(501), &nfs, &path, 501)
            .as_user(501, plain.clone())
            .unwrap(),
        plain
    );
    let refused = mounter(Fake::new(501), &nfs, &path, 9)
        .as_user(501, plain)
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "this process is uid 9 and fs.nfs.mount_user builder is uid 501: only root runs a mount as another user"
    );
}

/// Asserts that a user the machine does not know is refused with what `id` said, and that a command
/// that cannot be run at all is an error naming it.
#[test]
fn a_mount_user_the_machine_lacks_is_refused() {
    let (_dir, path, owner) = mountpoint("nfs-mount-nouser");
    let nfs = NfsSettings {
        mount_user: Some("ghost".to_owned()),
        ..loopback()
    };
    let runner = Fake::with(
        said(false, "", "id: ghost: no such user\n"),
        vec![],
        said(true, "", ""),
    );
    let why = mounter(runner, &nfs, &path, owner)
        .mount(LOOP, 7)
        .unwrap_err();
    assert_eq!(
        why.to_string(),
        "fs.nfs.mount_user ghost: id: ghost: no such user"
    );
    // An answer that is not a number is as good as none.
    let runner = Fake::with(said(true, "ghost\n", ""), vec![], said(true, "", ""));
    assert!(matches!(
        mounter(runner, &nfs, &path, owner).mount(LOOP, 7),
        Err(NfsError::User { .. })
    ));
    // A number from a command that failed is no answer.
    let runner = Fake::with(said(false, "5\n", "boom\n"), vec![], said(true, "", ""));
    let why = mounter(runner, &nfs, &path, owner)
        .mount(LOOP, 7)
        .unwrap_err();
    assert_eq!(why.to_string(), "fs.nfs.mount_user ghost: boom");
    let runner = Fake::refusing();
    let why = mounter(runner, &nfs, &path, owner)
        .mount(LOOP, 7)
        .unwrap_err();
    assert!(
        why.to_string()
            .starts_with("running /usr/bin/id -u ghost: "),
        "{why}"
    );
}

/// Asserts that a mount point owned by someone else than the mount user is refused before any
/// mount is tried, and the message says who owns it and who should.
#[test]
fn mount_refused_when_the_mount_point_has_another_owner_with_the_owner_in_the_message() {
    let (_dir, path, owner) = mountpoint("nfs-mount-owner");
    let nfs = NfsSettings {
        mount_user: Some("builder".to_owned()),
        ..loopback()
    };
    let runner = Fake::new(owner + 1);
    let why = mounter(Arc::clone(&runner), &nfs, &path, owner + 1)
        .mount(LOOP, 7)
        .unwrap_err();
    assert_eq!(
        why.to_string(),
        format!(
            "fs.mountpoint {} is owned by uid {owner}, not by the mount user (uid {}); give it to that user",
            path.display(),
            owner + 1
        )
    );
    assert!(
        runner
            .calls()
            .iter()
            .all(|call| call[0] != "/sbin/mount_nfs")
    );
    let gone = mounter(Fake::new(owner), &loopback(), &path.join("nothing"), owner)
        .mount(LOOP, 7)
        .unwrap_err();
    assert!(gone.to_string().starts_with("running stat "), "{gone}");
}

/// Asserts that a mount that fails is tried again, after the pause `fs.nfs.mount_retry`, for as many
/// tries as `fs.nfs.mount_wait` holds, and stops at the first that succeeds; and that one that never
/// succeeds is an error with what the last try said.
#[test]
fn mount_retries_until_the_server_listens_then_stops() {
    let (_dir, path, owner) = mountpoint("nfs-mount-retry");
    let refused = || said(false, "", "mount_nfs: can't mount: Connection refused\n");
    let runner = Fake::with(
        said(true, "0", ""),
        vec![refused(), refused()],
        said(true, "", ""),
    );
    mounter(Arc::clone(&runner), &loopback(), &path, owner)
        .mount(LOOP, 7)
        .unwrap();
    assert_eq!(runner.calls().len(), 3);
    assert_eq!(*locked(&runner.pauses), [Duration::from_secs(1); 2]);

    let runner = Fake::with(
        said(true, "0", ""),
        vec![refused(), refused(), refused(), refused()],
        said(true, "", ""),
    );
    let why = mounter(Arc::clone(&runner), &loopback(), &path, owner)
        .mount(LOOP, 7)
        .unwrap_err();
    assert_eq!(
        runner.calls().len(),
        3,
        "3s of wait, a pause of 1s: three tries"
    );
    assert_eq!(
        why.to_string(),
        format!(
            "mounting {} did not succeed in 3 tries: mount_nfs: can't mount: Connection refused",
            path.display()
        )
    );
}

/// Asserts that an unmount runs `umount` on the mount point, and that its failure is an error with
/// what it said.
#[test]
fn unmount_runs_on_shutdown() {
    let (_dir, path, owner) = mountpoint("nfs-mount-umount");
    let runner = Fake::new(0);
    mounter(Arc::clone(&runner), &loopback(), &path, owner)
        .unmount()
        .unwrap();
    assert_eq!(
        runner.calls(),
        [vec!["/sbin/umount".to_owned(), path.display().to_string()]]
    );
    let busy = Fake::with(
        said(true, "0", ""),
        vec![],
        said(false, "", "umount: Resource busy\n"),
    );
    let why = mounter(busy, &loopback(), &path, owner)
        .unmount()
        .unwrap_err();
    assert_eq!(
        why.to_string(),
        format!("unmounting {}: umount: Resource busy", path.display())
    );
}

/// Asserts the runner of this machine: what a program wrote and whether it succeeded, a program
/// that is not there, and an empty command.
#[test]
fn the_system_runner_runs_a_program_and_reports_what_it_said() {
    let command = |script: &str| vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()];
    let ok = System.run(&command("echo out; echo err >&2")).unwrap();
    assert_eq!(ok, said(true, "out\n", "err\n"));
    let failed = System.run(&command("exit 3")).unwrap();
    assert!(!failed.success);
    let missing = System
        .run(&["/nonexistent/program".to_owned()])
        .unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
    let empty = System.run(&[]).unwrap_err();
    assert_eq!(empty.kind(), std::io::ErrorKind::InvalidInput);
    System.pause(Duration::ZERO);
}

/// A runner whose listing of the mounts is `listing`, or that cannot run it.
struct Lister(Option<String>);

impl Runner for Lister {
    fn run(&self, argv: &[String]) -> std::io::Result<Ran> {
        assert_eq!(argv, ["/sbin/mount"]);
        match &self.0 {
            Some(text) => Ok(said(true, text, "")),
            None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        }
    }

    fn pause(&self, _how_long: Duration) {}
}

/// Asserts that the listing of the mounts finds an NFS mount at the mount point, by the path as it
/// is named and by the path it resolves to (macOS lists `/private/var` for `/var`), and not a
/// mount at another path whose name begins with it; and that a listing that cannot be run is an
/// error that names the command.
#[test]
fn a_mount_at_the_mount_point_is_found_in_the_listing() {
    let (dir, path, _) = mountpoint("nfs-mounted");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let nfs = NfsSettings::default();
    let at = |listing: &str, mountpoint: &std::path::Path| {
        Mounter::new(
            Arc::new(Lister(Some(listing.to_owned()))),
            &nfs,
            mountpoint.to_owned(),
            0,
        )
        .is_mounted()
        .unwrap()
    };
    let line =
        |at: &std::path::Path| format!("127.0.0.1:/ on {} (nfs, nodev, read-only)\n", at.display());
    assert!(at(&line(&path), &path));
    assert!(at(&line(&path), &link), "named by a link to it");
    assert!(!at("/dev/disk1s1 on / (apfs, local)\n", &path));
    assert!(!at(&line(&dir.path().join("mnt2")), &path));
    let gone = dir.path().join("gone");
    assert!(
        at(&line(&gone), &gone),
        "a path that does not resolve is compared as named"
    );
    let error = Mounter::new(Arc::new(Lister(None)), &nfs, path, 0)
        .is_mounted()
        .unwrap_err();
    assert!(
        error.to_string().starts_with("running /sbin/mount: "),
        "{error}"
    );
}

/// Asserts the note of the address a mount was made from: written next to the runtime directory's
/// files, an error that names the directory when it cannot be written, and read back by `agrees`,
/// which accepts the address it names and a mount with no note, and refuses another address with
/// both in the message.
#[test]
fn the_address_of_a_mount_is_noted_and_checked() {
    let dir = TestDir::under(std::env::temp_dir(), "nfs-mount-note");
    let mounter = Mounter::new(Fake::new(501), &loopback(), dir.path().join("mnt"), 501);
    let here: std::net::SocketAddr = "127.0.0.1:12049".parse().unwrap();
    mounter.agrees(dir.path(), here).unwrap();
    Mounter::record(dir.path(), here).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("nfs-mounted")).unwrap(),
        "127.0.0.1:12049"
    );
    mounter.agrees(dir.path(), here).unwrap();
    let there: std::net::SocketAddr = "127.0.0.1:12050".parse().unwrap();
    let why = mounter.agrees(dir.path(), there).unwrap_err();
    assert!(matches!(why, NfsError::MountedFrom { .. }), "{why}");
    assert!(
        why.to_string().contains(
            "is mounted from 127.0.0.1:12049, and this instance serves on 127.0.0.1:12050"
        ),
        "{why}"
    );
    let missing = dir.path().join("none");
    let why = Mounter::record(&missing, here).unwrap_err().to_string();
    assert!(
        why.starts_with("writing the address of the mount:"),
        "{why}"
    );
}
