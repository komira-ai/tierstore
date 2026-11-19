//! Serving a session on its rings (5.2 item 4): one thread per queue, pinned to that queue's CPU
//! where the kernel allows it, each running the queue's own current-thread runtime and reader
//! (made before INIT was answered, [`super::make_rings`]); registration in two phases; then the
//! shared reader loop on every queue.
//!
//! **Registration.** Every queue's thread registers its entries when told to: first every queue
//! but the last, then, once all of those succeeded, the last. The kernel makes the ring ready
//! (sends requests to it) when every queue has an entry, so the ring cannot be ready before the
//! last phase.
//!
//! **A failure leaves the session on `/dev/fuse`, never wedged.** A connection granted io_uring
//! holds its requests in the kernel until the ring is ready (K10), so a queue that does not
//! register would leave the mount waiting for ever. A REGISTER the kernel refuses turns io_uring
//! off for the connection (`fc->io_uring = 0`) and lets those requests flow on `/dev/fuse`; so on
//! any failure (the kernel's refusal, or a ring that could not submit) every queue's thread sends
//! a REGISTER the kernel refuses (a queue past the last, [`UringReader::poison`]) and ends,
//! dropping its ring, and the session serves on its `/dev/fuse` readers alone.

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use super::{Fallback, Prepared, Rings, Shared, TransportMode, reader_buffer};
use crate::fs::Fs;
use crate::session::SessionError;
use crate::tokens::Tokens;
use crate::transport::{Every, ReaderEnd, Stop, serve};

/// The readers a session's rings started, and the transport it serves on.
pub(crate) struct Started {
    /// One per queue (none after a fallback: those threads have ended).
    pub(crate) handles: Vec<JoinHandle<io::Result<ReaderEnd>>>,
    /// What each queue shares (none after a fallback).
    pub(crate) queues: Vec<Arc<Shared>>,
    pub(crate) mode: TransportMode,
}

/// What the session tells a queue's thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    /// Register every entry, and report.
    Register,
    /// Send a REGISTER of this queue number, past the last, which the kernel refuses; then end.
    Poison(u16),
    /// Serve.
    Serve,
}

/// What a queue's thread needs.
struct Queue {
    qid: u16,
    prepared: Prepared,
    buf_len: usize,
    fs: Arc<dyn Fs>,
    tokens: Tokens,
    stop: Arc<Stop>,
    every: Arc<Every>,
}

/// A queue's thread as the session drives it.
struct Running {
    orders: Sender<Order>,
    handle: JoinHandle<io::Result<ReaderEnd>>,
}

/// Starts a thread per queue of `rings` and registers every queue; `read_buffer` is the
/// `/dev/fuse` readers' buffer and `first_index` the index of the first queue's reader. Every
/// ring request is routed as `/dev/fuse` routes a request, and INTERRUPTs whose request no
/// `/dev/fuse` reader holds are held for the rings.
///
/// # Errors
///
/// [`SessionError::Start`] if a thread cannot be started (every reader is then stopped, and the
/// queues started end without registering).
pub(crate) fn start(
    rings: Rings,
    fs: &Arc<dyn Fs>,
    tokens: &Tokens,
    stop: &Arc<Stop>,
    every: &Arc<Every>,
    read_buffer: usize,
    first_index: usize,
) -> Result<Started, SessionError> {
    let Rings { memory, queues } = rings;
    let buf_len = reader_buffer(read_buffer, &memory);
    let count = queues.len();
    let past_last = u16::try_from(count).unwrap_or(u16::MAX);
    let (report_tx, report) = channel::<(u16, io::Result<()>)>();
    let mut running: Vec<Running> = Vec::with_capacity(count);
    let mut shared = Vec::with_capacity(count);
    for prepared in queues {
        let qid = prepared.shared.qid();
        shared.push(prepared.shared.clone());
        let (orders, orders_rx) = channel();
        let queue = Queue {
            qid,
            prepared,
            buf_len,
            fs: fs.clone(),
            tokens: tokens.clone(),
            stop: stop.clone(),
            every: every.clone(),
        };
        let report_tx = report_tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("tsfuse-uring-{qid}"))
            .spawn(move || queue.run(&orders_rx, &report_tx));
        // On a failure, the queues started so far see their orders end, and end without
        // registering.
        let started = running.len();
        let handle = spawned.map_err(|source| stop.failed_start(first_index + started, source))?;
        running.push(Running { orders, handle });
    }
    drop(report_tx);
    let last = count.saturating_sub(1);
    let refused = register_all(&running, &report, last, tokens);
    if let Some((qid, errno)) = refused {
        tokens.set_uring(false);
        let _ = ask(&running, &report, Order::Poison(past_last));
        for queue in running {
            drop(queue.orders);
            let _ = queue.handle.join();
        }
        return Ok(Started {
            handles: Vec::new(),
            queues: Vec::new(),
            mode: TransportMode::DevFuse(Fallback::Register { qid, errno }),
        });
    }
    let mut handles = Vec::with_capacity(count);
    for queue in running {
        let _ = queue.orders.send(Order::Serve);
        handles.push(queue.handle);
    }
    Ok(Started {
        handles,
        queues: shared,
        mode: TransportMode::Uring(memory),
    })
}

