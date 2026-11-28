//! The bounds of the S3 front at and past their limits: a GET over the reads in flight is refused
//! unopened and counted, a body held whole waits for a permit of the bodies and is refused without
//! one, a body over its limit is refused, a put whose conditions are not served is refused unread,
//! the documents that are parsed whole have a bounded tree, and the accept loop takes no
//! connection while the most are open. Each gauge is read from the text of `/metricz`.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::StatusCode;
use hyper::body::{Body, Frame};
use tokio::io::AsyncWriteExt as _;

use super::super::auth::sign;
use super::super::ops::WHOLE_BODY_LIMIT;
use super::super::pipes::{Serving, attach, read_response, status_of, until};
use super::super::testing::{BUCKET, Rig};
use super::super::xml::{
    COMPLETE_NODES, CREATE_NODES, DELETE_NODES, parse_complete, parse_create, parse_delete,
};
use super::{bound_families, limit_families};
use crate::Backend as _;

/// The line of `family` with `value` in the text of `/metricz` of the rig's server.
fn shows(rig: &Rig, family: &str, value: u64) -> bool {
    let text = rig.server.front_families();
    text.contains(&format!("\n{family} {value}\n"))
}

/// Every call either tier of the rig has had.
fn tier_calls(rig: &Rig) -> usize {
    rig.hot.call_log().len() + rig.cold.call_log().len()
}

/// Asserts the text of the families of the bounds in full: three gauges and a counter, each with
/// its type line.
#[test]
fn the_families_are_three_gauges_and_the_counter_of_refused_reads() {
    assert_eq!(
        bound_families(3, 2, 7, 1),
        "# TYPE tierstore_s3_connections_open gauge\n\
         tierstore_s3_connections_open 3\n\
         # TYPE tierstore_s3_reads_in_flight gauge\n\
         tierstore_s3_reads_in_flight 2\n\
         # TYPE tierstore_s3_reads_refused_total counter\n\
         tierstore_s3_reads_refused_total 7\n\
         # TYPE tierstore_s3_bodies_in_flight gauge\n\
         tierstore_s3_bodies_in_flight 1\n"
    );
}

