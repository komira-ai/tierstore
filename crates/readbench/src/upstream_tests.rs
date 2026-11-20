use std::sync::Arc;
use std::task::Poll;

use futures::StreamExt as _;
use futures::task::noop_waker_ref;
use tierstore_fetch_api::ids::PackKey;
use tierstore_fetch_api::upstream::DeviceRef;
use tierstore_format::ids::NodeId;

use super::{PartBody, PartTimer, Recorder, generation_of, status_of};
use crate::Error;
use crate::clock::ManualClock;

fn http(why: &str) -> Error {
    Error::Http {
        url: "u".into(),
        why: why.into(),
    }
}

#[test]
fn an_etag_of_32_hex_digits_is_the_generation_itself() {
    let g = generation_of("00112233445566778899aabbccddeeff");
    assert_eq!(g.0[0], 0x00);
    assert_eq!(g.0[15], 0xff);
    assert_eq!(g.0[7], 0x77);
}

#[test]
fn any_other_etag_is_the_digest_of_its_text() {
    let a = generation_of("not hex");
    assert_eq!(a, generation_of("not hex"));
    assert_ne!(a, generation_of("not hex either"));
    // 8 hex digits decode, but to too few bytes.
    assert_ne!(generation_of("00112233").0[..4], [0, 0x11, 0x22, 0x33]);
    // The first 16 bytes of the SHA-256 of "abc".
    assert_eq!(
        hex::encode(generation_of("abc").0),
        "ba7816bf8f01cfea414140de5dae2223"
    );
}

#[test]
fn a_status_is_read_from_the_error_the_client_makes_for_it() {
    assert_eq!(status_of(&http("status 503")), Some(503));
    assert_eq!(status_of(&http("status x")), None);
    assert_eq!(status_of(&http("transport: refused")), None);
    assert_eq!(status_of(&Error::Usage("status 503".into())), None);
}

/// A body whose source has nothing to give yet is pending, and is not recorded until it ends.
#[test]
fn a_body_with_nothing_yet_is_pending_and_unrecorded() {
    let recorder = Recorder::new();
    let timer = PartTimer {
        started: 0,
        clock: ManualClock::new(0),
        recorder: Arc::clone(&recorder),
        device: "n1d1".into(),
        class: "demand".into(),
    };
    let mut body = PartBody {
        inner: futures::stream::pending().boxed(),
        key: PackKey("k".into()),
        device: DeviceRef::Hot(NodeId(1)),
        want: 4,
        got: 0,
        first: None,
        done: false,
        timer,
    };
    let mut cx = std::task::Context::from_waker(noop_waker_ref());
    assert!(matches!(body.poll_next_unpin(&mut cx), Poll::Pending));
    assert!(recorder.parts().is_empty());
    drop(body);
    let parts = recorder.parts();
    assert_eq!(parts.len(), 1);
    assert!(!parts[0].ok);
}
