//! The io_uring transport against the kernel (5.2 item 4, row T-F5): a mount INIT'ed by
//! [`Session::init_uring`] and served by [`Session::serve`].
//!
//! What the session must do depends on the node's `fuse.enable_uring`, set at boot (5.2), and each
//! check says which case it ran:
//! - **Off** (every build node today, K10, and CI's runners): the session says
//!   [`Fallback::Disabled`], does not ask for `FUSE_OVER_IO_URING`, and serves on `/dev/fuse`: a
//!   `stat` and a `pread` reach the file system and return its bytes, read by the `/dev/fuse`
//!   reader.
//! - **On:** the session serves on io_uring, registering `depth` entries on every possible CPU's
//!   queue ([`UringMemory`] of [`POSSIBLE_CPUS`] queues), and the same `stat` and `pread` are
//!   read by the queues' threads and answered by COMMIT_AND_FETCH.
//!
//! In both cases a READ parked by the file system and completed from another thread reaches its
//! caller (on io_uring it holds its ring entry until then, `uring_entries_parked`), and a parked
//! READ whose caller is killed is answered `EINTR` once, through its own transport: on io_uring
//! the INTERRUPT arrives on `/dev/fuse` and the answer is a commit of the READ's ring entry (K11).
//! The 64 MiB rule is checked against the kernel in both cases: a tree mount with `max_pages` 256
//! (1 MiB payloads) never asks for io_uring.
//!
//! The tree is the one of the T-F0 checks (`kernel/mod.rs`): the root and the files `f` and `g`,
//! opened with `FOPEN_DIRECT_IO` so that each `pread` is one READ with a waiting caller. Every
//! wait is on an event (a READ parked, a caller's system call returning, a child ending, a reader
//! ending), never on a timer. An integration target: it mounts through `mount(2)` as root (the
//! farm) or `fusermount3` (CI), and says on stderr why it does not run where neither can.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod kernel;

use std::fs::File;
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};

use kernel::{
    FOPEN_DIRECT_IO, ROOT, VALID, attr, content, ino_of, one_at_a_time, skip, unavailable,
};
use tierstore_testkit::dir::TestDir;
use tsfuse::abi::{AttrOut, EntryOut, InitConfig, InitFlags, OpenOut, Operation};
use tsfuse::mount::{Mount, MountOptions, Mounter};
use tsfuse::{
    Completion, ENABLE_URING, Errno, Fallback, Fs, Handled, POSSIBLE_CPUS, ReaderEnd, Replier,
    Request, Serving, Session, TransportMode, UringConfig, UringMemory,
};

/// A READ the file system parked.
struct Parked {
    ino: u64,
    offset: u64,
    size: u32,
    replier: Replier,
}

/// What the file system did, watched by the test: the READs parked, and the name of the reader
/// thread that read each READ.
#[derive(Default)]
struct Events {
    state: Mutex<(Vec<Parked>, Vec<String>)>,
    changed: Condvar,
}

impl Events {
    /// Waits until a READ is parked, and takes it.
    fn parked(&self) -> Parked {
        let mut s = self
            .changed
            .wait_while(self.state.lock().unwrap(), |s| s.0.is_empty())
            .unwrap();
        s.0.remove(0)
    }

    /// The reader threads that read each READ, in order.
    fn readers(&self) -> Vec<String> {
        self.state.lock().unwrap().1.clone()
    }
}

/// The tree of `kernel/mod.rs`, every request answered inline, except READs, which are parked
/// when `park` is set.
struct Tree {
    park: bool,
    events: Arc<Events>,
}

impl Tree {
    fn new(park: bool) -> (Arc<Self>, Arc<Events>) {
        let events = Arc::new(Events::default());
        let tree = Self {
            park,
            events: events.clone(),
        };
        (Arc::new(tree), events)
    }
}

