//! Kernel conformance, multi-reader behaviour of a served session (5.2 items 4 (the buffered-READ
//! case), 5 and 6, "Kernel conformance tests, on a farm node" of `docs/design/fetch-cache-serve.md`):
//! a mount served by [`Session::serve`] with four cloned readers and [`Model`] as the [`Fs`].
//!
//! - **Item 6, N readers under concurrent load.**
//!   [`eight_callers_making_sixteen_direct_reads_each_get_exactly_their_bytes_from_four_readers`]:
//!   eight caller threads make sixteen direct `pread`s each, at distinct offsets and lengths of two
//!   4 MiB files. Half the READs are answered inline and half are parked and completed from tasks
//!   of a multi-thread runtime, in the reverse of the order they arrived. Every caller gets exactly
//!   its bytes, the file system saw 128 READs at 128 distinct `(ino, offset)`, every completion was
//!   accepted by the kernel (none lost, none `AlreadyAnswered`, none `DeviceError`), and no request
//!   stays registered once the readers end.
//! - **Item 6, stopping ends all readers.**
//!   [`stopping_ends_every_reader_and_leaves_parked_replies_answerable_on_their_own_device`]: with a
//!   READ parked on each of the four readers, [`Stopper::stop`] ends every reader as
//!   `ReaderEnd::Stopped`, the mount stays in the mount table, and each parked replier completed
//!   afterwards is accepted by the kernel (a replier holds its originating device, K1) and its
//!   caller gets its bytes. [`a_stop_made_while_a_reader_is_blocked_inline_ends_it_once_its_handler_returns`]:
//!   a stop while one reader is inside `Fs::handle` ends all four once the handler returns, and the
//!   inline answer of that handler still reaches its caller.
//! - **Item 4, the buffered-READ case (K8).**
//!   [`a_killed_reader_of_a_buffered_read_ends_with_no_interrupt_and_the_read_stays_parked`]: in
//!   the tree mount's own mode (`ENOSYS` OPEN under `NO_OPEN_SUPPORT`, `ASYNC_READ` granted) a READ
//!   of a buffered read has no waiting caller. A child process (this binary) is killed with
//!   SIGKILL while its READ is parked: it ends by signal 9 with the READ still parked, the READ's
//!   cancel token is never cancelled (no INTERRUPT reached the session), the request is still
//!   registered, and the late completion is accepted by the kernel.
//!
//! The rest of item 4 (an INTERRUPT answered `EINTR` once on the request's own clone, for a direct
//! READ whose caller is killed) and the four-clone out-of-order completion of item 5 are proved
//! through the library by `kernel_dispatch_integration.rs`
//! (`an_interrupt_read_by_another_clone_answers_each_parked_read_eintr_once`,
//! `reads_parked_on_four_clones_and_completed_by_tasks_out_of_order_reach_their_callers`).
//!
//! K-facts relied on: K1 (a reply is taken only on the clone that read the request), K5 (closing
//! every descriptor ends a request still waiting with `ECONNABORTED`, which frees a caller that
//! closes its file after the readers are gone), K8 (a buffered READ has no waiter and no INTERRUPT).
//! Every wait is on an event (a request parked, a caller's system call returning, a child ending),
//! never on a timer. An integration target: it mounts through `mount(2)` as root (the farm) or
//! `fusermount3` (CI), and says on stderr why it does not run where neither can.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod conformance;
mod kernel;

use std::collections::HashSet;
use std::fs::File;
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use conformance::{
    Done, Events, Mounted, Open, Opendir, Parked, ROOT, TreeSpec, child, memfd, park, pattern,
    serve,
};
use tsfuse::abi::InitFlags;
use tsfuse::{CancelToken, Completion, Handled, ReaderEnd};

use kernel::skip;

/// The readers (descriptors) of every session here: the mount's and three clones.
const READERS: usize = 4;
/// The size of each file of the load test.
const FILE_SIZE: u64 = 4 << 20;

/// A tree of the files `a` and `b`, each `size` bytes of its own pattern; their inodes.
fn two_files(size: u64) -> (Arc<TreeSpec>, [u64; 2]) {
    let mut tree = TreeSpec::new();
    let a = tree.file(
        ROOT,
        "a",
        size,
        Some(memfd("a", &pattern(1, size as usize))),
    );
    let b = tree.file(
        ROOT,
        "b",
        size,
        Some(memfd("b", &pattern(2, size as usize))),
    );
    (Arc::new(tree), [a, b])
}

