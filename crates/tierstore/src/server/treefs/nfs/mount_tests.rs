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
        timed_out: false,
    }
}

/// What a command killed at its limit gives.
fn timed_out() -> Ran {
    Ran {
        success: false,
        stdout: String::new(),
        stderr: String::new(),
        timed_out: true,
    }
}

/// A runner that records the commands (and the process's own `unmount(2)`, as the call
/// `unmount(2) <path>`), the limit each was given, and the pauses; and answers `id -u` with
/// `user_id`, a mount with the next of `mounts` (success when there are none left), `umount` with
/// `umount`, `diskutil` with `force`, the `unmount(2)` with the next of `syscall` (none: it
/// unmounts; a word: it is refused with it; refused when there are none left), the listing of the
/// mounts with `listing`, and everything else with success. While `failing_rounds` is above
/// zero, `diskutil` fails and the `unmount(2)` is refused, and each `unmount(2)` spends a round.
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    limits: Mutex<Vec<Duration>>,
    pauses: Mutex<Vec<Duration>>,
    user_id: Ran,
    mounts: Mutex<VecDeque<Ran>>,
    umount: Ran,
    force: Ran,
    syscall: Mutex<VecDeque<Option<&'static str>>>,
    failing_rounds: Mutex<u32>,
    listing: Ran,
    refuse_to_run: bool,
    /// The programs that cannot be run at all.
    unrunnable: Vec<&'static str>,
}

impl Fake {
    /// The runner of a process of user id `user_id`, with nothing mounted.
    fn base(user_id: u32) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            limits: Mutex::new(Vec::new()),
            pauses: Mutex::new(Vec::new()),
            user_id: said(true, &format!("{user_id}\n"), ""),
            mounts: Mutex::new(VecDeque::new()),
            umount: said(true, "", ""),
            force: said(true, "", ""),
            syscall: Mutex::new(VecDeque::new()),
            failing_rounds: Mutex::new(0),
            listing: said(true, "", ""),
            refuse_to_run: false,
            unrunnable: Vec::new(),
        }
    }

    fn new(user_id: u32) -> Arc<Self> {
        Arc::new(Self::base(user_id))
    }

    /// A runner of user id 0 whose listing has an NFS mount at `path`, and whose `umount` and
    /// `diskutil` answer `umount` and `force`.
    fn mounted(path: &std::path::Path, umount: Ran, force: Ran) -> Arc<Self> {
        Arc::new(Self::listing_mount(path, umount, force))
    }

    fn listing_mount(path: &std::path::Path, umount: Ran, force: Ran) -> Self {
        Self {
            umount,
            force,
            listing: said(
                true,
                &format!(
                    "127.0.0.1:/ on {} (nfs, nodev, read-only)\n",
                    path.display()
                ),
                "",
            ),
            ..Self::base(0)
        }
    }

    fn with(user_id: Ran, mounts: Vec<Ran>, umount: Ran) -> Arc<Self> {
        Arc::new(Self {
            user_id,
            mounts: Mutex::new(mounts.into()),
            umount,
            ..Self::base(0)
        })
    }

    fn refusing() -> Arc<Self> {
        Arc::new(Self {
            refuse_to_run: true,
            ..Self::base(0)
        })
    }

    fn calls(&self) -> Vec<Vec<String>> {
        locked(&self.calls).clone()
    }

    /// The programs run, and the `unmount(2)`, in order (a command run through sudo by the
    /// program after its `--`).
    fn programs(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|call| program_of(call).to_owned())
            .collect()
    }
}

/// The program of `argv`: a command run through sudo is the one after its `--`.
fn program_of(argv: &[String]) -> &str {
    argv.iter()
        .position(|word| word == "--")
        .map_or(argv[0].as_str(), |end| argv[end + 1].as_str())
}

