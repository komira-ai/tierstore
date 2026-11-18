//! The mount-to-tierstore wire protocol (`wire`).
//!
//! Proves every message round trips through a frame, the exact bytes of a frame, which
//! descriptors each kind carries, that a stream reader gets `Short` at every prefix length and
//! the right consumed length with bytes after the frame, and that every refusal of `Frame::decode`
//! fires: length, body size, magic, version, kind, descriptor count, and each invalid field, plus
//! the two refusals of `Frame::encode` and the epoch rule.
//!
//! Catches:
//! - a message whose fields change on a round trip, or a changed byte layout;
//! - a frame accepted with the wrong descriptor count, magic, version or an unknown kind;
//! - a truncated frame that decodes, or reports the wrong number of bytes needed;
//! - a body of more than `MAX_BODY` bytes accepted either way, or the exact maximum refused;
//! - an unknown class, wait outcome or refusal code accepted, a non-UTF-8 tree accepted, bytes
//!   after the body ignored, a body that ends inside a field read past its end;
//! - a tree reference of more than 65535 bytes truncated instead of refused;
//! - a reply of another epoch accepted.

use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, HolderId, JobId, Rank, TreeRef};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_fetch_api::wire::{
    Epoch, FRAME_HEADER_LEN, FRAME_MAGIC, FdRole, Frame, MAX_BODY, Message, PROTOCOL_VERSION,
    PackHandle, RefusalCode, WireError,
};

fn region() -> CounterRegion {
    CounterRegion {
        offset: 64,
        groups: 9,
    }
}

fn all_messages() -> Vec<Message> {
    let mut v = vec![
        Message::Handshake {
            holder: HolderId(0x0102_0304_0506_0708),
            version: 1,
        },
        Message::OpenPack {
            tree: TreeRef("tree/ünï".into()),
            pack: 7,
            holder: HolderId(9),
            region: region(),
        },
        Message::OpenPack {
            tree: TreeRef(String::new()),
            pack: 0,
            holder: HolderId(0),
            region: CounterRegion {
                offset: 0,
                groups: 0,
            },
        },
        Message::Wait {
            pack: PackHandle(3),
            range: ByteRange {
                offset: 1 << 40,
                len: u64::MAX,
            },
            job: JobId(77),
        },
        Message::Ensure {
            pack: PackHandle(4),
            runs: vec![
                CellRun { first: 0, count: 1 },
                CellRun {
                    first: u32::MAX,
                    count: u32::MAX,
                },
            ],
            class: Class::Prepare,
            job: JobId(5),
            rank: Rank(6),
        },
        Message::Ensure {
            pack: PackHandle(0),
            runs: vec![],
            class: Class::Background,
            job: JobId(0),
            rank: Rank(0),
        },
        Message::Ensure {
            pack: PackHandle(1),
            runs: vec![CellRun { first: 2, count: 3 }],
            class: Class::Demand,
            job: JobId(1),
            rank: Rank(u32::MAX),
        },
        Message::Release {
            holder: HolderId(5),
        },
        Message::HandshakeOk,
        Message::PackOpened {
            pack: PackHandle(8),
            pack_size: 1 << 33,
            region: region(),
        },
        Message::Waited(WaitOutcome::Ready),
        Message::Waited(WaitOutcome::Eio { cell: 12345 }),
        Message::Waited(WaitOutcome::Lost),
        Message::Ensured,
        Message::Released,
    ];
    for code in [
        RefusalCode::Version,
        RefusalCode::ListVersionUnsupported,
        RefusalCode::UnknownPack,
        RefusalCode::UnknownHandle,
        RefusalCode::BadRegion,
        RefusalCode::NotHandshaken,
    ] {
        v.push(Message::Refused { code });
    }
    v
}

fn frame(message: Message) -> Frame {
    Frame {
        id: 0xAABB_CCDD_0011_2233,
        epoch: Epoch(0x1122_3344_5566_7788),
        message,
    }
}

/// A frame built by hand: `len` is what follows the length field.
fn raw(kind: u8, fds: u8, body: &[u8]) -> Vec<u8> {
    raw_with(FRAME_MAGIC, PROTOCOL_VERSION, kind, fds, body)
}

fn raw_with(magic: u16, version: u8, kind: u8, fds: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&((FRAME_HEADER_LEN + body.len()) as u32).to_le_bytes());
    out.extend_from_slice(&magic.to_le_bytes());
    out.push(version);
    out.push(kind);
    out.extend_from_slice(&1u64.to_le_bytes());
    out.extend_from_slice(&2u64.to_le_bytes());
    out.push(fds);
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(body);
    out
}

