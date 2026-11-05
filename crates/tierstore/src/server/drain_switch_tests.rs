//! The note a node leaves of its late requests as it exits, and that a late request is counted
//! during the settle.

use std::time::Duration;

use bytes::Bytes;

use crate::Backend;

use super::ServerConfig;
use super::drain::{late_note, late_of};
use super::pipes::{Serving, exchange, read_rest, send, status_of, until};
use super::testing::BUCKET;

/// Asserts that the note says nothing when no request came late, and else one line for each front
/// that counted one, with the count and the front's name. It would catch a note that hides a late
/// request of the OCI front, or one that speaks when there is nothing to say (a drain that ended
/// clean leaves stderr as it was).
#[test]
fn the_note_names_each_front_that_counted_a_late_request() {
    assert_eq!(late_note(0, 0), "");
    assert_eq!(
        late_note(2, 0),
        "tierstore: 2 connections sent a request too late in the drain of the S3 front\n"
    );
    assert_eq!(
        late_note(0, 1),
        "tierstore: 1 connections sent a request too late in the drain of the OCI front\n"
    );
    assert_eq!(
        late_note(3, 4),
        "tierstore: 3 connections sent a request too late in the drain of the S3 front\n\
         tierstore: 4 connections sent a request too late in the drain of the OCI front\n"
    );
}

/// A settle that does not end while the test runs.
fn long_settle(config: &mut ServerConfig) {
    config.drain_lb_wait = Duration::from_secs(3600);
    config.drain_deadline = Duration::from_secs(7200);
}

/// Asserts that a late request can be counted during the settle, not only after it: once the
/// drain has begun an answer says `Connection: close`, the server then shuts its end of that
/// connection and lingers, and a request the client writes after the end of the stream is read,
/// dropped and counted while the server still accepts connections. And that the exit note of a
/// node without the OCI front names the S3 front's count alone, read from the server's counter.
/// It would catch a note or a page that claimed the count cannot rise before the settle ends.
#[tokio::test(start_paused = true)]
async fn a_request_after_a_closing_answer_is_counted_during_the_settle() {
    let serving = Serving::start(long_settle).await;
    let rig = &serving.rig;
    let _ = rig.hot_mem.put("warm", Bytes::from_static(b"w")).await;
    let mut client = serving.connect().unwrap();
    rig.server.begin_drain();
    let uri = format!("/{BUCKET}/warm");
    let answer = exchange(&mut client, rig, "GET", &uri, &[], b"").await;
    assert_eq!(status_of(&answer), 200);
    assert!(answer.contains("connection: close"), "{answer}");
    let mut rest = Vec::new();
    read_rest(&mut client, &mut rest).await;
    assert!(rest.is_empty(), "the server shut its end after the answer");
    assert_eq!(late_of(&rig.server, None), "");

    send(&mut client, rig, "GET", &uri, &[], b"").await;
    until(&rig.server, |server| server.late_requests() == 1).await;
    assert!(serving.connect().is_some(), "the settle is not over");
    assert_eq!(
        late_of(&rig.server, None),
        "tierstore: 1 connections sent a request too late in the drain of the S3 front\n"
    );
}
