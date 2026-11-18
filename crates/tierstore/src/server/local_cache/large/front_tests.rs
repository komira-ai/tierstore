//! The large-object path through the real S3 front over HTTP, as a client sees it: the status,
//! the headers and the exact bytes of a GET of a large object, a ranged one, a miss, a range
//! that cannot be satisfied, the client's own conditionals, a fill that fails, and a small
//! object that still comes from `obj/`.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{HeaderMap, Request, StatusCode};
use tierstore_fetch_api::upstream::{Located, Location, NotDefinitive};
use tierstore_format::ids::Generation;

use super::testing::{LargeRig, MIB, generation, hot, object};
use crate::Backend;
use crate::server::auth::sign;
use crate::server::testing::{BUCKET, KEY_ID, NOW, SECRET};
use crate::server::{Server, ServerConfig};

/// A key under the cache's policy.
const KEY: &str = "cas/big/one";

/// What a client got: status, headers and the raw body.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    /// The value of a header, or the empty text.
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }

    /// Whether the body is an error document with this `<Code>`.
    fn has_code(&self, code: &str) -> bool {
        String::from_utf8_lossy(&self.body).contains(&format!("<Code>{code}</Code>"))
    }
}

/// A rig and a server in front of its cache.
struct Front {
    rig: LargeRig,
    server: Arc<Server>,
    config: ServerConfig,
}

impl Front {
    fn new(name: &str) -> Self {
        let rig = LargeRig::new(name);
        rig.rig.clock.set(NOW);
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, rig.rig.clock.clone());
        config.rate_clock = rig.rig.clock.clone();
        config.cache = Some(rig.rig.cache.clone());
        let backend: Arc<dyn Backend> = Arc::new(rig.rig.cache.clone());
        let server = Server::new(backend, config.clone());
        Self {
            rig,
            server,
            config,
        }
    }

    /// A signed GET of `key` with these extra headers.
    async fn get(&self, key: &str, extra: &[(&str, &str)]) -> Answer {
        let uri = format!("/{BUCKET}/{key}");
        let request: Request<Full<Bytes>> = sign(&self.config, "GET", &uri, extra, b"");
        let response = self.server.handle(request).await;
        let (parts, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes().to_vec();
        Answer {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }

    /// How many of the upstream's GETs carried no `If-Match`.
    fn unpinned_gets(&self) -> usize {
        let gets = self.rig.gets();
        gets.iter().filter(|get| get.generation.is_none()).count()
    }
}

/// The ETag header a client sees for `generation`.
fn quoted(generation: Generation) -> String {
    format!("\"{generation}\"")
}

/// Asserts that a GET of a large stored object is 200 with the ETag of its generation, with
/// exactly the object's bytes (as many as its size), and that every GET the engine
/// sent upstream carried `If-Match` of that generation.
#[tokio::test]
async fn get_of_a_large_stored_object_is_served_whole_with_its_generation_as_etag() {
    let front = Front::new("tscache-lfront-whole");
    let bytes = object(3 * MIB + 100, 1);
    let g = generation(0xab);
    front.rig.stored(KEY, &bytes, g);
    let reply = front.get(KEY, &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("etag"), quoted(g));
    assert_eq!(reply.header("etag"), format!("\"{}\"", "ab".repeat(16)));
    assert_eq!(reply.body.len(), bytes.len());
    assert_eq!(reply.body, bytes.to_vec());
    let gets = front.rig.gets();
    assert!(!gets.is_empty());
    assert!(gets.iter().all(|get| get.generation == Some(g)));
    assert_eq!(front.rig.large.counters().counts().routed, 1);
}

/// Asserts that a ranged GET inside a large object is 206 with `content-range` of the range and
/// the object's size, with exactly the bytes of the range across a cell boundary.
#[tokio::test]
async fn ranged_get_of_a_large_object_is_partial_content_with_the_range_bytes() {
    let front = Front::new("tscache-lfront-range");
    let bytes = object(6 * MIB, 2);
    let g = generation(0x11);
    front.rig.stored(KEY, &bytes, g);
    let (a, b) = (2 * MIB - 10, 2 * MIB + 49);
    let range = format!("bytes={a}-{b}");
    let reply = front.get(KEY, &[("range", range.as_str())]).await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        reply.header("content-range"),
        format!("bytes {a}-{b}/{}", bytes.len())
    );
    assert_eq!(reply.body.len(), 60);
    assert_eq!(reply.header("etag"), quoted(g));
    assert_eq!(reply.body, bytes[a as usize..=b as usize].to_vec());
    assert!(front.rig.gets().iter().all(|get| get.generation == Some(g)));
}

/// Asserts that a GET of a large silo-origin object is 200 with the first part's ETag, exactly
/// the object's bytes, that the first upstream GET carried no `If-Match` and that every later
/// one carried that ETag.
#[tokio::test]
async fn get_of_a_large_silo_object_takes_its_etag_from_the_first_part() {
    let front = Front::new("tscache-lfront-silo");
    let bytes = object(6 * MIB, 3);
    let etag = generation(0x5e);
    front.rig.silo(KEY, &bytes, etag);
    let reply = front.get(KEY, &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("etag"), quoted(etag));
    assert_eq!(reply.body.len(), bytes.len());
    assert_eq!(reply.body, bytes.to_vec());
    let gets = front.rig.gets();
    assert_eq!(gets[0].generation, None);
    assert!(gets.len() >= 2);
    assert_eq!(front.unpinned_gets(), 1);
    assert!(gets[1..].iter().all(|get| get.generation == Some(etag)));
    assert_eq!(front.rig.large.counters().counts().alone, 1);
}

