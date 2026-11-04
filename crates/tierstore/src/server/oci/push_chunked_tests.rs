//! Chunked uploads to the OCI front: `PATCH` of chunks, `PUT` of the end, `DELETE` of the upload,
//! over an in-memory store, with no socket.

use std::time::Duration;

use hyper::header::HeaderMap;
use hyper::{Method, StatusCode};

use super::DEFAULT_STAGE_READ as STAGE_READ;
use super::push_tests::{Rig, digest_of, front_over, full, lengthy, login, scripted, unsized_body};
use crate::server::testing::Step;
use crate::{Backend, Error, MemoryBackend, Op};

/// The path that ends the upload `id` with `digest`.
fn end(id: &str, digest: &str) -> String {
    format!("{}?digest={digest}", Rig::session(id))
}

/// Asserts the whole of a chunked upload: each chunk is stored as it comes, as an object named by
/// its offset, and the answer says how many bytes the upload holds (a chunk with a `Content-Range`
/// that starts where the upload is, and one with none); the `PUT` makes the blob of the chunks in
/// order, serves it back whole, and leaves nothing of the upload.
#[tokio::test]
async fn chunks_are_stored_as_they_come_and_the_put_makes_the_blob() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    let first = rig.ask(Method::PATCH, &session, &[], full(b"hello ")).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.text());
    assert_eq!(first.header("range"), "0-5");
    assert_eq!(first.header("location"), session);
    assert_eq!(first.header("docker-upload-uuid"), id);
    let ranged = [("content-range", "6-10")];
    let second = rig
        .ask(Method::PATCH, &session, &ranged, full(b"world"))
        .await;
    assert_eq!(second.status, StatusCode::ACCEPTED, "{}", second.text());
    assert_eq!(second.header("range"), "0-10");
    let dir = format!("oci/uploads/{id}");
    let mut held = rig.staged().await;
    held.sort();
    let chunks = [
        format!("{dir}/begin"),
        format!("{dir}/c{:020}", 0),
        format!("{dir}/c{:020}", 6),
    ];
    assert_eq!(held, chunks);

    let digest = digest_of(b"hello world");
    let done = rig.send(Method::PUT, &end(&id, &digest)).await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
    assert_eq!(done.header("location"), format!("/v2/app/blobs/{digest}"));
    assert_eq!(done.header("docker-content-digest"), digest);
    assert_eq!(
        rig.blob(&digest).await.as_deref(),
        Some(b"hello world".as_slice())
    );
    assert!(rig.staged().await.is_empty());
    let pulled = rig
        .send(Method::GET, &format!("/v2/app/blobs/{digest}"))
        .await;
    assert_eq!(pulled.body, b"hello world");
    assert_eq!(rig.metrics.requests("upload", "accepted"), 3);
    assert_eq!(rig.metrics.requests("upload", "created"), 1);
}

/// Asserts that the `PUT` that ends an upload may carry the last chunk: the blob is the chunks
/// stored and then the body.
#[tokio::test]
async fn the_last_put_may_carry_the_last_chunk() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    rig.ask(Method::PATCH, &session, &[], full(b"ab")).await;
    let digest = digest_of(b"abcd");
    let ranged = [("content-range", "2-3")];
    let done = rig
        .ask(Method::PUT, &end(&id, &digest), &ranged, full(b"cd"))
        .await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
    assert_eq!(rig.blob(&digest).await.as_deref(), Some(b"abcd".as_slice()));
    assert!(rig.staged().await.is_empty());
}

/// Asserts that an upload with no chunk, ended by a `PUT` that carries the whole blob, makes the
/// blob (the monolithic upload in two calls).
#[tokio::test]
async fn a_put_alone_can_carry_the_whole_blob() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let digest = digest_of(b"whole");
    let done = rig
        .ask(Method::PUT, &end(&id, &digest), &[], full(b"whole"))
        .await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
    assert_eq!(
        rig.blob(&digest).await.as_deref(),
        Some(b"whole".as_slice())
    );
}