/// The offset of read `idx` of the load test: distinct per read, and `offset / SPACING` names the
/// read.
const SPACING: u64 = 20_000;

/// The length of read `idx` of the load test: from 1000 to 9999 bytes.
fn load_len(idx: u64) -> usize {
    (1000 + idx * 37 % 9000) as usize
}

/// Counts a finished caller thread when dropped (also when it panics), so the completer never
/// waits for reads that will not come.
struct Finished(Arc<Events>);

impl Drop for Finished {
    fn drop(&mut self) {
        self.0
            .update(|s| *s.counts.entry("callers").or_default() += 1);
    }
}

#[test]
fn eight_callers_making_sixteen_direct_reads_each_get_exactly_their_bytes_from_four_readers() {
    const CALLERS: usize = 8;
    const PER_CALLER: usize = 16;
    let Some((mounted, session)) = Mounted::new("tsfuse-load-readers", READERS) else {
        return;
    };
    let (tree, inos) = two_files(FILE_SIZE);
    let served = serve(
        session,
        tree.clone(),
        Open::Flags(conformance::FOPEN_DIRECT_IO),
        Opendir::Ok,
        |tree, read, replier, events| {
            // Even reads are answered inline, odd ones parked.
            if (read.offset / SPACING) % 2 == 1 {
                return park(read, replier, events);
            }
            let bytes = tree.bytes(read.ino, read.offset, u64::from(read.size));
            let done = Done::from(&replier.ok(&[&bytes]));
            events.update(|s| s.completions.push((read.unique, done, false)));
            Handled::Done
        },
    );
    let events = served.events.clone();

    let callers: Vec<_> = (0..CALLERS)
        .map(|t| {
            let (events, path) = (events.clone(), mounted.path(["a", "b"][t % 2]));
            thread::spawn(move || {
                let _finished = Finished(events);
                let file = File::open(path).unwrap();
                (0..PER_CALLER)
                    .map(|i| {
                        let idx = (t * PER_CALLER + i) as u64;
                        let mut buf = vec![0u8; load_len(idx)];
                        let n = file.read_at(&mut buf, idx * SPACING).unwrap();
                        buf.truncate(n);
                        (idx as usize, buf)
                    })
                    .collect()
            })
        })
        .collect();

    // Completed from tasks, each batch in the reverse of the order it was parked in.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let mut completed_parked = 0;
    loop {
        let batch: Vec<Parked> = events.wait_for(|s| {
            if !s.parked.is_empty() {
                Some(std::mem::take(&mut s.parked))
            } else if s.counts.get("callers") == Some(&CALLERS) {
                Some(Vec::new())
            } else {
                None
            }
        });
        if batch.is_empty() {
            break;
        }
        completed_parked += batch.len();
        let mut batch = batch;
        batch.sort_by_key(|p| std::cmp::Reverse(p.read.unique));
        let tasks: Vec<_> = batch
            .into_iter()
            .map(|p| {
                let (tree, events) = (tree.clone(), events.clone());
                runtime.spawn(async move {
                    tokio::task::yield_now().await;
                    let r = p.read;
                    let bytes = tree.bytes(r.ino, r.offset, u64::from(r.size));
                    let done = Done::from(&p.replier.ok(&[&bytes]));
                    events.update(|s| s.completions.push((r.unique, done, false)));
                })
            })
            .collect();
        for task in tasks {
            runtime.block_on(task).unwrap();
        }
    }

    for (t, caller) in callers.into_iter().enumerate() {
        let reads: Vec<(usize, Vec<u8>)> = caller.join().unwrap();
        for (idx, got) in reads {
            let (ino, offset) = (inos[t % 2], idx as u64 * SPACING);
            let len = load_len(idx as u64) as u64;
            assert_eq!(got, tree.bytes(ino, offset, len), "caller {t}, read {idx}");
        }
    }
    assert_eq!(completed_parked, CALLERS * PER_CALLER / 2);
    // An inline answer records its completion after the reply is written, so a caller can see
    // its bytes first: wait for all 128.
    events.wait_for(|s| (s.completions.len() >= CALLERS * PER_CALLER).then_some(()));
    let (reads, completions) = events.with(|s| (s.reads.clone(), s.completions.clone()));
    assert_eq!(events.with(|s| s.count(conformance::op::READ)), 128);
    assert_eq!(reads.len(), 128);
    let distinct: HashSet<(u64, u64)> = reads.iter().map(|r| (r.ino, r.offset)).collect();
    assert_eq!(
        distinct.len(),
        128,
        "every READ is a distinct (ino, offset)"
    );
    assert_eq!(completions.len(), 128);
    let uniques: HashSet<u64> = completions.iter().map(|c| c.0).collect();
    assert_eq!(uniques.len(), 128, "each READ was completed once");
    let not_written: Vec<_> = completions
        .iter()
        .filter(|c| c.1 != Done::Written)
        .collect();
    assert!(
        not_written.is_empty(),
        "completions the kernel refused: {not_written:?}"
    );

    let tokens = served.tokens.clone();
    mounted.unmount();
    assert_eq!(served.join(), vec![ReaderEnd::Unmounted; READERS]);
    // Checked once every reader has ended: another process may stat the new mount at any time.
    assert_eq!(tokens.registered(), 0);
}

