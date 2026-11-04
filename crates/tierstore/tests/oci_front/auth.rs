//! The front with keys required, over TLS: a registry client logs in with HTTP Basic, and each
//! kind of key may do what its rights say.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tierstore::SystemClock;
use tierstore::server::scoped::{Prefixes, ScopedKey};
use tierstore::server::{AccessKey, ServerConfig};

use crate::client::Reply;
use crate::rig::Rig;

const MANIFEST: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[]}"#;

/// The keys of the server: the full key, a read-only key, and four scoped keys.
fn server() -> ServerConfig {
    let mut server = ServerConfig::new("bucket", "FULL", "full-secret", Arc::new(SystemClock));
    server
        .read_only_keys
        .push(AccessKey::new("READER", "reader-secret"));
    let scoped = |id: &str, prefixes: &Prefixes<'_>| {
        ScopedKey::new(id, &format!("{id}-secret"), prefixes).expect("a scoped key")
    };
    let keys = [
        // Reads, and lists only, over the front's objects.
        scoped(
            "SCOPEDREAD",
            &Prefixes {
                read: &["oci/"],
                ..Prefixes::default()
            },
        ),
        // Writes over the front's objects, and reads nothing.
        scoped(
            "SCOPEDWRITE",
            &Prefixes {
                write: &["oci/"],
                ..Prefixes::default()
            },
        ),
        // Reads part of the front's objects: not every digest, so not a pull.
        scoped(
            "SCOPEDDEEP",
            &Prefixes {
                read: &["oci/blobs/sha256/ab/"],
                ..Prefixes::default()
            },
        ),
        // Reads and writes elsewhere.
        scoped(
            "SCOPEDELSEWHERE",
            &Prefixes {
                read: &["other/"],
                write: &["other/"],
                ..Prefixes::default()
            },
        ),
    ];
    server.scoped_keys.extend(keys);
    server
}

/// The `Authorization` value of a Basic login.
fn basic(id: &str, secret: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{id}:{secret}")))
}

/// The secret that `server()` gives the key `id`.
fn secret_of(id: &str) -> String {
    match id {
        "FULL" => "full-secret".to_owned(),
        "READER" => "reader-secret".to_owned(),
        other => format!("{other}-secret"),
    }
}

async fn ask_as(rig: &Rig, id: &str, method: &str, path: &str) -> Reply {
    let login = basic(id, &secret_of(id));
    let headers = [("authorization", login.as_str())];
    rig.ask(method, path, &headers).await.expect("an answer")
}

#[track_caller]
fn assert_error(reply: &Reply, status: u16, code: &str) {
    assert_eq!(reply.status, status, "{reply:?}");
    assert_eq!(reply.error_code(), code);
    assert_eq!(
        reply.must("docker-distribution-api-version"),
        "registry/2.0"
    );
}

/// Asserts the flow of a registry client with the read-only key: `GET /v2/` with no login is 401
/// with the Basic challenge, the same call with the login is 200, and the manifest and the blob
/// are then pulled with it, byte for byte. The login and the 401 are counted as such.
#[tokio::test]
async fn a_client_logs_in_with_a_read_only_key_and_pulls() {
    let rig = Rig::start_with_keys(&server()).await;
    let manifest = rig.seed(MANIFEST.as_bytes()).await;
    let layer = rig.seed(b"layer bytes").await;

    let first = rig.get("/v2/").await;
    assert_error(&first, 401, "UNAUTHORIZED");
    assert_eq!(first.must("www-authenticate"), "Basic realm=\"tierstore\"");

    let ping = ask_as(&rig, "READER", "GET", "/v2/").await;
    assert_eq!((ping.status, ping.body.as_slice()), (200, b"{}".as_slice()));

    let pulled = ask_as(
        &rig,
        "READER",
        "GET",
        &format!("/v2/app/manifests/{manifest}"),
    )
    .await;
    assert_eq!(
        (pulled.status, pulled.body.as_slice()),
        (200, MANIFEST.as_bytes())
    );
    let blob = ask_as(&rig, "READER", "GET", &format!("/v2/app/blobs/{layer}")).await;
    assert_eq!(
        (blob.status, blob.body.as_slice()),
        (200, b"layer bytes".as_slice())
    );
    let head = ask_as(&rig, "READER", "HEAD", &format!("/v2/app/blobs/{layer}")).await;
    assert_eq!(head.status, 200);

    assert_eq!(rig.metrics.requests("ping", "unauthorized"), 1);
    assert_eq!(rig.metrics.requests("ping", "ok"), 1);
    assert_eq!(rig.metrics.requests("blob", "ok"), 2);
    rig.stop().await;
}

