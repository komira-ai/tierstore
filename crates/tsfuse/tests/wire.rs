//! Proves the decoding of requests and opcodes, and the dispatch of a request through an `Fs`
//! trait object that answers inline or parks.
//!
//! Catches:
//! - a header field decoded big-endian, at the wrong offset, or with the high bits of `unique`
//!   lost;
//! - an off-by-one at 39, 40 or `len` bytes, or a `len` shorter than the header or longer than
//!   the buffer accepted;
//! - an opcode number mapped to the wrong opcode, or an unimplemented one accepted;
//! - a parked reply written to a device other than the one that read the request.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod common;

use std::sync::{Arc, Mutex};
use std::thread;

use common::FakeDevice;
use tsfuse::{Fs, Handled, Opcode, Replier, Request, RequestHeader, Tokens};

fn header(len: u32, opcode: u32, unique: u64) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend(len.to_le_bytes());
    h.extend(opcode.to_le_bytes());
    h.extend(unique.to_le_bytes());
    h.extend(0x1111_2222_3333_4444u64.to_le_bytes());
    h.extend(1000u32.to_le_bytes());
    h.extend(2000u32.to_le_bytes());
    h.extend(3000u32.to_le_bytes());
    h.extend([0u8; 4]);
    h
}

#[test]
fn a_header_decodes_every_field_little_endian_including_high_unique_bits() {
    let buf = header(40, 15, 0xAABB_CCDD_0102_0304);
    assert_eq!(buf.len(), 40);
    let h = RequestHeader::parse(&buf).unwrap();
    assert_eq!(h.len, 40);
    assert_eq!(h.opcode, 15);
    assert_eq!(h.unique, 0xAABB_CCDD_0102_0304);
    assert_eq!(h.nodeid, 0x1111_2222_3333_4444);
    assert_eq!((h.uid, h.gid, h.pid), (1000, 2000, 3000));
    assert_eq!(h.op(), Some(Opcode::Read));
    let h2 = h;
    assert_eq!(h, h2);
}

#[test]
fn a_buffer_shorter_than_the_header_is_not_a_request() {
    assert!(RequestHeader::parse(&[]).is_none());
    let buf = header(40, 1, 1);
    assert!(RequestHeader::parse(&buf[..39]).is_none());
    assert!(Request::parse(&buf[..39]).is_none());
    assert!(RequestHeader::parse(&buf[..40]).is_some());
}

#[test]
fn a_len_below_the_header_or_beyond_the_buffer_is_rejected() {
    assert!(RequestHeader::parse(&header(39, 1, 1)).is_none());
    assert!(RequestHeader::parse(&header(0, 1, 1)).is_none());
    let mut buf = header(45, 1, 1);
    buf.extend([0u8; 4]); // 44 bytes: one short of len
    assert!(RequestHeader::parse(&buf).is_none());
    buf.push(0); // 45 bytes: exactly len
    assert!(RequestHeader::parse(&buf).is_some());
    assert!(RequestHeader::parse(&header(u32::MAX, 1, 1)).is_none());
}

#[test]
fn a_request_body_is_the_bytes_after_the_header_up_to_len() {
    let mut buf = header(46, 16, 9);
    buf.extend([1, 2, 3, 4, 5, 6]);
    buf.extend([0xFF; 10]); // beyond len: not part of the request
    let req = Request::parse(&buf).unwrap();
    assert_eq!(req.header.len, 46);
    assert_eq!(req.body, &[1, 2, 3, 4, 5, 6]);
    let empty = header(40, 3, 9);
    assert!(Request::parse(&empty).unwrap().body.is_empty());
    let copy = req;
    assert_eq!(copy.body.len(), 6);
    assert!(format!("{req:?}").contains("Request"));
}

const IMPLEMENTED: [(Opcode, u32); 24] = [
    (Opcode::Lookup, 1),
    (Opcode::Forget, 2),
    (Opcode::Getattr, 3),
    (Opcode::Setattr, 4),
    (Opcode::Readlink, 5),
    (Opcode::Mkdir, 9),
    (Opcode::Unlink, 10),
    (Opcode::Rename, 12),
    (Opcode::Open, 14),
    (Opcode::Read, 15),
    (Opcode::Write, 16),
    (Opcode::Statfs, 17),
    (Opcode::Release, 18),
    (Opcode::Fsync, 20),
    (Opcode::Flush, 25),
    (Opcode::Init, 26),
    (Opcode::Opendir, 27),
    (Opcode::Readdir, 28),
    (Opcode::Releasedir, 29),
    (Opcode::Create, 35),
    (Opcode::Interrupt, 36),
    (Opcode::Destroy, 38),
    (Opcode::BatchForget, 42),
    (Opcode::Readdirplus, 44),
];

