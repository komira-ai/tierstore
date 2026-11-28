//! The process: a start over a fake mount command and a real loopback socket, a stop, the checks a
//! start makes, and the signals.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::boot::BootSource;
use super::super::error::NfsError;
use super::super::fixtures::{EPOCH, attached, handle_of, loopback, settings};
use super::super::fs::TreeNfs;
use super::super::mount::{Ran, Runner};
use super::super::wire_client::{Client, Cred, Dec, Enc, GETATTR, NFS};
use super::{Leaving, Running, Serve, serve_with, timer};
use crate::server::treefs::config::{FsSettings, builders, from_params};
use crate::server::treefs::ino;
use crate::server::treefs::locked;
use crate::server::treefs::testing::SIGNALS;
use crate::server::treefs::testing::until;
use futures::future::BoxFuture;
use nfsserve::tcp::NFSTcpListener;

/// A check a runner makes when it runs `umount`.
type Check = Box<dyn FnOnce() -> bool + Send>;

/// Records the commands (and the process's own `unmount(2)`, as the call `unmount(2) <path>`),
/// answers every one with success unless told to fail the mount, `umount`, the forced unmount or
/// the `unmount(2)`, and keeps a listing of the mounts that a mount adds its line to and an
/// unmount empties.
pub(super) struct Recorder {
    calls: Mutex<Vec<Vec<String>>>,
    mount_works: bool,
    /// Whether `umount` unmounts; if not it says `Resource busy`.
    umount_works: bool,
    /// Whether `diskutil unmount force` unmounts.
    force_works: bool,
    /// Whether the `unmount(2)` unmounts; if not it says `refused`.
    syscall_works: bool,
    /// The rounds of an unmount in which the forced unmount and the `unmount(2)` fail whatever
    /// the two above say; one is spent at each `unmount(2)`.
    failing_rounds: Mutex<u32>,
    /// The pauses asked for.
    pauses: Mutex<Vec<Duration>>,
    /// The program whose run panics.
    panics: Option<&'static str>,
    /// What the listing of the mounts says.
    listing: Mutex<String>,
    /// What `umount` checks before it runs, and what the check found.
    check: Mutex<Option<Check>>,
    checked: Mutex<Option<bool>>,
}

impl Recorder {
    /// A runner with nothing mounted, whose every command works but, unless `mount_works`, the
    /// mount.
    pub(super) fn new(mount_works: bool) -> Arc<Self> {
        Self::made(mount_works, String::new())
    }

