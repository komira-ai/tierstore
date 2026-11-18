//! Proves that a reply is written to the device that read its request, whichever task completes
//! it, and that the encoding of a reply is exact.
//!
//! Catches:
//! - a reply written to the completing task's device or to a shared device instead of the
//!   originating clone;
//! - a wrong `len`, `error` sign or `unique` in the out header, or a payload part dropped or
//!   reordered;
//! - a device write failure reported as success;
//! - a late completion of an older registration removing the entry of a newer one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod common;

use std::sync::Arc;
use std::thread;

use common::FakeDevice;
use tsfuse::{
    Completion, Errno, Interrupted, OUT_HEADER_LEN, ReplyDevice, Tokens, encode_out_header,
};

#[test]
fn with_four_clones_each_parked_reply_lands_on_its_originating_device_only() {
    let tokens = Tokens::new();
    let devs: Vec<Arc<FakeDevice>> = (0..4).map(|_| FakeDevice::new()).collect();
    let mut repliers = Vec::new();
    for (i, d) in devs.iter().enumerate() {
        for k in 0..3u64 {
            let unique = 100 * (i as u64 + 1) + k;
            let dev: Arc<dyn ReplyDevice> = d.clone();
            repliers.push((i, unique, tokens.register(unique, dev)));
        }
    }
    assert_eq!(tokens.registered(), 12);
    // Complete in reverse registration order, each from its own thread.
    let handles: Vec<_> = repliers
        .into_iter()
        .rev()
        .map(|(i, unique, r)| {
            thread::spawn(move || {
                assert_eq!(r.unique(), unique);
                let payload = vec![i as u8; unique as usize % 7 + 1];
                let tail = [0xEE, unique as u8];
                (unique, payload.clone(), r.ok(&[&payload, &tail]))
            })
        })
        .collect();
    let mut expected: Vec<Vec<(u64, Vec<u8>)>> = vec![Vec::new(); 4];
    for h in handles {
        let (unique, payload, done) = h.join().unwrap();
        assert!(matches!(done, Completion::Written));
        let i = (unique / 100 - 1) as usize;
        let mut body = payload;
        body.extend([0xEE, unique as u8]);
        expected[i].push((unique, body));
    }
    assert_eq!(tokens.registered(), 0);
    for (i, d) in devs.iter().enumerate() {
        let mut got: Vec<(u64, Vec<u8>)> = d
            .msgs()
            .into_iter()
            .map(|m| {
                assert_eq!(m.error, 0);
                assert_eq!(m.len as usize, OUT_HEADER_LEN + m.payload.len());
                (m.unique, m.payload)
            })
            .collect();
        got.sort();
        expected[i].sort();
        assert_eq!(got, expected[i], "device {i}");
        assert_eq!(got.len(), 3);
    }
}

#[test]
fn an_error_reply_carries_the_negated_errno_and_no_payload() {
    let tokens = Tokens::new();
    let d = FakeDevice::new();
    let r = tokens.register(9, d.clone());
    assert!(matches!(r.error(Errno::EIO), Completion::Written));
    let m = d.msgs();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].len, 16);
    assert_eq!(m[0].error, -5);
    assert_eq!(m[0].unique, 9);
    assert!(m[0].payload.is_empty());
    assert_eq!(tokens.registered(), 0);
}

#[test]
fn the_out_header_layout_is_len_error_unique_little_endian() {
    let h = encode_out_header(0x0102_0304_0506_0708, Some(Errno::ENOSYS), 100);
    assert_eq!(&h[0..4], &116u32.to_le_bytes());
    assert_eq!(&h[4..8], &(-38i32).to_le_bytes());
    assert_eq!(&h[8..16], &0x0102_0304_0506_0708u64.to_le_bytes());
    let ok = encode_out_header(u64::MAX, None, 0);
    assert_eq!(&ok[0..4], &16u32.to_le_bytes());
    assert_eq!(&ok[4..8], &[0, 0, 0, 0]);
    assert_eq!(&ok[8..16], &[0xFF; 8]);
}

#[test]
fn an_out_header_length_beyond_u32_saturates() {
    let h = encode_out_header(1, None, u32::MAX as usize);
    assert_eq!(&h[0..4], &u32::MAX.to_le_bytes());
    let h = encode_out_header(1, None, usize::MAX >> 8);
    assert_eq!(&h[0..4], &u32::MAX.to_le_bytes());
}

#[test]
fn the_errno_constants_are_the_linux_values() {
    assert_eq!(Errno::EINTR.0, 4);
    assert_eq!(Errno::EIO.0, 5);
    assert_eq!(Errno::EAGAIN.0, 11);
    assert_eq!(Errno::ENOSYS.0, 38);
    assert_eq!(OUT_HEADER_LEN, 16);
}

#[test]
fn a_failing_device_reports_device_error_and_still_unregisters() {
    let tokens = Tokens::new();
    let d = FakeDevice::failing();
    let r = tokens.register(1, d.clone());
    match r.ok(&[b"x"]) {
        Completion::DeviceError(e) => assert_eq!(e.to_string(), "device gone"),
        other => panic!("expected DeviceError, got {other:?}"),
    }
    assert_eq!(tokens.registered(), 0);
    assert!(d.raw().is_empty());
}