/// Asserts that a key the storage tier definitively does not hold is 404 with the S3 code
/// `NoSuchKey` and no GET sent, and that a miss that is not definitive is 503
/// `ServiceUnavailable`, which is not remembered: the next read asks again and is served.
#[tokio::test]
async fn definitive_miss_is_no_such_key_and_other_misses_are_service_unavailable() {
    let front = Front::new("tscache-lfront-miss");
    let missing = front.get("cas/big/none", &[]).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert!(missing.has_code("NoSuchKey"));
    assert_eq!(front.rig.gets().len(), 0);
    assert_eq!(front.rig.large.counters().counts().misses, 1);

    let bytes = object(2 * MIB, 4);
    front.rig.stored(KEY, &bytes, generation(1));
    let unsure = Located::NotDefinitive(NotDefinitive::TooFewAnswers);
    front.rig.upstream.script_locate(Ok(unsure));
    let reply = front.get(KEY, &[]).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(reply.has_code("ServiceUnavailable"));
    assert_eq!(front.rig.gets().len(), 0);
    assert_eq!(front.rig.large.counters().counts().unavailable, 1);

    let again = front.get(KEY, &[]).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body, bytes.to_vec());
}

/// Asserts that a range that starts past the end of a large object is 416 `InvalidRange` with
/// no byte of the object and no engine fill.
#[tokio::test]
async fn unsatisfiable_range_of_a_large_object_is_invalid_range() {
    let front = Front::new("tscache-lfront-416");
    let bytes = object(2 * MIB, 5);
    front.rig.stored(KEY, &bytes, generation(2));
    let past = format!("bytes={}-", 2 * MIB + 5);
    let reply = front.get(KEY, &[("range", past.as_str())]).await;
    assert_eq!(reply.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(reply.has_code("InvalidRange"));
    assert_eq!(front.rig.gets().len(), 0);
    assert_eq!(front.rig.engine.stats().fills, 0);
}

/// Asserts that the client's own conditionals are judged against the generation: `If-Match` of
/// the ETag is 200 with the bytes, of another tag is 412 `PreconditionFailed`, and
/// `If-None-Match` of the ETag is 304 with no body, of another tag 200.
#[tokio::test]
async fn client_conditionals_are_judged_against_the_generation() {
    let front = Front::new("tscache-lfront-cond");
    let bytes = object(2 * MIB + 7, 6);
    let g = generation(0xc3);
    front.rig.stored(KEY, &bytes, g);
    let tag = quoted(g);
    let matched = front.get(KEY, &[("if-match", tag.as_str())]).await;
    assert_eq!(matched.status, StatusCode::OK);
    assert_eq!(matched.body, bytes.to_vec());

    let other = quoted(generation(0xc4));
    let failed = front.get(KEY, &[("if-match", other.as_str())]).await;
    assert_eq!(failed.status, StatusCode::PRECONDITION_FAILED);
    assert!(failed.has_code("PreconditionFailed"));

    let fresh = front.get(KEY, &[("if-none-match", tag.as_str())]).await;
    assert_eq!(fresh.status, StatusCode::NOT_MODIFIED);
    assert!(fresh.body.is_empty());
    assert_eq!(fresh.header("etag"), tag);

    let changed = front.get(KEY, &[("if-none-match", other.as_str())]).await;
    assert_eq!(changed.status, StatusCode::OK);
    assert_eq!(changed.body, bytes.to_vec());
}

/// Asserts that when every copy holds another generation than the location names, so the first
/// cell cannot be filled, the client is told 503 `ServiceUnavailable` and the answer holds not
/// one byte of the other object.
#[tokio::test]
async fn fill_failure_on_the_first_cell_is_503_with_none_of_the_other_bytes() {
    let front = Front::new("tscache-lfront-fail");
    let named = generation(0x01);
    let other_bytes = object(3 * MIB, 7);
    let location = Location {
        generation: Some(named),
        size: Some(other_bytes.len() as u64),
        sources: vec![hot(other_bytes.len() as u64)],
        conflicts: Vec::new(),
    };
    let held = generation(0x02);
    front.rig.insert(KEY, &other_bytes, held, location);
    let reply = front.get(KEY, &[]).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(reply.has_code("ServiceUnavailable"));
    assert_ne!(reply.header("etag"), quoted(held));
    assert!(reply.body.len() < 1024);
    let sample = &other_bytes[MIB as usize..MIB as usize + 64];
    assert!(!reply.body.windows(64).any(|window| window == sample));
    let gets = front.rig.gets();
    assert!(!gets.is_empty());
    assert!(gets.iter().all(|get| get.generation == Some(named)));
    let counts = front.rig.large.counters().counts();
    assert_eq!((counts.routed, counts.fill_failures), (1, 1));
}

/// Asserts that a small object (at most the whole-fill limit) is served 200 with its exact bytes
/// from `obj/` through the memory upstream, with no engine GET and no fill, and that a second
/// read is served the same way and still routes nothing to the engine.
#[tokio::test]
async fn small_object_through_the_front_is_served_from_obj_without_a_fill() {
    let front = Front::new("tscache-lfront-small");
    let bytes = object(1000, 8);
    front.rig.rig.put(KEY, &bytes).await;
    front.rig.stored(KEY, &bytes, generation(3));
    let reply = front.get(KEY, &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, bytes.to_vec());
    assert_eq!(reply.body.len(), 1000);
    assert_eq!(front.rig.gets().len(), 0);
    assert_eq!(front.rig.engine.stats().fills, 0);
    assert_eq!(front.rig.large.counters().counts().small, 1);

    let again = front.get(KEY, &[]).await;
    assert_eq!(again.body, bytes.to_vec());
    let counts = front.rig.large.counters().counts();
    assert_eq!((counts.routed, counts.small), (0, 1));
    assert_eq!(front.rig.gets().len(), 0);
}