    fn made(mount_works: bool, listing: String) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            mount_works,
            umount_works: true,
            force_works: true,
            syscall_works: true,
            failing_rounds: Mutex::new(0),
            pauses: Mutex::new(Vec::new()),
            panics: None,
            listing: Mutex::new(listing),
            check: Mutex::new(None),
            checked: Mutex::new(None),
        })
    }

    /// A runner whose listing of the mounts has an NFS mount at `path`.
    pub(super) fn mounted_at(path: &std::path::Path) -> Arc<Self> {
        Self::made(true, line_of(path))
    }

    /// A runner with an NFS mount at `path` that `umount` finds busy, and that the forced unmount
    /// unmounts if `forced`; if not, the `unmount(2)` fails too.
    pub(super) fn busy_at(path: &std::path::Path, forced: bool) -> Arc<Self> {
        Arc::new(Self {
            umount_works: false,
            force_works: forced,
            syscall_works: forced,
            ..Arc::into_inner(Self::mounted_at(path)).unwrap()
        })
    }

    /// A runner with an NFS mount at `path` that `umount` finds busy, and whose forced unmount and
    /// `unmount(2)` both fail for `rounds` rounds of an unmount; then the forced unmount works.
    pub(super) fn stuck_for(path: &std::path::Path, rounds: u32) -> Arc<Self> {
        Arc::new(Self {
            failing_rounds: Mutex::new(rounds),
            ..Arc::into_inner(Self::busy_at(path, true)).unwrap()
        })
    }

    /// The pauses asked for.
    pub(super) fn pauses(&self) -> Vec<Duration> {
        locked(&self.pauses).clone()
    }

    /// Whether this round of an unmount still fails.
    fn failing(&self) -> bool {
        *locked(&self.failing_rounds) > 0
    }

    fn panicking(program: &'static str) -> Arc<Self> {
        Arc::new(Self {
            panics: Some(program),
            ..Arc::into_inner(Self::new(true)).unwrap()
        })
    }

    pub(super) fn calls(&self) -> Vec<Vec<String>> {
        locked(&self.calls).clone()
    }

    /// The programs run, in order.
    pub(super) fn programs(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .map(|call| call[0].clone())
            .collect()
    }

    /// Makes `umount` run `check` first, and note what it found.
    pub(super) fn check_at_umount(&self, check: impl FnOnce() -> bool + Send + 'static) {
        *locked(&self.check) = Some(Box::new(check));
    }

    /// Makes `umount` call NFS's NULL procedure on `connection` first, and note whether the
    /// server answered.
    pub(super) fn probe_at_umount(&self, mut connection: std::net::TcpStream) {
        self.check_at_umount(move || null_answered(&mut connection));
    }

    /// What the check `umount` ran found: none if it ran none.
    pub(super) fn checked_at_umount(&self) -> Option<bool> {
        *locked(&self.checked)
    }

    /// Empties the listing of the mounts, as an unmount by hand does.
    pub(super) fn unmounted_by_hand(&self) {
        locked(&self.listing).clear();
    }

    /// Whether the listing of the mounts has a mount now.
    pub(super) fn has_mount(&self) -> bool {
        !locked(&self.listing).is_empty()
    }

    /// The port the mount command named.
    pub(super) fn mounted_port(&self) -> Option<u16> {
        let calls = self.calls();
        let line = calls.iter().find(|call| call[0] == "/sbin/mount_nfs")?;
        line[2]
            .split(',')
            .find_map(|option| option.strip_prefix("port="))
            .and_then(|port| port.parse().ok())
    }
}

/// Whether the server on `connection` answers a call of NFS's NULL procedure: a server that is
/// ending its connections reads no more requests and shuts its side, so the read ends with nothing.
fn null_answered(connection: &mut std::net::TcpStream) -> bool {
    use std::io::Read as _;
    // xid 7, CALL, RPC version 2, NFS version 3, NULL, no credential, no verifier.
    let words = [7u32, 0, 2, 100_003, 3, 0, 0, 0, 0, 0];
    let body: Vec<u8> = words.iter().flat_map(|word| word.to_be_bytes()).collect();
    let header = (1u32 << 31) | u32::try_from(body.len()).unwrap();
    let record = [header.to_be_bytes().as_slice(), &body].concat();
    let mut answer = [0u8; 8];
    connection.write_all(&record).is_ok() && connection.read_exact(&mut answer).is_ok()
}

/// The line of the listing of the mounts for an NFS mount at `path`.
fn line_of(path: &std::path::Path) -> String {
    format!(
        "127.0.0.1:/ on {} (nfs, nodev, nosuid, read-only)\n",
        path.display()
    )
}

impl Runner for Recorder {
    fn run_within(&self, argv: &[String], _limit: Duration) -> std::io::Result<Ran> {
        locked(&self.calls).push(argv.to_vec());
        assert_ne!(self.panics, Some(argv[0].as_str()), "the command dies");
        if argv[0] == "/sbin/umount"
            && let Some(check) = locked(&self.check).take()
        {
            *locked(&self.checked) = Some(check());
        }
        let (success, stderr) = match argv[0].as_str() {
            "/sbin/mount_nfs" if self.mount_works => {
                *locked(&self.listing) = line_of(std::path::Path::new(&argv[4]));
                (true, "")
            }
            "/sbin/mount_nfs" => (false, "refused"),
            "/sbin/umount" if self.umount_works => {
                locked(&self.listing).clear();
                (true, "")
            }
            "/sbin/umount" => (
                false,
                "umount(mnt): Resource busy -- try 'diskutil unmount'",
            ),
            "/usr/sbin/diskutil" if self.force_works && !self.failing() => {
                locked(&self.listing).clear();
                (true, "")
            }
            "/usr/sbin/diskutil" => (false, "Unmount failed for mnt"),
            _ => (true, ""),
        };
        Ok(Ran {
            success,
            stdout: if argv[0] == "/sbin/mount" {
                locked(&self.listing).clone()
            } else {
                String::new()
            },
            stderr: stderr.to_owned(),
            timed_out: false,
        })
    }