/// Asserts that a chunk that does not start where the upload is (it starts over, or it leaves a
/// gap) is refused 416 `BLOB_UPLOAD_INVALID` with the `Range` the upload holds and its `Location`,
/// that nothing is stored for it, and that the upload goes on and ends with the right bytes.
#[tokio::test]
async fn a_chunk_out_of_order_is_416_and_the_upload_is_as_it_was() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    rig.ask(Method::PATCH, &session, &[], full(b"01234")).await;
    for range in ["0-4", "9-12", "6-9"] {
        let headers = [("content-range", range)];
        let answer = rig
            .ask(Method::PATCH, &session, &headers, full(b"abcd"))
            .await;
        let (_, detail) =
            answer.assert_error(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID");
        assert_eq!(detail["received"], 5, "{range}");
        assert_eq!(answer.header("range"), "0-4");
        assert_eq!(answer.header("location"), session);
        assert_eq!(answer.header("docker-upload-uuid"), id);
    }
    assert_eq!(rig.staged().await.len(), 2);
    let headers = [("content-range", "5-8")];
    let next = rig
        .ask(Method::PATCH, &session, &headers, full(b"5678"))
        .await;
    assert_eq!(next.header("range"), "0-8");
    let digest = digest_of(b"012345678");
    let done = rig.send(Method::PUT, &end(&id, &digest)).await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
    assert_eq!(rig.metrics.requests("upload", "range_not_satisfiable"), 3);
}

/// Asserts that a `Content-Range` that is not a range is 416 too: text that is not `<first>-<last>`,
/// a last byte before the first, a range that says more or fewer bytes than the chunk has; and that
/// a range is taken for a chunk whose length is not known, since the bytes decide.
#[tokio::test]
async fn a_chunk_range_that_is_wrong_is_416() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    for range in ["abc", "bytes 0-2", "5-2", "0-4", "0-1", "0-", "-3", "0-x"] {
        let headers = [("content-range", range)];
        let answer = rig
            .ask(Method::PATCH, &session, &headers, full(b"abc"))
            .await;
        answer.assert_error(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID");
    }
    assert_eq!(rig.staged().await.len(), 1);
    let headers = [("content-range", "0-2")];
    let body = unsized_body(&[b"a", b"bc"]);
    let accepted = rig.ask(Method::PATCH, &session, &headers, body).await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED, "{}", accepted.text());
    assert_eq!(accepted.header("range"), "0-2");
}

/// Asserts that a `Content-Range` is held to the bytes of a chunk whose length is not known: a
/// body of more or fewer bytes than the range says is 416, stored as nothing, and the upload is as
/// it was; and that the same holds for the body of the `PUT` that ends the upload, which then ends
/// it when the range is right.
#[tokio::test]
async fn a_range_is_held_to_the_bytes_of_a_body_of_unknown_length() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    let headers = [("content-range", "0-2")];
    for body in [unsized_body(&[b"a", b"bcd"]), unsized_body(&[b"a", b"b"])] {
        let answer = rig.ask(Method::PATCH, &session, &headers, body).await;
        let (_, detail) =
            answer.assert_error(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID");
        assert_eq!(detail["received"], 0);
    }
    let none = rig
        .ask(Method::PATCH, &session, &headers, unsized_body(&[]))
        .await;
    none.assert_error(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID");
    assert_eq!(rig.staged().await.len(), 1);
    let first = rig.ask(Method::PATCH, &session, &[], full(b"abc")).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.text());

    let digest = digest_of(b"abcdef");
    let target = end(&id, &digest);
    let headers = [("content-range", "3-5")];
    for body in [unsized_body(&[b"d", b"efg"]), unsized_body(&[b"d", b"e"])] {
        let answer = rig.ask(Method::PUT, &target, &headers, body).await;
        let (_, detail) =
            answer.assert_error(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID");
        assert_eq!(detail["received"], 3);
    }
    assert_eq!(rig.staged().await.len(), 2);
    assert_eq!(rig.blob(&digest).await, None);
    let body = unsized_body(&[b"d", b"ef"]);
    let done = rig.ask(Method::PUT, &target, &headers, body).await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
    assert_eq!(
        rig.blob(&digest).await.as_deref(),
        Some(b"abcdef".as_slice())
    );
    assert!(rig.staged().await.is_empty());
}

/// Asserts that an empty chunk whose length is not known stores nothing, as one of known length
/// does: the next chunk is stored at the same offset and the upload ends with its bytes.
#[tokio::test]
async fn an_empty_chunk_of_unknown_length_stores_nothing() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    for body in [unsized_body(&[]), unsized_body(&[b""])] {
        let answer = rig.ask(Method::PATCH, &session, &[], body).await;
        assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.text());
        assert_eq!(answer.header("range"), "0-0");
    }
    assert_eq!(rig.staged().await.len(), 1);
    let next = rig.ask(Method::PATCH, &session, &[], full(b"ab")).await;
    assert_eq!(next.status, StatusCode::ACCEPTED, "{}", next.text());
    assert_eq!(next.header("range"), "0-1");
    let digest = digest_of(b"ab");
    let done = rig.send(Method::PUT, &end(&id, &digest)).await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
}

