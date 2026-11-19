//! Kernel conformance, item 7 of `docs/design/fetch-cache-serve.md` 5.2 ("Takeover with N > 1
//! readers: kill mid-load with parked reads on several clones, resume with every retained
//! descriptor, exactly-once, no EIO"), with a real server process that is killed. Where
//! `takeover_integration.rs` hands over cooperatively inside one process, here the server is a
//! child process (this test binary run again on [`child_resumes_the_handover_on_stdin`]) that
//! the supervisor, this process, kills with `SIGKILL`: none of the child's destructors runs, so
//! it writes no `EIO` for what it held. The mount has four readers (the mount's descriptor and
//! three clones, K1), the supervisor keeps a handover of the descriptors, and the child serves a
//! `Model` whose first four READs are held at a barrier (so each reader holds one) and then
//! every READ is parked. Twelve callers read one direct-IO file: eight of them are parked in the
//! child when it is killed, four start after (their requests wait in the kernel's pending queue,
//! with no reader). The checks run through the library: `Session::init`, `clone_readers`,
//! `handover`, `Handover::send` and `receive`, `Session::resume`, `Session::serve`, `ReplyPath`.
//!
//! - [`a_killed_server_with_a_read_parked_on_every_clone_is_resumed_with_no_error_and_each_read_answered_once`]:
//!   the supervisor keeps every descriptor. After the kill, `Session::resume` sends RESEND (K6) and
//!   the new server reads each of the eight parked READs once, with `UNIQUE_RESEND` set in its
//!   unique and the old unique under it (K4), and each of the four later READs once without it;
//!   all twelve callers get their own bytes, none `EIO` or `ECONNABORTED`; nothing is read twice.
//!   The child's own `Session::resume` sent RESEND with nothing in flight, which resends nothing
//!   (K6).
//! - [`a_clone_the_supervisor_did_not_keep_loses_its_parked_read_to_econnaborted_when_the_server_dies`]:
//!   the supervisor keeps every descriptor but the last clone. When the child dies the last
//!   reference to that clone closes, and the reads parked on it end with `ECONNABORTED` (K5);
//!   every other caller gets its bytes after the resume, and each resent READ is read and
//!   answered once. The reads of the lost clone are not read again.
//!
//! The descriptors reach the child without `unsafe`: the child's standard input is one end of a
//! `SOCK_SEQPACKET` socket pair (`Stdio::from(OwnedFd)`), and the supervisor sends the handover
//! on the other end. The test skips where the kernel did not offer `FUSE_HAS_RESEND` in INIT
//! (before Linux 6.9). Root-only facts are not used.
//!
//! Not covered here: the io_uring half of item 7 (K14: a request held by a ring entry is not
//! resent, and one caught mid-dispatch gets `ECANCELED`) waits for the io_uring transport (row
//! T-F5 of the design); no test is written for it.
//!
//! K-facts relied on: K1 (a reply goes to the descriptor that read the request), K4 and K6
//! (RESEND, bit 63 of the unique, a request answered under the unique as read again), K5 (the
//! requests of a clone end with `ECONNABORTED` when its last descriptor closes). Every wait is on
//! an event: the child's report on its standard output, a caller's system call returning, the
//! child's end, a reader's end.
//!
//! An integration target: it mounts through `mount(2)` as root (the farm) or fusermount3 (CI),
//! and says on stderr why it does not run where neither can.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod conformance;
mod kernel;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::process::ExitStatusExt as _;
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use conformance::{
    Caller, Done, FOPEN_DIRECT_IO, Mounted, Open, Opendir, ROOT, Read, Served, TreeSpec,
    answer_read, memfd, park, pattern, pread, reader_name, serve,
};
use kernel::skip;
use tsfuse::abi::{InitFlags, UNIQUE_RESEND};
use tsfuse::{Handled, Handover, ReaderEnd, ReplyConfig, ReplyPath, Session};