    fn force_unmount(&self, path: &std::path::Path, _limit: Duration) -> std::io::Result<()> {
        locked(&self.calls).push(vec!["unmount(2)".to_owned(), path.display().to_string()]);
        let mut rounds = locked(&self.failing_rounds);
        if *rounds > 0 || !self.syscall_works {
            *rounds = rounds.saturating_sub(1);
            return Err(std::io::Error::other("refused"));
        }
        locked(&self.listing).clear();
        Ok(())
    }

    fn pause(&self, how_long: Duration) {
        locked(&self.pauses).push(how_long);
    }
}

pub(super) fn ask(path: &std::path::Path, request: &str) -> serde_json::Value {
    let stream = UnixStream::connect(path).unwrap();
    (&stream)
        .write_all(format!("{request}\n").as_bytes())
        .unwrap();
    let mut line = String::new();
    let _ = BufReader::new(&stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Yields to the runtime until nothing listens at `address`: the aborted server drops its socket
/// when the runtime next runs it.
pub(super) async fn closed(address: std::net::SocketAddr) {
    while std::net::TcpStream::connect(address).is_ok() {
        tokio::task::yield_now().await;
    }
}

pub(super) fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Asserts a start: the control socket takes an attach, the socket named in the mount command is
/// served (a client of the protocol reads the tree through it), the mount command is the documented
/// one for that port as the user of the process, the core counts no lookups, and a stop unmounts
/// and closes the socket.
#[tokio::test]
async fn a_start_serves_mounts_and_a_stop_unmounts() {
    let (rig, root) = attached("nfs-run-start");
    let settings = settings(&rig, loopback());
    let runner = Recorder::new(true);
    let running = Running::start(
        (&settings, &rig.calls()),
        runner.clone(),
        euid(),
        None,
        EPOCH,
    )
    .await
    .unwrap();
    let port = runner.mounted_port().unwrap();
    assert_ne!(port, 0);
    assert_eq!(
        runner.calls()[1][2],
        format!(
            "vers=3,tcp,ro,rdirplus,locallocks,actimeo=3600,hard,deadtimeout=90,port={port},\
             mountport={port}"
        )
    );

    let answer = ask(
        &settings.control_socket,
        &format!("attach {root} for run-2"),
    );
    assert_eq!(answer["ok"], serde_json::json!(true));
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut client = Client::connect(address).await;
    let handle = client.mount("/").await;
    assert_eq!(handle, handle_of(ino::ROOT));
    // The user of the process, and any other user, is served.
    let reply = client
        .call(
            NFS,
            GETATTR,
            Cred::Sys(euid(), 0),
            Enc::default().opaque(&handle),
        )
        .await;
    assert_eq!(Dec::new(&reply.body).u32(), 0);
    let other = client
        .call(
            NFS,
            GETATTR,
            Cred::Sys(euid() + 1, 0),
            Enc::default().opaque(&handle),
        )
        .await;
    assert_eq!(Dec::new(&other.body).u32(), 0);

    running.leave(Leaving::Signal).await.unwrap();
    let calls = runner.calls();
    assert_eq!(
        calls.last().unwrap(),
        &[
            "/sbin/umount".to_owned(),
            settings.mountpoint.display().to_string()
        ]
    );
    closed(address).await;
}

/// Asserts what a stop does with a mount `umount` refuses (a job still has a file of it open): it
/// unmounts it by force, `diskutil unmount force` on the mount point, and the mount is gone; and
/// that a stop whose every step fails (`umount`, `diskutil`, the `unmount(2)`) does not end: it
/// pauses `LEAVE_RETRY` and unmounts again, round after round, while the mount stays listed, and
/// returns once a round unmounts. Catches a stop that leaves a mount without its server.
#[tokio::test]
async fn a_stop_unmounts_a_busy_mount_by_force() {
    let (rig, _) = attached("nfs-run-stop-forced");
    let settings = settings(&rig, loopback());
    let busy = Recorder::busy_at(&settings.mountpoint, true);
    let running = Running::start((&settings, &rig.calls()), busy.clone(), euid(), None, EPOCH)
        .await
        .unwrap();
    running.leave(Leaving::Signal).await.unwrap();
    let point = settings.mountpoint.display().to_string();
    let calls = busy.calls();
    assert_eq!(
        calls[calls.len() - 2..],
        [
            vec!["/sbin/umount".to_owned(), point.clone()],
            ["/usr/sbin/diskutil", "unmount", "force", &point]
                .map(str::to_owned)
                .to_vec(),
        ]
    );
    assert!(!busy.has_mount());

    let stuck = Recorder::stuck_for(&settings.mountpoint, 2);
    let running = Running::start(
        (&settings, &rig.calls()),
        stuck.clone(),
        euid(),
        None,
        EPOCH,
    )
    .await
    .unwrap();
    let before = stuck.calls().len();
    running.leave(Leaving::Signal).await.unwrap();
    let rounds: Vec<String> = stuck.programs()[before..].to_vec();
    let round = [
        "/sbin/mount",
        "/sbin/umount",
        "/usr/sbin/diskutil",
        "unmount(2)",
    ];
    assert_eq!(
        rounds,
        [&round[..], &round[..], &round[..3]]
            .concat()
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(stuck.pauses(), [super::super::mount::LEAVE_RETRY; 2]);
    assert!(!stuck.has_mount());
}

/// Asserts that a stop unmounts while the server still answers the clients, and stops the server
/// after: at the moment `umount` runs, a client's call is answered. A server stopped first ends
/// every connection, so the unmount meets a server that is gone (macOS then refuses `umount` even of
/// a mount no one uses, and the forced unmount waits for the server first). Catches a stop that
/// ends the server before it unmounts.
#[tokio::test]
async fn a_stop_unmounts_while_the_server_still_answers() {
    let (rig, _) = attached("nfs-run-stop-order");
    let settings = settings(&rig, loopback());
    let runner = Recorder::new(true);
    let running = Running::start(
        (&settings, &rig.calls()),
        runner.clone(),
        euid(),
        None,
        EPOCH,
    )
    .await
    .unwrap();
    let connection = std::net::TcpStream::connect(running.address).unwrap();
    runner.probe_at_umount(connection);
    running.leave(Leaving::Signal).await.unwrap();
    assert_eq!(runner.checked_at_umount(), Some(true));
}

/// Asserts that a stop finds the mount already gone (unmounted by hand while the process served)
/// and succeeds without an unmount, where `umount` would say `not currently mounted` and the
/// process would end with status 1.
#[tokio::test]
async fn a_stop_after_the_mount_is_gone_succeeds_without_an_unmount() {
    let (rig, _) = attached("nfs-run-stop-gone");
    let settings = settings(&rig, loopback());
    let runner = Recorder::new(true);
    let running = Running::start(
        (&settings, &rig.calls()),
        runner.clone(),
        euid(),
        None,
        EPOCH,
    )
    .await
    .unwrap();
    assert!(runner.has_mount());
    runner.unmounted_by_hand();
    running.leave(Leaving::Signal).await.unwrap();
    assert_eq!(
        runner.programs(),
        ["/sbin/mount", "/sbin/mount_nfs", "/sbin/mount"]
    );
}

/// The `nfs` params of an `fs` interface that serves only (`mount = false`) on any free loopback
/// port.
const NFS_TABLE: &str = "[nfs]\nlisten = \"127.0.0.1:0\"\nmount = false\n";

/// Asserts that this release starts on the `nfs` params of an `fs` interface, read by the loader
/// of the interface's params, and serves every user over the wire: a user the params do not name
/// (the one a release up to 0.3.166 listed in `allowed_uids`), the user of the process (the mount
/// user, which that release served by default), and another; and that `nfs.mount = false` serves
/// without running any command, then stops without one. Catches a release that refuses a user it
/// was not told of.
#[tokio::test]
async fn a_start_on_params_that_list_no_users_serves_every_user() {
    let (rig, _) = attached("nfs-run-nomount");
    let (listed, other) = (9_999, 7_777);
    let default_user = euid();
    assert!(![listed, other].contains(&default_user));
    let text = format!("client_group = \"77\"\nowner = \"1000:1001\"\n{NFS_TABLE}");
    let loaded = from_params(&text, &builders).unwrap();
    let settings = settings(&rig, loaded.nfs);
    let runner = Recorder::new(true);
    let running = Running::start(
        (&settings, &rig.calls()),
        runner.clone(),
        default_user,
        None,
        EPOCH,
    )
    .await
    .unwrap();
    assert!(runner.calls().is_empty());

    let mut client = Client::connect(running.address).await;
    let handle = client.mount("/").await;
    // Each case is named by its place: a user of the old list, the user of the process, another.
    for (case, uid) in [listed, default_user, other].into_iter().enumerate() {
        let reply = client
            .call(
                NFS,
                GETATTR,
                Cred::Sys(uid, 0),
                Enc::default().opaque(&handle),
            )
            .await;
        let mut body = Dec::new(&reply.body);
        assert_eq!(body.u32(), 0, "case {case} is served");
        assert_eq!(body.attr().fileid, ino::ROOT, "case {case}");
    }
    drop(client);

    running.leave(Leaving::Signal).await.unwrap();
    assert!(runner.calls().is_empty());
}

/// Asserts that a mount that fails every try fails the start with what the command said, and leaves
/// no server behind; and that an address that is taken is an error naming it.
#[tokio::test]
async fn a_start_whose_mount_fails_or_whose_address_is_taken_is_refused() {
    let (rig, _) = attached("nfs-run-refused");
    let settings = settings(&rig, loopback());
    let runner = Recorder::new(false);
    let why = Running::start(
        (&settings, &rig.calls()),
        runner.clone(),
        euid(),
        None,
        EPOCH,
    )
    .await
    .err()
    .unwrap();
    assert!(
        why.to_string()
            .contains("did not succeed in 3 tries: refused"),
        "{why}"
    );
    assert!(matches!(why, NfsError::Mount { .. }), "{why}");
    let port = runner.mounted_port().unwrap();
    closed(std::net::SocketAddr::from(([127, 0, 0, 1], port))).await;

    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = super::super::config::NfsSettings {
        listen: held.local_addr().unwrap(),
        ..loopback()
    };
    let why = Running::start(
        (&self::settings(&rig, taken), &rig.calls()),
        Recorder::new(true),
        euid(),
        None,
        EPOCH,
    )
    .await
    .err()
    .unwrap();
    assert!(
        why.to_string()
            .starts_with(&format!("fs.nfs.listen {}: ", held.local_addr().unwrap())),
        "{why}"
    );
}

/// Asserts that a user who is not the process's and not root's is refused before anything is
/// made: the runtime directory is not even created.
#[tokio::test]
async fn a_start_as_the_wrong_user_makes_nothing() {
    let (rig, _) = attached("nfs-run-wrong");
    let nfs = super::super::config::NfsSettings {
        mount_user: Some("builder".to_owned()),
        ..loopback()
    };
    struct Ids;
    impl Runner for Ids {
        fn run_within(&self, _argv: &[String], _limit: Duration) -> std::io::Result<Ran> {
            Ok(Ran {
                success: false,
                stdout: String::new(),
                stderr: "no such user".to_owned(),
                timed_out: false,
            })
        }
        fn force_unmount(&self, _path: &std::path::Path, _limit: Duration) -> std::io::Result<()> {
            unreachable!("a start that finds no user unmounts nothing")
        }
        fn pause(&self, _how_long: Duration) {}
    }
    let mut settings = settings(&rig, nfs);
    settings.runtime_dir = rig.dir.path().join("never-made");
    let why = Running::start(
        (&settings, &rig.calls()),
        Arc::new(Ids),
        euid(),
        None,
        EPOCH,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(why.to_string(), "fs.nfs.mount_user builder: no such user");
    assert!(!settings.runtime_dir.exists());
}

/// Asserts that a start refused by what the neutral start does is the neutral start's error
/// (`NfsError::Start`), which names the key: the control socket's path cannot be bound.
#[tokio::test]
async fn a_start_the_neutral_start_refuses_ends_with_its_status() {
    let (rig, _) = attached("nfs-run-neutral");
    let mut settings = settings(&rig, loopback());
    settings.control_socket = rig.dir.path().join("no-such-dir").join("control.sock");
    let why = Running::start(
        (&settings, &rig.calls()),
        Recorder::new(true),
        euid(),
        None,
        EPOCH,
    )
    .await
    .err()
    .unwrap();
    assert!(why.to_string().contains("fs.control_socket"), "{why}");
    assert!(matches!(why, NfsError::Start(_)), "{why}");
}

/// Asserts the process end to end: it starts, mounts, and a SIGTERM unmounts it and ends it with
/// success; and that a start that fails ends `serve_with` with the error.
#[test]
fn serve_runs_until_sigterm_and_unmounts() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (rig, _) = attached("nfs-run-serve");
    let settings = settings(&rig, loopback());
    let runner = Recorder::new(true);
    let (mine, theirs, calls) = (settings.clone(), runner.clone(), rig.calls());
    let thread = std::thread::spawn(move || {
        serve_with((&mine, &calls), theirs, euid(), &BootSource::fixed())
    });
    // The mount comes after the handlers were put in place.
    until(|| runner.mounted_port().is_some());
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
        .unwrap();
    thread.join().unwrap().unwrap();
    assert_eq!(runner.calls().last().unwrap()[0], "/sbin/umount");

    let bad = FsSettings {
        cache_path: rig.dir.path().join("none"),
        control_socket: rig.dir.path().join("none").join("control.sock"),
        ..settings
    };
    assert!(
        serve_with(
            (&bad, &rig.calls()),
            Recorder::new(true),
            euid(),
            &BootSource::fixed()
        )
        .is_err()
    );
}

/// Asserts that the timer a start gives the file system is a future that is ready after the time
/// given, and not before; the clock is the runtime's, paused, so no time passes.
#[tokio::test(start_paused = true)]
async fn the_patience_of_a_start_is_a_timer_of_fs_nfs_jukebox_after() {
    let patience = timer(Duration::from_secs(5));
    let mut waiting = Box::pin(patience());
    assert!(futures::poll!(&mut waiting).is_pending());
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(futures::poll!(&mut waiting).is_ready());
}

/// Asserts that a mount command that dies is a failed start with the reason, and an unmount
/// command that dies is a failed stop with the reason, and not a panic of the process.
#[tokio::test]
async fn a_command_that_dies_fails_the_start_or_the_stop() {
    let (rig, _) = attached("nfs-run-dies");
    let settings = settings(&rig, loopback());
    let why = Running::start(
        (&settings, &rig.calls()),
        Recorder::panicking("/sbin/mount_nfs"),
        euid(),
        None,
        EPOCH,
    )
    .await
    .err()
    .unwrap();
    assert!(
        why.to_string().starts_with("waiting for the mount: "),
        "{why}"
    );

    let (rig, _) = attached("nfs-run-dies-umount");
    let settings = self::settings(&rig, loopback());
    let running = Running::start(
        (&settings, &rig.calls()),
        Recorder::panicking("/sbin/umount"),
        euid(),
        None,
        EPOCH,
    )
    .await
    .unwrap();
    let why = running.leave(Leaving::Signal).await.unwrap_err();
    assert!(
        why.to_string().starts_with("waiting for the unmount: "),
        "{why}"
    );
}

/// What a server whose accept failed gives.
async fn accept_fails() -> std::io::Result<()> {
    Err(std::io::Error::other("accept failed"))
}

/// What a server that panics gives.
async fn dies() -> std::io::Result<()> {
    panic!("the server dies")
}

/// A server that fails at once, as one whose accept failed.
fn failing<'a>(
    _listener: &'a NFSTcpListener<TreeNfs>,
    _ending: BoxFuture<'static, ()>,
) -> BoxFuture<'a, std::io::Result<()>> {
    Box::pin(accept_fails())
}

/// A server that panics at once.
fn panicking<'a>(
    _listener: &'a NFSTcpListener<TreeNfs>,
    _ending: BoxFuture<'static, ()>,
) -> BoxFuture<'a, std::io::Result<()>> {
    Box::pin(dies())
}