#[test]
fn every_implemented_opcode_round_trips_through_its_uapi_value() {
    for (op, code) in IMPLEMENTED {
        assert_eq!(op.code(), code, "{op:?}");
        assert_eq!(Opcode::from_code(code), Some(op), "{code}");
        let h = RequestHeader::parse(&header(40, code, 1)).unwrap();
        assert_eq!(h.op(), Some(op));
    }
}

#[test]
fn an_unimplemented_opcode_is_none() {
    let implemented: Vec<u32> = IMPLEMENTED.iter().map(|(_, c)| *c).collect();
    for code in [
        0,
        6,
        7,
        8,
        11,
        13,
        19,
        21,
        22,
        23,
        24,
        30,
        31,
        32,
        33,
        34,
        37,
        39,
        40,
        41,
        43,
        45,
        46,
        100,
        4096,
        u32::MAX,
    ] {
        assert!(!implemented.contains(&code));
        assert_eq!(Opcode::from_code(code), None, "{code}");
    }
    let h = RequestHeader::parse(&header(40, 6, 1)).unwrap();
    assert_eq!(h.op(), None);
}

#[test]
fn only_forget_batch_forget_and_interrupt_expect_no_reply() {
    for (op, _) in IMPLEMENTED {
        let silent = matches!(op, Opcode::Forget | Opcode::BatchForget | Opcode::Interrupt);
        assert_eq!(op.no_reply(), silent, "{op:?}");
    }
}

/// Answers GETATTR inline and parks READ, completing it later from another thread.
struct TinyFs {
    parked: Mutex<Vec<Replier>>,
}

impl Fs for TinyFs {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        match req.header.op() {
            Some(Opcode::Getattr) => {
                let _ = replier.ok(&[req.body]);
                Handled::Done
            }
            _ => {
                self.parked.lock().unwrap().push(replier);
                Handled::Parked
            }
        }
    }
}

#[test]
fn an_fs_answers_inline_or_parks_and_completes_on_the_originating_device() {
    let tiny = Arc::new(TinyFs {
        parked: Mutex::new(Vec::new()),
    });
    let fs: Arc<dyn Fs> = tiny.clone();
    let tokens = Tokens::new();
    let d0 = FakeDevice::new();
    let d1 = FakeDevice::new();

    let mut getattr = header(43, 3, 700);
    getattr.extend([7, 8, 9]);
    let read = header(40, 15, 701);

    let r = tokens.register(700, d0.clone());
    assert_eq!(
        fs.handle(Request::parse(&getattr).unwrap(), r),
        Handled::Done
    );
    assert_eq!(d0.msgs()[0].payload, [7, 8, 9]);
    assert_eq!(tokens.registered(), 0);

    let r = tokens.register(701, d1.clone());
    assert_eq!(
        fs.handle(Request::parse(&read).unwrap(), r),
        Handled::Parked
    );
    assert_eq!(tokens.registered(), 1);
    assert_eq!(d0.msgs().len(), 1);
    assert!(d1.raw().is_empty());

    // The parked replier is `Send + 'static`: complete it from another thread.
    let parked = tiny.parked.lock().unwrap().pop().unwrap();
    let done = thread::spawn(move || parked.ok(&[b"later"]))
        .join()
        .unwrap();
    assert!(matches!(done, tsfuse::Completion::Written));
    assert_eq!(d0.msgs().len(), 1);
    let m = d1.msgs();
    assert_eq!((m.len(), m[0].unique, m[0].error), (1, 701, 0));
    assert_eq!(m[0].payload, b"later");
    assert_eq!(tokens.registered(), 0);
    assert_ne!(Handled::Done, Handled::Parked);
    assert_eq!(format!("{:?}", Handled::Parked), "Parked");
}
