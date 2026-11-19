//! REGISTER and COMMIT_AND_FETCH through a served session, and a queue's own ends.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a unit test: a failed unwrap or expect is the failure it reports"
)]

use super::*;

#[test]
fn every_queue_but_the_last_registers_first_then_the_last_with_the_right_buffers() {
    let r = rig(host(3), depth(2), offered_uring());
    let shown = format!("{:?}", r.session);
    assert!(
        shown.contains("rings: Some(Rings { memory: UringMemory { queues: 3, depth: 2,"),
        "{shown}"
    );
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs).unwrap();
    assert_eq!(
        serving.transport(),
        TransportMode::Uring(UringMemory::of(3, 2, 8192))
    );
    let s = r.kernel.lock();
    assert_eq!(s.registers.len(), 6, "3 queues x depth 2");
    let (first, last) = s.registers.split_at(4);
    assert!(first.iter().all(|t| t >> 32 < 2), "queues 0 and 1 first");
    assert_eq!(last, &[tag(2, 0), tag(2, 1)], "then the last queue");
    let mut registered = 0u64;
    for t in &s.registers {
        let b = &s.bufs[t];
        assert_eq!(b.header.len(), ENTRY_HEADER_LEN);
        assert_eq!(b.payload.len(), 8192);
        registered += (b.header.len() + b.payload.len()) as u64;
    }
    assert_eq!(
        registered,
        UringMemory::of(3, 2, 8192).bytes,
        "what is registered is what was accounted"
    );
    drop(s);
    let shown = format!("{serving:?}");
    assert!(shown.contains("Shared { qid: 2, held: 0, .. }"), "{shown}");
    serving.stopper().stop();
    let ends = serving.join().unwrap();
    assert_eq!(
        ends,
        vec![ReaderEnd::Stopped; 4],
        "the /dev/fuse reader and 3 queues"
    );
}

#[test]
fn a_refused_register_before_the_last_queue_leaves_the_session_on_dev_fuse() {
    let r = rig(host(3), depth(2), offered_uring());
    r.kernel.lock().refuse.insert(1, 95);
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    assert_eq!(
        serving.transport(),
        TransportMode::DevFuse(Fallback::Register { qid: 1, errno: 95 })
    );
    assert!(
        r.kernel.lock().registers.iter().all(|t| t >> 32 != 2),
        "the last queue never registered, so the ring was never ready"
    );
    let mut poisoned = r.kernel.lock().poisoned.clone();
    poisoned.sort_unstable();
    assert_eq!(
        poisoned,
        vec![(0, 3), (1, 3), (2, 3)],
        "every queue sent a REGISTER past the last, which turns io_uring off"
    );
    // The `/dev/fuse` reader serves, and an unknown INTERRUPT is answered EAGAIN there again.
    (&r.dev)
        .write_all(&request(GETATTR, 8, 1, &[0; 16]))
        .unwrap();
    let mut buf = [0u8; 256];
    let n = (&r.dev).read(&mut buf).unwrap();
    assert_eq!(
        &buf[16..n],
        [b"attr-".as_slice(), &8u64.to_le_bytes()].concat()
    );
    (&r.dev)
        .write_all(&request(INTERRUPT, 9, 0, &77u64.to_le_bytes()))
        .unwrap();
    let n = (&r.dev).read(&mut buf).unwrap();
    assert_eq!(n, 16);
    assert_eq!(i32::from_le_bytes(buf[4..8].try_into().unwrap()), -11);
    assert_eq!(serving.uring_entries_held(), 0);
    serving.stopper().stop();
    assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Stopped]);
}

#[test]
fn a_refused_register_of_the_last_queue_leaves_the_session_on_dev_fuse() {
    let r = rig(host(2), depth(1), offered_uring());
    r.kernel.lock().refuse.insert(1, 12);
    let serving = r.session.serve(Arc::new(TestFs::default())).unwrap();
    assert_eq!(
        serving.transport(),
        TransportMode::DevFuse(Fallback::Register { qid: 1, errno: 12 })
    );
    assert_eq!(r.kernel.lock().registers, vec![tag(0, 0), tag(1, 0)]);
    let mut poisoned = r.kernel.lock().poisoned.clone();
    poisoned.sort_unstable();
    assert_eq!(poisoned, vec![(0, 2), (1, 2)]);
    serving.stopper().stop();
    assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Stopped]);
}