impl Runner for Fake {
    fn run_within(&self, argv: &[String], limit: Duration) -> std::io::Result<Ran> {
        if self.refuse_to_run {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        }
        locked(&self.calls).push(argv.to_vec());
        locked(&self.limits).push(limit);
        if self.unrunnable.contains(&program_of(argv)) {
            return Err(std::io::Error::other("no such program"));
        }
        let failing = *locked(&self.failing_rounds) > 0;
        Ok(match program_of(argv) {
            "/usr/bin/id" => self.user_id.clone(),
            "/sbin/mount_nfs" => locked(&self.mounts)
                .pop_front()
                .unwrap_or_else(|| said(true, "", "")),
            "/sbin/umount" => self.umount.clone(),
            "/usr/sbin/diskutil" if failing => said(false, "Unmount failed\n", ""),
            "/usr/sbin/diskutil" => self.force.clone(),
            "/sbin/mount" => self.listing.clone(),
            _ => said(true, "", ""),
        })
    }

    fn force_unmount(&self, path: &std::path::Path, limit: Duration) -> std::io::Result<()> {
        locked(&self.calls).push(vec!["unmount(2)".to_owned(), path.display().to_string()]);
        locked(&self.limits).push(limit);
        let mut rounds = locked(&self.failing_rounds);
        if *rounds > 0 {
            *rounds -= 1;
            return Err(std::io::Error::other("refused"));
        }
        match locked(&self.syscall).pop_front() {
            Some(None) => Ok(()),
            Some(Some(word)) => Err(std::io::Error::other(word)),
            None => Err(std::io::Error::other("refused")),
        }
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
        format!(
            "vers=3,tcp,ro,rdirplus,locallocks,actimeo=3600,hard,deadtimeout=90,port={port},\
             mountport={port}"
        ),
        "127.0.0.1:/".to_owned(),
        path.display().to_string(),
    ]
}

/// Asserts the command that mounts: the options are exactly the documented set with the dead
/// timeout of the defaults (`fs.fetch_timeout` 60 s and 30 s) and the port twice (the server serves
/// MOUNT and NFS on one), the export is the loopback address and `/`, and the mount point is last;
/// and that a mount runs that command and nothing else when no user is named.
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

/// Asserts that an unmount of a mount that is there runs `umount` on the mount point and nothing
/// else when `umount` works, and that a mount point with nothing mounted is left alone (a stop after
/// someone unmounted it by hand succeeds, running only the listing).
#[test]
fn unmount_runs_on_shutdown() {
    let (_dir, path, owner) = mountpoint("nfs-mount-umount");
    let listing = vec!["/sbin/mount".to_owned()];
    let umount = vec!["/sbin/umount".to_owned(), path.display().to_string()];
    let runner = Fake::mounted(&path, said(true, "", ""), said(true, "", ""));
    mounter(Arc::clone(&runner), &loopback(), &path, owner)
        .unmount()
        .unwrap();
    assert_eq!(runner.calls(), [listing.clone(), umount]);

    let none = Fake::new(0);
    mounter(Arc::clone(&none), &loopback(), &path, owner)
        .unmount()
        .unwrap();
    assert_eq!(none.calls(), [listing]);
}

