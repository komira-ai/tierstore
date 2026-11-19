//! The dispatch of a served session (5.2, 5.4): a request is registered when it is read, before
//! `Fs::handle`, and its replier is answered later by a task, on the reader that read it; an
//! INTERRUPT read by another reader cancels the task and is answered once; a parked reply
//! completes while every reader is blocked in an inline operation.
//!
//! The devices are socket pairs (`common::fake`), one per reader, as clones are several
//! descriptors of one connection; the kernel's own refusal of a reply on the wrong clone is the
//! subject of `kernel_dispatch_integration.rs`.
//!
//! Catches:
//! - a parked reply completed by a task written to another reader's device;
//! - an INTERRUPT read by another reader that does not reach the parked request's task, or that
//!   writes EINTR anywhere but the request's own reader;
//! - an INTERRUPT delivered twice that answers the request twice;
//! - a parked reply that needs a free reader to be written (it would wait behind readers blocked
//!   in an inline `pread`).
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod common;

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};

use common::fake::{Kernel, fake_device, pending, read_message, read_reply};
use common::{getattr, init_request, request};
use tokio::sync::oneshot;
use tsfuse::abi::{InitConfig, InterruptIn};
use tsfuse::{Completion, DevFd, Fs, Handled, Handover, ReaderEnd, Replier, Request, Session};

/// The terms of a session initialised by a 7.40 kernel.
fn negotiated() -> tsfuse::abi::Negotiated {
    let (device, kernel) = fake_device();
    kernel.send(&init_request(7, 40, 0)).unwrap();
    let session = Session::init(device, &InitConfig::tree()).unwrap();
    assert_eq!(read_reply(&kernel).0, 0);
    *session.negotiated()
}

/// The session of `devices`, resumed with the terms of [`negotiated`] as a supervisor's handover
/// gives them.
fn resume(devices: Vec<DevFd>) -> Session {
    let fds = devices.into_iter().map(OwnedFd::from).collect();
    Session::resume(Handover::new(negotiated(), fds).unwrap()).unwrap()
}

/// `readers` devices and the kernel's end of each.
fn devices(readers: usize) -> (Vec<DevFd>, Vec<Kernel>) {
    (0..readers).map(|_| fake_device()).unzip()
}

/// A state the test and the readers share, and the condition variable a waiter watches it with.
struct Shared<T> {
    state: Mutex<T>,
    changed: Condvar,
}

impl<T: Default> Shared<T> {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(T::default()),
            changed: Condvar::new(),
        })
    }

    /// Changes the state and wakes every waiter.
    fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let r = f(&mut self.state.lock().unwrap());
        self.changed.notify_all();
        r
    }

    /// Waits until `f` returns `Some`: on a change of the state, never on a timer.
    fn wait_for<R>(&self, mut f: impl FnMut(&mut T) -> Option<R>) -> R {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(r) = f(&mut state) {
                return r;
            }
            state = self.changed.wait(state).unwrap();
        }
    }
}

/// How each parked task ended: the unique, what woke it, and its completion.
type Ended = (u64, &'static str, Completion);

/// Parks every LOOKUP in a task of `runtime` that waits for its data (sent by the test through
/// the sender kept in `fills`) or for the request's cancellation, whichever comes first, and
/// reports how it ended; answers the rest `ENOSYS`.
struct AsyncFs {
    runtime: tokio::runtime::Handle,
    fills: Arc<Shared<BTreeMap<u64, oneshot::Sender<Vec<u8>>>>>,
    ended: Mutex<Sender<Ended>>,
}

impl Fs for AsyncFs {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        if req.header.opcode != 1 {
            let _ = replier.error(tsfuse::Errno::ENOSYS);
            return Handled::Done;
        }
        let unique = req.header.unique;
        let (fill, filled) = oneshot::channel::<Vec<u8>>();
        let token = replier.cancel_token();
        let ended = self.ended.lock().unwrap().clone();
        self.runtime.spawn(async move {
            let end = tokio::select! {
                _ = token.cancelled() => (unique, "cancelled", replier.ok(&[b"late"])),
                data = filled => (unique, "filled", replier.ok(&[&data.unwrap()])),
            };
            ended.send(end).unwrap();
        });
        self.fills.update(|f| f.insert(unique, fill));
        Handled::Parked
    }
}

