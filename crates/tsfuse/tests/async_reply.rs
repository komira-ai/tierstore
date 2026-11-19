//! Proves that a reply is the out header then the payload's parts, in order, as one message, and
//! that a reply completed after its handler returned (from another thread, or from a task awaiting
//! its data) reaches the device that read the request, once.
//!
//! Ports the tests of `src/reply.rs` of fuser 0.18.0 (MIT; notice in
//! `crates/tsfuse/FUSER-LICENSE.md`) that `abi_reply.rs` does not hold: serialize_empty,
//! serialize_slice, serialize_struct, reply_raw and async_reply. Its reply_empty, reply_error,
//! reply_data, reply_entry, reply_attr, reply_open, reply_write, reply_statfs, reply_create and
//! reply_directory have the vectors of the fork's `src/ll/reply.rs`, ported in `abi_reply.rs`;
//! reply_lock, reply_bmap and reply_xattr_* are dropped (those opcodes are not implemented), and
//! so is reply_xtimes (macOS only). In the fork a payload is one value serialized to bytes; here
//! it is the parts given to [`tsfuse::Replier::ok`], so the serialize tests check the bytes those
//! parts become.
//!
//! async_reply is extended (5.2): with four devices standing for four clones, parked replies
//! completed by tasks of a multi-thread runtime, in the reverse of the order they were read, each
//! land on the device that read the request; a task that awaits its data and the request's
//! cancellation at once sees an INTERRUPT read on another clone, and its late completion writes
//! nothing.
//!
//! Catches:
//! - a reply whose parts are reordered, dropped, or written as more than one message;
//! - a struct payload written in host order rather than as its wire bytes;
//! - a reply completed on another thread that never reaches the device;
//! - a completion from a task landing on another clone than the one that read the request;
//! - a task woken by an INTERRUPT whose late completion writes a second reply.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod common;

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread;

use common::FakeDevice;
use tokio::sync::oneshot;
use tsfuse::{Completion, Interrupted, Replier, ReplyDevice, Tokens};
use zerocopy::{Immutable, IntoBytes};

/// fuser's test struct: two bytes and a little-endian `u16`.
#[derive(Debug, IntoBytes, Immutable)]
#[repr(C)]
struct Data {
    a: u8,
    b: u8,
    c: u16,
}

/// The bytes one reply to request `0xdeadbeef` writes.
fn reply(answer: impl FnOnce(Replier) -> Completion) -> Vec<u8> {
    let tokens = Tokens::new();
    let device = FakeDevice::new();
    let done = answer(tokens.register(0xdead_beef, device.clone()));
    assert!(matches!(done, Completion::Written), "{done:?}");
    let raw = device.raw();
    assert_eq!(raw.len(), 1, "one reply is one message");
    raw[0].clone()
}

/// The out header of a successful reply of `payload` bytes to `0xdeadbeef`.
fn ok_header(payload: u8) -> Vec<u8> {
    vec![
        16 + payload,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0xef,
        0xbe,
        0xad,
        0xde,
        0x00,
        0x00,
        0x00,
        0x00,
    ]
}

#[test]
fn serialize_empty() {
    // No part, and empty parts, add nothing after the header.
    assert_eq!(reply(|r| r.ok(&[])), ok_header(0));
    assert_eq!(reply(|r| r.ok(&[&[], &[]])), ok_header(0));
}

#[test]
fn serialize_slice() {
    let data: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
    let mut expected = ok_header(4);
    expected.extend([0x12, 0x34, 0x56, 0x78]);
    assert_eq!(reply(|r| r.ok(&[&data])), expected);
}

#[test]
fn serialize_struct() {
    let data = Data {
        a: 0x12,
        b: 0x34,
        c: 0x5678,
    };
    assert_eq!(data.as_bytes(), [0x12, 0x34, 0x78, 0x56]);
    let mut expected = ok_header(4);
    expected.extend([0x12, 0x34, 0x78, 0x56]);
    assert_eq!(reply(|r| r.ok(&[data.as_bytes()])), expected);
}

#[test]
fn serialize_several_parts_in_order() {
    let data = Data {
        a: 0x12,
        b: 0x34,
        c: 0x5678,
    };
    let mut expected = ok_header(7);
    expected.extend([0x12, 0x34, 0x78, 0x56, 0xaa, 0xbb, 0xcc]);
    let got = reply(|r| r.ok(&[data.as_bytes(), &[0xaa], &[], &[0xbb, 0xcc]]));
    assert_eq!(got, expected);
}

#[test]
fn reply_raw() {
    let data = Data {
        a: 0x12,
        b: 0x34,
        c: 0x5678,
    };
    assert_eq!(
        reply(|r| r.ok(&[data.as_bytes()])),
        vec![
            0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x00, 0x00,
            0x00, 0x00, 0x12, 0x34, 0x78, 0x56,
        ],
    );
}