#[test]
fn every_message_round_trips_through_a_frame_and_reports_its_length() {
    for m in all_messages() {
        let f = frame(m);
        let bytes = f.encode().expect("encodes");
        let (back, used) = Frame::decode(&bytes).expect("decodes");
        assert_eq!(back, f);
        assert_eq!(used, bytes.len());
    }
}

#[test]
fn a_handshake_frame_has_exactly_the_documented_bytes() {
    let f = Frame {
        id: 9,
        epoch: Epoch(5),
        message: Message::Handshake {
            holder: HolderId(7),
            version: 1,
        },
    };
    let mut want = Vec::new();
    want.extend_from_slice(&33u32.to_le_bytes());
    want.extend_from_slice(b"TP");
    want.push(1);
    want.push(1);
    want.extend_from_slice(&9u64.to_le_bytes());
    want.extend_from_slice(&5u64.to_le_bytes());
    want.push(2);
    want.extend_from_slice(&[0, 0, 0]);
    want.extend_from_slice(&7u64.to_le_bytes());
    want.push(1);
    assert_eq!(f.encode().expect("encodes"), want);
}

#[test]
fn the_constants_are_the_documented_values() {
    assert_eq!(FRAME_MAGIC, 0x5054);
    assert_eq!(PROTOCOL_VERSION, 1);
    assert_eq!(FRAME_HEADER_LEN, 24);
    assert_eq!(MAX_BODY, 1 << 20);
}

#[test]
fn each_kind_carries_the_descriptors_the_protocol_names() {
    for m in all_messages() {
        let want: &[FdRole] = match &m {
            Message::Handshake { .. } => &[FdRole::CounterMemfd, FdRole::Lifeline],
            Message::PackOpened { .. } => &[FdRole::DataFd, FdRole::StateMemfd],
            _ => &[],
        };
        assert_eq!(m.fds(), want, "{m:?}");
        let bytes = frame(m).encode().expect("encodes");
        assert_eq!(usize::from(bytes[24]), want.len());
    }
}

#[test]
fn a_frame_followed_by_another_decodes_alone_and_the_rest_is_left() {
    let a = frame(Message::Released).encode().expect("encodes");
    let b = frame(Message::Ensured).encode().expect("encodes");
    let mut both = a.clone();
    both.extend_from_slice(&b);
    let (first, used) = Frame::decode(&both).expect("decodes");
    assert_eq!(first.message, Message::Released);
    assert_eq!(used, a.len());
    let (second, used2) = Frame::decode(&both[used..]).expect("decodes");
    assert_eq!(second.message, Message::Ensured);
    assert_eq!(used2, b.len());
}

#[test]
fn every_proper_prefix_of_a_frame_is_short_and_names_the_bytes_needed() {
    for m in all_messages() {
        let bytes = frame(m).encode().expect("encodes");
        for n in 0..bytes.len() {
            let need = if n < 4 { 4 } else { bytes.len() };
            assert_eq!(
                Frame::decode(&bytes[..n]),
                Err(WireError::Short { need, have: n }),
                "prefix {n} of {}",
                bytes.len()
            );
        }
    }
}

#[test]
fn a_length_shorter_than_the_header_is_invalid() {
    let mut bytes = raw(129, 0, &[]);
    bytes[..4].copy_from_slice(&(FRAME_HEADER_LEN as u32 - 1).to_le_bytes());
    assert_eq!(
        Frame::decode(&bytes),
        Err(WireError::Invalid("length shorter than the frame header"))
    );
    bytes[..4].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        Frame::decode(&bytes),
        Err(WireError::Invalid("length shorter than the frame header"))
    );
}

#[test]
fn a_length_field_of_the_bare_header_is_a_frame_with_no_body() {
    let (f, used) = Frame::decode(&raw(129, 0, &[])).expect("decodes");
    assert_eq!(f.message, Message::HandshakeOk);
    assert_eq!(used, 28);
}

#[test]
fn a_length_field_past_the_body_limit_is_too_long_before_the_bytes_arrive() {
    let len = (FRAME_HEADER_LEN + MAX_BODY + 1) as u32;
    let bytes = len.to_le_bytes();
    assert_eq!(Frame::decode(&bytes), Err(WireError::TooLong(MAX_BODY + 1)));
    let huge = u32::MAX.to_le_bytes();
    assert_eq!(
        Frame::decode(&huge),
        Err(WireError::TooLong(u32::MAX as usize - FRAME_HEADER_LEN))
    );
}