/// Sends `order` to every queue of `running` and collects each report; the first failure.
fn ask(
    running: &[Running],
    report: &Receiver<(u16, io::Result<()>)>,
    order: Order,
) -> Option<(u16, i32)> {
    let asked = running
        .iter()
        .filter(|queue| queue.orders.send(order).is_ok())
        .count();
    let mut failed = None;
    for _ in 0..asked {
        // Every queue's thread ended, one without its report (it panicked): a failure.
        let ended = || {
            (
                u16::MAX,
                Err(io::Error::other("a queue ended without its report")),
            )
        };
        if let (qid, Err(e)) = report.recv().unwrap_or_else(|_| ended()) {
            failed.get_or_insert((qid, e.raw_os_error().unwrap_or(5)));
        }
    }
    failed
}

/// Registers every queue but `last`, then `last`; the first failure, if any. Sets the session's
/// INTERRUPT routing to io_uring mode before the last queue registers, since the ring may carry
/// requests from then on.
fn register_all(
    running: &[Running],
    report: &Receiver<(u16, io::Result<()>)>,
    last: usize,
    tokens: &Tokens,
) -> Option<(u16, i32)> {
    let (first, rest) = running.split_at(last.min(running.len()));
    if let Some(failed) = ask(first, report, Order::Register) {
        return Some(failed);
    }
    tokens.set_uring(true);
    ask(rest, report, Order::Register)
}

/// Pins the calling thread to CPU `cpu`, whose requests its queue carries; a CPU that is offline
/// or outside the thread's allowed set is left to the scheduler.
fn pin(cpu: u16) {
    let cpu = usize::from(cpu);
    let fits = cpu < rustix::thread::CpuSet::MAX_CPU;
    let mut set = rustix::thread::CpuSet::new();
    let _ = fits
        .then(|| set.set(cpu))
        .map(|()| rustix::thread::sched_setaffinity(None, &set));
}