/// Asserts that a mount `umount` cannot unmount (busy, or busy with its server gone: the words macOS
/// says for each) is unmounted by `diskutil unmount force` on the mount point, as the mount user
/// (through `sudo -n -u` for a root process), and that when that fails too the error has what both
/// said, `diskutil`'s words from its standard output. Catches a stop that leaves the mount behind.
#[test]
fn an_unmount_umount_refuses_is_forced() {
    let (_dir, path, owner) = mountpoint("nfs-mount-forced");
    let shown = path.display().to_string();
    let force = vec![
        "/usr/sbin/diskutil".to_owned(),
        "unmount".to_owned(),
        "force".to_owned(),
        shown.clone(),
    ];
    let busy = format!("umount({shown}): Resource busy -- try 'diskutil unmount'\n");
    let gone = format!("umount: unmount({shown}): Operation not permitted\n");
    for refusal in [&busy, &gone] {
        let runner = Fake::mounted(&path, said(false, "", refusal), said(true, "", ""));
        mounter(Arc::clone(&runner), &loopback(), &path, owner)
            .unmount()
            .unwrap();
        assert_eq!(runner.calls().last(), Some(&force));
        assert_eq!(runner.calls().len(), 3, "{:?}", runner.calls());
    }

    let nfs = NfsSettings {
        mount_user: Some("builder".to_owned()),
        ..loopback()
    };
    let as_root = Arc::new(Fake {
        user_id: said(true, "501\n", ""),
        ..Fake::listing_mount(&path, said(false, "", &busy), said(true, "", ""))
    });
    mounter(Arc::clone(&as_root), &nfs, &path, 0)
        .unmount()
        .unwrap();
    let sudo = ["/usr/bin/sudo", "-n", "-u", "builder", "--"].map(str::to_owned);
    assert_eq!(
        as_root.calls().last().unwrap()[..],
        [&sudo[..], &force[..]].concat()[..]
    );

    let stuck = Fake::mounted(
        &path,
        said(false, "", &gone),
        said(false, &format!("Unmount failed for {shown}\n"), ""),
    );
    let why = mounter(stuck, &loopback(), &path, owner)
        .unmount()
        .unwrap_err();
    assert_eq!(
        why.to_string(),
        format!(
            "unmounting {shown}: umount: unmount({shown}): Operation not permitted; diskutil \
             unmount force: Unmount failed for {shown}; unmount(2) with MNT_FORCE: refused"
        )
    );
}

/// Asserts the runner of this machine: what a program wrote and whether it succeeded, a program
/// that is not there, and an empty command.
#[test]
fn the_system_runner_runs_a_program_and_reports_what_it_said() {
    // A limit no command here comes near: each ends by itself.
    let ample = Duration::from_secs(3600);
    let command = |script: &str| vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()];
    let ok = System
        .run_within(&command("echo out; echo err >&2"), ample)
        .unwrap();
    assert_eq!(ok, said(true, "out\n", "err\n"));
    let failed = System.run_within(&command("exit 3"), ample).unwrap();
    assert!(!failed.success && !failed.timed_out);
    let missing = System
        .run_within(&["/nonexistent/program".to_owned()], ample)
        .unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
    let empty = System.run_within(&[], ample).unwrap_err();
    assert_eq!(empty.kind(), std::io::ErrorKind::InvalidInput);
    System.pause(Duration::ZERO);
}

/// Asserts that a command that outlives its limit is killed: the runner says it timed out, and the
/// process is gone (collected, so even a signal 0 finds no such process). The command never ends
/// by itself, so the outcome does not depend on how long anything takes. Catches a helper command
/// that keeps the file service waiting.
#[test]
fn the_system_runner_kills_a_command_that_outlives_its_limit() {
    let mut pid = None;
    let argv = ["/bin/sleep".to_owned(), "3600".to_owned()];
    let ran =
        super::run_noting(&argv, Duration::from_millis(5), |child| pid = Some(child)).unwrap();
    assert_eq!(ran, timed_out());
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid.unwrap()).unwrap()).unwrap();
    assert!(rustix::process::test_kill_process(pid).is_err());
}

/// Asserts that a command that has ended but whose helper (a process it started, in its process
/// group) keeps its output open is killed at its limit with the helper, and is said to have timed
/// out, instead of a wait for the output for as long as the helper lives. Catches a runner that
/// waits on the output of a helper that never ends.
#[test]
fn a_command_whose_helper_keeps_its_output_open_is_killed_at_its_limit() {
    let argv = [
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "/bin/sleep 3600 & echo started".to_owned(),
    ];
    let ran = System.run_within(&argv, Duration::from_millis(5)).unwrap();
    assert_eq!(ran, timed_out());
}