#[test]
fn a_length_field_of_exactly_the_body_limit_is_short_until_all_its_bytes_arrive() {
    let len = (FRAME_HEADER_LEN + MAX_BODY) as u32;
    assert_eq!(
        Frame::decode(&len.to_le_bytes()),
        Err(WireError::Short {
            need: 4 + FRAME_HEADER_LEN + MAX_BODY,
            have: 4
        })
    );
}

#[test]
fn a_wrong_magic_is_refused_with_the_value_it_had() {
    let bytes = raw_with(0x4242, PROTOCOL_VERSION, 129, 0, &[]);
    assert_eq!(Frame::decode(&bytes), Err(WireError::BadMagic(0x4242)));
    assert_eq!(
        WireError::BadMagic(0x4242).to_string(),
        "frame: bad magic 0x4242"
    );
}

#[test]
fn a_protocol_version_this_release_does_not_speak_is_refused() {
    for v in [0u8, 2, 255] {
        let bytes = raw_with(FRAME_MAGIC, v, 129, 0, &[]);
        assert_eq!(Frame::decode(&bytes), Err(WireError::Version(v)));
    }
}

#[test]
fn an_unknown_kind_is_refused_whatever_follows() {
    for kind in [0u8, 6, 127, 128, 135, 255] {
        assert_eq!(
            Frame::decode(&raw(kind, 0, &[1, 2, 3])),
            Err(WireError::UnknownKind(kind))
        );
    }
}

#[test]
fn a_descriptor_count_that_is_not_the_kinds_is_refused() {
    let hs = |fds| {
        let mut body = 1u64.to_le_bytes().to_vec();
        body.push(1);
        raw(1, fds, &body)
    };
    assert!(Frame::decode(&hs(2)).is_ok());
    for got in [0u8, 1, 3, 255] {
        assert_eq!(
            Frame::decode(&hs(got)),
            Err(WireError::FdCount {
                kind: 1,
                expected: 2,
                got: usize::from(got)
            })
        );
    }
    assert_eq!(
        Frame::decode(&raw(129, 1, &[])),
        Err(WireError::FdCount {
            kind: 129,
            expected: 0,
            got: 1
        })
    );
    assert_eq!(
        WireError::FdCount {
            kind: 129,
            expected: 0,
            got: 1
        }
        .to_string(),
        "frame: kind 129 carries 0 descriptors, the frame says 1"
    );
}

#[test]
fn an_unknown_class_in_an_ensure_is_refused() {
    let mut body = Vec::new();
    body.extend_from_slice(&1u64.to_le_bytes());
    body.push(3);
    body.extend_from_slice(&[0; 8 + 4 + 4]);
    assert_eq!(
        Frame::decode(&raw(4, 0, &body)),
        Err(WireError::Invalid("unknown class"))
    );
}

#[test]
fn an_unknown_wait_outcome_is_refused() {
    assert_eq!(
        Frame::decode(&raw(131, 0, &[3])),
        Err(WireError::Invalid("unknown wait outcome"))
    );
}

#[test]
fn an_unknown_refusal_code_is_refused() {
    for code in [0u16, 7, 0x100, u16::MAX] {
        assert_eq!(
            Frame::decode(&raw(134, 0, &code.to_le_bytes())),
            Err(WireError::Invalid("unknown refusal code"))
        );
    }
}

#[test]
fn a_tree_reference_that_is_not_utf8_is_refused() {
    let mut body = Vec::new();
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&[0xFF, 0xFE]);
    body.extend_from_slice(&[0; 4 + 8 + 8 + 4]);
    assert_eq!(
        Frame::decode(&raw(2, 0, &body)),
        Err(WireError::Invalid("tree ref is not UTF-8"))
    );
}

#[test]
fn bytes_after_the_body_are_refused_for_a_reply_and_for_a_request() {
    assert_eq!(
        Frame::decode(&raw(129, 0, &[0])),
        Err(WireError::Invalid("bytes after the body"))
    );
    let mut body = 7u64.to_le_bytes().to_vec();
    body.push(0);
    assert_eq!(
        Frame::decode(&raw(5, 0, &body)),
        Err(WireError::Invalid("bytes after the body"))
    );
}

#[test]
fn a_body_that_ends_inside_a_field_is_refused_for_every_kind_at_every_length() {
    for m in all_messages() {
        let full = frame(m).encode().expect("encodes");
        let body = &full[4 + FRAME_HEADER_LEN..];
        let kind = full[7];
        let fds = full[24];
        for n in 0..body.len() {
            // A shorter body with a length field that says so: the field the cut falls in has
            // too few bytes, or (an `ensure` whose run count is cut off) too few runs.
            let cut = raw(kind, fds, &body[..n]);
            assert_eq!(
                Frame::decode(&cut),
                Err(WireError::Invalid("body ends inside a field")),
                "kind {kind}, body cut to {n} of {}",
                body.len()
            );
        }
    }
}