impl Fs for Tree {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        let node = req.header.nodeid;
        let _ = match Operation::parse(&req) {
            Ok(Operation::Lookup { name }) => {
                let ino = std::str::from_utf8(name).ok().and_then(ino_of);
                match (node, ino) {
                    (ROOT, Some(ino)) => {
                        let entry = EntryOut::new(ino, 1, VALID, VALID, attr(ino));
                        replier.ok(&[entry.as_bytes()])
                    }
                    _ => replier.error(Errno(2)),
                }
            }
            Ok(Operation::Getattr(_)) => replier.ok(&[AttrOut::new(VALID, attr(node)).as_bytes()]),
            Ok(Operation::Open(_)) => {
                let out = OpenOut {
                    fh: node,
                    open_flags: FOPEN_DIRECT_IO,
                    backing_id: 0,
                };
                replier.ok(&[out.as_bytes()])
            }
            Ok(Operation::Read(r)) => {
                let name = std::thread::current().name().unwrap_or("").to_owned();
                let mut s = self.events.state.lock().unwrap();
                s.1.push(name);
                if self.park {
                    s.0.push(Parked {
                        ino: node,
                        offset: r.offset,
                        size: r.size,
                        replier,
                    });
                    self.events.changed.notify_all();
                    return Handled::Parked;
                }
                replier.ok(&[&content(node, r.offset, u64::from(r.size))])
            }
            Ok(Operation::Flush(_) | Operation::Release(_)) => replier.ok(&[]),
            _ => replier.error(Errno::ENOSYS),
        };
        Handled::Done
    }
}

/// Whether this node runs FUSE over io_uring (`fuse.enable_uring`).
fn uring_on() -> bool {
    std::fs::read_to_string(ENABLE_URING).is_ok_and(|t| matches!(t.trim(), "Y" | "1"))
}

/// The number of possible CPUs, as the kernel lists them.
fn possible_cpus() -> u32 {
    let text = std::fs::read_to_string(POSSIBLE_CPUS).unwrap();
    text.trim()
        .split(',')
        .map(|r| match r.split_once('-') {
            Some((lo, hi)) => hi.parse::<u32>().unwrap() - lo.parse::<u32>().unwrap() + 1,
            None => 1,
        })
        .sum()
}

/// The transport this node allows a tree mount of 32 pages, and the prefix of the names of the
/// threads that read its requests.
fn expected() -> (TransportMode, &'static str) {
    match uring_on() {
        true => (
            TransportMode::Uring(UringMemory::of(
                possible_cpus(),
                UringConfig::DEPTH.get(),
                128 << 10,
            )),
            "tsfuse-uring-",
        ),
        false => (TransportMode::DevFuse(Fallback::Disabled), "tsfuse-reader-"),
    }
}

/// A mount of the tree; dropping it aborts the connection and unmounts, so a failed check never
/// leaves a caller waiting.
struct Mounted {
    mount: Option<Mount>,
    mp: PathBuf,
    _dir: TestDir,
}

impl Mounted {
    /// Mounts in a scratch directory named after `name` and runs INIT asking for io_uring with
    /// `config`. `None` (said on stderr) where no FUSE mount can be made.
    fn new(name: &str, config: &InitConfig) -> Option<(Self, Session)> {
        if let Some(why) = unavailable() {
            skip(why);
            return None;
        }
        // A queue costs three descriptors (its ring, its runtime's epoll and wake descriptors),
        // so a mount on io_uring takes about 3 x 88 on a build node, and the checks here run in
        // parallel: the soft limit is raised to the hard one, as a server's unit does.
        let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        let _ = rustix::process::setrlimit(
            rustix::process::Resource::Nofile,
            rustix::process::Rlimit {
                current: limit.maximum,
                maximum: limit.maximum,
            },
        );
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let mp = dir.path().join("mnt");
        std::fs::create_dir(&mp).unwrap();
        let options = MountOptions {
            allow_other: false,
            ..MountOptions::tree("tsfuse-uring")
        };
        let (device, mount) = one_at_a_time(|| Mounter::default().mount(&mp, &options)).unwrap();
        let session = Session::init_uring(device, config, &UringConfig::default()).unwrap();
        let mounted = Self {
            mount: Some(mount),
            mp,
            _dir: dir,
        };
        Some((mounted, session))
    }

