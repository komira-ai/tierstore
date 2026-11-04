//! Transport failures: unreachable stores, answers cut short, and the retries of ranged reads.

use super::*;

/// An endpoint nothing listens on: a port that was bound and then released.
async fn dead_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// Catches: an unreachable store reported as success, as an empty result, or as a
/// service answer: every operation, with or without Object Lock, fails `Transport`
/// (the caller retries or fails over on that class, and only on it).
#[tokio::test]
async fn every_operation_against_an_unreachable_store_fails_with_transport() {
    for object_lock in [false, true] {
        let s = store(
            dead_endpoint().await,
            Capabilities {
                conditional_put: false,
                object_lock,
            },
        );
        let range = ByteRange::new(0, 4).unwrap();
        let prefix = KeyPrefix::new("p/").unwrap();
        let errs = [
            s.create_bucket().await.unwrap_err(),
            s.put_new(&key(), Bytes::from("abc"), None)
                .await
                .unwrap_err(),
            s.get_range(&key(), range).await.unwrap_err(),
            s.delete(&key()).await.unwrap_err(),
            s.list(&prefix, None, PageSize::MAX).await.unwrap_err(),
        ];
        for err in errs {
            assert!(
                matches!(&err, ObjectStoreError::Transport(_)),
                "object_lock {object_lock}: {err}"
            );
        }
    }
}

/// An answer that promises 50 bytes, sends 3 and closes.
fn cut_short(status: &str, headers: &[&str]) -> String {
    let mut s = format!("HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: 50\r\n");
    for h in headers {
        s.push_str(h);
        s.push_str("\r\n");
    }
    s.push_str("\r\nabc");
    s
}

/// Catches: a body cut off by the connection closing taken for the bytes (a truncated
/// listing read as the end of a bucket, a short segment read as the whole): the read
/// and the listing fail `Transport`.
#[tokio::test]
async fn an_answer_cut_short_is_a_transport_error_for_reads_and_listings() {
    // The read is sent three times (`READ_RETRIES`), every send cut short.
    let mut cuts = vec![cut_short("206 Partial Content", &["content-range: bytes 0-49/50"]); 3];
    cuts.push(cut_short("200 OK", &[]));
    let (endpoint, _) = script(cuts).await;
    let s = store(endpoint, Capabilities::default());
    let err = s
        .get_range(&key(), ByteRange::new(0, 50).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    let err = s
        .list(&KeyPrefix::new("p/").unwrap(), None, PageSize::MAX)
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
}

/// Catches: a small streamed write whose body fails still sent (an object stored from a
/// broken source): the body's own error comes back and nothing reaches the wire.
#[tokio::test]
async fn a_small_streamed_write_with_a_failing_body_sends_nothing() {
    let (endpoint, seen) = script(vec![]).await;
    let s = store(endpoint, Capabilities::default());
    let body: objstore_s3::BodyStream = futures::stream::iter(vec![
        Ok(Bytes::from("ab")),
        Err(ObjectStoreError::Body("the source broke".into())),
    ])
    .boxed();
    let err = s.put_stream(&key(), 4, body).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Body(m) if m == "the source broke"),
        "{err}"
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// Catches a ranged read that gives up on the first dropped connection: a store (or the
/// path to it) that closes a pooled connection after the request went out answers
/// nothing, the client reports "connection closed before message completed", and that
/// failed a whole batch of reads though the next attempt would have succeeded. The first
/// connection here is closed after its request is read; the second answers.
#[tokio::test]
async fn a_ranged_read_survives_a_connection_closed_before_its_answer() {
    let (endpoint, seen) = script(vec![
        String::new(),
        answer(
            "206 Partial Content",
            &["content-range: bytes 2-5/10"],
            "2345",
        ),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let got = s
        .get_range(&key(), ByteRange::new(2, 4).unwrap())
        .await
        .expect("one dropped connection must not fail the read");
    assert_eq!(got, Bytes::from("2345"));
    assert_eq!(seen.lock().unwrap().len(), 2, "the read was sent again");
}

/// Catches the same failure part way through the body: the head says 4 bytes, 2 arrive,
/// the connection closes.
#[tokio::test]
async fn a_ranged_read_survives_a_body_cut_short() {
    let cut = "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 2-5/10\r\n\
               content-length: 4\r\n\r\n23"
        .to_owned();
    let (endpoint, seen) = script(vec![
        cut,
        answer(
            "206 Partial Content",
            &["content-range: bytes 2-5/10"],
            "2345",
        ),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let got = s
        .get_range(&key(), ByteRange::new(2, 4).unwrap())
        .await
        .expect("a body cut short must be read again");
    assert_eq!(got, Bytes::from("2345"));
    assert_eq!(seen.lock().unwrap().len(), 2);
}

/// Catches a retry with no bound: a store that closes every connection fails the read
/// after a fixed number of sends (the first and two more), with the transport error.
#[tokio::test]
async fn a_ranged_read_gives_up_after_its_retries() {
    let (endpoint, seen) = script(vec![String::new(); 3]).await;
    let s = store(endpoint, Capabilities::default());
    let err = s
        .get_range(&key(), ByteRange::new(2, 4).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert!(err.to_string().contains("connection closed"), "{err}");
    assert_eq!(seen.lock().unwrap().len(), 3);
}

/// Catches a retry of a timeout: a store that never answers is already `request_timeout`
/// slow, and sending again would triple the wait of a read that cannot succeed.
#[tokio::test]
async fn a_ranged_read_that_timed_out_is_not_sent_again() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(Mutex::new(0usize));
    let count = Arc::clone(&accepted);
    let first = Arc::new(Notify::new());
    let told = Arc::clone(&first);
    let held = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            *count.lock().unwrap() += 1;
            held.push(sock);
            told.notify_one();
        }
    });
    let s = timed_store(endpoint, Duration::from_secs(30), None);
    let reading =
        tokio::spawn(async move { s.get_range(&key(), ByteRange::new(2, 4).unwrap()).await });
    // The store has the connection; from now on the client waits for nothing but its
    // timer, and the paused clock moves on to it.
    first.notified().await;
    tokio::time::pause();
    let err = reading.await.unwrap().unwrap_err();
    held.abort();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert_eq!(*accepted.lock().unwrap(), 1);
}

/// Serves forever: the first connection reads one whole request and is closed with no
/// answer; every later connection is answered `200`. Counts the requests it read.
async fn cut_then_ok() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reads);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let counted = Arc::clone(&counted);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // The head and the three body bytes of the PUT.
                while !buf.ends_with(b"abc") {
                    match sock.read(&mut chunk).await {
                        Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                        _ => return,
                    }
                }
                if counted.fetch_add(1, Ordering::SeqCst) > 0 {
                    let _ = sock.write_all(answer("200 OK", &[], "").as_bytes()).await;
                }
                let _ = sock.shutdown().await;
            });
        }
    });
    (endpoint, reads)
}

