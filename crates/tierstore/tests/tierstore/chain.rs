//! One winner through a chain: a presigned `PUT` to a node whose store is another tierstore.
//!
//! The design for chaining says a put that a build node forwards is presigned at the storage
//! tier by the build node's tierstore. These tests run the hop for real, on loopback: a front
//! that takes presigned puts (the build node) over an `S3Backend` that reaches a storage tier
//! (a front over a proxy with in-memory tiers). They show what reaches the storage tier, which
//! is a request signed with a header and not a presigned one.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hyper::StatusCode;
use tierstore::s3::{Capabilities, Credentials, S3Config};
use tierstore::server::{Grant, PresignKey, Server, ServerConfig};
use tierstore::{Backend, MemoryBackend, Proxy, ProxyConfig, S3Backend, SystemClock, TestClock};
use tokio::net::TcpListener;

use crate::hold::Gate;
use crate::recheck::{BUCKET, SECRET, answered, put_request};
use crate::support::{assert_absent, body, read};

const KEY_ID: &str = "AKIDSTORAGE";
const KEY_SECRET: &str = "storage-secret-0123456789";
const HOT_AT_BUDGET: &str = r#"{"hot_bytes":1000,"copy_lag":0,"time":0}"#;
const WRITERS: usize = 8;
const KEY: &str = "cas/b/contended";

/// A storage tier on a loopback port, with the tiers behind it.
struct Storage {
    addr: SocketAddr,
    hot: Arc<MemoryBackend>,
    cold: Arc<MemoryBackend>,
}

async fn storage(hot_full: bool) -> Storage {
    let hot = Arc::new(MemoryBackend::new());
    let cold = Arc::new(MemoryBackend::new());
    if hot_full {
        hot.put("_tiering/usage", body(HOT_AT_BUDGET))
            .await
            .unwrap();
    }
    let mut config = ProxyConfig::new("storage", 1000, Arc::new(TestClock::new()));
    config.linger = Duration::from_millis(1);
    config.usage_refresh = Duration::ZERO;
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    let served = ServerConfig::new(BUCKET, KEY_ID, KEY_SECRET, Arc::new(SystemClock));
    let server = Server::new(Arc::new(proxy), served);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _serving = tokio::spawn(async move { server.serve(listener).await });
    Storage { addr, hot, cold }
}

/// A front that takes presigned puts, over a backend that reaches the storage tier with the
/// storage tier's own key.
fn build_node(storage: &Storage) -> Arc<Server> {
    let config = S3Config {
        endpoint: format!("http://{}", storage.addr),
        region: "us-east-1".to_owned(),
        bucket: BUCKET.to_owned(),
        credentials: Credentials::new(KEY_ID, KEY_SECRET),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    let upstream = S3Backend::new(config).unwrap();
    let grant = Grant {
        prefix: "cas/b/".to_owned(),
        max_expires_secs: 600,
        max_object_bytes: 1 << 20,
    };
    let key = PresignKey::new("pack-presign", SECRET, grant).unwrap();
    let mut config = ServerConfig::new(BUCKET, "FULL", "full-secret", Arc::new(SystemClock));
    config.presign_keys = vec![key];
    Server::new(Arc::new(upstream), config)
}

/// Eight presigned `PUT`s of one key with different bodies, at once, to the build node. Gives
/// each body with the status and text it was answered.
async fn eight_puts(node: &Arc<Server>, key: &str) -> Vec<(Bytes, StatusCode, String)> {
    let mut tasks = Vec::new();
    for writer in 0..WRITERS {
        let node = node.clone();
        let key = key.to_owned();
        let data = Bytes::from(format!("writer {writer} wrote this body. ").repeat(writer + 1));
        tasks.push(tokio::spawn(async move {
            let length = data.len().to_string();
            let headers = [("content-length", length.as_str())];
            let request = put_request(&key, &headers, data.to_vec());
            let (status, _, text) = answered(&node, request).await;
            (data, status, text)
        }));
    }
    let mut done = Vec::new();
    for task in tasks {
        done.push(task.await.unwrap());
    }
    done
}

/// The winner's body, after checking that exactly one answer is 200 and all the others are 412.
fn the_winner(done: &[(Bytes, StatusCode, String)]) -> Bytes {
    let ok: Vec<&Bytes> = done
        .iter()
        .filter(|(_, status, _)| *status == StatusCode::OK)
        .map(|(data, _, _)| data)
        .collect();
    assert_eq!(ok.len(), 1, "{done:?}");
    let refused = done
        .iter()
        .filter(|(_, status, _)| *status == StatusCode::PRECONDITION_FAILED)
        .count();
    assert_eq!(refused, WRITERS - 1, "{done:?}");
    ok[0].clone()
}

/// Asserts the claim for the hop when the storage tier's hot tier has room: eight presigned
/// `PUT`s of one key to the build node, which forwards them to the storage tier, are answered
/// one 200 and seven 412, and the storage tier's hot tier holds the winner's bytes whole while
/// its cold tier holds nothing. Defect it catches: a hop that turns the storage tier's refusal
/// into a success, or that cannot tell the winner's body from a loser's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_presigned_puts_through_a_hop_have_one_winner_in_the_storage_hot_tier() {
    let storage = storage(false).await;
    let node = build_node(&storage);
    let done = eight_puts(&node, KEY).await;

    let winner = the_winner(&done);
    assert_eq!(read(&*storage.hot, KEY).await, winner);
    assert_absent(&*storage.cold, KEY).await;
}

/// Asserts what the code does through the hop when the storage tier's hot tier is full, which
/// differs from the claim that a forwarded put is presigned at the storage tier. A presigned
/// `PUT` straight to a storage tier whose hot tier is full is refused 503 (see
/// `with_the_hot_tier_full_no_presigned_put_is_written_anywhere`). Through the hop the same
/// eight puts are accepted: the build node's backend reaches the storage tier with a request
/// signed by header, which the storage tier does not take for presigned, so its put goes to the
/// cold tier. One of the eight wins there, because the cold tier's create-only write decides
/// among puts that all go to it; the cold tier holds the winner's bytes whole; the hot tier holds
/// nothing. This PINS WHAT THE CODE DOES: the presigned fence does not survive a hop, so the weaker
/// path (a put that is not presigned) applies to a forwarded put. Defect it catches: a hop that gives two winners
/// when every put goes to the cold tier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn through_a_hop_a_presigned_put_is_not_presigned_at_a_storage_tier_whose_hot_tier_is_full() {
    let storage = storage(true).await;
    let node = build_node(&storage);
    let done = eight_puts(&node, KEY).await;

    let winner = the_winner(&done);
    assert_eq!(read(&*storage.cold, KEY).await, winner);
    assert_absent(&*storage.hot, KEY).await;
}