/// Asserts that the end of an upload reads each stored chunk in ranges of at most `STAGE_READ`
/// bytes, by the size the listing gave, and never a whole object: a chunk of two and a half times
/// that is read in three ranges, and the blob is the chunks' bytes.
#[tokio::test]
async fn the_chunks_of_an_upload_are_read_in_bounded_ranges() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    let size = usize::try_from(STAGE_READ).expect("a size") * 5 / 2;
    let bytes: Vec<u8> = (0..size).map(|at| (at % 251) as u8).collect();
    let chunk = rig.ask(Method::PATCH, &session, &[], full(&bytes)).await;
    assert_eq!(chunk.status, StatusCode::ACCEPTED, "{}", chunk.text());
    let digest = digest_of(&bytes);
    let done = rig.send(Method::PUT, &end(&id, &digest)).await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
    let ranges = rig.faulty.get_ranges();
    let half = STAGE_READ / 2;
    assert_eq!(
        ranges,
        [
            Some((0, STAGE_READ)),
            Some((STAGE_READ, STAGE_READ)),
            Some((2 * STAGE_READ, half)),
        ]
    );
    assert_eq!(rig.blob(&digest).await.as_deref(), Some(bytes.as_slice()));
}

/// Asserts that the size of a range read at the end of an upload is the one the front is given:
/// a chunk of ten bytes with a range of four is read in ranges of 4, 4 and 2, a range of zero is
/// taken as one byte, and the blob is the chunk's bytes either way.
#[tokio::test]
async fn the_range_of_a_read_of_a_chunk_is_the_one_the_front_is_given() {
    for (given, want) in [
        (4, vec![Some((0, 4)), Some((4, 4)), Some((8, 2))]),
        (0, (0..10).map(|at| Some((at, 1))).collect()),
    ] {
        let rig = Rig::new().with_stage_read(given);
        let id = rig.begin().await;
        let session = Rig::session(&id);
        let chunk = rig
            .ask(Method::PATCH, &session, &[], full(b"0123456789"))
            .await;
        assert_eq!(chunk.status, StatusCode::ACCEPTED, "{}", chunk.text());
        let digest = digest_of(b"0123456789");
        let done = rig.send(Method::PUT, &end(&id, &digest)).await;
        assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
        assert_eq!(rig.faulty.get_ranges(), want, "range {given}");
        let blob = rig.blob(&digest).await;
        assert_eq!(blob.as_deref(), Some(&b"0123456789"[..]));
    }
}