#[test]
fn an_ensure_that_names_more_runs_than_it_carries_is_refused() {
    let mut body = Vec::new();
    body.extend_from_slice(&1u64.to_le_bytes());
    body.push(0);
    body.extend_from_slice(&1u64.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    body.extend_from_slice(&[0; 8]);
    assert_eq!(
        Frame::decode(&raw(4, 0, &body)),
        Err(WireError::Invalid("body ends inside a field"))
    );
}

#[test]
fn a_tree_reference_longer_than_its_body_is_refused() {
    let mut body = 200u16.to_le_bytes().to_vec();
    body.extend_from_slice(b"short");
    assert_eq!(
        Frame::decode(&raw(2, 0, &body)),
        Err(WireError::Invalid("body ends inside a field"))
    );
}

#[test]
fn a_tree_reference_of_65535_bytes_is_sent_and_one_more_byte_is_refused() {
    let open = |len: usize| {
        frame(Message::OpenPack {
            tree: TreeRef("t".repeat(len)),
            pack: 1,
            holder: HolderId(2),
            region: region(),
        })
    };
    let max = open(65535);
    let bytes = max.encode().expect("fits");
    assert_eq!(Frame::decode(&bytes).expect("decodes").0, max);
    assert_eq!(
        open(65536).encode(),
        Err(WireError::Invalid("tree ref longer than 65535 bytes"))
    );
}

fn ensure_of(runs: usize) -> Frame {
    frame(Message::Ensure {
        pack: PackHandle(1),
        runs: vec![CellRun { first: 1, count: 1 }; runs],
        class: Class::Demand,
        job: JobId(1),
        rank: Rank(1),
    })
}

#[test]
fn the_largest_body_that_fits_is_sent_and_one_run_more_is_too_long() {
    // The fixed part of an ensure body is 25 bytes and a run is 8.
    let fits = (MAX_BODY - 25) / 8;
    let f = ensure_of(fits);
    let bytes = f.encode().expect("fits");
    assert!(bytes.len() - 4 - FRAME_HEADER_LEN <= MAX_BODY);
    assert_eq!(Frame::decode(&bytes).expect("decodes").0, f);
    assert_eq!(
        ensure_of(fits + 1).encode(),
        Err(WireError::TooLong(25 + 8 * (fits + 1)))
    );
}

#[test]
fn a_count_of_runs_that_could_never_fit_a_body_is_too_long() {
    let runs = MAX_BODY / 8 + 1;
    assert_eq!(ensure_of(runs).encode(), Err(WireError::TooLong(8 * runs)));
}

#[test]
fn the_largest_body_an_ensure_can_carry_is_accepted_by_decode() {
    // 25 + 8 * 131068 = 1048569 bytes, the most an ensure body can be within MAX_BODY.
    let mut body = Vec::new();
    body.extend_from_slice(&1u64.to_le_bytes());
    body.push(0);
    body.extend_from_slice(&1u64.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    let runs = (MAX_BODY - 25) / 8;
    body.extend_from_slice(&(runs as u32).to_le_bytes());
    for _ in 0..runs {
        body.extend_from_slice(&[0; 8]);
    }
    let (f, used) = Frame::decode(&raw(4, 0, &body)).expect("decodes");
    assert_eq!(used, 4 + FRAME_HEADER_LEN + body.len());
    match f.message {
        Message::Ensure { runs: r, .. } => assert_eq!(r.len(), runs),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_reply_is_accepted_only_under_the_epoch_of_the_connection() {
    assert!(Epoch(7).accepts(Epoch(7)));
    assert!(Epoch(0).accepts(Epoch(0)));
    assert!(!Epoch(7).accepts(Epoch(8)));
    assert!(!Epoch(7).accepts(Epoch(0)));
    assert_eq!(Epoch::default(), Epoch(0));
}

#[test]
fn wire_errors_say_what_was_wrong() {
    assert_eq!(
        WireError::Short { need: 9, have: 3 }.to_string(),
        "frame: 9 bytes needed, 3 present"
    );
    assert_eq!(
        WireError::TooLong(5).to_string(),
        "frame: body of 5 bytes is too long"
    );
    assert_eq!(
        WireError::Version(2).to_string(),
        "frame: version 2 is not supported"
    );
    assert_eq!(
        WireError::UnknownKind(9).to_string(),
        "frame: unknown kind 9"
    );
    assert_eq!(WireError::Invalid("x").to_string(), "frame: x");
}

#[test]
fn pack_handles_order_by_value() {
    assert!(PackHandle(1) < PackHandle(2));
}
