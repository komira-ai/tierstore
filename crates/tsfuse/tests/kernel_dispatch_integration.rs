//! The dispatch of a served session against the kernel (5.2 items 2 and 3, 5.4): a mount served by
//! [`Session::serve`] with four cloned readers and an [`Fs`] that parks its READs.
//!
//! - **Reply affinity with N clones.** Four READs held on four different readers (each reader
//!   waits at a barrier until all four hold one, so no reader reads two) are completed by tasks of
//!   another runtime in the reverse of the order they were read. Every completion is accepted by
//!   the kernel, which takes a reply only on the clone whose processing queue holds the request
//!   (K1), and every caller gets its bytes.
//! - **INTERRUPT answered once.** Two child processes, each with a READ parked on a reader that
//!   then waits inside `Fs::handle` for the READ's cancellation, so that the INTERRUPT the kernel
//!   queues when the child is killed can only be read by another clone. Each child ends, killed by
//!   signal 9, which it does only once its READ is answered (K8): the `EINTR` went to the READ's
//!   own clone. The late completion of each parked replier writes nothing. The child is this test
//!   binary running [`child_reads_the_file_named_on_stdin`], one `pread` of its own: a tool such
//!   as `cat` may copy by `splice`, which goes through the page cache even for an
//!   `FOPEN_DIRECT_IO` file, and under the session's `ASYNC_READ` such a READ has no waiting
//!   caller and gets no INTERRUPT (K8).
//! - **Async reply while every reader is blocked.** A READ parked, then both readers blocked
//!   inline (a stalled `pread`): the parked READ, completed by a task, reaches its caller while no
//!   reader is free.
//!
//! The tree is the one of the T-F0 checks (`kernel/mod.rs`): the root and the files `f` and `g`,
//! opened with `FOPEN_DIRECT_IO` so that each `pread` is one READ with a waiting caller. Every
//! wait is on an event (a request handled, a caller's system call returning, a child ending),
//! never on a timer. An integration target: it mounts through `mount(2)` as root (the farm) or
//! `fusermount3` (CI), and says on stderr why it does not run where neither can.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod kernel;

use std::collections::HashSet;
use std::fs::File;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::os::unix::fs::FileExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::channel;
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::thread::{self, ThreadId};

use kernel::{
    FOPEN_DIRECT_IO, ROOT, VALID, attr, content, ino_of, one_at_a_time, skip, unavailable,
};
use tierstore_testkit::dir::TestDir;
use tokio::sync::oneshot;
use tsfuse::abi::{AttrOut, EntryOut, InitConfig, OpenOut, Operation};
use tsfuse::mount::{Mount, MountOptions, Mounter};
use tsfuse::{Completion, Errno, Fs, Handled, ReaderEnd, Replier, Request, Session, Tokens};

/// The readers (descriptors) of every session here: the mount's and three clones.
const READERS: usize = 4;

/// A READ as the file system saw it.
#[derive(Debug, Clone, Copy)]
struct Read {
    unique: u64,
    ino: u64,
    offset: u64,
    size: u32,
}

/// A parked READ: its replier and the reader thread that read it.
struct Parked {
    read: Read,
    replier: Replier,
    reader: ThreadId,
}

/// What the readers did, watched by the test.
#[derive(Default)]
struct State {
    parked: Vec<Parked>,
    /// Readers waiting inside `Fs::handle`.
    blocked: usize,
    /// Readers that saw their READ cancelled.
    cancelled: Vec<ThreadId>,
    /// The gate of an inline READ is open.
    open: bool,
}

/// [`State`] and the condition variable a waiter watches it with.
#[derive(Default)]
struct Events {
    state: Mutex<State>,
    changed: Condvar,
}

impl Events {
    fn update<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let r = f(&mut self.state.lock().unwrap());
        self.changed.notify_all();
        r
    }

    /// Waits until `f` returns `Some`, on a change of the state.
    fn wait_for<R>(&self, mut f: impl FnMut(&mut State) -> Option<R>) -> R {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(r) = f(&mut state) {
                return r;
            }
            state = self.changed.wait(state).unwrap();
        }
    }
}