/// Asserts that a store that says the blob exists without reading the body (as a proxy whose cold
/// tier has it does) does not make the push a success: the stored bytes and the body are read to
/// their end and must hash to the digest. Wrong bytes are 400 `DIGEST_INVALID` and a cut body is
/// 400 `BLOB_UPLOAD_INVALID`, each leaving the upload as it was; the right bytes are 201 and
/// delete the upload.
#[tokio::test]
async fn a_blob_the_store_has_is_still_checked_against_the_body() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    rig.ask(Method::PATCH, &session, &[], full(b"abc")).await;
    let digest = digest_of(b"abcdef");
    let target = end(&id, &digest);

    let exists = || Error::AlreadyExists("blob".to_owned());
    rig.faulty.fail_next_with(Op::Put, exists());
    let wrong = rig.ask(Method::PUT, &target, &[], full(b"xyz")).await;
    wrong.assert_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID");
    assert_eq!(rig.staged().await.len(), 2);

    rig.faulty.fail_next_with(Op::Put, exists());
    let steps = vec![Step::Data(b"de"), Step::Fail];
    let cut = rig.ask(Method::PUT, &target, &[], scripted(steps)).await;
    cut.assert_error(StatusCode::BAD_REQUEST, "BLOB_UPLOAD_INVALID");
    assert_eq!(rig.staged().await.len(), 2);

    rig.faulty.fail_next_with(Op::Put, exists());
    let right = rig.ask(Method::PUT, &target, &[], full(b"def")).await;
    assert_eq!(right.status, StatusCode::CREATED, "{}", right.text());
    assert!(rig.staged().await.is_empty());
}

/// Asserts that an empty chunk is accepted and stores nothing, so a second one is no conflict.
#[tokio::test]
async fn an_empty_chunk_is_accepted_and_stores_nothing() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    for _ in 0..2 {
        let answer = rig.ask(Method::PATCH, &session, &[], full(b"")).await;
        assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.text());
        assert_eq!(answer.header("range"), "0-0");
    }
    assert_eq!(rig.staged().await.len(), 1);
}

/// Asserts that an id that is not an open upload is 404 `BLOB_UPLOAD_UNKNOWN` on `PATCH`, `PUT` and
/// `DELETE`: one that was never made, one that is not an id this front makes (which no key is built
/// from), and one that ended; and that no call stores anything.
#[tokio::test]
async fn an_upload_that_is_not_open_is_404() {
    let rig = Rig::new();
    let ended = rig.begin().await;
    let digest = digest_of(b"x");
    rig.ask(Method::PUT, &end(&ended, &digest), &[], full(b"x"))
        .await;
    let never = "0".repeat(32);
    for id in [never.as_str(), "not-an-id", "..", ended.as_str()] {
        let calls = [
            (Method::PATCH, Rig::session(id)),
            (Method::PUT, end(id, &digest)),
            (Method::DELETE, Rig::session(id)),
        ];
        for (method, target) in calls {
            let answer = rig.ask(method.clone(), &target, &[], full(b"x")).await;
            answer.assert_error(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN");
        }
    }
    assert!(rig.staged().await.is_empty());
    assert_eq!(rig.metrics.requests("upload", "not_found"), 12);
}

/// Asserts that the end of an upload is checked against all its bytes, the chunks and the body
/// together: a digest that is not theirs is 400 `DIGEST_INVALID` naming the digest found, no blob
/// is stored, the chunks stay, and the same upload ends with the right digest.
#[tokio::test]
async fn the_digest_of_the_put_must_be_that_of_all_the_bytes() {
    let rig = Rig::new();
    let id = rig.begin().await;
    rig.ask(Method::PATCH, &Rig::session(&id), &[], full(b"abc"))
        .await;
    let wrong = digest_of(b"abd");
    let answer = rig
        .ask(Method::PUT, &end(&id, &wrong), &[], full(b"def"))
        .await;
    let (_, detail) = answer.assert_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID");
    assert_eq!(detail["digest"], wrong);
    assert_eq!(detail["found"], digest_of(b"abcdef"));
    assert!(rig.objects().await.is_empty());
    assert_eq!(rig.staged().await.len(), 2);
    let right = digest_of(b"abcdef");
    let done = rig
        .ask(Method::PUT, &end(&id, &right), &[], full(b"def"))
        .await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
    assert_eq!(
        rig.blob(&right).await.as_deref(),
        Some(b"abcdef".as_slice())
    );
}

/// Asserts that a `PUT` with no digest, or one that is not `sha256:` and 64 hex digits, is 400
/// `DIGEST_INVALID` and leaves the upload open; and that a `PUT` whose `Content-Range` is out of
/// order is 416 and leaves it too.
#[tokio::test]
async fn a_put_without_a_good_digest_leaves_the_upload_open() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    rig.ask(Method::PATCH, &session, &[], full(b"abc")).await;
    for target in [
        session.clone(),
        format!("{session}?digest=sha256:abc"),
        format!("{session}?other=1"),
    ] {
        let answer = rig.ask(Method::PUT, &target, &[], full(b"")).await;
        answer.assert_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID");
    }
    let headers = [("content-range", "0-2")];
    let digest = digest_of(b"abcabc");
    let late = rig
        .ask(Method::PUT, &end(&id, &digest), &headers, full(b"abc"))
        .await;
    late.assert_error(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID");
    assert_eq!(rig.staged().await.len(), 2);
    assert!(rig.objects().await.is_empty());
}

/// Asserts that `DELETE` cancels an upload: 204, the chunks and the session are deleted, no blob
/// is made, and a second `DELETE` is 404.
#[tokio::test]
async fn a_delete_cancels_the_upload_and_deletes_what_it_stored() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    rig.ask(Method::PATCH, &session, &[], full(b"abc")).await;
    assert_eq!(rig.staged().await.len(), 2);
    let gone = rig.send(Method::DELETE, &session).await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.text());
    assert_eq!(gone.header("content-length"), "0");
    assert!(rig.staged().await.is_empty());
    assert!(rig.objects().await.is_empty());
    let again = rig.send(Method::DELETE, &session).await;
    again.assert_error(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN");
    assert_eq!(rig.metrics.requests("upload", "no_content"), 1);
}