/// Asserts the `unmount(2)` of the system runner on a directory that is not a mount point: an
/// error from the system, not a panic and not a success; and that an answer that does not come
/// within the limit is a `TimedOut` error that says the limit.
#[test]
fn the_system_unmount_reports_what_the_system_said_or_that_it_did_not_answer() {
    let dir = TestDir::under(std::env::temp_dir(), "nfs-mount-syscall");
    let refused = System
        .force_unmount(dir.path(), Duration::from_secs(3600))
        .unwrap_err();
    assert_ne!(refused.kind(), std::io::ErrorKind::TimedOut, "{refused}");
    let (held, answer) = std::sync::mpsc::channel::<std::io::Result<()>>();
    let late = super::answer_within(answer, Duration::from_millis(1)).unwrap_err();
    assert_eq!(
        (late.kind(), late.to_string()),
        (std::io::ErrorKind::TimedOut, "no answer in 1ms".to_owned())
    );
    drop(held);
    let thread = super::on_thread(|| Ok(())).unwrap();
    assert!(super::answer_within(thread, Duration::from_secs(3600)).is_ok());
}

/// A runner whose listing of the mounts is `listing`, or that cannot run it.
struct Lister(Option<String>);

impl Runner for Lister {
    fn run_within(&self, argv: &[String], _limit: Duration) -> std::io::Result<Ran> {
        assert_eq!(argv, ["/sbin/mount"]);
        match &self.0 {
            Some(text) => Ok(said(true, text, "")),
            None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        }
    }

    fn force_unmount(&self, _path: &std::path::Path, _limit: Duration) -> std::io::Result<()> {
        unreachable!("a listing unmounts nothing")
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
    // The listing names the path the kernel resolved (`/private/var` where the test directory is
    // under `/var`), which is not the path as named.
    let real = std::fs::canonicalize(&path).unwrap();
    assert!(at(&line(&path), &path));
    assert!(at(&line(&real), &path), "listed by the path it resolves to");
    assert!(at(&line(&real), &link), "named by a link to it");
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
/// files, an error that names the directory when it cannot be written, and read back by
/// `made_from`, which gives none for a mount with no note.
#[test]
fn the_address_of_a_mount_is_noted_and_read() {
    let dir = TestDir::under(std::env::temp_dir(), "nfs-mount-note");
    let here: std::net::SocketAddr = "127.0.0.1:12049".parse().unwrap();
    assert_eq!(Mounter::made_from(dir.path()), None);
    Mounter::record(dir.path(), here).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("nfs-mounted")).unwrap(),
        "127.0.0.1:12049"
    );
    assert_eq!(
        Mounter::made_from(dir.path()),
        Some("127.0.0.1:12049".to_owned())
    );
    let missing = dir.path().join("none");
    let why = Mounter::record(&missing, here).unwrap_err().to_string();
    assert!(
        why.starts_with("writing the address of the mount:"),
        "{why}"
    );
}

/// A writer whose bytes the test reads back: the log of the process.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        locked(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    /// The lines written at `level` and above while `run` ran, and what it gave.
    fn of<T>(level: tracing::Level, run: impl FnOnce() -> T) -> (T, String) {
        let log = Self::default();
        let given = tracing::subscriber::with_default(
            crate::server::build_info::stderr_log::LineLog::new(log.clone(), level),
            run,
        );
        let text = String::from_utf8(locked(&log.0).clone()).unwrap();
        (given, text)
    }
}

/// The programs of an unmount's round: the listing, `umount`, `diskutil`, the `unmount(2)`.
const ROUND: [&str; 4] = [
    "/sbin/mount",
    "/sbin/umount",
    "/usr/sbin/diskutil",
    "unmount(2)",
];