/// What a check does with a READ.
type OnRead = dyn Fn(Read, Replier, &Events) -> Handled + Send + Sync;

/// The tree of `kernel/mod.rs`, answered inline, except READ, which `on_read` handles.
struct Tree {
    events: Arc<Events>,
    on_read: Box<OnRead>,
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
                let read = Read {
                    unique: req.header.unique,
                    ino: node,
                    offset: r.offset,
                    size: r.size,
                };
                return (self.on_read)(read, replier, &self.events);
            }
            Ok(Operation::Flush(_) | Operation::Release(_)) => replier.ok(&[]),
            _ => replier.error(Errno::ENOSYS),
        };
        Handled::Done
    }
}

/// A mount of the tree; dropping it aborts the connection (as root; the abort file is root's),
/// so a failed check ends every caller still waiting, and unmounts.
struct Mounted {
    mount: Option<Mount>,
    mp: PathBuf,
    _dir: TestDir,
}

impl Mounted {
    /// Mounts in a scratch directory named after `name`, runs INIT with the tree's terms and binds
    /// clones up to [`READERS`]. `None` (said on stderr) where no FUSE mount can be made.
    fn new(name: &str) -> Option<(Self, Session)> {
        if let Some(why) = unavailable() {
            skip(why);
            return None;
        }
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let mp = dir.path().join("mnt");
        std::fs::create_dir(&mp).unwrap();
        let options = MountOptions {
            allow_other: false,
            ..MountOptions::tree("tsfuse-dispatch")
        };
        let (device, mount) = one_at_a_time(|| Mounter::default().mount(&mp, &options)).unwrap();
        let mut session = Session::init(device, &InitConfig::tree()).unwrap();
        session
            .clone_readers(NonZeroUsize::new(READERS).unwrap())
            .unwrap();
        assert_eq!(session.readers(), READERS);
        let mounted = Self {
            mount: Some(mount),
            mp,
            _dir: dir,
        };
        Some((mounted, session))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.mp.join(name)
    }

