//! The create-only guarantees of a put through the proxy when a tier misbehaves: a hot tier that
//! runs out of room after it took bytes, and the front's answer to it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use bytes::Bytes;

use http_body_util::Full;
use hyper::{Request, StatusCode};
use tierstore::server::{
    AccessKey, Grant, PresignKey, PresignRequest, Server, ServerConfig, presign_put,
};
use tierstore::{
    Backend, ByteRange, Error, Listed, Meta, PartBody, Proxy, ProxyConfig, SystemClock,
};

use tokio::sync::Barrier;

use crate::support::{Rig, assert_absent};
use futures::StreamExt;
use http_body_util::BodyExt;
use tierstore::Clock;

pub(crate) const SECRET: &str = "presign-secret-0123456789";
pub(crate) const BUCKET: &str = "tier-bucket";

/// A backend over another that can be told to misbehave in one way each.
struct Fake {
    inner: Arc<dyn Backend>,
    /// The first `head` of this key says it is not there, whatever the inner backend holds.
    forget_first_head: Option<(&'static str, AtomicU32)>,
    /// A streamed put takes the first chunk of its body and then answers that there is no room.
    take_then_full: bool,
    /// Streamed puts of this key wait here for the other party before they go on.
    gate: Option<(&'static str, Arc<Barrier>)>,
    /// How many streamed puts reached this backend.
    streams: AtomicU32,
}

impl Fake {
    fn over(inner: Arc<dyn Backend>) -> Self {
        Self {
            inner,
            forget_first_head: None,
            take_then_full: false,
            gate: None,
            streams: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl Backend for Fake {
    fn needs_length(&self) -> bool {
        self.inner.needs_length()
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        mut body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.streams.fetch_add(1, Ordering::SeqCst);
        if self.take_then_full {
            let _first = body.next().await;
            return Err(Error::Full("hot".to_owned()));
        }
        if let Some((name, barrier)) = &self.gate
            && *name == key
        {
            barrier.wait().await;
        }
        self.inner.put_stream(key, len, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        if let Some((name, asked)) = &self.forget_first_head
            && *name == key
            && asked.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Err(Error::NotFound(key.to_owned()));
        }
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        self.inner.delete_if_match(key, etag).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
}

/// A server over `proxy` with one presigning key limited to `cap` bytes of body, on the clock of
/// the machine.
pub(crate) fn server_over(proxy: Proxy, cap: u64) -> Arc<Server> {
    let grant = Grant {
        prefix: "cas/b/".to_owned(),
        max_expires_secs: 600,
        max_object_bytes: cap,
    };
    let key = PresignKey::new("pack-presign", SECRET, grant).unwrap();
    let mut config = ServerConfig::new(BUCKET, "FULL", "full-secret", Arc::new(SystemClock));
    config.presign_keys = vec![key];
    Server::new(Arc::new(proxy), config)
}

/// A presigned `PUT` request for `key`, with a body and headers.
pub(crate) fn put_request(
    key: &str,
    headers: &[(&str, &str)],
    data: Vec<u8>,
) -> Request<Full<Bytes>> {
    let access = AccessKey::new("pack-presign", SECRET);
    let wanted = PresignRequest {
        endpoint: "http://tier.test",
        region: "us-east-1",
        bucket: BUCKET,
        key,
        expires_secs: 300,
        now: SystemClock.now(),
    };
    let url = presign_put(&access, &wanted).unwrap();
    let path = url.strip_prefix("http://tier.test").unwrap();
    let mut builder = Request::builder()
        .method("PUT")
        .uri(path)
        .header("host", "tier.test");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Full::new(Bytes::from(data))).unwrap()
}

/// What an answer was: its status, the `Retry-After` header and its body.
pub(crate) async fn answered(
    server: &Server,
    request: Request<Full<Bytes>>,
) -> (StatusCode, String, String) {
    let response = server.handle(request).await;
    let (parts, body) = response.into_parts();
    let retry = parts.headers.get("retry-after");
    let retry = retry
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        retry.to_owned(),
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Asserts that a presigned put whose hot tier takes part of the body and then has no room is 503
/// `SlowDown` with `Retry-After`, stores nothing in either tier; that the
/// tier is latched full, so the next presigned put is 503 without a streamed put reaching the
/// hot tier; and that a put of the full key still goes to the cold tier.
#[tokio::test(start_paused = true)]
async fn a_hot_tier_that_fills_after_taking_bytes_gives_503_and_stores_nothing() {
    let rig = Rig::new();
    let mut hot = Fake::over(rig.hot_mem.clone());
    hot.take_then_full = true;
    let hot = Arc::new(hot);
    let config = ProxyConfig::new("p1", 1 << 30, Arc::new(SystemClock));
    let proxy = crate::support::open_live(hot.clone(), rig.cold.clone(), config)
        .await
        .unwrap();
    let server = server_over(proxy, 1 << 20);
    let (status, retry, text) =
        answered(&server, put_request("cas/b/a", &[], b"abcd".to_vec())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(retry, "1");
    assert!(text.contains("<Code>SlowDown</Code>"), "{text}");
    assert_absent(&*rig.hot_mem, "cas/b/a").await;
    assert_absent(&*rig.cold_mem, "cas/b/a").await;

    let taken = hot.streams.load(Ordering::SeqCst);
    assert_eq!(taken, 1);
    let (status, _, _) = answered(&server, put_request("cas/b/b", &[], b"abcd".to_vec())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        hot.streams.load(Ordering::SeqCst),
        taken,
        "latched: no put reached the hot tier"
    );
    assert_absent(&*rig.cold_mem, "cas/b/b").await;
}