/// Asserts that the mount is `hard` and carries `deadtimeout` in whole seconds of
/// `fs.nfs.dead_timeout` (rounded up), and nothing else beside the documented options. Catches a
/// mount that waits for ever for a server that is gone, and one whose dead timeout is not the
/// setting (one shorter than `fs.fetch_timeout` would end the mount during a fill the server still
/// waits for; the config refuses that, see `config_tests.rs`).
#[test]
fn the_mount_is_hard_and_dies_after_the_dead_timeout_of_the_setting() {
    let (_dir, path, owner) = mountpoint("nfs-mount-dead");
    for (dead, seconds) in [
        (Duration::from_secs(125), 125),
        (Duration::from_millis(90_500), 91),
    ] {
        let nfs = NfsSettings {
            dead_timeout: dead,
            ..loopback()
        };
        let command = mounter(Fake::new(0), &nfs, &path, owner).mount_command(LOOP, 7);
        let options: Vec<&str> = command[2].split(',').collect();
        assert!(options.contains(&"hard"), "{options:?}");
        assert!(!options.contains(&"soft"), "{options:?}");
        assert_eq!(
            options,
            [
                "vers=3",
                "tcp",
                "ro",
                "rdirplus",
                "locallocks",
                "actimeo=3600",
                "hard",
                &format!("deadtimeout={seconds}"),
                "port=7",
                "mountport=7",
            ]
        );
    }
}

/// Asserts that every command of the mount and of the unmount, and the `unmount(2)`, is given the
/// limit `fs.nfs.unmount_wait`, and that the mount user's number is asked once (the unmount uses
/// the start's answer, one command fewer to bound). Catches a command run without a limit.
#[test]
fn every_command_has_the_limit_of_the_setting() {
    let (_dir, path, owner) = mountpoint("nfs-mount-limits");
    let nfs = NfsSettings {
        mount_user: Some("builder".to_owned()),
        unmount_wait: Duration::from_secs(7),
        ..loopback()
    };
    let runner = Arc::new(Fake {
        user_id: said(true, &format!("{owner}\n"), ""),
        ..Fake::listing_mount(
            &path,
            said(false, "", "busy\n"),
            said(false, "Unmount failed\n", ""),
        )
    });
    locked(&runner.syscall).push_back(None);
    let mounter = mounter(Arc::clone(&runner), &nfs, &path, owner);
    mounter.mount(LOOP, 7).unwrap();
    mounter.unmount().unwrap();
    assert_eq!(
        runner.programs(),
        [
            "/usr/bin/id",
            "/sbin/mount_nfs",
            "/sbin/mount",
            "/sbin/umount",
            "/usr/sbin/diskutil",
            "unmount(2)"
        ]
    );
    assert_eq!(*locked(&runner.limits), [Duration::from_secs(7); 6]);
}

/// Asserts that an `umount` killed at its limit is followed by `diskutil unmount force`, which
/// unmounts: the unmount succeeds, each step says in a line what it found, and one warning says
/// force was needed and why `umount` failed. Catches an unmount that waits on a stuck `umount`.
#[test]
fn an_umount_that_times_out_is_followed_by_diskutil_with_a_warning() {
    let (_dir, path, owner) = mountpoint("nfs-mount-umount-late");
    let shown = path.display().to_string();
    let runner = Fake::mounted(&path, timed_out(), said(true, "", ""));
    let (unmounted, info) = Log::of(tracing::Level::INFO, || {
        mounter(Arc::clone(&runner), &loopback(), &path, owner).unmount()
    });
    unmounted.unwrap();
    assert_eq!(runner.programs(), ROUND[..3]);
    let warning = format!(
        "tierstore fs: umount of {shown} failed (did not end in 20000ms and was killed); \
         unmounted by diskutil unmount force, so what still reads the mount gets an error, not \
         a wait"
    );
    assert_eq!(
        info.lines().collect::<Vec<_>>(),
        [
            format!("tierstore fs: /sbin/umount {shown}: did not end in 20000ms and was killed"),
            format!("tierstore fs: /usr/sbin/diskutil unmount force {shown}: unmounted"),
            warning.clone(),
        ]
    );
    let (_, warnings) = Log::of(tracing::Level::WARN, || {
        mounter(
            Fake::mounted(&path, timed_out(), said(true, "", "")),
            &loopback(),
            &path,
            owner,
        )
        .unmount()
    });
    assert_eq!(warnings, format!("{warning}\n"));
}

