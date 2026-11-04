//! The keys the front asks for: which logins are accepted, what each kind of key may do, and how
//! the front answers and counts a refusal. No socket: the requests are made to the front itself.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use hyper::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use hyper::{Method, StatusCode};
use sha2::{Digest as _, Sha256};

use super::auth::{Need, Refused};
use super::{OciAuth, OciFront, OciMetrics, OciPrefix};
use crate::server::gc::GcKey;
use crate::server::scoped::{Prefixes, ScopedKey};
use crate::server::{AccessKey, ServerConfig};
use crate::{Backend, MemoryBackend, TestClock};

fn server() -> ServerConfig {
    let clock = Arc::new(TestClock::new());
    let mut server = ServerConfig::new("bucket", "FULL", "full-secret", clock);
    server
        .read_only_keys
        .push(AccessKey::new("READER", "reader-secret"));
    let gc = GcKey::new("GC", "gc-secret", "oci/").expect("a gc key");
    server.gc_keys.push(gc);
    let write = Prefixes {
        write: &["oci/"],
        ..Prefixes::default()
    };
    let key = ScopedKey::new("WRITER", "writer-secret", &write).expect("a key");
    server.scoped_keys.push(key);
    let read = Prefixes {
        read: &["oci/"],
        ..Prefixes::default()
    };
    let key = ScopedKey::new("SCOPEDREAD", "scopedread-secret", &read).expect("a key");
    server.scoped_keys.push(key);
    server
}

fn auth() -> OciAuth {
    OciAuth::from_server(&server(), &OciPrefix::new("oci/").expect("a prefix"))
}

fn login(id: &str, secret: &str) -> HeaderMap {
    let value = format!("Basic {}", STANDARD.encode(format!("{id}:{secret}")));
    with(&value)
}

fn with(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(value).expect("a value"),
    );
    headers
}

/// Asserts what each method asks of a key: the ping and the methods the front does not serve ask
/// for either right, a read of anything else asks to pull, and the four methods that change a
/// registry ask to push.
#[test]
fn a_method_asks_for_the_right_it_needs() {
    let asked = |method: Method, ping: bool| Need::of(&method, ping);
    assert_eq!(asked(Method::GET, true), Need::Any);
    assert_eq!(asked(Method::HEAD, true), Need::Any);
    assert_eq!(asked(Method::GET, false), Need::Pull);
    assert_eq!(asked(Method::HEAD, false), Need::Pull);
    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        assert_eq!(asked(method, false), Need::Push);
    }
    assert_eq!(asked(Method::POST, true), Need::Push);
    assert_eq!(asked(Method::OPTIONS, false), Need::Any);
}

/// Asserts that a request with no login, with a header that is not text, with another scheme, with
/// a scheme and no credentials, with credentials that are not base64, that are not UTF-8, that have
/// no colon, that name no key of the server or that have a wrong secret (also the right secret followed by NUL bytes) is `Unauthorized`; the scheme
/// is matched without regard to case, and the login is split at its first colon, so a secret with more after it is a wrong one.
#[test]
fn a_login_is_checked_in_every_part() {
    let auth = auth();
    let unauthorized =
        |headers: &HeaderMap| auth.check(headers, Need::Any) == Err(Refused::Unauthorized);
    assert!(unauthorized(&HeaderMap::new()));
    let mut binary = HeaderMap::new();
    binary.insert(
        AUTHORIZATION,
        HeaderValue::from_bytes(&[0xff, 0xfe]).expect("bytes"),
    );
    assert!(unauthorized(&binary));
    assert!(unauthorized(&with("Bearer abc")));
    assert!(unauthorized(&with("Basic")));
    assert!(unauthorized(&with("Basic %%%")));
    let not_utf8 = format!("Basic {}", STANDARD.encode([0xff, b':', 0xfe]));
    assert!(unauthorized(&with(&not_utf8)));
    let no_colon = format!("Basic {}", STANDARD.encode("FULL"));
    assert!(unauthorized(&with(&no_colon)));
    assert!(unauthorized(&login("NOBODY", "full-secret")));
    assert!(unauthorized(&login("FULL", "wrong")));
    assert!(unauthorized(&login("FULL", "full-secre")));
    assert!(unauthorized(&login("FULL", "full-secret-and-more")));
    assert!(unauthorized(&login("FULL", "")));
    assert!(unauthorized(&login("FULL", "full-secret\0")));
    assert!(unauthorized(&login("FULL", "full-secret\0\0")));
    assert_eq!(auth.check(&login("FULL", "full-secret"), Need::Any), Ok(()));
    let lower = format!("basic {}", STANDARD.encode("FULL:full-secret"));
    assert_eq!(auth.check(&with(&lower), Need::Any), Ok(()));
    let colon = format!("Basic {}", STANDARD.encode("FULL:full-secret:more"));
    assert!(unauthorized(&with(&colon)));
}