/// Asserts that a chunk whose body is cut stores nothing (400 `BLOB_UPLOAD_INVALID`), the upload
/// is as it was, and the chunk can be sent again.
#[tokio::test]
async fn a_cut_chunk_stores_nothing_and_the_upload_goes_on() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    let steps = vec![Step::Data(b"ab"), Step::Fail];
    let cut = rig.ask(Method::PATCH, &session, &[], scripted(steps)).await;
    cut.assert_error(StatusCode::BAD_REQUEST, "BLOB_UPLOAD_INVALID");
    assert_eq!(rig.staged().await.len(), 1);
    let again = rig.ask(Method::PATCH, &session, &[], full(b"ab")).await;
    assert_eq!(again.header("range"), "0-1");
}

/// Asserts that a chunk body that stalls (the paused clock of the test passes the idle time) is
/// refused 400 `BLOB_UPLOAD_INVALID` and stores nothing.
#[tokio::test(start_paused = true)]
async fn a_chunk_that_stalls_stores_nothing() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let steps = vec![Step::Data(b"ab"), Step::Wait(Duration::from_secs(3600))];
    let session = Rig::session(&id);
    let stalled = rig.ask(Method::PATCH, &session, &[], scripted(steps)).await;
    stalled.assert_error(StatusCode::BAD_REQUEST, "BLOB_UPLOAD_INVALID");
    assert_eq!(rig.staged().await.len(), 1);
}

/// Asserts that a chunk stored at the offset by another request between the listing and the put
/// is 416, as an out-of-order chunk is.
#[tokio::test]
async fn a_chunk_stored_at_the_same_offset_meanwhile_is_416() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let exists = Error::AlreadyExists("c".to_owned());
    rig.faulty.fail_next_with(Op::Put, exists);
    let answer = rig
        .ask(Method::PATCH, &Rig::session(&id), &[], full(b"ab"))
        .await;
    answer.assert_error(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID");
}

/// Asserts that a store that fails is 503 `UNAVAILABLE` where the upload is looked up (its object,
/// its chunks) and where a chunk is stored, and a cancel's deletes that fail are said and not
/// answered: the upload is gone from the client's side (204).
#[tokio::test]
async fn a_store_that_fails_is_503_and_a_failed_delete_is_not_an_answer() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let session = Rig::session(&id);
    for op in [Op::Head, Op::List, Op::Put] {
        rig.faulty.fail_next(op, 1);
        let answer = rig.ask(Method::PATCH, &session, &[], full(b"ab")).await;
        answer.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    }
    assert_eq!(rig.staged().await.len(), 1);
    rig.faulty.fail_next(Op::Delete, 1);
    let gone = rig.send(Method::DELETE, &session).await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.text());
}