/// Asserts that when `umount` and `diskutil` are both killed at their limit, the process unmounts
/// by itself (`unmount(2)` with `MNT_FORCE`), in exactly that order, and warns once that it took
/// force. Catches an unmount that stops at the commands, which may be refused to a service.
#[test]
fn when_umount_and_diskutil_time_out_the_process_unmounts_by_itself() {
    let (_dir, path, owner) = mountpoint("nfs-mount-syscall-works");
    let shown = path.display().to_string();
    let runner = Fake::mounted(&path, timed_out(), timed_out());
    locked(&runner.syscall).push_back(None);
    let (unmounted, warnings) = Log::of(tracing::Level::WARN, || {
        mounter(Arc::clone(&runner), &loopback(), &path, owner).unmount()
    });
    unmounted.unwrap();
    assert_eq!(runner.programs(), ROUND);
    assert_eq!(runner.calls()[3], ["unmount(2)".to_owned(), shown.clone()]);
    assert_eq!(
        warnings,
        format!(
            "tierstore fs: umount of {shown} failed (did not end in 20000ms and was killed); \
             unmounted by this process's unmount(2) with MNT_FORCE, so what still reads the \
             mount gets an error, not a wait\n"
        )
    );
}

/// Asserts that when every step fails the unmount is an error that says what each said, in the
/// order they ran; and that the unmount of a process that ends does not give up: it says why,
/// pauses `LEAVE_RETRY` and tries the whole round again, and returns once a round unmounts.
/// Catches a stop that ends the process while its mount is up.
#[test]
fn when_every_step_fails_the_unmount_says_so_and_the_leave_tries_again() {
    let (_dir, path, owner) = mountpoint("nfs-mount-all-fail");
    let shown = path.display().to_string();
    let runner = Fake::mounted(
        &path,
        timed_out(),
        said(false, &format!("Unmount failed for {shown}\n"), ""),
    );
    locked(&runner.syscall).push_back(Some("Operation not permitted"));
    let why = mounter(Arc::clone(&runner), &loopback(), &path, owner)
        .unmount()
        .unwrap_err();
    assert_eq!(
        why.to_string(),
        format!(
            "unmounting {shown}: did not end in 20000ms and was killed; diskutil unmount force: \
             Unmount failed for {shown}; unmount(2) with MNT_FORCE: Operation not permitted"
        )
    );
    assert_eq!(runner.programs(), ROUND);

    let retried = Arc::new(Fake {
        failing_rounds: Mutex::new(2),
        ..Fake::listing_mount(&path, timed_out(), said(true, "", ""))
    });
    let (_, errors) = Log::of(tracing::Level::ERROR, || {
        mounter(Arc::clone(&retried), &loopback(), &path, owner).unmount_or_retry();
    });
    assert_eq!(*locked(&retried.pauses), [super::LEAVE_RETRY; 2]);
    assert_eq!(
        retried.programs(),
        [&ROUND[..], &ROUND[..], &ROUND[..3]].concat()
    );
    assert_eq!(errors.lines().count(), 2, "{errors}");
    assert!(
        errors
            .lines()
            .all(|line| line
                .ends_with("the process goes on serving the mount and tries again in 10s")),
        "{errors}"
    );
}