/// Asserts the rights of each kind of key over the prefix `oci/`: the full key does both, a
/// read-only key and a scoped key with a read prefix pull, a scoped key with a write prefix pushes,
/// and a GC key (list and delete) does neither.
#[test]
fn each_kind_of_key_has_the_rights_of_its_role() {
    let auth = auth();
    let check = |id: &str, secret: &str, need| auth.check(&login(id, secret), need);
    let rights = |id: &str, secret: &str| {
        (
            check(id, secret, Need::Pull).is_ok(),
            check(id, secret, Need::Push).is_ok(),
        )
    };
    assert_eq!(rights("FULL", "full-secret"), (true, true));
    assert_eq!(rights("READER", "reader-secret"), (true, false));
    assert_eq!(rights("SCOPEDREAD", "scopedread-secret"), (true, false));
    assert_eq!(rights("WRITER", "writer-secret"), (false, true));
    assert_eq!(rights("GC", "gc-secret"), (false, false));
    assert_eq!(check("GC", "gc-secret", Need::Any), Err(Refused::Denied));
    assert_eq!(check("WRITER", "writer-secret", Need::Any), Ok(()));
}

/// Asserts that a scoped key reads or writes the front's objects only with a prefix that holds
/// all of them: a prefix inside the front's, and one elsewhere, give no right, and a prefix above
/// it does.
#[test]
fn a_scoped_prefix_must_cover_the_fronts_objects() {
    let prefixes = |read: &'static [&'static str], write: &'static [&'static str]| Prefixes {
        read,
        write,
        ..Prefixes::default()
    };
    let rights = |front: &str, read: &'static [&'static str], write: &'static [&'static str]| {
        let mut server = ServerConfig::new("b", "F", "s", Arc::new(TestClock::new()));
        let key = ScopedKey::new("K", "k-secret", &prefixes(read, write)).expect("a key");
        server.scoped_keys.push(key);
        let auth = OciAuth::from_server(&server, &OciPrefix::new(front).expect("a prefix"));
        let headers = login("K", "k-secret");
        (
            auth.check(&headers, Need::Pull).is_ok(),
            auth.check(&headers, Need::Push).is_ok(),
        )
    };
    assert_eq!(rights("oci/", &["oci/"], &["oci/"]), (true, true));
    assert_eq!(
        rights("oci/", &["oci/blobs/sha256/ab/"], &["other/"]),
        (false, false)
    );
    assert_eq!(
        rights("oci/", &["oci/blobs/"], &["oci/blobs/sha256/"]),
        (true, true)
    );
    assert_eq!(
        rights("images/oci/", &["oci/"], &["images/"]),
        (false, true)
    );
}

struct Rig {
    front: OciFront,
    metrics: Arc<OciMetrics>,
    digest: String,
}

async fn rig(auth: Option<OciAuth>) -> Rig {
    let memory = Arc::new(MemoryBackend::new());
    let content = b"layer bytes";
    let hex = hex::encode(Sha256::digest(content));
    let key = format!("oci/blobs/sha256/{hex}");
    memory
        .put(&key, Bytes::from_static(content))
        .await
        .expect("put");
    let metrics = Arc::new(OciMetrics::default());
    let prefix = OciPrefix::new("oci/").expect("a prefix");
    let front = OciFront::new(memory, prefix, Arc::clone(&metrics)).with_auth(auth);
    Rig {
        front,
        metrics,
        digest: format!("sha256:{hex}"),
    }
}

