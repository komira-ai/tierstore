//! `S3Store::new_tuned` and the read retries it sets, against a server on loopback that reads each
//! request and closes the connection without an answer.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use objstore_s3::s3::{Credentials, READ_RETRIES, S3Config, S3Store, S3Tuning};
use objstore_s3::{ByteRange, Capabilities, ObjectKey, ObjectStore, ObjectStoreError};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// An endpoint that reads one request head from each connection and drops the connection, and the
/// count of the requests it has read.
async fn hang_up() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed before sending a request");
                buf.extend_from_slice(&chunk[..n]);
            }
            seen.fetch_add(1, Ordering::SeqCst);
        }
    });
    (endpoint, count)
}

fn store(endpoint: String, tuning: S3Tuning) -> S3Store {
    S3Store::new_tuned(
        S3Config {
            endpoint,
            region: "us-east-1".into(),
            bucket: "test-bucket".into(),
            credentials: Credentials::new("AKIDEXAMPLE", "secret"),
            capabilities: Capabilities::default(),
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            min_body_rate: None,
        },
        tuning,
    )
    .unwrap()
}

async fn read_sends(retries: u32) -> usize {
    let (endpoint, count) = hang_up().await;
    let tuning = S3Tuning {
        read_retries: retries,
        read_retry_wait: Duration::from_millis(1),
        ..S3Tuning::default()
    };
    let s = store(endpoint, tuning);
    let key = ObjectKey::new("cas/seg-1").unwrap();
    let err = s
        .get_range(&key, ByteRange::new(0, 4).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    count.load(Ordering::SeqCst)
}

/// A read is sent once more for each retry the tuning gives, and `read_retries = 0` sends it
/// once.
#[tokio::test]
async fn a_read_is_sent_once_more_for_each_retry_the_tuning_gives() {
    assert_eq!(read_sends(0).await, 1);
    assert_eq!(read_sends(4).await, 5);
}

/// `S3Store::new` is the store with the default tuning: three sends of a failing read.
#[tokio::test]
async fn new_sends_a_failing_read_the_default_number_of_times() {
    assert_eq!(read_sends(READ_RETRIES).await, READ_RETRIES as usize + 1);
    assert_eq!(S3Tuning::default().read_retries, READ_RETRIES);
}