#[test]
fn a_stale_replier_of_a_reused_unique_does_not_remove_the_newer_entry() {
    let tokens = Tokens::new();
    let da = FakeDevice::new();
    let db = FakeDevice::new();
    let a = tokens.register(5, da.clone());
    let b = tokens.register(5, db.clone());
    assert_eq!(tokens.registered(), 1);
    // A's slot is not answered, so A writes on its own device, and B's entry stays.
    assert!(matches!(a.ok(&[b"a"]), Completion::Written));
    assert_eq!(da.msgs().len(), 1);
    assert_eq!(tokens.registered(), 1);
    // An INTERRUPT now reaches B, on B's device.
    let other = FakeDevice::new();
    assert!(matches!(
        tokens.interrupt(5, 77, &*other),
        Interrupted::Cancelled
    ));
    let m = db.msgs();
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].error, m[0].unique), (-4, 5));
    assert!(other.raw().is_empty());
    assert!(matches!(b.ok(&[]), Completion::AlreadyAnswered));
    assert_eq!(db.msgs().len(), 1);
    assert_eq!(tokens.registered(), 0);
}

#[test]
fn a_unique_can_be_registered_again_after_its_reply() {
    let tokens = Tokens::new();
    let d = FakeDevice::new();
    assert!(matches!(
        tokens.register(3, d.clone()).ok(&[]),
        Completion::Written
    ));
    assert_eq!(tokens.registered(), 0);
    let again = tokens.register(3, d.clone());
    assert_eq!(tokens.registered(), 1);
    assert!(matches!(again.ok(&[b"z"]), Completion::Written));
    assert_eq!(d.msgs().len(), 2);
    assert_eq!(tokens.registered(), 0);
}

#[test]
fn a_replier_outliving_its_table_still_answers() {
    let d = FakeDevice::new();
    let r = Tokens::new().register(8, d.clone());
    assert!(matches!(r.ok(&[b"late"]), Completion::Written));
    assert_eq!(d.msgs()[0].payload, b"late");
}

#[test]
fn debug_output_names_the_state_and_clones_share_the_table() {
    let tokens = Tokens::new();
    assert_eq!(format!("{tokens:?}"), "Tokens { registered: 0 }");
    let shared = tokens.clone();
    let d = FakeDevice::new();
    let r = shared.register(42, d);
    assert_eq!(tokens.registered(), 1);
    assert_eq!(format!("{tokens:?}"), "Tokens { registered: 1 }");
    assert_eq!(format!("{r:?}"), "Replier { unique: 42, .. }");
    let t = r.cancel_token();
    assert_eq!(format!("{t:?}"), "CancelToken { cancelled: false }");
    assert!(!t.is_cancelled());
    let _ = r.ok(&[]);
    assert_eq!(tokens.registered(), 0);
    assert!(!t.is_cancelled());
}

#[test]
fn dropping_an_unanswered_replier_writes_one_eio_on_its_device_and_unregisters() {
    let tokens = Tokens::new();
    let origin = FakeDevice::new();
    let other = FakeDevice::new();
    let r = tokens.register(31, origin.clone());
    assert_eq!(tokens.registered(), 1);
    drop(r);
    let m = origin.msgs();
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].len, m[0].error, m[0].unique), (16, -5, 31));
    assert!(m[0].payload.is_empty());
    assert!(other.raw().is_empty());
    assert_eq!(tokens.registered(), 0);
}

#[test]
fn dropping_an_unanswered_replier_on_a_failing_device_still_unregisters() {
    let tokens = Tokens::new();
    let d = FakeDevice::failing();
    drop(tokens.register(32, d.clone()));
    assert!(d.raw().is_empty());
    assert_eq!(tokens.registered(), 0);
}

#[test]
fn a_replier_completed_by_ok_or_error_writes_nothing_more_when_it_is_dropped() {
    let tokens = Tokens::new();
    let d = FakeDevice::new();
    assert!(matches!(
        tokens.register(33, d.clone()).ok(&[b"x"]),
        Completion::Written
    ));
    assert!(matches!(
        tokens.register(34, d.clone()).error(Errno::ENOSYS),
        Completion::Written
    ));
    let m = d.msgs();
    assert_eq!(m.len(), 2);
    assert_eq!((m[0].error, m[0].unique), (0, 33));
    assert_eq!((m[1].error, m[1].unique), (-38, 34));
}

#[test]
fn an_interrupt_after_a_replier_was_dropped_is_answered_eagain() {
    let tokens = Tokens::new();
    let origin = FakeDevice::new();
    let reader = FakeDevice::new();
    drop(tokens.register(35, origin.clone()));
    assert!(matches!(
        tokens.interrupt(35, 36, &*reader),
        Interrupted::Unknown
    ));
    let m = reader.msgs();
    assert_eq!((m.len(), m[0].error, m[0].unique), (1, -11, 36));
    assert_eq!(origin.msgs().len(), 1);
}

#[test]
fn a_replier_taken_by_an_interrupt_writes_nothing_when_it_is_dropped() {
    let tokens = Tokens::new();
    let origin = FakeDevice::new();
    let reader = FakeDevice::new();
    let r = tokens.register(37, origin.clone());
    assert!(matches!(
        tokens.interrupt(37, 38, &*reader),
        Interrupted::Cancelled
    ));
    drop(r);
    let m = origin.msgs();
    assert_eq!((m.len(), m[0].error), (1, -4));
}

#[test]
fn an_errno_of_i32_min_wraps_instead_of_overflowing() {
    let h = encode_out_header(1, Some(Errno(i32::MIN)), 0);
    assert_eq!(&h[4..8], &i32::MIN.to_le_bytes());
}