/// A device that hands each reply to a channel, as the fork's `ReplySender::Sync` does.
struct ChannelDevice(SyncSender<Vec<u8>>);

impl ReplyDevice for ChannelDevice {
    fn send(&self, parts: &[&[u8]]) -> io::Result<()> {
        self.0.send(parts.concat()).map_err(io::Error::other)
    }
}

#[test]
fn async_reply() {
    let (tx, rx) = sync_channel::<Vec<u8>>(1);
    let tokens = Tokens::new();
    let replier = tokens.register(0xdead_beef, Arc::new(ChannelDevice(tx)));
    let completer = thread::spawn(move || replier.ok(&[]));
    assert_eq!(rx.recv().unwrap(), ok_header(0));
    assert!(matches!(completer.join().unwrap(), Completion::Written));
    assert_eq!(tokens.registered(), 0);
}

#[test]
fn async_replies_completed_by_tasks_out_of_order_land_each_on_its_originating_device() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let tokens = Tokens::new();
    let devices: Vec<Arc<FakeDevice>> = (0..4).map(|_| FakeDevice::new()).collect();
    // Three requests read by each device, in turn: uniques are even, as the kernel's are.
    let mut parked = Vec::new();
    for k in 0..3u64 {
        for (i, device) in devices.iter().enumerate() {
            let unique = 2 * (10 * k + i as u64 + 1);
            let dev: Arc<dyn ReplyDevice> = device.clone();
            parked.push((i, unique, tokens.register(unique, dev)));
        }
    }
    assert_eq!(tokens.registered(), 12);
    // Each parked request waits in its own task for its data; the data arrive in the reverse
    // of the order the requests were read, one at a time, so the order is the test's.
    for (i, unique, replier) in parked.into_iter().rev() {
        let (fill, filled) = oneshot::channel::<Vec<u8>>();
        let task = runtime.spawn(async move {
            let data = filled.await.unwrap();
            replier.ok(&[&data])
        });
        fill.send(vec![i as u8; unique as usize % 5 + 1]).unwrap();
        let done = runtime.block_on(task).unwrap();
        assert!(
            matches!(done, Completion::Written),
            "unique {unique}: {done:?}"
        );
    }
    assert_eq!(tokens.registered(), 0);
    for (i, device) in devices.iter().enumerate() {
        let got: Vec<(u64, Vec<u8>)> = device
            .msgs()
            .into_iter()
            .map(|m| {
                assert_eq!((m.error, m.len as usize), (0, 16 + m.payload.len()));
                (m.unique, m.payload)
            })
            .collect();
        let expected: Vec<(u64, Vec<u8>)> = (0..3u64)
            .rev()
            .map(|k| {
                let unique = 2 * (10 * k + i as u64 + 1);
                (unique, vec![i as u8; unique as usize % 5 + 1])
            })
            .collect();
        assert_eq!(
            got, expected,
            "device {i}: its own replies, in completion order"
        );
    }
}

#[test]
fn a_task_awaiting_its_data_and_its_cancel_sees_the_interrupt_and_answers_once() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let tokens = Tokens::new();
    let origin = FakeDevice::new();
    let other = FakeDevice::new();
    let replier = tokens.register(40, origin.clone());
    let token = replier.cancel_token();
    let (_fill, filled) = oneshot::channel::<Vec<u8>>();
    let task = runtime.spawn(async move {
        tokio::select! {
            _ = token.cancelled() => ("cancelled", replier.ok(&[b"late"])),
            data = filled => ("filled", replier.ok(&[&data.unwrap()])),
        }
    });
    // The INTERRUPT is read on another clone; EINTR goes to the request's own.
    assert!(matches!(
        tokens.interrupt(40, 41, &*other),
        Interrupted::Cancelled
    ));
    let (woken_by, done) = runtime.block_on(task).unwrap();
    assert_eq!(woken_by, "cancelled");
    assert!(matches!(done, Completion::AlreadyAnswered), "{done:?}");
    // The same INTERRUPT again (the kernel can deliver it twice): the request is no longer
    // registered, so it is answered EAGAIN on the device given for it, and EINTR is not repeated.
    assert!(matches!(
        tokens.interrupt(40, 41, &*other),
        Interrupted::Unknown
    ));
    let m = origin.msgs();
    assert_eq!(m.len(), 1, "one answer to the request");
    assert_eq!((m[0].len, m[0].error, m[0].unique), (16, -4, 40));
    let m = other.msgs();
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].len, m[0].error, m[0].unique), (16, -11, 41));
    assert_eq!(tokens.registered(), 0);
}