/// Makes `runner`'s `umount` note whether the listening socket of the mount it made still takes
/// a connection.
fn listening_at_umount(runner: &Arc<Recorder>) {
    let me = Arc::clone(runner);
    runner.check_at_umount(move || {
        let port = me.mounted_port().expect("the mount came first");
        std::net::TcpStream::connect(std::net::SocketAddr::from(([127, 0, 0, 1], port))).is_ok()
    });
}

/// Asserts that a server that stops by itself (it failed, or it panicked) ends the serving with
/// that reason, and that the leave unmounts while the listening socket still takes connections and
/// closes the socket only after: the unmount comes before the socket goes. Catches a process whose
/// server died and that leaves its mount without a server, or unmounts after its socket closed.
#[tokio::test]
async fn a_server_that_stops_by_itself_is_unmounted_before_its_socket_closes() {
    let cases = [
        (failing as Serve, "accept failed"),
        (panicking as Serve, "it panicked"),
    ];
    for (serve, said) in cases {
        let (rig, _) = attached("nfs-run-server-stops");
        let settings = settings(&rig, loopback());
        let runner = Recorder::new(true);
        let mut running = Running::start_serving(
            (&settings, &rig.calls()),
            runner.clone(),
            euid(),
            None,
            EPOCH,
            serve,
        )
        .await
        .unwrap();
        let address = running.address;
        listening_at_umount(&runner);
        let leaving = running.ended().await;
        assert_eq!(leaving, Leaving::ServerStopped(said.to_owned()), "{said}");
        running.leave(leaving).await.unwrap();
        assert_eq!(runner.checked_at_umount(), Some(true), "{said}");
        assert!(!runner.has_mount(), "{said}");
        closed(address).await;
    }
}