impl Queue {
    /// The queue's thread: carries out each order, reporting the outcome of a REGISTER or a
    /// poison, until told to serve (it then serves until its reader ends) or until the orders end.
    fn run(
        self,
        orders: &Receiver<Order>,
        report: &Sender<(u16, io::Result<()>)>,
    ) -> io::Result<ReaderEnd> {
        pin(self.qid);
        let Self {
            qid,
            prepared,
            buf_len,
            fs,
            tokens,
            stop,
            every,
        } = self;
        let Prepared {
            mut reader,
            runtime,
            shared: _,
        } = prepared;
        runtime.block_on(async move {
            loop {
                match orders.recv() {
                    Ok(Order::Register) => {
                        let _ = report.send((qid, reader.register()));
                    }
                    Ok(Order::Poison(bad)) => {
                        let _ = report.send((qid, reader.poison(bad)));
                        return Ok(ReaderEnd::Stopped);
                    }
                    Ok(Order::Serve) => break,
                    Err(_) => return Ok(ReaderEnd::Stopped),
                }
            }
            let mut buf = vec![0u8; buf_len];
            serve(reader, &*fs, &tokens, &stop, &every, &mut buf).await
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "a unit test: a failed unwrap is the failure it reports"
    )]

    use super::*;
    use crate::uring::tests::{FakeKernel, FakeRing, TestFs};
    use crate::uring::{UringMemory, make_rings};

    /// The reports a queue's thread sends.
    type Reports = Receiver<(u16, io::Result<()>)>;

    /// A queue's thread, on a fake ring, with its orders and reports.
    fn queue() -> (
        Arc<FakeKernel>,
        Sender<Order>,
        Reports,
        JoinHandle<io::Result<ReaderEnd>>,
    ) {
        let kernel = Arc::new(FakeKernel::default());
        let k = kernel.clone();
        let rings = make_rings(UringMemory::of(1, 1, 8192), &mut |qid| {
            Ok(Box::new(FakeRing::new(qid, &k)) as Box<dyn super::super::Ring>)
        })
        .unwrap();
        let prepared = rings.queues.into_iter().next().unwrap();
        let queue = Queue {
            qid: 0,
            prepared,
            buf_len: 9000,
            fs: Arc::new(TestFs::default()),
            tokens: Tokens::new(),
            stop: Arc::new(Stop::default()),
            every: Arc::new(Every(Vec::new())),
        };
        let (orders, orders_rx) = channel();
        let (report_tx, report) = channel();
        let handle = std::thread::spawn(move || queue.run(&orders_rx, &report_tx));
        (kernel, orders, report, handle)
    }

    #[test]
    fn a_queue_whose_orders_end_before_it_is_told_to_serve_ends_stopped_unregistered() {
        let (kernel, orders, _report, handle) = queue();
        drop(orders);
        assert_eq!(handle.join().unwrap().unwrap(), ReaderEnd::Stopped);
        assert!(kernel.lock().registers.is_empty());
        let (kernel, orders, report, handle) = queue();
        orders.send(Order::Register).unwrap();
        assert!(report.recv().unwrap().1.is_ok());
        drop(orders);
        assert_eq!(handle.join().unwrap().unwrap(), ReaderEnd::Stopped);
        assert_eq!(kernel.lock().registers.len(), 1);
    }

    #[test]
    fn a_queue_told_to_poison_reports_the_refusal_and_ends() {
        let (kernel, orders, report, handle) = queue();
        orders.send(Order::Poison(1)).unwrap();
        let (qid, result) = report.recv().unwrap();
        assert_eq!(qid, 0);
        assert!(result.is_ok());
        assert_eq!(handle.join().unwrap().unwrap(), ReaderEnd::Stopped);
        assert_eq!(kernel.lock().poisoned, vec![(0, 1)]);
    }

    #[test]
    fn a_report_channel_that_ends_early_is_a_failure() {
        let (orders, orders_rx) = channel();
        let running = vec![Running {
            orders,
            handle: std::thread::spawn(move || {
                // Takes its order and ends without a report.
                let _ = orders_rx.recv();
                Ok(ReaderEnd::Stopped)
            }),
        }];
        let (report_tx, report) = channel::<(u16, io::Result<()>)>();
        drop(report_tx);
        assert_eq!(ask(&running, &report, Order::Register), Some((u16::MAX, 5)));
    }

    #[test]
    fn a_thread_is_pinned_to_its_cpu_only_when_the_set_holds_it() {
        // CPU 0 exists everywhere; a CPU past the set is left to the scheduler (no panic).
        pin(0);
        pin(u16::MAX);
    }
}