    /// Unmounts; the readers then end. A failed unmount leaves the mount to the drop.
    fn unmount(mut self) {
        let mount = self.mount.clone().unwrap();
        one_at_a_time(|| mount.unmount(false)).unwrap();
        self.mount = None;
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

/// A served session. Dropping it before [`Self::join`] (a failed check) opens the gate of every
/// reader waiting inline, drops every parked replier (each answers `EIO`) and stops the readers,
/// so every descriptor of the connection closes and the kernel ends any request still waiting
/// with `ECONNABORTED` (K5): a caller of this process is never left in the kernel's
/// uninterruptible wait, which would keep the test process from exiting.
struct Served {
    serving: Option<tsfuse::Serving>,
    events: Arc<Events>,
}

impl Served {
    /// Waits for every reader to end: how each ended.
    fn join(mut self) -> Vec<ReaderEnd> {
        self.serving.take().unwrap().join().unwrap()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(serving) = self.serving.take() {
            self.events.update(|s| {
                s.open = true;
                s.parked.clear();
            });
            serving.stopper().stop();
        }
    }
}

/// Serves `session` with a [`Tree`] whose READs go to `on_read`.
fn serve(
    session: Session,
    on_read: impl Fn(Read, Replier, &Events) -> Handled + Send + Sync + 'static,
) -> (Served, Arc<Events>, Tokens) {
    let events = Arc::new(Events::default());
    let tokens = session.tokens().clone();
    let tree = Tree {
        events: events.clone(),
        on_read: Box::new(on_read),
    };
    let served = Served {
        serving: Some(session.serve(Arc::new(tree)).unwrap()),
        events: events.clone(),
    };
    (served, events, tokens)
}

/// A caller of the mount on a thread of its own: it opens a file, reads it once and closes it.
struct Caller {
    bytes: std::sync::mpsc::Receiver<Vec<u8>>,
    thread: thread::JoinHandle<()>,
}

impl Caller {
    /// The bytes its read returned.
    fn bytes(&self) -> Vec<u8> {
        self.bytes.recv().unwrap()
    }

    /// Waits until it has closed the file (its FLUSH answered), so an unmount finds it idle.
    fn join(self) {
        self.thread.join().unwrap();
    }
}

/// Opens `path` and reads `len` bytes at `offset` on a thread of its own; the bytes are sent
/// before the file is closed, since the close sends a FLUSH that needs a free reader.
fn pread(path: PathBuf, offset: u64, len: usize) -> Caller {
    let (tx, bytes) = channel();
    let thread = thread::spawn(move || {
        let file = File::open(path).unwrap();
        let mut buf = vec![0u8; len];
        let n = file.read_at(&mut buf, offset).unwrap();
        buf.truncate(n);
        tx.send(buf).unwrap();
        drop(file);
    });
    Caller { bytes, thread }
}

/// A multi-thread runtime of two workers: the tasks that complete parked replies.
fn completers() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap()
}

/// Completes `parked` with its bytes from a task that waits for its data, as a fill does.
fn complete_from_a_task(runtime: &tokio::runtime::Runtime, parked: Parked) -> Completion {
    let (fill, filled) = oneshot::channel::<Vec<u8>>();
    let replier = parked.replier;
    let task = runtime.spawn(async move { replier.ok(&[&filled.await.unwrap()]) });
    let r = parked.read;
    fill.send(content(r.ino, r.offset, u64::from(r.size)))
        .unwrap();
    runtime.block_on(task).unwrap()
}

#[test]
fn reads_parked_on_four_clones_and_completed_by_tasks_out_of_order_reach_their_callers() {
    let Some((mounted, session)) = Mounted::new("tsfuse-dispatch-affinity") else {
        return;
    };
    let barrier = Arc::new(Barrier::new(READERS));
    let (serving, events, tokens) = serve(session, move |read, replier, events| {
        let reader = thread::current().id();
        events.update(|s| {
            s.parked.push(Parked {
                read,
                replier,
                reader,
            });
        });
        // Hold this reader until every reader holds a READ: each READ is on its own clone.
        barrier.wait();
        Handled::Parked
    });

    let offsets: Vec<u64> = (0..READERS as u64).map(|i| i * 65_536 + 512).collect();
    let callers: Vec<_> = offsets
        .iter()
        .map(|&offset| (offset, pread(mounted.path("f"), offset, 4096)))
        .collect();
    let mut parked =
        events.wait_for(|s| (s.parked.len() == READERS).then(|| std::mem::take(&mut s.parked)));
    let readers: HashSet<ThreadId> = parked.iter().map(|p| p.reader).collect();
    assert_eq!(
        readers.len(),
        READERS,
        "each READ was read by its own reader"
    );

    // Completed by tasks of another runtime, in the reverse of the order they were read.
    parked.sort_by_key(|p| std::cmp::Reverse(p.read.unique));
    let runtime = completers();
    for p in parked {
        let unique = p.read.unique;
        let done = complete_from_a_task(&runtime, p);
        assert!(
            matches!(done, Completion::Written),
            "the kernel took the reply to {unique} on its clone: {done:?}"
        );
    }
    for (offset, caller) in callers {
        assert_eq!(caller.bytes(), content(2, offset, 4096), "offset {offset}");
        caller.join();
    }
    mounted.unmount();
    assert_eq!(serving.join(), vec![ReaderEnd::Unmounted; READERS]);
    // Checked once every reader has ended: another process may stat the new mount at any time,
    // and its request is in the table while a reader answers it.
    assert_eq!(tokens.registered(), 0);
}

/// The name of [`child_reads_the_file_named_on_stdin`], as the child's command line gives it.
const CHILD: &str = "child_reads_the_file_named_on_stdin";

/// Not a check of its own: the child process of
/// [`an_interrupt_read_by_another_clone_answers_each_parked_read_eintr_once`]. It reads the path
/// of a file of the mount from its standard input, opens the file and reads 4 KiB at offset 0 with
/// one `pread`, a direct READ whose caller waits for the answer; the parent kills it while the
/// READ is parked.
#[test]
#[ignore = "the child process of an_interrupt_read_by_another_clone_answers_each_parked_read_eintr_once, which runs it with --ignored"]
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
fn an_interrupt_read_by_another_clone_answers_each_parked_read_eintr_once() {
    let Some((mounted, session)) = Mounted::new("tsfuse-dispatch-interrupt") else {
        return;
    };
    let (serving, events, tokens) = serve(session, |read, replier, events| {
        let token = replier.cancel_token();
        let reader = thread::current().id();
        events.update(|s| {
            s.parked.push(Parked {
                read,
                replier,
                reader,
            });
        });
        // This reader reads nothing more until the READ is cancelled, so the INTERRUPT that
        // cancels it is read by another clone.
        futures::executor::block_on(token.cancelled());
        events.update(|s| s.cancelled.push(reader));
        Handled::Parked
    });

    let mut children = [
        child_reading(&mounted.path("f")),
        child_reading(&mounted.path("g")),
    ];
    let parked = events.wait_for(|s| (s.parked.len() == 2).then(|| std::mem::take(&mut s.parked)));
    let files: HashSet<u64> = parked.iter().map(|p| p.read.ino).collect();
    let both: HashSet<u64> = ["f", "g"].into_iter().filter_map(ino_of).collect();
    assert_eq!(files, both, "each child's READ is parked");
    assert_ne!(parked[0].reader, parked[1].reader, "on two readers");

    for child in &mut children {
        child.kill().unwrap();
    }
    for child in &mut children {
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(9), "{status:?}");
    }
    let cancelled = events.wait_for(|s| (s.cancelled.len() == 2).then(|| s.cancelled.clone()));
    let holders: HashSet<ThreadId> = parked.iter().map(|p| p.reader).collect();
    assert_eq!(cancelled.into_iter().collect::<HashSet<_>>(), holders);
    for p in parked {
        let late = p.replier.ok(&[b"late"]);
        assert!(matches!(late, Completion::AlreadyAnswered), "{late:?}");
    }
    mounted.unmount();
    assert_eq!(serving.join(), vec![ReaderEnd::Unmounted; READERS]);
    // Checked once every reader has ended: another process may stat the new mount at any time,
    // and its request is in the table while a reader answers it.
    assert_eq!(tokens.registered(), 0);
}