#[test]
fn a_request_is_answered_by_a_commit_and_the_entry_fetches_the_next() {
    let r = rig(host(1), depth(1), offered_uring());
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    // Depth 1: the second request waits for the first one's commit, which frees the entry.
    r.kernel.deliver(0, 16, request(GETATTR, 10, 1, &[0; 16]));
    r.kernel.deliver(0, 16, request(GETATTR, 12, 1, &[0; 16]));
    let s = r.kernel.wait(|s| s.replies.len() == 2);
    for (reply, unique) in s.replies.iter().zip([10u64, 12]) {
        assert_eq!(
            *reply,
            Reply {
                qid: 0,
                entry: 0,
                commit_id: unique,
                unique,
                error: 0,
                payload: [b"attr-".as_slice(), &unique.to_le_bytes()].concat(),
            }
        );
    }
    drop(s);
    let seen = fs.wait_seen(2);
    assert_eq!(seen[0].opcode, GETATTR);
    assert_eq!(
        seen[0].bytes,
        vec![0; 16],
        "the first argument came from op_in"
    );
    assert!(
        fs.inline
            .lock()
            .unwrap()
            .iter()
            .all(|c| matches!(c, Completion::Written))
    );
    serving.stopper().stop();
    serving.join().unwrap();
}

#[test]
fn a_ring_request_is_rebuilt_from_the_header_op_in_and_payload() {
    let r = rig(host(1), depth(1), offered_uring());
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    // LOOKUP: no first argument (op_in empty), the name in the payload.
    r.kernel.deliver(0, 0, request(LOOKUP, 20, 1, b"name\0"));
    // WRITE: `fuse_write_in` (40 bytes) in op_in, the data in the payload.
    let mut write = vec![1u8; 40];
    write.extend_from_slice(&[9u8; 100]);
    r.kernel.deliver(0, 40, request(WRITE, 22, 2, &write));
    let seen = fs.wait_seen(2);
    assert_eq!(
        seen[0],
        Seen {
            opcode: LOOKUP,
            unique: 20,
            bytes: b"name\0".to_vec()
        }
    );
    assert_eq!(
        seen[1],
        Seen {
            opcode: WRITE,
            unique: 22,
            bytes: write
        }
    );
    let s = r.kernel.wait(|s| s.replies.len() == 2);
    assert_eq!(
        s.replies[1].error, -38,
        "WRITE answered ENOSYS by the test fs"
    );
    drop(s);
    serving.stopper().stop();
    serving.join().unwrap();
}

#[test]
fn a_parked_read_holds_its_entry_until_another_thread_completes_it() {
    let r = rig(host(2), depth(2), offered_uring());
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    r.kernel.deliver(1, 40, request(READ, 30, 2, &[0; 40]));
    r.kernel.deliver(1, 40, request(READ, 32, 2, &[0; 40]));
    let parked = fs.take_parked(2);
    assert_eq!(serving.uring_entries_held(), 2, "uring_entries_parked");
    assert!(r.kernel.lock().replies.is_empty());
    // A third request of that queue waits for an entry (K12), then flows once one is free.
    r.kernel.deliver(1, 16, request(GETATTR, 34, 1, &[0; 16]));
    assert_eq!(r.kernel.lock().waiting[&1].len(), 1);
    // Completed from this thread, out of order: each is committed on its own entry.
    let mut parked = parked.into_iter();
    let (first, second) = (parked.next().unwrap(), parked.next().unwrap());
    let (u1, u2) = (first.unique(), second.unique());
    assert!(matches!(second.ok(&[b"second"]), Completion::Written));
    assert!(matches!(
        first.ok(&[b"first", b"-two"]),
        Completion::Written
    ));
    let s = r.kernel.wait(|s| s.replies.len() == 3);
    let by_unique: HashMap<u64, &Reply> = s.replies.iter().map(|r| (r.unique, r)).collect();
    assert_eq!(by_unique[&u2].payload, b"second");
    assert_eq!(by_unique[&u1].payload, b"first-two");
    assert_eq!(by_unique[&u1].commit_id, u1);
    assert_ne!(by_unique[&u1].entry, by_unique[&u2].entry);
    assert_eq!(by_unique[&34].qid, 1);
    drop(s);
    assert_eq!(serving.uring_entries_held(), 0);
    serving.stopper().stop();
    serving.join().unwrap();
}