#[test]
fn stopping_ends_every_reader_and_leaves_parked_replies_answerable_on_their_own_device() {
    let Some((mounted, session)) = Mounted::new("tsfuse-load-stop", READERS) else {
        return;
    };
    let (tree, inos) = two_files(1 << 20);
    let barrier = Arc::new(Barrier::new(READERS));
    let served = serve(
        session,
        tree.clone(),
        Open::Flags(conformance::FOPEN_DIRECT_IO),
        Opendir::Ok,
        move |_, read, replier, events| {
            let handled = park(read, replier, events);
            // Each reader holds its READ until all four hold one, so no reader reads two.
            barrier.wait();
            handled
        },
    );
    let events = served.events.clone();
    let tokens = served.tokens.clone();

    let callers: Vec<_> = (0..READERS as u64)
        .map(|i| {
            (
                i * 65_536 + 100,
                conformance::pread(mounted.path("a"), i * 65_536 + 100, 4096),
            )
        })
        .collect();
    let parked =
        events.wait_for(|s| (s.parked.len() == READERS).then(|| std::mem::take(&mut s.parked)));
    let readers: HashSet<&str> = parked.iter().map(|p| p.reader.as_str()).collect();
    assert_eq!(
        readers.len(),
        READERS,
        "each READ is parked by its own reader"
    );

    served.serving().stopper().stop();
    assert_eq!(served.join(), vec![ReaderEnd::Stopped; READERS]);
    assert!(
        mounted.entry().is_some(),
        "a stop leaves the mount in the table"
    );
    assert_eq!(
        tokens.registered(),
        READERS,
        "the parked requests are still registered"
    );

    for p in parked {
        let r = p.read;
        let bytes = tree.bytes(r.ino, r.offset, u64::from(r.size));
        let done = p.replier.ok(&[&bytes]);
        assert!(
            matches!(done, Completion::Written),
            "reply to {}: {done:?}",
            r.unique
        );
    }
    for (offset, caller) in callers {
        assert_eq!(
            caller.bytes(),
            tree.bytes(inos[0], offset, 4096),
            "offset {offset}"
        );
        caller.join();
    }
    assert_eq!(tokens.registered(), 0);
    mounted.unmount();
}

#[test]
fn a_stop_made_while_a_reader_is_blocked_inline_ends_it_once_its_handler_returns() {
    let Some((mounted, session)) = Mounted::new("tsfuse-load-stop-inline", READERS) else {
        return;
    };
    let (tree, inos) = two_files(1 << 20);
    let served = serve(
        session,
        tree.clone(),
        Open::Flags(conformance::FOPEN_DIRECT_IO),
        Opendir::Ok,
        |tree, read, replier, events| {
            events.update(|s| *s.counts.entry("inside").or_default() += 1);
            // The reader waits inside the handler until the test opens the gate.
            events.wait_for(|s| s.counts.get("open").map(|_| ()));
            let bytes = tree.bytes(read.ino, read.offset, u64::from(read.size));
            let _ = replier.ok(&[&bytes]);
            Handled::Done
        },
    );
    let events = served.events.clone();
    let caller = conformance::pread(mounted.path("b"), 4096, 8192);
    events.wait_for(|s| s.counts.get("inside").map(|_| ()));

    served.serving().stopper().stop();
    events.update(|s| *s.counts.entry("open").or_default() += 1);
    assert_eq!(served.join(), vec![ReaderEnd::Stopped; READERS]);
    assert_eq!(caller.bytes(), tree.bytes(inos[1], 4096, 8192));
    caller.join();
    mounted.unmount();
}