    /// Unmounts and checks that every reader ended with the mount: the `/dev/fuse` reader and,
    /// on io_uring, one per queue.
    fn unmount_and_join(mut self, serving: Serving) {
        let on = matches!(serving.transport(), TransportMode::Uring(_));
        let mount = self.mount.clone().unwrap();
        one_at_a_time(|| mount.unmount(false)).unwrap();
        self.mount = None;
        let ends = serving.join().unwrap();
        let readers = match on {
            true => 1 + possible_cpus() as usize,
            false => 1,
        };
        assert_eq!(
            ends.len(),
            readers,
            "the /dev/fuse reader and one per queue"
        );
        for end in ends {
            assert!(
                matches!(end, ReaderEnd::Unmounted | ReaderEnd::Destroyed),
                "{end:?}"
            );
        }
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(mount) = self.mount.take() {
            let _ = mount.abort();
            let _ = one_at_a_time(|| mount.unmount(true));
        }
    }
}

/// The tree mount's terms on io_uring: 32 pages (128 KiB payloads, K21).
fn tree32() -> InitConfig {
    InitConfig {
        max_pages: 32,
        ..InitConfig::tree()
    }
}

#[test]
fn a_session_asked_for_io_uring_serves_on_the_transport_the_kernel_allows() {
    let Some((mounted, session)) = Mounted::new("uring-serve", &tree32()) else {
        return;
    };
    let on = uring_on();
    let granted = session
        .negotiated()
        .granted
        .contains(InitFlags::OVER_IO_URING);
    let (want, thread) = expected();
    let _ = writeln!(
        std::io::stderr(),
        "fuse.enable_uring is {on}: expecting {want:?}"
    );
    assert_eq!(session.transport(), want);
    assert_eq!(granted, on, "FUSE_OVER_IO_URING asked for only when used");
    let (tree, events) = Tree::new(false);
    let serving = session.serve(tree).unwrap();
    assert_eq!(serving.transport(), want, "every REGISTER taken");

    let meta = std::fs::metadata(mounted.mp.join("f")).unwrap();
    assert_eq!(meta.len(), kernel::FILE_SIZE);
    let file = File::open(mounted.mp.join("g")).unwrap();
    let mut buf = vec![0u8; 100_000];
    let n = file.read_at(&mut buf, 5_000).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(buf, content(3, 5_000, 100_000));
    drop(file);
    let readers = events.readers();
    assert!(!readers.is_empty());
    assert!(
        readers.iter().all(|r| r.starts_with(thread)),
        "every READ read by a {thread}* thread: {readers:?}"
    );
    mounted.unmount_and_join(serving);
}

#[test]
fn a_parked_read_completed_from_another_thread_reaches_its_caller() {
    let Some((mounted, session)) = Mounted::new("uring-parked", &tree32()) else {
        return;
    };
    let (want, _) = expected();
    let (tree, events) = Tree::new(true);
    let serving = session.serve(tree).unwrap();
    assert_eq!(serving.transport(), want);
    let path = mounted.mp.join("f");
    let caller = std::thread::spawn(move || {
        let mut buf = vec![0u8; 8192];
        let n = File::open(path).unwrap().read_at(&mut buf, 4096).unwrap();
        buf.truncate(n);
        buf
    });
    let parked = events.parked();
    // Another request may hold an entry for a moment (a RELEASE the kernel sends in the
    // background, a `stat` of the new mount by another process): at least the READ holds one.
    let held = serving.uring_entries_held();
    match want {
        TransportMode::Uring(_) => assert!(held >= 1, "the READ holds its entry"),
        TransportMode::DevFuse(_) => assert_eq!(held, 0),
    }
    let data = content(parked.ino, parked.offset, u64::from(parked.size));
    let done = std::thread::spawn(move || parked.replier.ok(&[&data]))
        .join()
        .unwrap();
    assert!(matches!(done, Completion::Written), "{done:?}");
    assert_eq!(caller.join().unwrap(), content(2, 4096, 8192));
    mounted.unmount_and_join(serving);
}