#[test]
fn parked_requests_answered_by_tasks_go_to_their_readers_and_an_interrupt_is_answered_once() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (ended_tx, ended): (Sender<Ended>, Receiver<Ended>) = channel();
    let fs = Arc::new(AsyncFs {
        runtime: runtime.handle().clone(),
        fills: Shared::new(),
        ended: Mutex::new(ended_tx),
    });
    let (devs, kernels) = devices(3);
    let session = resume(devs);
    let tokens = session.tokens().clone();
    let serving = session.serve(fs.clone()).unwrap();

    // A LOOKUP read by each reader; each parks in a task.
    for (i, kernel) in kernels.iter().enumerate() {
        kernel
            .send(&request(1, 100 + 2 * i as u64, 1, b"x\0"))
            .unwrap();
    }
    fs.fills.wait_for(|f| (f.len() == 3).then_some(()));
    assert_eq!(tokens.registered(), 3);

    // An INTERRUPT of reader 0's request, read by reader 2: EINTR on reader 0's device, and the
    // task sees the cancel; its late reply writes nothing.
    let interrupt = request(36, 100 | 1, 0, InterruptIn { unique: 100 }.as_bytes());
    kernels[2].send(&interrupt).unwrap();
    assert_eq!(read_reply(&kernels[0]), (-4, 100, 16));
    let (unique, woken_by, done) = ended.recv().unwrap();
    assert_eq!((unique, woken_by), (100, "cancelled"));
    assert!(matches!(done, Completion::AlreadyAnswered), "{done:?}");
    // The same INTERRUPT again, read by reader 1: the request is answered, so it is EAGAIN, on
    // the first device that takes it (reader 0's here; the kernel would refuse it on every
    // device, the request being gone), and never a second EINTR.
    kernels[1].send(&interrupt).unwrap();
    assert_eq!(read_reply(&kernels[0]), (-11, 100 | 1, 16));

    // The data of the other two arrive out of order; each reply goes to its own reader.
    for unique in [104, 102] {
        let fill = fs.fills.update(|f| f.remove(&unique)).unwrap();
        fill.send(vec![unique as u8; 3]).unwrap();
        let (got, woken_by, done) = ended.recv().unwrap();
        assert_eq!((got, woken_by), (unique, "filled"));
        assert!(matches!(done, Completion::Written), "{done:?}");
    }
    assert_eq!(read_message(&kernels[2]), (0, 104, vec![104; 3]));
    assert_eq!(read_message(&kernels[1]), (0, 102, vec![102; 3]));
    for kernel in &kernels {
        assert!(!pending(kernel), "each request was answered once");
    }
    assert_eq!(tokens.registered(), 0);

    serving.stopper().stop();
    assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Stopped; 3]);
}

/// What [`BlockingFs`] has seen.
#[derive(Default)]
struct Blocking {
    parked: Vec<Replier>,
    blocked: usize,
    open: bool,
}

/// Parks LOOKUP; blocks the reader in GETATTR (an inline `pread` that stalls) until the test
/// opens the gate, then answers it.
struct BlockingFs(Arc<Shared<Blocking>>);

impl Fs for BlockingFs {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        if req.header.opcode == 1 {
            self.0.update(|s| s.parked.push(replier));
            return Handled::Parked;
        }
        self.0.update(|s| s.blocked += 1);
        self.0.wait_for(|s| s.open.then_some(()));
        let _ = replier.ok(&[b"inline"]);
        Handled::Done
    }
}

#[test]
fn a_parked_reply_completes_while_every_reader_is_blocked_inline() {
    let shared: Arc<Shared<Blocking>> = Shared::new();
    let (devs, kernels) = devices(2);
    let session = resume(devs);
    let serving = session.serve(Arc::new(BlockingFs(shared.clone()))).unwrap();

    kernels[0].send(&request(1, 10, 1, b"x\0")).unwrap();
    let replier = shared.wait_for(|s| s.parked.pop());
    // Both readers stall in an inline operation.
    kernels[0].send(&getattr(12)).unwrap();
    kernels[1].send(&getattr(14)).unwrap();
    shared.wait_for(|s| (s.blocked == 2).then_some(()));

    // The parked reply is written from another thread while no reader is free.
    let completer = std::thread::spawn(move || replier.ok(&[b"parked"]));
    assert!(matches!(completer.join().unwrap(), Completion::Written));
    assert_eq!(read_message(&kernels[0]), (0, 10, b"parked".to_vec()));
    assert_eq!(
        shared.update(|s| (s.blocked, s.open)),
        (2, false),
        "both readers were still blocked"
    );

    shared.update(|s| s.open = true);
    assert_eq!(read_message(&kernels[0]), (0, 12, b"inline".to_vec()));
    assert_eq!(read_message(&kernels[1]), (0, 14, b"inline".to_vec()));
    for kernel in &kernels {
        assert!(!pending(kernel));
    }
    serving.stopper().stop();
    assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Stopped; 2]);
}