/// The name of [`child_reads_one_buffered_range_of_the_file_named_on_stdin`].
const BUFFERED_CHILD: &str = "child_reads_one_buffered_range_of_the_file_named_on_stdin";

/// Not a check of its own: the child process of
/// [`a_killed_reader_of_a_buffered_read_ends_with_no_interrupt_and_the_read_stays_parked`]. It
/// reads the path of a file of the mount from its standard input and reads 4 KiB at offset 0 with
/// one `pread` of a file opened in the tree mount's mode (a buffered read); the parent kills it
/// while the READ is parked.
#[test]
#[ignore = "the child process of a_killed_reader_of_a_buffered_read_ends_with_no_interrupt_and_the_read_stays_parked, which runs it with --ignored"]
fn child_reads_one_buffered_range_of_the_file_named_on_stdin() {
    let mut path = String::new();
    std::io::stdin().read_line(&mut path).unwrap();
    let file = File::open(path.trim_end()).unwrap();
    let mut buf = vec![0u8; 4096];
    let _ = file.read_at(&mut buf, 0);
}

#[test]
fn a_killed_reader_of_a_buffered_read_ends_with_no_interrupt_and_the_read_stays_parked() {
    let Some((mounted, session)) = Mounted::new("tsfuse-load-buffered", READERS) else {
        return;
    };
    if !session.negotiated().granted.contains(InitFlags::ASYNC_READ) {
        skip("the kernel did not offer ASYNC_READ, so a buffered READ has no waiting caller");
        return;
    }
    let (tree, inos) = two_files(1 << 20);
    let tokens_seen: Arc<Mutex<Vec<CancelToken>>> = Arc::default();
    let kept = tokens_seen.clone();
    let served = serve(
        session,
        tree.clone(),
        Open::Enosys,
        Opendir::Enosys,
        move |_, read, replier, events| {
            kept.lock().unwrap().push(replier.cancel_token());
            park(read, replier, events)
        },
    );
    let events = served.events.clone();
    let tokens = served.tokens.clone();

    let mut child = child(BUFFERED_CHILD)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let path: PathBuf = mounted.path("a");
    writeln!(child.stdin.take().unwrap(), "{}", path.display()).unwrap();
    events.wait_for(|s| (!s.parked.is_empty()).then_some(()));

    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(
        status.signal(),
        Some(9),
        "the child ended by SIGKILL: {status:?}"
    );
    // A round trip through the readers after the child is gone: an INTERRUPT queued by its death
    // is read before it (the kernel hands out INTERRUPTs first). Another reader may still be
    // routing that INTERRUPT, so its absence is checked once every reader has ended, below.
    rustix::fs::statfs(&mounted.mp).unwrap();

    let parked = events.update(|s| std::mem::take(&mut s.parked));
    assert!(!parked.is_empty(), "the READ is still parked");
    assert!(
        tokens.registered() >= 1,
        "the parked request is still registered"
    );
    for p in parked {
        let r = p.read;
        assert_eq!(r.ino, inos[0]);
        let bytes = tree.bytes(r.ino, r.offset, u64::from(r.size));
        let done = p.replier.ok(&[&bytes]);
        assert!(
            matches!(done, Completion::Written),
            "late reply to {}: {done:?}",
            r.unique
        );
    }
    mounted.unmount();
    assert_eq!(served.join(), vec![ReaderEnd::Unmounted; READERS]);
    assert_eq!(tokens.registered(), 0);
    // Every reader has ended, so every INTERRUPT the kernel queued has been routed: a cancelled
    // token would be the one that INTERRUPT found (nothing else cancels a token).
    for token in tokens_seen.lock().unwrap().iter() {
        assert!(!token.is_cancelled(), "no INTERRUPT reached the session");
    }
}
