//! INTERRUPT in io_uring mode: held for a ring request not delivered yet (K11).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a unit test: a failed unwrap or expect is the failure it reports"
)]

use super::*;

#[test]
fn an_interrupt_on_dev_fuse_for_a_ring_request_not_yet_delivered_is_held_and_answers_it_once() {
    let r = rig(host(1), depth(1), offered_uring());
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    // The kernel refuses an EAGAIN for a ring request (it is on no clone): a device whose
    // writes are refused stands for that.
    r.dev.shutdown(std::net::Shutdown::Read).unwrap();
    (&r.dev)
        .write_all(&request(INTERRUPT, 101, 0, &100u64.to_le_bytes()))
        .unwrap();
    let tokens = serving.tokens().clone();
    while tokens.held() == 0 {
        std::thread::yield_now();
    }
    r.kernel.deliver(0, 40, request(READ, 100, 2, &[0; 40]));
    let s = r.kernel.wait(|s| !s.replies.is_empty());
    assert_eq!(
        (s.replies[0].unique, s.replies[0].error),
        (100, -4),
        "EINTR"
    );
    drop(s);
    let parked = fs.take_parked(1);
    let replier = parked.into_iter().next().unwrap();
    assert!(replier.cancel_token().is_cancelled());
    assert!(matches!(
        replier.ok(&[b"late"]),
        Completion::AlreadyAnswered
    ));
    assert_eq!(tokens.held(), 0);
    serving.stopper().stop();
    serving.join().unwrap();
    assert_eq!(r.kernel.lock().replies.len(), 1, "answered once");
}

#[test]
fn an_interrupt_of_a_delivered_ring_request_answers_it_eintr_by_a_commit() {
    let r = rig(host(1), depth(1), offered_uring());
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    r.kernel.deliver(0, 40, request(READ, 110, 2, &[0; 40]));
    let parked = fs.take_parked(1);
    (&r.dev)
        .write_all(&request(INTERRUPT, 111, 0, &110u64.to_le_bytes()))
        .unwrap();
    let s = r.kernel.wait(|s| !s.replies.is_empty());
    assert_eq!((s.replies[0].unique, s.replies[0].error), (110, -4));
    drop(s);
    let replier = parked.into_iter().next().unwrap();
    assert!(matches!(replier.ok(&[b"x"]), Completion::AlreadyAnswered));
    serving.stopper().stop();
    serving.join().unwrap();
}

/// A device that takes every reply, or refuses each; it may register a request first.
struct Dev {
    refuse: bool,
    sent: Mutex<Vec<Vec<u8>>>,
    register_first: Mutex<Option<(Tokens, u64, Arc<Dev>)>>,
}

impl Dev {
    fn new(refuse: bool) -> Arc<Self> {
        Arc::new(Self {
            refuse,
            sent: Mutex::new(Vec::new()),
            register_first: Mutex::new(None),
        })
    }

    fn errors(&self) -> Vec<(u64, i32)> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    u64::from_le_bytes(m[8..16].try_into().unwrap()),
                    i32::from_le_bytes(m[4..8].try_into().unwrap()),
                )
            })
            .collect()
    }
}

impl ReplyDevice for Dev {
    fn send(&self, parts: &[&[u8]]) -> io::Result<()> {
        if let Some((tokens, unique, device)) = self.register_first.lock().unwrap().take() {
            std::mem::forget(tokens.register(unique, device));
        }
        if self.refuse {
            return Err(io::Error::from_raw_os_error(2));
        }
        self.sent.lock().unwrap().push(parts.concat());
        Ok(())
    }
}

#[test]
fn on_dev_fuse_an_unmatched_interrupt_is_never_held() {
    let tokens = Tokens::new();
    let refusing = Dev::new(true);
    let routed = tokens.interrupt_routed(5, 6, &*refusing);
    assert!(matches!(routed, Some(Interrupted::DeviceError(_))));
    assert_eq!(tokens.held(), 0);
    let taking = Dev::new(false);
    assert!(matches!(
        tokens.interrupt_routed(5, 7, &*taking),
        Some(Interrupted::Unknown)
    ));
    assert_eq!(taking.errors(), vec![(7, -11)]);
}