/// Catches: a PUT sent again by the client after its connection closed before the
/// answer. The first PUT may have landed; a resend would meet its own object (or write
/// over a changed one on a store without conditional writes), so the caller, who knows
/// the key and can read it back, is the only one who may try again. Exactly one PUT
/// reaches the store and the transport error comes back.
#[tokio::test]
async fn a_put_cut_before_its_answer_is_sent_once_and_fails_as_transport() {
    use std::sync::atomic::Ordering;
    let (endpoint, reads) = cut_then_ok().await;
    let s = store(endpoint, Capabilities::default());
    let err = s
        .put_new(&key(), Bytes::from("abc"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert_eq!(reads.load(Ordering::SeqCst), 1, "the PUT was sent again");
    // Through put_stream too (the flusher's path for a small object).
    let body: objstore_s3::BodyStream = futures::stream::iter(vec![Ok(Bytes::from("abc"))]).boxed();
    s.put_stream(&key(), 3, body).await.unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}

/// Catches: a `len` that sends a GET (the object's bytes come over the wire), or that
/// takes any failure for a missing object (an ingest would then call a denied bucket
/// empty): only a 404 is `None`; a 403 is `Service` with its status.
#[tokio::test]
async fn len_sends_a_head_and_only_a_404_is_none() {
    let (endpoint, seen) = script(vec![
        "HTTP/1.1 200 OK\r\nconnection: close\r\netag: \"abc\"\r\n\
         last-modified: Wed, 21 Oct 2026 07:28:00 GMT\r\ncontent-length: 33\r\n\r\n"
            .to_owned(),
        answer("404 Not Found", &[], ""),
        answer("403 Forbidden", &[], ""),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    assert_eq!(s.len(&key()).await.unwrap(), Some(33));
    assert_eq!(s.len(&key()).await.unwrap(), None);
    let err = s.len(&key()).await.unwrap_err();
    // A HEAD answer has no body, so there is no error code to read.
    assert!(
        matches!(&err, ObjectStoreError::Service { status: 403, .. }),
        "{err}"
    );
    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with("HEAD /test-bucket/cas/seg-1 HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert_eq!(heads.len(), 3);
}