#[test]
fn a_forget_on_the_ring_is_committed_at_once_and_never_reaches_the_file_system() {
    let r = rig(host(1), depth(1), offered_uring());
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    r.kernel
        .deliver(0, 8, request(FORGET, 40, 5, &1u64.to_le_bytes()));
    r.kernel.deliver(
        0,
        8,
        request(BATCH_FORGET, 42, 0, &[1, 0, 0, 0, 0, 0, 0, 0]),
    );
    r.kernel.deliver(0, 16, request(GETATTR, 44, 1, &[0; 16]));
    let s = r.kernel.wait(|s| s.replies.len() == 3);
    assert_eq!(
        s.replies
            .iter()
            .map(|r| (r.unique, r.error, r.payload.len()))
            .collect::<Vec<_>>(),
        vec![(40, 0, 0), (42, 0, 0), (44, 0, 13)]
    );
    drop(s);
    let seen = fs.wait_seen(1);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].unique, 44);
    serving.stopper().stop();
    serving.join().unwrap();
}

#[test]
fn a_reply_larger_than_the_payload_buffer_is_answered_eio_and_refused() {
    let r = rig(host(1), depth(1), offered_uring());
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    r.kernel.deliver(0, 0, request(STATFS, 50, 1, &[]));
    let s = r.kernel.wait(|s| s.replies.len() == 1);
    assert_eq!((s.replies[0].unique, s.replies[0].error), (50, -5));
    assert!(s.replies[0].payload.is_empty());
    drop(s);
    fs.wait_seen(1);
    let inline = fs.inline.lock().unwrap();
    let Completion::DeviceError(e) = &inline[0] else {
        panic!("{:?}", inline[0]);
    };
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        e.to_string(),
        "a FUSE reply of 65552 bytes does not fit an io_uring entry (a 16-byte header and a \
         payload of at most 8192 bytes); answered EIO"
    );
    drop(inline);
    serving.stopper().stop();
    serving.join().unwrap();
}

#[test]
fn a_ring_request_whose_lengths_do_not_add_up_is_answered_eio_and_not_delivered() {
    let r = rig(host(1), depth(1), offered_uring());
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    // A header length shorter than its header plus payload.
    let mut short = request(GETATTR, 60, 1, &[0; 16]);
    short[0..4].copy_from_slice(&30u32.to_le_bytes());
    r.kernel.deliver(0, 0, short);
    // A first argument longer than `op_in` holds (129 bytes).
    let mut long = request(GETATTR, 62, 1, &[0; 129]);
    long.truncate(40);
    long[0..4].copy_from_slice(&(40u32 + 129).to_le_bytes());
    r.kernel.deliver(0, 0, long);
    r.kernel.deliver(0, 16, request(GETATTR, 64, 1, &[0; 16]));
    let s = r.kernel.wait(|s| s.replies.len() == 3);
    assert_eq!(
        s.replies
            .iter()
            .map(|r| (r.unique, r.error))
            .collect::<Vec<_>>(),
        vec![(60, -5), (62, -5), (64, 0)]
    );
    drop(s);
    assert_eq!(fs.wait_seen(1).len(), 1);
    serving.stopper().stop();
    serving.join().unwrap();
}

#[test]
fn a_commit_the_kernel_refuses_with_enoent_loses_that_entry_only() {
    let r = rig(host(1), depth(2), offered_uring());
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    r.kernel.lock().commit_fail.push_back(-2);
    r.kernel.deliver(0, 16, request(GETATTR, 70, 1, &[0; 16]));
    drop(r.kernel.wait(|s| s.replies.len() == 1));
    r.kernel.deliver(0, 16, request(GETATTR, 72, 1, &[0; 16]));
    let s = r.kernel.wait(|s| s.replies.len() == 2);
    assert_eq!(
        s.replies[1].unique, 72,
        "the queue goes on with its other entry"
    );
    assert_eq!(s.free[&0].len(), 1, "one entry lost, one back");
    drop(s);
    serving.stopper().stop();
    assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Stopped; 2]);
}

#[test]
fn a_queue_ends_with_the_kernel_s_end_of_its_entries() {
    for (res, end) in [
        (-107, ReaderEnd::Unmounted),
        (-103, ReaderEnd::Aborted),
        (-125, ReaderEnd::Unmounted),
    ] {
        let kernel = Arc::new(FakeKernel::default());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        rt.block_on(async {
            let ring = Box::new(FakeRing::new(0, &kernel));
            let mut reader = UringReader::new(ring, Shared::new(0), 1, 8192).unwrap();
            reader.register().unwrap();
            kernel.complete(0, res);
            let mut buf = vec![0u8; 9000];
            let got = crate::Transport::receive(&mut reader, &mut buf)
                .await
                .unwrap();
            assert_eq!(got, crate::Received::Ended(end), "res {res}");
        });
    }
}