/// Asserts what a command killed at its limit does to each use: a listing of the mounts that timed
/// out is an error naming `/sbin/mount` (and an unmount then tries its steps all the same, as if
/// mounted), an `id` that timed out is a refused mount user, and a mount that timed out on every
/// try is a failed mount, each saying the limit.
#[test]
fn a_command_killed_at_its_limit_is_a_failure_that_says_so() {
    let (_dir, path, owner) = mountpoint("nfs-mount-killed");
    let listing_late = Arc::new(Fake {
        listing: timed_out(),
        ..Fake::base(0)
    });
    let late = mounter(Arc::clone(&listing_late), &loopback(), &path, owner);
    assert_eq!(
        late.is_mounted().unwrap_err().to_string(),
        "running /sbin/mount: did not end in 20000ms and was killed"
    );
    late.unmount().unwrap();
    assert_eq!(listing_late.programs(), ["/sbin/mount", "/sbin/umount"]);

    let nfs = NfsSettings {
        mount_user: Some("builder".to_owned()),
        ..loopback()
    };
    let id_late = Fake::with(timed_out(), vec![], said(true, "", ""));
    assert_eq!(
        mounter(id_late, &nfs, &path, owner)
            .mount(LOOP, 7)
            .unwrap_err()
            .to_string(),
        "fs.nfs.mount_user builder: did not end in 20000ms and was killed"
    );

    let mount_late = Fake::with(
        said(true, "0", ""),
        vec![timed_out(), timed_out(), timed_out()],
        said(true, "", ""),
    );
    assert_eq!(
        mounter(mount_late, &loopback(), &path, owner)
            .mount(LOOP, 7)
            .unwrap_err()
            .to_string(),
        format!(
            "mounting {} did not succeed in 3 tries: did not end in 20000ms and was killed",
            path.display()
        )
    );
}

/// The `ExitTimeOut` of the launchd job at `path` from the repository's root, in seconds.
fn exit_timeout(path: &str) -> u64 {
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    let text = std::fs::read_to_string(&file).unwrap();
    let (_, after) = text.split_once("<key>ExitTimeOut</key>").unwrap();
    let (_, value) = after.split_once("<integer>").unwrap();
    let (number, _) = value.split_once("</integer>").unwrap();
    number.trim().parse().unwrap()
}

/// Asserts that launchd waits for an unmount to end before it kills the file service: the
/// `ExitTimeOut` of both fs jobs (the package's and the hostkeeper unit) is more than the longest
/// unmount, four bounded steps (the listing, `umount`, `diskutil`, `unmount(2)`), each the limit
/// and the wait to collect a killed command, for the default limit and for the largest one the
/// configuration takes. Catches a job file whose launchd kills the process in the middle of its
/// unmount.
#[test]
fn the_fs_jobs_wait_longer_than_the_longest_unmount() {
    let longest = |limit: Duration| (limit + super::REAP_WAIT) * 4;
    for job in [
        "packaging/macos-pkg/tierstore-fs.plist",
        "packaging/hostkeeper/units/tierstore-fs.plist",
    ] {
        let waits = Duration::from_secs(exit_timeout(job));
        assert!(
            waits > longest(NfsSettings::default().unmount_wait),
            "{job}"
        );
        assert!(
            waits > longest(crate::server::treefs::nfs::config::MOST_UNMOUNT_WAIT),
            "{job}"
        );
    }
}

/// Asserts that a step of the unmount whose program cannot be run at all is a failed step, not the
/// end of the unmount: the next step runs, and the error says what the system said for each.
/// Catches an unmount that gives up at the first command it cannot run.
#[test]
fn a_step_that_cannot_be_run_is_a_failed_step_and_the_next_runs() {
    let (_dir, path, owner) = mountpoint("nfs-mount-unrunnable");
    let shown = path.display().to_string();
    let runner = Arc::new(Fake {
        unrunnable: vec!["/sbin/umount", "/usr/sbin/diskutil"],
        ..Fake::listing_mount(&path, said(true, "", ""), said(true, "", ""))
    });
    let why = mounter(Arc::clone(&runner), &loopback(), &path, owner)
        .unmount()
        .unwrap_err();
    assert_eq!(runner.programs(), ROUND);
    assert_eq!(
        why.to_string(),
        format!(
            "unmounting {shown}: running /sbin/umount {shown}: no such program; diskutil unmount \
             force: running /usr/sbin/diskutil unmount force {shown}: no such program; unmount(2) \
             with MNT_FORCE: refused"
        )
    );
}
