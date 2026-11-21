//! The process: a start over a fake mount command and a real loopback socket, a stop, the checks a
//! start makes, and the signals.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::boot::BootSource;
use super::super::fixtures::{EPOCH, attached, handle_of, loopback, settings};
use super::super::mount::{Ran, Runner};
use super::super::wire_client::{Client, Cred, Dec, Enc, GETATTR, NFS};
use super::{Running, serve, serve_with, timer};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::ino;
use crate::server::treefs::locked;
use crate::server::treefs::testing::SIGNALS;
use crate::server::treefs::testing::until;

/// Records the commands, answers every one with success unless told to fail the mount, `umount` or
/// the forced unmount, and keeps a listing of the mounts that a mount adds its line to and an
/// unmount empties.
pub(super) struct Recorder {
    calls: Mutex<Vec<Vec<String>>>,
    mount_works: bool,
    /// Whether `umount` unmounts; if not it says `Resource busy`.
    umount_works: bool,
    /// Whether `diskutil unmount force` unmounts.
    force_works: bool,
    /// The program whose run panics.
    panics: Option<&'static str>,
    /// What the listing of the mounts says.
    listing: Mutex<String>,
    /// A connection to the server, on which `umount` makes a call of NFS's NULL procedure, and
    /// whether the server answered it.
    probe: Mutex<Option<std::net::TcpStream>>,
    answered_at_umount: Mutex<Option<bool>>,
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
            panics: None,
            listing: Mutex::new(listing),
            probe: Mutex::new(None),
            answered_at_umount: Mutex::new(None),
        })
    }

    /// A runner whose listing of the mounts has an NFS mount at `path`.
    pub(super) fn mounted_at(path: &std::path::Path) -> Arc<Self> {
        Self::made(true, line_of(path))
    }

    /// A runner with an NFS mount at `path` that `umount` finds busy, and that the forced unmount
    /// unmounts if `forced`.
    pub(super) fn busy_at(path: &std::path::Path, forced: bool) -> Arc<Self> {
        Arc::new(Self {
            umount_works: false,
            force_works: forced,
            ..Arc::into_inner(Self::mounted_at(path)).unwrap()
        })
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

    /// Makes `umount` call NFS's NULL procedure on `connection` first, and note whether the
    /// server answered.
    pub(super) fn probe_at_umount(&self, connection: std::net::TcpStream) {
        *locked(&self.probe) = Some(connection);
    }

    /// Whether the server answered the call `umount` made: none if it made none.
    pub(super) fn answered_at_umount(&self) -> Option<bool> {
        *locked(&self.answered_at_umount)
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
    fn run(&self, argv: &[String]) -> std::io::Result<Ran> {
        locked(&self.calls).push(argv.to_vec());
        assert_ne!(self.panics, Some(argv[0].as_str()), "the command dies");
        if argv[0] == "/sbin/umount"
            && let Some(mut connection) = locked(&self.probe).take()
        {
            *locked(&self.answered_at_umount) = Some(null_answered(&mut connection));
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
            "/usr/sbin/diskutil" if self.force_works => {
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
        })
    }

    fn pause(&self, _how_long: Duration) {}
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
    let running = Running::start(&settings, runner.clone(), euid(), None, EPOCH)
        .await
        .unwrap();
    let port = runner.mounted_port().unwrap();
    assert_ne!(port, 0);
    assert_eq!(
        runner.calls()[1][2],
        format!("vers=3,tcp,ro,rdirplus,locallocks,actimeo=3600,hard,port={port},mountport={port}")
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
    // The owner of the entries is the user of the process, so that user is served by default.
    let reply = client
        .call(
            NFS,
            GETATTR,
            Cred::Sys(euid(), 0),
            Enc::default().opaque(&handle),
        )
        .await;
    assert_eq!(Dec::new(&reply.body).u32(), 0);
    let denied = client
        .call(
            NFS,
            GETATTR,
            Cred::Sys(euid() + 1, 0),
            Enc::default().opaque(&handle),
        )
        .await;
    assert_eq!(Dec::new(&denied.body).u32(), 13);

    running.stop().await.unwrap();
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
/// that a stop whose forced unmount fails too is an error with what both said, the mount left
/// listed. Catches a stop that leaves a hard mount to a server that is about to exit.
#[tokio::test]
async fn a_stop_unmounts_a_busy_mount_by_force() {
    let (rig, _) = attached("nfs-run-stop-forced");
    let settings = settings(&rig, loopback());
    let busy = Recorder::busy_at(&settings.mountpoint, true);
    let running = Running::start(&settings, busy.clone(), euid(), None, EPOCH)
        .await
        .unwrap();
    running.stop().await.unwrap();
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

    let stuck = Recorder::busy_at(&settings.mountpoint, false);
    let running = Running::start(&settings, stuck.clone(), euid(), None, EPOCH)
        .await
        .unwrap();
    let why = running.stop().await.unwrap_err();
    assert_eq!(
        why.to_string(),
        format!(
            "unmounting {point}: umount(mnt): Resource busy -- try 'diskutil unmount'; diskutil \
             unmount force: Unmount failed for mnt"
        )
    );
    assert!(stuck.has_mount());
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
    let running = Running::start(&settings, runner.clone(), euid(), None, EPOCH)
        .await
        .unwrap();
    let connection = std::net::TcpStream::connect(running.address).unwrap();
    runner.probe_at_umount(connection);
    running.stop().await.unwrap();
    assert_eq!(runner.answered_at_umount(), Some(true));
}

/// Asserts that a stop finds the mount already gone (unmounted by hand while the process served)
/// and succeeds without an unmount, where `umount` would say `not currently mounted` and the
/// process would end with status 1.
#[tokio::test]
async fn a_stop_after_the_mount_is_gone_succeeds_without_an_unmount() {
    let (rig, _) = attached("nfs-run-stop-gone");
    let settings = settings(&rig, loopback());
    let runner = Recorder::new(true);
    let running = Running::start(&settings, runner.clone(), euid(), None, EPOCH)
        .await
        .unwrap();
    assert!(runner.has_mount());
    runner.unmounted_by_hand();
    running.stop().await.unwrap();
    assert_eq!(
        runner.programs(),
        ["/sbin/mount", "/sbin/mount_nfs", "/sbin/mount"]
    );
}

/// Asserts that `fs.nfs.allowed_uids` replaces the default list, over the wire: a user in the
/// configured list is served, and a user of the default list (the one of the process, the mount
/// user) that the configured list leaves out is answered `NFS3ERR_ACCES`; and that
/// `fs.nfs.mount = false` serves without running any command, then stops without one.
#[tokio::test]
async fn a_start_without_a_mount_serves_only_and_the_list_of_users_is_taken() {
    let (rig, _) = attached("nfs-run-nomount");
    let listed = 9_999;
    let default_user = euid();
    assert_ne!(listed, default_user);
    let nfs = super::super::config::NfsSettings {
        mount: false,
        allowed_uids: Some(vec![listed]),
        ..loopback()
    };
    let mut settings = settings(&rig, nfs);
    settings.nfs.listen = "127.0.0.1:0".parse().unwrap();
    let runner = Recorder::new(true);
    let running = Running::start(&settings, runner.clone(), default_user, None, EPOCH)
        .await
        .unwrap();
    assert!(runner.calls().is_empty());

    let mut client = Client::connect(running.address).await;
    let handle = client.mount("/").await;
    let status_of = |uid: u32| {
        let handle = handle.clone();
        async move {
            let mut client = Client::connect(running.address).await;
            let reply = client
                .call(
                    NFS,
                    GETATTR,
                    Cred::Sys(uid, 0),
                    Enc::default().opaque(&handle),
                )
                .await;
            Dec::new(&reply.body).u32()
        }
    };
    assert_eq!(status_of(listed).await, 0, "the configured user is served");
    assert_eq!(
        status_of(default_user).await,
        13,
        "the default user, left out of the configured list, is NFS3ERR_ACCES"
    );
    drop(client);

    running.stop().await.unwrap();
    assert!(runner.calls().is_empty());
}

/// Asserts that a mount that fails every try fails the start with what the command said, and leaves
/// no server behind; and that an address that is taken is an error naming it.
#[tokio::test]
async fn a_start_whose_mount_fails_or_whose_address_is_taken_is_refused() {
    let (rig, _) = attached("nfs-run-refused");
    let settings = settings(&rig, loopback());
    let runner = Recorder::new(false);
    let why = Running::start(&settings, runner.clone(), euid(), None, EPOCH)
        .await
        .err()
        .unwrap();
    assert!(
        why.to_string()
            .contains("did not succeed in 3 tries: refused"),
        "{why}"
    );
    assert_eq!(why.status(), 1);
    let port = runner.mounted_port().unwrap();
    closed(std::net::SocketAddr::from(([127, 0, 0, 1], port))).await;

    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = super::super::config::NfsSettings {
        listen: held.local_addr().unwrap(),
        ..loopback()
    };
    let why = Running::start(
        &self::settings(&rig, taken),
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
        fn run(&self, _argv: &[String]) -> std::io::Result<Ran> {
            Ok(Ran {
                success: false,
                stdout: String::new(),
                stderr: "no such user".to_owned(),
            })
        }
        fn pause(&self, _how_long: Duration) {}
    }
    let mut settings = settings(&rig, nfs);
    settings.runtime_dir = rig.dir.path().join("never-made");
    let why = Running::start(&settings, Arc::new(Ids), euid(), None, EPOCH)
        .await
        .err()
        .unwrap();
    assert_eq!(why.to_string(), "fs.nfs.mount_user builder: no such user");
    assert!(!settings.runtime_dir.exists());
}

/// Asserts that a start refused by what the neutral start does has that error's status: the control
/// socket's path cannot be bound.
#[tokio::test]
async fn a_start_the_neutral_start_refuses_ends_with_its_status() {
    let (rig, _) = attached("nfs-run-neutral");
    let mut settings = settings(&rig, loopback());
    settings.control_socket = rig.dir.path().join("no-such-dir").join("control.sock");
    let why = Running::start(&settings, Recorder::new(true), euid(), None, EPOCH)
        .await
        .err()
        .unwrap();
    assert!(why.to_string().contains("fs.control_socket"), "{why}");
    assert_eq!(why.status(), 1);
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
    let (mine, theirs) = (settings.clone(), runner.clone());
    let thread =
        std::thread::spawn(move || serve_with(&mine, theirs, euid(), &BootSource::fixed()));
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
    assert!(serve_with(&bad, Recorder::new(true), euid(), &BootSource::fixed()).is_err());
    // With the commands of this machine, none of which a start that fails this early runs.
    assert!(serve(&bad).is_err());
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
        &settings,
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
        &settings,
        Recorder::panicking("/sbin/umount"),
        euid(),
        None,
        EPOCH,
    )
    .await
    .unwrap();
    let why = running.stop().await.unwrap_err();
    assert!(
        why.to_string().starts_with("waiting for the unmount: "),
        "{why}"
    );
}