/// Asserts that a GET that finds every permit of the reads taken is answered 503 `SlowDown` with
/// `Retry-After`, makes no call to either tier and is counted; that the read in flight holds its
/// permit until the body of its answer is dropped, unread; and that the next GET then reads.
#[tokio::test]
async fn a_get_over_the_reads_in_flight_is_refused_unopened_until_a_read_ends() {
    let rig = Rig::with(1 << 20, |config| config.max_concurrent_reads = 1).await;
    assert_eq!(rig.put("obj", b"abcd").await.status, StatusCode::OK);
    let uri = format!("/{BUCKET}/obj");
    let first = rig
        .server
        .handle(sign(&rig.config, "GET", &uri, &[], b""))
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    assert!(shows(&rig, "tierstore_s3_reads_in_flight", 1));

    let calls = tier_calls(&rig);
    let over = rig.get("obj", None).await;
    assert_eq!(over.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(over.code(), "SlowDown");
    assert_eq!(over.header("retry-after"), "1");
    assert_eq!(tier_calls(&rig), calls);
    assert!(shows(&rig, "tierstore_s3_reads_refused_total", 1));
    assert!(shows(&rig, "tierstore_s3_reads_in_flight", 1));

    drop(first);
    assert!(shows(&rig, "tierstore_s3_reads_in_flight", 0));
    let next = rig.get("obj", None).await;
    assert_eq!((next.status, next.body.as_str()), (StatusCode::OK, "abcd"));
    assert!(shows(&rig, "tierstore_s3_reads_in_flight", 0));
    assert!(shows(&rig, "tierstore_s3_reads_refused_total", 1));
}

/// Asserts that a HEAD and an answer held whole hold no permit of the reads: with the bound at
/// one and the only permit taken by a read in flight, a HEAD is answered.
#[tokio::test]
async fn a_head_takes_no_permit_of_the_reads() {
    let rig = Rig::with(1 << 20, |config| config.max_concurrent_reads = 1).await;
    assert_eq!(rig.put("obj", b"abcd").await.status, StatusCode::OK);
    let uri = format!("/{BUCKET}/obj");
    let held = rig
        .server
        .handle(sign(&rig.config, "GET", &uri, &[], b""))
        .await;
    let head = rig.send("HEAD", &uri, &[], b"").await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-length"), "4");
    drop(held);
}

/// The `CreateBucketConfiguration` of the served bucket's region, a body read whole.
const CREATE: &[u8] = b"<CreateBucketConfiguration><LocationConstraint>us-east-1\
                        </LocationConstraint></CreateBucketConfiguration>";

/// Asserts that a body held whole holds a permit of the bodies: with the only one taken and no
/// wait, a CreateBucket with a body is answered 503 `SlowDown` with `Retry-After`, while a call
/// with no body (a listing) is answered; once the permit is free, the CreateBucket is answered
/// and gives it back. The gauge shows the permit taken, then none.
#[tokio::test]
async fn a_body_held_whole_is_refused_slow_down_while_every_permit_is_taken() {
    let rig = Rig::with(1 << 20, |config| {
        config.max_concurrent_puts = 1;
        config.put_permit_wait = std::time::Duration::ZERO;
    })
    .await;
    let bucket = format!("/{BUCKET}");
    let taken = Arc::clone(&rig.server.puts)
        .try_acquire_owned()
        .expect("the only permit");
    assert!(shows(&rig, "tierstore_s3_bodies_in_flight", 1));

    let over = rig.send("PUT", &bucket, &[], CREATE).await;
    assert_eq!(over.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(over.code(), "SlowDown");
    assert_eq!(over.header("retry-after"), "1");
    assert_eq!(rig.list("").await.status, StatusCode::OK);

    drop(taken);
    assert!(shows(&rig, "tierstore_s3_bodies_in_flight", 0));
    assert_eq!(
        rig.send("PUT", &bucket, &[], CREATE).await.status,
        StatusCode::OK
    );
    assert_eq!(rig.server.put_permits_free(), 1);
}

/// Asserts the limit of a body held whole: a CreateBucket body of one byte over
/// [`WHOLE_BODY_LIMIT`] is refused with 413 `EntityTooLarge`, while one of exactly the limit is
/// read (and then refused as no `CreateBucketConfiguration`), so the limit is a byte and not
/// less; and that the limit is below the server's own, which a streamed put keeps.
#[tokio::test]
async fn a_body_held_whole_over_its_limit_is_refused_too_large() {
    let rig = Rig::new(1 << 20).await;
    assert!(WHOLE_BODY_LIMIT < rig.config.max_request_bytes);
    let bucket = format!("/{BUCKET}");
    let over = rig
        .send("PUT", &bucket, &[], &vec![b' '; WHOLE_BODY_LIMIT + 1])
        .await;
    assert_eq!(over.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(over.code(), "EntityTooLarge");
    let at = rig
        .send("PUT", &bucket, &[], &vec![b'x'; WHOLE_BODY_LIMIT])
        .await;
    assert_ne!(at.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_ne!(at.status, StatusCode::OK);
}

/// A body that counts how often it is asked for a piece, and has one piece to give.
struct Watched {
    asked: Arc<AtomicUsize>,
    piece: Option<Bytes>,
}

impl Body for Watched {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(self.piece.take().map(|piece| Ok(Frame::data(piece))))
    }
}

/// Asserts that a put whose conditions the front does not serve (`If-None-Match` of a tag) is
/// refused with 501 `NotImplemented` before one piece of its body is asked for, so a body of any
/// length is not held to be refused, and nothing is stored.
#[tokio::test]
async fn a_put_whose_conditions_are_not_served_is_refused_unread() {
    let rig = Rig::new(1 << 20).await;
    let uri = format!("/{BUCKET}/conditional");
    let body = b"never read";
    let (parts, _) = sign(
        &rig.config,
        "PUT",
        &uri,
        &[("if-none-match", "\"a-tag\"")],
        body,
    )
    .into_parts();
    let asked = Arc::new(AtomicUsize::new(0));
    let watched = Watched {
        asked: Arc::clone(&asked),
        piece: Some(Bytes::from_static(body)),
    };
    let refused = rig
        .send_raw(hyper::Request::from_parts(parts, watched))
        .await;
    assert_eq!(refused.status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(refused.code(), "NotImplemented");
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert!(rig.hot_mem.get("conditional", None).await.is_err());
}

/// The part `number` of a completion, with its ETag and every checksum S3 sends, laid out on
/// lines as a client that indents its XML writes it.
fn part(number: u32) -> String {
    let sum = "A".repeat(43) + "=";
    format!(
        "\n  <Part>\n    <PartNumber>{number}</PartNumber>\n    \
         <ETag>&quot;{number:032x}&quot;</ETag>\n    <ChecksumCRC32>{sum}</ChecksumCRC32>\n    \
         <ChecksumCRC32C>{sum}</ChecksumCRC32C>\n    \
         <ChecksumCRC64NVME>{sum}</ChecksumCRC64NVME>\n    \
         <ChecksumSHA1>{sum}</ChecksumSHA1>\n    <ChecksumSHA256>{sum}</ChecksumSHA256>\n  </Part>"
    )
}

/// Asserts the bound of the tree of a completion's document: one of the 10,000 parts S3 takes,
/// each with its ETag and five checksums and indented, is parsed whole and is under
/// [`WHOLE_BODY_LIMIT`]; a document that packs more than [`COMPLETE_NODES`] nodes into its bytes
/// is not that document, though it is far under the limit.
#[test]
fn a_completion_of_more_nodes_than_its_parts_take_is_refused() {
    let parts: String = (1..=10_000).map(part).collect();
    let most = format!("<CompleteMultipartUpload>{parts}\n</CompleteMultipartUpload>");
    assert!(most.len() <= WHOLE_BODY_LIMIT, "{}", most.len());
    let parsed = parse_complete(most.as_bytes()).expect("10,000 parts");
    assert_eq!(parsed.len(), 10_000);
    assert_eq!(parsed[9_999], (10_000, format!("{:032x}", 10_000)));

    let packed = "<x/>".repeat(COMPLETE_NODES as usize);
    let over = format!("<CompleteMultipartUpload>{packed}</CompleteMultipartUpload>");
    assert!(over.len() < WHOLE_BODY_LIMIT);
    assert_eq!(parse_complete(over.as_bytes()), None);
}

/// Asserts the bound of the tree of a DeleteObjects document: the 1000 objects S3 takes, each
/// with a version and indented, are parsed; a document that packs more than [`DELETE_NODES`]
/// nodes into its bytes is not that document.
#[test]
fn a_delete_of_more_nodes_than_its_objects_take_is_refused() {
    let objects: String = (0..1000)
        .map(|n| {
            format!("\n  <Object>\n    <Key>k{n}</Key>\n    <VersionId>v</VersionId>\n  </Object>")
        })
        .collect();
    let most = format!("<Delete>\n  <Quiet>true</Quiet>{objects}\n</Delete>");
    let parsed = parse_delete(most.as_bytes()).expect("1000 objects");
    assert_eq!(parsed.objects.len(), 1000);
    assert!(parsed.quiet);

    let packed = "<x/>".repeat(DELETE_NODES as usize);
    let over = format!("<Delete>{packed}</Delete>");
    assert_eq!(parse_delete(over.as_bytes()), None);
}

/// Asserts the bound of the tree of a CreateBucket document: the served region's is parsed, and
/// one that packs more than [`CREATE_NODES`] nodes into its bytes is not that document.
#[test]
fn a_create_bucket_of_more_nodes_than_its_elements_take_is_refused() {
    assert_eq!(parse_create(CREATE).as_deref(), Some("us-east-1"));
    let packed = "<x/>".repeat(CREATE_NODES as usize);
    let over = format!("<CreateBucketConfiguration>{packed}</CreateBucketConfiguration>");
    assert_eq!(parse_create(over.as_bytes()), None);
}

/// Asserts that the accept loop waits for room while the most connections are open: with a bound
/// of one and one connection open, the wait for room stays pending, and it ends once that
/// connection closes.
#[tokio::test(start_paused = true)]
async fn the_room_for_a_connection_waits_while_the_most_are_open() {
    let rig = Rig::with(1 << 20, |config| config.max_connections = 1).await;
    let client = attach(&rig.server);
    until(&rig.server, |server| server.open_connections() == 1).await;
    assert!(shows(&rig, "tierstore_s3_connections_open", 1));
    let room = rig.server.room_for_a_connection();
    let mut room = std::pin::pin!(room);
    assert!(futures::poll!(room.as_mut()).is_pending());

    drop(client);
    room.await;
    assert_eq!(rig.server.open_connections(), 0);
    assert!(shows(&rig, "tierstore_s3_connections_open", 0));
}

/// Asserts that the accept loop never has more connections open than the bound: three clients
/// connect at once to a server that holds one, each sends a request and keeps its connection
/// open, and each is answered only once the client before it has closed its connection, with
/// exactly one connection open while each answer is read.
#[tokio::test(start_paused = true)]
async fn the_accept_loop_holds_no_more_connections_than_the_bound() {
    let serving = Serving::start(|config| config.max_connections = 1).await;
    let server = Arc::clone(&serving.rig.server);
    let mut clients = Vec::new();
    for _ in 0..3 {
        let mut client = serving.connect().expect("a server that takes connections");
        let probe = "GET /healthz HTTP/1.1\r\nhost: tier.test\r\n\r\n";
        client.write_all(probe.as_bytes()).await.expect("written");
        clients.push(client);
    }
    for mut client in clients {
        let answer = read_response(&mut client).await;
        assert_eq!(status_of(&answer), 200, "{answer}");
        // This connection is open, kept alive, and the one before it was closed before it was
        // taken: exactly one.
        assert_eq!(server.open_connections(), 1);
        drop(client);
    }
}

/// Asserts the text of the gauges of the limits in full: three gauges, each with its type line.
#[test]
fn the_limits_are_three_gauges_each_with_its_type() {
    assert_eq!(
        limit_families(8192, 512, 32),
        "# TYPE tierstore_s3_connections_limit gauge\n\
         tierstore_s3_connections_limit 8192\n\
         # TYPE tierstore_s3_reads_limit gauge\n\
         tierstore_s3_reads_limit 512\n\
         # TYPE tierstore_s3_bodies_limit gauge\n\
         tierstore_s3_bodies_limit 32\n"
    );
}

/// Asserts that `/metricz` shows each limit as the server was started with it, beside what is
/// held: a GET in flight holds one of the two reads, so the reads held stand at one under a limit
/// of two while its answer is open, and at none once it is dropped, while the limit stays.
#[tokio::test]
async fn metricz_shows_each_limit_beside_what_is_held() {
    let rig = Rig::with(1 << 20, |config| {
        config.max_connections = 7;
        config.max_concurrent_reads = 2;
        config.max_concurrent_puts = 5;
    })
    .await;
    assert_eq!(rig.put("obj", b"abcd").await.status, StatusCode::OK);
    let text = rig.server.metrics_probe();
    for line in [
        "\ntierstore_s3_connections_limit 7\n",
        "\ntierstore_s3_reads_limit 2\n",
        "\ntierstore_s3_bodies_limit 5\n",
        "\ntierstore_s3_reads_in_flight 0\n",
    ] {
        assert!(text.contains(line), "{line:?} not in:\n{text}");
    }
    let uri = format!("/{BUCKET}/obj");
    let open = rig
        .server
        .handle(sign(&rig.config, "GET", &uri, &[], b""))
        .await;
    assert_eq!(open.status(), StatusCode::OK);
    let held = rig.server.metrics_probe();
    assert!(
        held.contains("\ntierstore_s3_reads_in_flight 1\n"),
        "{held}"
    );
    assert!(held.contains("\ntierstore_s3_reads_limit 2\n"), "{held}");
    drop(open);
    let after = rig.server.metrics_probe();
    assert!(
        after.contains("\ntierstore_s3_reads_in_flight 0\n"),
        "{after}"
    );
    assert!(after.contains("\ntierstore_s3_reads_limit 2\n"), "{after}");
}