/// Asserts that everything that is not a login of a key of the server with its secret is the same
/// 401 with the same challenge, for a blob as for the ping: no header, another scheme, text that is
/// not base64, no colon in the login, an id the server does not have, and a wrong secret, a secret
/// that is only a prefix of the right one included.
#[tokio::test]
async fn a_login_that_is_not_a_key_of_the_server_is_401() {
    let rig = Rig::start_with_keys(&server()).await;
    let layer = rig.seed(b"layer bytes").await;
    let no_colon = format!("Basic {}", STANDARD.encode("FULL"));
    let logins = [
        None,
        Some("Bearer abc".to_owned()),
        Some("Basic".to_owned()),
        Some("Basic !!!not-base64".to_owned()),
        Some(no_colon),
        Some(basic("NOBODY", "full-secret")),
        Some(basic("FULL", "wrong")),
        Some(basic("FULL", "full-secre")),
        Some(basic("FULL", "")),
    ];
    for login in &logins {
        for path in [
            "/v2/".to_owned(),
            format!("/v2/app/blobs/{layer}"),
            "/nowhere".to_owned(),
        ] {
            let headers: Vec<(&str, &str)> = login
                .iter()
                .map(|value| ("authorization", value.as_str()))
                .collect();
            let reply = rig.ask("GET", &path, &headers).await.expect("an answer");
            assert_error(&reply, 401, "UNAUTHORIZED");
            let challenge = reply.must("www-authenticate");
            assert_eq!(challenge, "Basic realm=\"tierstore\"", "{login:?} {path}");
        }
    }
    rig.stop().await;
}

/// Asserts what each kind of key may do on the blob route: the full key and the keys with a read
/// prefix over the front's objects pull; a write-only scoped key, a scoped key that reads only part
/// of the front's objects and a scoped key elsewhere are refused 403 `DENIED`, as is every key
/// that does not read, on a pull of a blob.
#[tokio::test]
async fn only_keys_that_read_over_the_front_may_pull() {
    let rig = Rig::start_with_keys(&server()).await;
    let layer = rig.seed(b"layer bytes").await;
    let path = format!("/v2/app/blobs/{layer}");
    for id in ["FULL", "READER", "SCOPEDREAD"] {
        let reply = ask_as(&rig, id, "GET", &path).await;
        assert_eq!(reply.status, 200, "{id}");
    }
    for id in ["SCOPEDWRITE", "SCOPEDDEEP", "SCOPEDELSEWHERE"] {
        let reply = ask_as(&rig, id, "GET", &path).await;
        assert_error(&reply, 403, "DENIED");
        assert!(reply.header("www-authenticate").is_none(), "{id}");
    }
    assert_eq!(rig.metrics.requests("blob", "denied"), 3);
    rig.stop().await;
}

/// Asserts what each kind of key may do with `PUT`, `POST`, `PATCH` and `DELETE`: the full key and
/// the scoped key with a write prefix over the front's objects may push, which the front answers
/// 405 as it always has (it serves no upload); the read-only key, the read scoped key and the
/// scoped key elsewhere are refused 403 `DENIED` before that.
#[tokio::test]
async fn only_keys_that_write_over_the_front_may_push() {
    let rig = Rig::start_with_keys(&server()).await;
    let layer = rig.seed(b"layer bytes").await;
    let path = format!("/v2/app/blobs/{layer}");
    for method in ["PUT", "POST", "PATCH", "DELETE"] {
        for id in ["FULL", "SCOPEDWRITE"] {
            let reply = ask_as(&rig, id, method, &path).await;
            assert_error(&reply, 405, "UNSUPPORTED");
        }
        for id in ["READER", "SCOPEDREAD", "SCOPEDDEEP", "SCOPEDELSEWHERE"] {
            let reply = ask_as(&rig, id, method, &path).await;
            assert_error(&reply, 403, "DENIED");
        }
    }
    rig.stop().await;
}

/// Asserts the login call: `GET /v2/` is 200 for a key that may pull or push (a write-only scoped
/// key can log in to push) and 403 for a key that may do neither.
#[tokio::test]
async fn the_ping_needs_a_key_that_may_pull_or_push() {
    let rig = Rig::start_with_keys(&server()).await;
    for id in ["FULL", "READER", "SCOPEDREAD", "SCOPEDWRITE"] {
        let reply = ask_as(&rig, id, "GET", "/v2/").await;
        assert_eq!(reply.status, 200, "{id}");
    }
    for id in ["SCOPEDDEEP", "SCOPEDELSEWHERE"] {
        let reply = ask_as(&rig, id, "GET", "/v2/").await;
        assert_error(&reply, 403, "DENIED");
    }
    rig.stop().await;
}