#[test]
fn a_destroy_on_the_ring_is_answered_and_ends_its_queue() {
    let r = rig(host(2), depth(1), offered_uring());
    let serving = r.session.serve(Arc::new(TestFs::default())).unwrap();
    r.kernel.deliver(1, 0, request(DESTROY, 90, 0, &[]));
    let s = r.kernel.wait(|s| s.replies.len() == 1);
    assert_eq!((s.replies[0].unique, s.replies[0].error), (90, 0));
    drop(s);
    serving.stopper().stop();
    let ends = serving.join().unwrap();
    assert_eq!(
        ends,
        vec![ReaderEnd::Stopped, ReaderEnd::Stopped, ReaderEnd::Destroyed]
    );
}

// The queue's own ends.

#[test]
fn a_queue_that_cannot_register_reports_why() {
    let kernel = Arc::new(FakeKernel::default());
    kernel.lock().refuse.insert(0, 22);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let ring = Box::new(FakeRing::new(0, &kernel));
        let mut reader = UringReader::new(ring, Shared::new(0), 2, 8192).unwrap();
        let err = reader.register().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(22));
        let mut buf = vec![0u8; 9000];
        let got = crate::Transport::receive(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(got, crate::Received::Ended(ReaderEnd::Unmounted));
        // Its ring still sends the REGISTER that turns io_uring off.
        reader.poison(1).unwrap();
        assert_eq!(kernel.lock().poisoned, vec![(0, 1)]);
    });
}

#[test]
fn a_poison_the_kernel_takes_is_an_error() {
    let kernel = Arc::new(FakeKernel::default());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let ring = Box::new(FakeRing::new(0, &kernel));
        let mut reader = UringReader::new(ring, Shared::new(0), 1, 8192).unwrap();
        // Queue 0 is this ring's own: the fake kernel takes it as a REGISTER.
        let err = reader.poison(0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the kernel took a REGISTER of a queue past the last"
        );
    });
}

#[test]
fn a_reply_from_a_task_of_the_queue_s_own_runtime_is_committed() {
    let r = rig(host(1), depth(1), offered_uring());
    let serving = r.session.serve(Arc::new(TestFs::default())).unwrap();
    // The task runs on the queue's thread while its reader waits for the ring: its reply
    // must wake the reader, or the entry never goes back to the kernel.
    r.kernel.deliver(0, 16, request(FLUSH, 120, 2, &[0; 16]));
    r.kernel.deliver(0, 16, request(GETATTR, 122, 1, &[0; 16]));
    let s = r.kernel.wait(|s| s.replies.len() == 2);
    assert_eq!((s.replies[0].unique, s.replies[0].error), (120, 0));
    assert_eq!(
        s.replies[1].unique, 122,
        "the entry fetched the next request"
    );
    drop(s);
    serving.stopper().stop();
    serving.join().unwrap();
}

#[test]
fn a_ring_that_cannot_submit_while_serving_ends_its_queue_with_the_error() {
    let r = rig(host(2), depth(1), offered_uring());
    let fs = Arc::new(TestFs::default());
    let serving = r.session.serve(fs.clone()).unwrap();
    r.kernel.lock().fail_submit = Some(16);
    // Answered inline; its commit cannot be submitted.
    r.kernel.deliver(1, 16, request(GETATTR, 130, 1, &[0; 16]));
    fs.wait_seen(1);
    // The entry leaves the held count as its commit is queued, just before the submit that
    // fails: from then on the queue ends with the error, whatever the stop does.
    while serving.uring_entries_held() != 0 {
        std::thread::yield_now();
    }
    serving.stopper().stop();
    let err = serving.join().unwrap_err();
    assert!(
        matches!(&err, SessionError::Reader { index: 2, source } if source.raw_os_error() == Some(16)),
        "{err:?}"
    );
}

#[test]
fn a_ring_that_cannot_submit_its_registers_leaves_the_session_on_dev_fuse() {
    let r = rig(host(2), depth(1), offered_uring());
    r.kernel.lock().fail_submit = Some(16);
    let serving = r.session.serve(Arc::new(TestFs::default())).unwrap();
    assert_eq!(
        serving.transport(),
        TransportMode::DevFuse(Fallback::Register { qid: 0, errno: 16 })
    );
    assert!(r.kernel.lock().registers.is_empty());
    serving.stopper().stop();
    assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Stopped]);
}