/// Asserts that a start that fails after it made the mount (the note of the mount's address cannot
/// be written: a directory is where the file goes) unmounts while its socket still takes
/// connections, and then fails with why it failed. Catches a failed start that leaves its mount
/// without a server.
#[tokio::test]
async fn a_start_that_fails_after_its_mount_unmounts_before_its_socket_closes() {
    let (rig, _) = attached("nfs-run-start-fails-late");
    let settings = settings(&rig, loopback());
    std::fs::create_dir_all(settings.runtime_dir.join("nfs-mounted")).unwrap();
    let runner = Recorder::new(true);
    listening_at_umount(&runner);
    let why = Running::start(
        (&settings, &rig.calls()),
        runner.clone(),
        euid(),
        None,
        EPOCH,
    )
    .await
    .err()
    .unwrap();
    assert!(
        why.to_string()
            .starts_with("writing the address of the mount:"),
        "{why}"
    );
    assert_eq!(runner.checked_at_umount(), Some(true));
    assert!(!runner.has_mount());
    let port = runner.mounted_port().unwrap();
    closed(std::net::SocketAddr::from(([127, 0, 0, 1], port))).await;
}

/// Asserts what each reason of a leave says in the log line of the leave.
#[test]
fn each_reason_of_a_leave_is_said() {
    let said: Vec<String> = [
        Leaving::Signal,
        Leaving::ServerStopped("accept failed".to_owned()),
        Leaving::StartFailed("no note".to_owned()),
        Leaving::Ended,
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(
        said,
        [
            "a signal to stop",
            "the NFS server stopped (accept failed)",
            "the start failed after the mount (no note)",
            "the listening socket was handed over",
        ]
    );
}