impl Rig {
    async fn status(
        &self,
        method: Method,
        path: &str,
        headers: &HeaderMap,
    ) -> (StatusCode, HeaderMap) {
        let response = self.front.handle(&method, path, headers).await;
        (response.status(), response.headers().clone())
    }
}

/// Asserts the answers of a front with keys, as a client sees them: 401 with the Basic challenge and
/// no login, 200 with one, 403 without the right, 405 for a push of a key that may push, and that
/// each is counted under its own outcome. The 401 and the 403 carry the version header too.
#[tokio::test]
async fn the_front_answers_and_counts_each_refusal() {
    let rig = rig(Some(auth())).await;
    let blob = format!("/v2/app/blobs/{}", rig.digest);
    let none = HeaderMap::new();

    let (status, headers) = rig.status(Method::GET, "/v2/", &none).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["www-authenticate"], "Basic realm=\"tierstore\"");
    assert_eq!(headers["docker-distribution-api-version"], "registry/2.0");

    let reader = login("READER", "reader-secret");
    assert_eq!(
        rig.status(Method::GET, "/v2/", &reader).await.0,
        StatusCode::OK
    );
    assert_eq!(
        rig.status(Method::GET, &blob, &reader).await.0,
        StatusCode::OK
    );
    let (status, headers) = rig.status(Method::PUT, &blob, &reader).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!headers.contains_key("www-authenticate"));
    assert_eq!(headers["docker-distribution-api-version"], "registry/2.0");

    let full = login("FULL", "full-secret");
    let (status, headers) = rig.status(Method::PUT, &blob, &full).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers["allow"], "GET, HEAD");

    let metrics = &rig.metrics;
    assert_eq!(metrics.requests("ping", "unauthorized"), 1);
    assert_eq!(metrics.requests("ping", "ok"), 1);
    assert_eq!(metrics.requests("blob", "ok"), 1);
    assert_eq!(metrics.requests("blob", "denied"), 1);
    assert_eq!(metrics.requests("blob", "not_allowed"), 1);
    assert_eq!(metrics.requests("blob", "unauthorized"), 0);
}

/// The message of the one error the front answers `method` of `path` with.
async fn message(rig: &Rig, method: Method, path: &str, headers: HeaderMap) -> String {
    let response = rig.front.handle(&method, path, &headers).await;
    let body = http_body_util::BodyExt::collect(response.into_body()).await;
    let bytes = body.expect("a body").to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    document["errors"][0]["message"]
        .as_str()
        .expect("text")
        .to_owned()
}

/// Asserts the message of each refusal: a 403 says whether the key may not pull, may not push or may
/// do neither, and names no key.
#[tokio::test]
async fn a_denial_says_which_right_is_missing() {
    let rig = rig(Some(auth())).await;
    let blob = format!("/v2/app/blobs/{}", rig.digest);
    let writer = login("WRITER", "writer-secret");
    assert_eq!(
        message(&rig, Method::GET, &blob, writer).await,
        "this access key may not pull"
    );
    let reader = login("READER", "reader-secret");
    assert_eq!(
        message(&rig, Method::DELETE, &blob, reader).await,
        "this access key may not push"
    );
    let gc = login("GC", "gc-secret");
    assert_eq!(
        message(&rig, Method::GET, "/v2/", gc).await,
        "this access key may neither pull nor push here"
    );
    let unknown = login("NOBODY", "x");
    assert_eq!(
        message(&rig, Method::GET, &blob, unknown).await,
        "this registry asks for an access key: log in with its id and secret"
    );
}

/// Asserts that a front made without keys is anonymous as before, whatever a request carries.
#[tokio::test]
async fn a_front_without_keys_asks_for_none() {
    let rig = rig(None).await;
    let blob = format!("/v2/app/blobs/{}", rig.digest);
    let wrong = login("NOBODY", "x");
    assert_eq!(
        rig.status(Method::GET, &blob, &wrong).await.0,
        StatusCode::OK
    );
    let none = HeaderMap::new();
    assert_eq!(
        rig.status(Method::GET, "/v2/", &none).await.0,
        StatusCode::OK
    );
    assert_eq!(
        rig.status(Method::PUT, &blob, &none).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
}
