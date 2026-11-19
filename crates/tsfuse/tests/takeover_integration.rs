//! Takeover against the kernel (5.2, item 5; section 14 row "Takeover with N > 1 clones and
//! parked reads on several: supervisor retains every fd, no EIO, exactly-once"): a session with
//! three readers (the mount's descriptor and two clones) parks one READ on each; the server
//! hands over and goes away; the supervisor, which kept every descriptor as a list of
//! descriptors, passes them to the next server by `SCM_RIGHTS`; the next server resumes without
//! INIT and sends RESEND before it reads. Of the three parked READs, the old server answers one
//! after the hand-over and before the RESEND (the kernel takes it, and does not resend it), one
//! after the RESEND (the kernel refuses it with `ENOENT`), and drops the third. The next server
//! sees exactly the second and third again, with bit 63 set in their unique, and answers each
//! once; every caller gets its own bytes and none an error.
//!
//! An integration target: it mounts through `mount(2)` as root (the farm) or `fusermount3`
//! (CI), and says on stderr why it does not run where neither can, or where the kernel does not
//! offer `FUSE_HAS_RESEND` (before Linux 6.9). It blocks only on kernel events and on the old
//! server's own reports, never on a timer.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod kernel;

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Barrier, Mutex};

use kernel::{
    FILES, FOPEN_DIRECT_IO, VALID, attr, caller, content, errno, ino_of, pread, skip, unavailable,
};
use tierstore_testkit::dir::TestDir;
use tsfuse::abi::{
    AttrOut, EntryOut, InitConfig, InitFlags, OpenOut, ReadIn, StatfsOut, UNIQUE_RESEND,
};
use tsfuse::mount::{Mount, MountOptions, Mounter};
use tsfuse::{Completion, Errno, Fs, Handled, Handover, ReaderEnd, Replier, Request, Session};

/// The number of readers: the mount's descriptor and two clones.
const READERS: usize = 3;
/// The bytes each caller reads.
const LEN: u32 = 4096;

/// A mount that is aborted (where this user may) and detached if the test fails before it
/// unmounts, so that no caller stays blocked on it.
struct Mounted(Option<Mount>);

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(mount) = self.0.take() {
            let _ = mount.abort();
            let _ = mount.unmount(true);
        }
    }
}

/// Answers what the tiny tree of `tests/kernel` answers, with every file opened direct (one
/// READ per `pread`), and hands a READ back to the caller.
fn answer(req: &Request<'_>, replier: Replier) -> Option<(ReadIn, Replier)> {
    let h = req.header;
    let done = match h.opcode {
        1 => {
            let name = req.body.split(|b| *b == 0).next().unwrap_or_default();
            let ino = ino_of(&String::from_utf8_lossy(name));
            match (h.nodeid, ino) {
                (1, Some(ino)) => {
                    let entry = EntryOut::new(ino, 1, VALID, VALID, attr(ino));
                    replier.ok(&[entry.as_bytes()])
                }
                _ => replier.error(Errno(errno::ENOENT)),
            }
        }
        3 => replier.ok(&[AttrOut::new(VALID, attr(h.nodeid)).as_bytes()]),
        14 => {
            let open = OpenOut {
                fh: h.nodeid,
                open_flags: FOPEN_DIRECT_IO,
                backing_id: 0,
            };
            replier.ok(&[open.as_bytes()])
        }
        15 => return Some((ReadIn::read_prefix(req.body).unwrap().0, replier)),
        17 => replier.ok(&[StatfsOut::default().as_bytes()]),
        18 | 25 | 27 | 29 => replier.ok(&[]),
        _ => replier.error(Errno::ENOSYS),
    };
    assert!(
        !matches!(done, Completion::DeviceError(_)),
        "opcode {}: {done:?}",
        h.opcode
    );
    None
}

/// The old server: parks every READ, after holding its reader until each reader holds one (so
/// one READ is parked on every descriptor), and reports each with the reader that read it.
struct Old {
    gate: Barrier,
    parked: Mutex<Sender<(u64, String, Replier)>>,
}

impl Fs for Old {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        let Some((read, replier)) = answer(&req, replier) else {
            return Handled::Done;
        };
        self.gate.wait();
        let reader = std::thread::current().name().unwrap_or_default().to_owned();
        let _ = self
            .parked
            .lock()
            .unwrap()
            .send((read.offset, reader, replier));
        Handled::Parked
    }
}

/// The next server: answers every READ with the file's bytes, and records its unique and offset.
#[derive(Default)]
struct New {
    reads: Mutex<Vec<(u64, u64)>>,
}