/// The name of [`child_reads_the_file_named_on_stdin`], as the child's command line gives it.
const CHILD: &str = "child_reads_the_file_named_on_stdin";

/// Not a check of its own: the child process of
/// [`a_parked_read_whose_caller_is_killed_is_answered_eintr_once_on_its_own_transport`]. It reads
/// the path of a file of the mount from its standard input, opens the file and reads 4 KiB at
/// offset 0 with one `pread`, a direct READ whose caller waits for the answer; the parent kills it
/// while the READ is parked.
#[test]
#[ignore = "the child process of a_parked_read_whose_caller_is_killed_is_answered_eintr_once_on_its_own_transport, which runs it with --ignored"]
fn child_reads_the_file_named_on_stdin() {
    let mut path = String::new();
    std::io::stdin().read_line(&mut path).unwrap();
    let file = File::open(path.trim_end()).unwrap();
    let mut buf = vec![0u8; 4096];
    let _ = file.read_at(&mut buf, 0);
}

/// A child process that reads `path` once ([`child_reads_the_file_named_on_stdin`]).
fn child_reading(path: &Path) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD, "--ignored", "--test-threads=1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = one_at_a_time(|| command.spawn()).unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", path.display()).unwrap();
    child
}

#[test]
fn a_parked_read_whose_caller_is_killed_is_answered_eintr_once_on_its_own_transport() {
    let Some((mounted, session)) = Mounted::new("uring-interrupt", &tree32()) else {
        return;
    };
    let (want, _) = expected();
    let (tree, events) = Tree::new(true);
    let serving = session.serve(tree).unwrap();
    assert_eq!(serving.transport(), want);
    let mut child = child_reading(&mounted.mp.join("f"));
    let parked = events.parked();
    let token = parked.replier.cancel_token();
    child.kill().unwrap();
    // The child ends only once its READ is answered (K8): EINTR, written by the INTERRUPT.
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9), "{status:?}");
    assert!(token.is_cancelled());
    let late = parked.replier.ok(&[b"late"]);
    assert!(matches!(late, Completion::AlreadyAnswered), "{late:?}");
    mounted.unmount_and_join(serving);
}

#[test]
fn a_tree_mount_of_1_mib_requests_never_asks_for_io_uring() {
    let Some((mounted, session)) = Mounted::new("uring-memory", &InitConfig::tree()) else {
        return;
    };
    let want = match uring_on() {
        // 1 MiB payloads: 88 MiB at depth 1 on 88 CPUs, past 64 MiB at depth 5 anywhere.
        true => TransportMode::DevFuse(Fallback::OverMemoryLimit {
            memory: UringMemory::of(possible_cpus(), UringConfig::DEPTH.get(), 1 << 20),
            limit: UringConfig::MEMORY_LIMIT,
        }),
        false => TransportMode::DevFuse(Fallback::Disabled),
    };
    assert_eq!(session.transport(), want);
    assert!(
        !session
            .negotiated()
            .granted
            .contains(InitFlags::OVER_IO_URING)
    );
    let (tree, _) = Tree::new(false);
    let serving = session.serve(tree).unwrap();
    assert_eq!(serving.transport(), want);
    let meta = std::fs::metadata(mounted.mp.join("f")).unwrap();
    assert_eq!(meta.len(), kernel::FILE_SIZE);
    mounted.unmount_and_join(serving);
}