/// Asserts that a chunk that cannot be read back when the upload ends fails the end (503) and
/// leaves the upload open, and that chunks that cannot be deleted after the blob is stored do not
/// fail it: the blob is made (201) and served.
#[tokio::test]
async fn the_end_of_an_upload_fails_with_its_chunks_and_not_with_their_deletion() {
    let rig = Rig::new();
    let id = rig.begin().await;
    rig.ask(Method::PATCH, &Rig::session(&id), &[], full(b"abc"))
        .await;
    let digest = digest_of(b"abc");
    rig.faulty.fail_next(Op::Get, 1);
    let failed = rig.send(Method::PUT, &end(&id, &digest)).await;
    failed.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    assert!(rig.objects().await.is_empty());
    assert_eq!(rig.staged().await.len(), 2);
    rig.faulty.fail_next(Op::Delete, 1);
    let done = rig.send(Method::PUT, &end(&id, &digest)).await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
    assert_eq!(rig.blob(&digest).await.as_deref(), Some(b"abc".as_slice()));
    let pulled = rig
        .send(Method::GET, &format!("/v2/app/blobs/{digest}"))
        .await;
    assert_eq!(pulled.body, b"abc");
}

/// Asserts that a store that needs the length of a body refuses a chunk and an end that have none
/// (411) and leaves the upload as it was, and takes the end that has one, the length it is given
/// being that of the chunks and of the body.
#[tokio::test]
async fn a_store_that_needs_a_length_refuses_a_chunk_without_one() {
    let (store, front) = lengthy();
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        login("FULL", "full-secret").parse().expect("a value"),
    );
    let started = front
        .handle_with(
            &Method::POST,
            "/v2/app/blobs/uploads/",
            &headers,
            full(b""),
            Duration::from_secs(30),
        )
        .await;
    let id = started.headers()["docker-upload-uuid"]
        .to_str()
        .expect("text")
        .to_owned();
    let session = Rig::session(&id);
    let idle = Duration::from_secs(30);
    let body = unsized_body(&[b"a", b"b"]);
    let refused = front
        .handle_with(&Method::PATCH, &session, &headers, body, idle)
        .await;
    assert_eq!(refused.status(), StatusCode::LENGTH_REQUIRED);
    let body = full(b"ab");
    let taken = front
        .handle_with(&Method::PATCH, &session, &headers, body, idle)
        .await;
    assert_eq!(taken.status(), StatusCode::ACCEPTED);
    let target = end(&id, &digest_of(b"abc"));
    let body = unsized_body(&[b"c"]);
    let refused = front
        .handle_with(&Method::PUT, &target, &headers, body, idle)
        .await;
    assert_eq!(refused.status(), StatusCode::LENGTH_REQUIRED);
    let created = front
        .handle_with(&Method::PUT, &target, &headers, full(b"c"), idle)
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let key = format!("oci/blobs/sha256/{}", super::push_tests::hex_of(b"abc"));
    assert_eq!(
        store.0.get(&key, None).await.expect("the blob").as_ref(),
        b"abc"
    );
}

/// Asserts the front over a memory backend with no fault: the upload objects live under the
/// front's prefix, so another prefix keeps them apart.
#[tokio::test]
async fn the_uploads_live_under_the_prefix_of_the_front() {
    let memory = std::sync::Arc::new(MemoryBackend::new());
    let (front, _) = front_over(memory.clone());
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        login("FULL", "full-secret").parse().expect("a value"),
    );
    let idle = Duration::from_secs(30);
    let started = front
        .handle_with(
            &Method::POST,
            "/v2/app/blobs/uploads/",
            &headers,
            full(b""),
            idle,
        )
        .await;
    assert_eq!(started.status(), StatusCode::ACCEPTED);
    let listed = memory.list("").await.expect("a listing");
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0].key.starts_with("oci/uploads/"),
        "{}",
        listed[0].key
    );
}