#[test]
fn a_parked_read_completed_by_a_task_reaches_its_caller_while_every_reader_is_blocked_inline() {
    let Some((mounted, session)) = Mounted::new("tsfuse-dispatch-blocked") else {
        return;
    };
    let f = ino_of("f").unwrap();
    let (serving, events, tokens) = serve(session, move |read, replier, events| {
        let reader = thread::current().id();
        if read.ino == f {
            events.update(|s| {
                s.parked.push(Parked {
                    read,
                    replier,
                    reader,
                });
            });
            return Handled::Parked;
        }
        // A stalled `pread`: the reader waits inline until the test opens the gate.
        events.update(|s| s.blocked += 1);
        events.wait_for(|s| s.open.then_some(()));
        let _ = replier.ok(&[&content(read.ino, read.offset, u64::from(read.size))]);
        Handled::Done
    });

    let parked_caller = pread(mounted.path("f"), 8192, 4096);
    let parked = events.wait_for(|s| s.parked.pop());
    // Every reader stalls in an inline READ of `g`.
    let blocked_callers: Vec<_> = (0..READERS as u64)
        .map(|i| (i * 4096, pread(mounted.path("g"), i * 4096, 4096)))
        .collect();
    events.wait_for(|s| (s.blocked == READERS).then_some(()));

    let runtime = completers();
    let done = complete_from_a_task(&runtime, parked);
    assert!(matches!(done, Completion::Written), "{done:?}");
    assert_eq!(parked_caller.bytes(), content(f, 8192, 4096));
    assert_eq!(
        events.update(|s| (s.blocked, s.open)),
        (READERS, false),
        "the parked READ was answered while every reader was blocked"
    );

    events.update(|s| s.open = true);
    let g = ino_of("g").unwrap();
    for (offset, caller) in blocked_callers {
        assert_eq!(caller.bytes(), content(g, offset, 4096));
        caller.join();
    }
    parked_caller.join();
    mounted.unmount();
    assert_eq!(serving.join(), vec![ReaderEnd::Unmounted; READERS]);
    // Checked once every reader has ended: another process may stat the new mount at any time,
    // and its request is in the table while a reader answers it.
    assert_eq!(tokens.registered(), 0);
}