#[test]
fn in_uring_mode_an_interrupt_no_device_takes_is_held_until_its_request_registers() {
    let tokens = Tokens::new();
    tokens.set_uring(true);
    let refusing = Dev::new(true);
    assert!(tokens.interrupt_routed(5, 6, &*refusing).is_none());
    // The same INTERRUPT again: still one hold, one EINTR.
    assert!(tokens.interrupt_routed(5, 8, &*refusing).is_none());
    assert_eq!(tokens.held(), 1);
    let ring = Dev::new(false);
    let replier = tokens.register(5, ring.clone());
    assert_eq!(
        ring.errors(),
        vec![(5, -4)],
        "EINTR on the request's own device"
    );
    assert!(replier.cancel_token().is_cancelled());
    assert_eq!(tokens.registered(), 0);
    assert_eq!(tokens.held(), 0);
    assert!(matches!(replier.ok(&[b"x"]), Completion::AlreadyAnswered));
    assert_eq!(
        ring.errors().len(),
        1,
        "answered once; the drop writes nothing"
    );
    // A request registered without a hold is answered normally.
    let other = tokens.register(9, ring.clone());
    assert!(matches!(other.ok(&[b"y"]), Completion::Written));
    assert_eq!(ring.errors().len(), 2);
}

#[test]
fn in_uring_mode_an_interrupt_taken_on_a_device_or_of_a_registered_request_is_not_held() {
    let tokens = Tokens::new();
    tokens.set_uring(true);
    let taking = Dev::new(false);
    assert!(matches!(
        tokens.interrupt_routed(5, 6, &*taking),
        Some(Interrupted::Unknown)
    ));
    let ring = Dev::new(false);
    let r = tokens.register(7, ring.clone());
    assert!(matches!(
        tokens.interrupt_routed(7, 8, &*taking),
        Some(Interrupted::Cancelled)
    ));
    assert_eq!(ring.errors(), vec![(7, -4)]);
    assert!(matches!(r.ok(&[]), Completion::AlreadyAnswered));
    assert_eq!(tokens.held(), 0);
    tokens.set_uring(false);
    assert!(matches!(
        tokens.interrupt_routed(1, 2, &*Dev::new(true)),
        Some(Interrupted::DeviceError(_))
    ));
}

#[test]
fn a_request_registered_while_its_interrupt_was_refused_is_cancelled_not_held() {
    let tokens = Tokens::new();
    tokens.set_uring(true);
    let ring = Dev::new(false);
    let refusing = Dev::new(true);
    *refusing.register_first.lock().unwrap() = Some((tokens.clone(), 5, ring.clone()));
    assert!(matches!(
        tokens.interrupt_routed(5, 6, &*refusing),
        Some(Interrupted::Cancelled)
    ));
    assert_eq!(ring.errors(), vec![(5, -4)]);
    assert_eq!(tokens.held(), 0);
}

#[test]
fn past_the_cap_the_oldest_held_interrupt_is_dropped() {
    let tokens = Tokens::new();
    tokens.set_uring(true);
    let refusing = Dev::new(true);
    for target in 1..=(MAX_HELD as u64 + 1) {
        assert!(tokens.interrupt_routed(target, 0, &*refusing).is_none());
    }
    assert_eq!(tokens.held(), MAX_HELD);
    let ring = Dev::new(false);
    let oldest = tokens.register(1, ring.clone());
    assert!(ring.errors().is_empty(), "the oldest hold was dropped");
    assert!(matches!(oldest.ok(&[]), Completion::Written));
    let newest = tokens.register(MAX_HELD as u64 + 1, ring.clone());
    assert!(newest.cancel_token().is_cancelled());
    std::mem::forget(newest);
}