/// The readers of the mount: the mount's descriptor and three clones.
const READERS: usize = 4;
/// The bytes each caller reads, and the distance between the offsets of two callers.
const LEN: u64 = 4096;
/// The callers parked in the server when it is killed.
const PARKED: usize = 8;
/// The callers that start after the kill.
const LATER: usize = 4;
/// The marker of the child's report of a parked READ on its standard output (the test harness
/// may print a line of its own in front of it).
const MARK: &str = "tsfuse-parked ";
/// The reader the supervisor does not keep in the second test.
const LOST_READER: &str = "tsfuse-reader-3";
/// The read of a caller whose clone is gone ends with this errno.
const ECONNABORTED: i32 = conformance::errno::ECONNABORTED;

/// The tree of both processes: the root and the file `data` of twelve callers' ranges.
fn tree() -> (Arc<TreeSpec>, u64) {
    let size = (PARKED + LATER) as u64 * LEN;
    let mut tree = TreeSpec::new();
    let data = memfd("takeover-data", &pattern(7, size as usize));
    let ino = tree.file(ROOT, "data", size, Some(data));
    (Arc::new(tree), ino)
}

/// A READ the child reported as parked.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reported {
    unique: u64,
    ino: u64,
    offset: u64,
    reader: String,
}

/// The server process, killed when dropped (a failed check must leave none behind).
struct ServerProcess {
    child: Child,
    out: BufReader<ChildStdout>,
}

impl ServerProcess {
    /// Runs the child with `stdin` as its standard input.
    fn spawn(stdin: OwnedFd) -> Self {
        let mut child = conformance::child("child_resumes_the_handover_on_stdin")
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Self { child, out }
    }

    /// Reads the child's reports of `count` parked READs.
    fn parked(&mut self, count: usize) -> Vec<Reported> {
        let mut reports = Vec::new();
        let mut line = String::new();
        while reports.len() < count {
            line.clear();
            let n = self.out.read_line(&mut line).unwrap();
            assert_ne!(
                n,
                0,
                "the server ended after {} reports: {reports:?}",
                reports.len()
            );
            let Some((_, rest)) = line.split_once(MARK) else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let mut number = || fields.next().unwrap().parse::<u64>().unwrap();
            let (unique, ino, offset) = (number(), number(), number());
            let reader = fields.next().unwrap().to_owned();
            reports.push(Reported {
                unique,
                ino,
                offset,
                reader,
            });
        }
        reports
    }