impl Fs for New {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        if let Some((read, replier)) = answer(&req, replier) {
            self.reads
                .lock()
                .unwrap()
                .push((req.header.unique, read.offset));
            let data = content(req.header.nodeid, read.offset, u64::from(read.size));
            assert!(matches!(replier.ok(&[&data]), Completion::Written));
        }
        Handled::Done
    }
}

#[test]
fn takeover_with_a_read_parked_on_every_clone_answers_each_once_and_no_caller_sees_an_error() {
    if let Some(why) = unavailable() {
        skip(why);
        return;
    }
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tsfuse-takeover");
    let mp = dir.path().join("mnt");
    std::fs::create_dir(&mp).unwrap();
    let options = MountOptions {
        allow_other: false,
        ..MountOptions::tree("tsfuse-takeover")
    };
    let (device, mount) = Mounter::default().mount(&mp, &options).unwrap();
    let mut mounted = Mounted(Some(mount));
    let mut session = Session::init(device, &InitConfig::tree()).unwrap();
    if !session.negotiated().granted.contains(InitFlags::HAS_RESEND) {
        skip("the kernel does not offer FUSE_HAS_RESEND (Linux 6.9 or later does)");
        return;
    }
    session
        .clone_readers(NonZeroUsize::new(READERS).unwrap())
        .unwrap();

    // The supervisor's store: every descriptor, the terms first, as a service manager keeps
    // them.
    let store: Vec<OwnedFd> = session.handover().unwrap().into_fds().unwrap();
    assert_eq!(store.len(), READERS + 1);

    let (report, parked) = channel();
    let old = Arc::new(Old {
        gate: Barrier::new(READERS),
        parked: Mutex::new(report),
    });
    let serving = session.serve(old).unwrap();
    let path = mp.join(FILES[0].0);
    let callers: Vec<_> = (0..READERS as u64)
        .map(|i| {
            let path = path.clone();
            caller(move || pread(&path, i * u64::from(LEN), LEN as usize))
        })
        .collect();
    let mut held: Vec<(u64, String, Replier)> =
        (0..READERS).map(|_| parked.recv().unwrap()).collect();
    held.sort_by_key(|(offset, _, _)| *offset);
    let readers: BTreeSet<&str> = held.iter().map(|(_, r, _)| r.as_str()).collect();
    assert_eq!(
        readers.len(),
        READERS,
        "one READ parked on each descriptor: {readers:?}"
    );

    // The old server hands over: its readers stop, and what it drops is not answered.
    assert_eq!(
        serving.hand_over().unwrap(),
        vec![ReaderEnd::Stopped; READERS]
    );
    let mut held = held
        .into_iter()
        .map(|(offset, _, replier)| (offset, replier));
    let ino = FILES[0].1;
    let (offset, first) = held.next().unwrap();
    let answered = first.ok(&[&content(ino, offset, u64::from(LEN))]);
    assert!(matches!(answered, Completion::Written), "{answered:?}");

    // The supervisor passes its store to the next server, which resumes it (RESEND first).
    let kept = Handover::from_fds(store).unwrap();
    let (supervisor, next) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::SEQPACKET,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    kept.send(supervisor.as_fd()).unwrap();
    let resumed = Session::resume(Handover::receive(next.as_fd()).unwrap()).unwrap();
    assert_eq!(resumed.readers(), READERS);

    // After the RESEND the old unique is gone from every processing queue.
    let (offset, second) = held.next().unwrap();
    let late = second.ok(&[&content(ino, offset, u64::from(LEN))]);
    let Completion::DeviceError(late) = late else {
        panic!("a reply under the unique before RESEND was taken: {late:?}");
    };
    assert_eq!(late.raw_os_error(), Some(errno::ENOENT));
    drop(held);

    let new = Arc::new(New::default());
    let serving = resumed.serve(new.clone()).unwrap();
    for (i, c) in callers.into_iter().enumerate() {
        let offset = i as u64 * u64::from(LEN);
        let got = c.join().unwrap().unwrap();
        assert_eq!(got, content(ino, offset, u64::from(LEN)), "caller {i}");
    }
    let mut reads = new.reads.lock().unwrap().clone();
    reads.sort_by_key(|(_, offset)| *offset);
    let offsets: Vec<u64> = reads.iter().map(|(_, o)| *o).collect();
    assert_eq!(
        offsets,
        [u64::from(LEN), 2 * u64::from(LEN)],
        "each resent READ once"
    );
    assert!(
        reads.iter().all(|(unique, _)| unique & UNIQUE_RESEND != 0),
        "{reads:?}"
    );

    mounted.0.take().unwrap().unmount(false).unwrap();
    assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Unmounted; READERS]);
    drop(kept);
}