    /// Kills the child with `SIGKILL` and waits for it: the signal it ended by.
    fn kill(&mut self) -> Option<i32> {
        self.child.kill().unwrap();
        self.child.wait().unwrap().signal()
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The child process: receives a handover on its standard input, resumes it, and serves a model
/// that holds each of its first four READs at a barrier (so that each reader holds one), then
/// parks every READ, reporting each as one line on standard output. It serves until it is
/// killed.
#[test]
#[ignore = "the child process of the takeover tests of this file, which run it with --ignored"]
fn child_resumes_the_handover_on_stdin() {
    let handover = Handover::receive(std::io::stdin().as_fd()).unwrap();
    let readers = handover.devices();
    let session = Session::resume(handover).unwrap();
    let (tree, _) = tree();
    let gate = Barrier::new(readers);
    let seen = AtomicUsize::new(0);
    let served = serve(
        session,
        tree,
        Open::Flags(FOPEN_DIRECT_IO),
        Opendir::Ok,
        move |_, read, replier, events| {
            if seen.fetch_add(1, Ordering::SeqCst) < readers {
                gate.wait();
            }
            let reader = reader_name();
            let mut out = std::io::stdout().lock();
            writeln!(
                out,
                "{MARK}{} {} {} {reader}",
                read.unique, read.ino, read.offset
            )
            .unwrap();
            out.flush().unwrap();
            drop(out);
            park(read, replier, events)
        },
    );
    served.join();
}

/// A mount whose server process was killed with a READ parked on every reader.
struct Killed {
    mounted: Mounted,
    tree: Arc<TreeSpec>,
    ino: u64,
    /// The eight callers parked in the server, then the four that started after the kill, by
    /// offset (`i * LEN`).
    callers: Vec<Caller>,
    /// The READs the server reported parked, by offset.
    parked: BTreeMap<u64, Reported>,
}

/// Mounts with four readers, has a child process serve and park a READ on each of them for eight
/// callers, kills it, and starts four more callers. The supervisor keeps every descriptor, or
/// every one but the last clone when `drop_last_clone` is set. `None` where no mount can be made
/// or the kernel does not offer `FUSE_HAS_RESEND`.
fn kill_the_server(name: &str, drop_last_clone: bool) -> Option<(Killed, Handover)> {
    let (mounted, session) = Mounted::new(name, READERS)?;
    if !session.negotiated().granted.contains(InitFlags::HAS_RESEND) {
        skip("the kernel does not offer FUSE_HAS_RESEND (Linux 6.9 or later does)");
        return None;
    }
    let (tree, ino) = tree();

    // The supervisor's handover, and the one it passes to the child.
    let full = session.handover().unwrap();
    let negotiated = *full.negotiated();
    let mut fds = full.into_fds().unwrap();
    assert_eq!(fds.len(), READERS + 1, "the terms and every descriptor");
    let mut devices = fds.split_off(1);
    if drop_last_clone {
        drop(devices.pop());
    }
    let kept = Handover::new(negotiated, devices).unwrap();
    drop(fds);
    let for_child = session.handover().unwrap();
    // The supervisor's own descriptors close: only the handovers hold the devices now.
    drop(session);

    let (supervisor, child_end) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::SEQPACKET,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    let mut server = ServerProcess::spawn(child_end);
    for_child.send(supervisor.as_fd()).unwrap();
    drop(for_child);

    let mut callers: Vec<Caller> = (0..PARKED as u64)
        .map(|i| pread(mounted.path("data"), i * LEN, LEN as usize))
        .collect();
    let reports = server.parked(PARKED);
    let parked: BTreeMap<u64, Reported> = reports
        .into_iter()
        .map(|report| (report.offset, report))
        .collect();
    assert_eq!(
        parked.keys().copied().collect::<Vec<_>>(),
        (0..PARKED as u64).map(|i| i * LEN).collect::<Vec<_>>(),
        "every caller's READ was parked once"
    );
    assert!(parked.values().all(|r| r.ino == ino));
    let readers: BTreeSet<&str> = parked.values().map(|r| r.reader.as_str()).collect();
    assert_eq!(
        readers.len(),
        READERS,
        "a READ parked on each reader: {readers:?}"
    );

    assert_eq!(server.kill(), Some(9), "the server ends by SIGKILL");
    callers.extend(
        (PARKED..PARKED + LATER).map(|i| pread(mounted.path("data"), i as u64 * LEN, LEN as usize)),
    );
    let killed = Killed {
        mounted,
        tree,
        ino,
        callers,
        parked,
    };
    Some((killed, kept))
}

/// Resumes `kept` and serves it with a model that answers every READ inline from the tree
/// through a reply path.
fn resume_and_serve(kept: Handover, tree: Arc<TreeSpec>) -> Served {
    let resumed = Session::resume(kept).unwrap();
    let path =
        Arc::new(ReplyPath::new(ReplyConfig::for_session(resumed.negotiated(), READERS)).unwrap());
    serve(
        resumed,
        tree,
        Open::Flags(FOPEN_DIRECT_IO),
        Opendir::Ok,
        move |tree, read, replier, events| {
            answer_read(&path, tree, read, replier, events);
            Handled::Done
        },
    )
}

/// The READs the served model saw, by offset.
fn reads_by_offset(served: &Served) -> BTreeMap<u64, Vec<Read>> {
    let mut by_offset: BTreeMap<u64, Vec<Read>> = BTreeMap::new();
    for read in served.events.with(|s| s.reads.clone()) {
        by_offset.entry(read.offset).or_default().push(read);
    }
    by_offset
}

/// Checks that the new server read each offset of `resent` once with `UNIQUE_RESEND` set (under
/// the old unique), each of the later offsets once without it, and nothing else; that every
/// completion was written; and that the callers of `ok` got their bytes.
fn check_reads(killed: &Killed, served: &Served, resent: &BTreeSet<u64>) {
    let reads = reads_by_offset(served);
    let later: BTreeSet<u64> = (PARKED..PARKED + LATER).map(|i| i as u64 * LEN).collect();
    let expected: BTreeSet<u64> = resent.union(&later).copied().collect();
    assert_eq!(
        reads.keys().copied().collect::<BTreeSet<_>>(),
        expected,
        "the new server read the resent and the later ranges, nothing else"
    );
    for (offset, seen) in &reads {
        assert_eq!(seen.len(), 1, "the READ at {offset} is read once: {seen:?}");
        let read = seen[0];
        assert_eq!((read.ino, read.size), (killed.ino, LEN as u32), "{read:?}");
        match killed
            .parked
            .get(offset)
            .filter(|_| resent.contains(offset))
        {
            Some(old) => assert_eq!(
                read.unique,
                old.unique | UNIQUE_RESEND,
                "the READ at {offset} comes back under its old unique with bit 63 set"
            ),
            None => assert_eq!(
                read.unique & UNIQUE_RESEND,
                0,
                "the later READ at {offset} is not a resend"
            ),
        }
    }
    // `answer_read` records a completion after its reply is written, so a caller can have its
    // bytes first: wait until every READ read has one.
    let completions = served
        .events
        .wait_for(|s| (s.completions.len() >= expected.len()).then(|| s.completions.clone()));
    assert_eq!(
        completions.len(),
        expected.len(),
        "one answer per READ read"
    );
    assert!(
        completions
            .iter()
            .all(|(_, done, _)| *done == Done::Written),
        "{completions:?}"
    );
}

/// Unmounts after every caller has closed its file, and checks how the readers ended.
fn finish(killed: Killed, served: Served, callers: Vec<Caller>, readers: usize) {
    for caller in callers {
        caller.join();
    }
    killed.mounted.unmount();
    assert_eq!(served.join(), vec![ReaderEnd::Unmounted; readers]);
}

#[test]
fn a_killed_server_with_a_read_parked_on_every_clone_is_resumed_with_no_error_and_each_read_answered_once()
 {
    let Some((mut killed, kept)) = kill_the_server("tsfuse-conformance-takeover-all", false) else {
        return;
    };
    let served = resume_and_serve(kept, killed.tree.clone());

    let callers = std::mem::take(&mut killed.callers);
    for (i, caller) in callers.iter().enumerate() {
        let offset = i as u64 * LEN;
        assert_eq!(
            caller.bytes(),
            killed.tree.bytes(killed.ino, offset, LEN),
            "caller {i} reads its own range, with no error"
        );
    }
    let resent: BTreeSet<u64> = killed.parked.keys().copied().collect();
    check_reads(&killed, &served, &resent);
    finish(killed, served, callers, READERS);
}

#[test]
fn a_clone_the_supervisor_did_not_keep_loses_its_parked_read_to_econnaborted_when_the_server_dies()
{
    let Some((mut killed, kept)) = kill_the_server("tsfuse-conformance-takeover-lost", true) else {
        return;
    };
    let lost: BTreeSet<u64> = killed
        .parked
        .values()
        .filter(|r| r.reader == LOST_READER)
        .map(|r| r.offset)
        .collect();
    assert!(!lost.is_empty(), "a READ was parked on {LOST_READER}");
    let callers = std::mem::take(&mut killed.callers);

    // The kernel ends them as the server dies, before any resume.
    for offset in &lost {
        let got = callers[(offset / LEN) as usize].result();
        assert_eq!(
            got.as_ref().err().and_then(std::io::Error::raw_os_error),
            Some(ECONNABORTED),
            "the caller at {offset} (parked on the clone nobody kept): {got:?}"
        );
    }

    assert_eq!(kept.devices(), READERS - 1);
    let served = resume_and_serve(kept, killed.tree.clone());
    for (i, caller) in callers.iter().enumerate() {
        let offset = i as u64 * LEN;
        if lost.contains(&offset) {
            continue;
        }
        assert_eq!(
            caller.bytes(),
            killed.tree.bytes(killed.ino, offset, LEN),
            "caller {i} reads its own range, with no error"
        );
    }
    let resent: BTreeSet<u64> = killed
        .parked
        .keys()
        .filter(|offset| !lost.contains(offset))
        .copied()
        .collect();
    check_reads(&killed, &served, &resent);
    finish(killed, served, callers, READERS - 1);
}
