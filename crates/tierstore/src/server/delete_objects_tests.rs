//! DeleteObjects (`POST /<bucket>?delete`), through `Server::handle`: the document it reads, the
//! integrity header it needs, the result it gives for each key, and who may call it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use hyper::StatusCode;

use crate::{Backend, ByteRange, Error, Listed, Meta, Op};

use http_body_util::{BodyExt, Full};
use hyper::Request;

use super::auth::sign;
use super::presign::presign_url;
use super::presign::tests::request;
use super::testing::{BUCKET, KEY_ID, Reply, Rig, SECRET, checksum, texts};
use super::xml::escape;
use super::{AccessKey, Prefixes, ScopedKey, Server, ServerConfig};

const MD5: &str = "content-md5";

/// The document of a DeleteObjects call for `keys`.
pub(super) fn document(keys: &[&str], quiet: Option<&str>) -> String {
    let mut xml = String::from("<Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    if let Some(quiet) = quiet {
        xml.push_str(&format!("<Quiet>{quiet}</Quiet>"));
    }
    for key in keys {
        xml.push_str(&format!("<Object><Key>{}</Key></Object>", escape(key)));
    }
    xml.push_str("</Delete>");
    xml
}

/// Sends the call with `body` and the integrity header `name` worked out for it.
async fn call(rig: &Rig, name: &str, body: &str) -> Reply {
    let sum = checksum(name, body.as_bytes());
    let uri = format!("/{BUCKET}?delete");
    rig.send("POST", &uri, &[(name, sum.as_str())], body.as_bytes())
        .await
}

/// The call for `keys`, with `Content-MD5`.
async fn delete(rig: &Rig, keys: &[&str]) -> Reply {
    call(rig, MD5, &document(keys, None)).await
}

/// A rig with the objects `names` in it, copied to the cold tier too.
async fn rig_with(names: &[&str]) -> Rig {
    let rig = Rig::new(1 << 20).await;
    for name in names {
        assert_eq!(
            rig.put(name, b"body").await.status,
            StatusCode::OK,
            "{name}"
        );
    }
    rig.settle().await;
    rig
}

/// The `Code` and `Message` of each `Error` of a result, with its key.
pub(super) fn errors(body: &str) -> Vec<(String, String, String)> {
    let doc = roxmltree::Document::parse(body).unwrap();
    let text = |node: roxmltree::Node, name: &str| {
        let child = node.children().find(|c| c.tag_name().name() == name);
        child.and_then(|c| c.text()).unwrap_or_default().to_owned()
    };
    doc.descendants()
        .filter(|node| node.tag_name().name() == "Error")
        .map(|node| (text(node, "Key"), text(node, "Code"), text(node, "Message")))
        .collect()
}

/// Asserts that a call lists a `Deleted` for each key in the order asked, a key that is not
/// there included (S3 does), that the objects are gone from the server and from both tiers once
/// the reconciler has run, and that the answer is a `DeleteResult` in the S3 namespace.
#[tokio::test(start_paused = true)]
async fn the_keys_are_deleted_from_both_tiers_and_each_is_listed() {
    let rig = rig_with(&["a", "dir/b", "keep"]).await;
    assert!(rig.cold_mem.head("a").await.is_ok());
    let reply = delete(&rig, &["a", "dir/b", "missing"]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .body
            .contains("<DeleteResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">")
    );
    assert_eq!(texts(&reply.body, "Key"), ["a", "dir/b", "missing"]);
    assert_eq!(texts(&reply.body, "Deleted").len(), 3);
    assert!(errors(&reply.body).is_empty());
    rig.settle().await;
    for name in ["a", "dir/b"] {
        assert_eq!(rig.get(name, None).await.status, StatusCode::NOT_FOUND);
        assert!(rig.hot_mem.head(name).await.is_err(), "{name} in hot");
        assert!(rig.cold_mem.head(name).await.is_err(), "{name} in cold");
    }
    assert_eq!(rig.get("keep", None).await.body, "body");
}

/// Asserts that `Quiet` true leaves the `Deleted` entries out and still deletes, that `Quiet`
/// false lists them, and that a failure is listed whether or not the call is quiet.
#[tokio::test(start_paused = true)]
async fn quiet_leaves_out_the_deleted_but_not_the_errors() {
    let rig = rig_with(&["a", "b"]).await;
    let quiet = call(&rig, MD5, &document(&["a", "_journal/x"], Some("true"))).await;
    assert_eq!(quiet.status, StatusCode::OK);
    assert!(texts(&quiet.body, "Deleted").is_empty());
    let listed = errors(&quiet.body);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "_journal/x");
    assert_eq!(rig.get("a", None).await.status, StatusCode::NOT_FOUND);
    let loud = call(&rig, MD5, &document(&["b"], Some("false"))).await;
    assert_eq!(texts(&loud.body, "Key"), ["b"]);
    assert_eq!(rig.get("b", None).await.status, StatusCode::NOT_FOUND);
}

/// Asserts that a key the proxy keeps for itself is an `Error` with `AccessDenied` in the
/// result, beside the `Deleted` of the others, that the object under it is not touched, and that
/// the call does not fail as a whole.
#[tokio::test(start_paused = true)]
async fn a_reserved_key_is_an_access_denied_of_its_own() {
    let rig = rig_with(&["a"]).await;
    let reserved = ["_journal/j", "_tiering/lease", "_entries/e"];
    let earlier = rig.hot.call_log().len();
    let reply = delete(&rig, &["_journal/j", "a", "_tiering/lease", "_entries/e"]).await;
    assert_eq!(reply.status, StatusCode::OK);
    let listed = errors(&reply.body);
    let keys: Vec<&str> = listed.iter().map(|(key, _, _)| key.as_str()).collect();
    assert_eq!(keys, reserved);
    for (key, code, message) in &listed {
        assert_eq!(code, "AccessDenied", "{key}");
        assert!(message.contains(key.as_str()), "{key}: {message}");
    }
    assert_eq!(texts(&reply.body, "Deleted").len(), 1);
    assert_eq!(rig.get("a", None).await.status, StatusCode::NOT_FOUND);
    let touched = rig.hot.call_log();
    let touched = touched
        .iter()
        .skip(earlier)
        .filter(|(op, key)| *op == Op::Delete && reserved.contains(&key.as_str()));
    assert_eq!(touched.count(), 0);
}

/// The deletes of the key `a` that the two tiers of the rig saw.
fn deletes_of_a(rig: &Rig) -> usize {
    let logs = [rig.hot.call_log(), rig.cold.call_log()];
    let same = |(op, key): &&(Op, String)| *op == Op::Delete && key == "a";
    logs.iter().map(|log| log.iter().filter(same).count()).sum()
}

/// Asserts that a key listed twice is deleted once, as a call for it alone deletes it, and
/// answered twice, in the order asked.
#[tokio::test(start_paused = true)]
async fn a_key_listed_twice_is_deleted_once_and_answered_twice() {
    let alone = rig_with(&["a", "b"]).await;
    delete(&alone, &["a"]).await;
    let rig = rig_with(&["a", "b"]).await;
    let reply = delete(&rig, &["a", "b", "a"]).await;
    assert_eq!(texts(&reply.body, "Key"), ["a", "b", "a"]);
    assert!(deletes_of_a(&alone) > 0);
    assert_eq!(deletes_of_a(&rig), deletes_of_a(&alone));
}

/// Asserts that a call with no `Content-MD5` and no checksum header is 400 `InvalidRequest`
/// and deletes nothing, and that each of `Content-MD5`, CRC-32, CRC-32C, CRC-64/NVMe, SHA-1 and
/// SHA-256 of the body is accepted.
#[tokio::test(start_paused = true)]
async fn an_integrity_header_is_needed_and_any_of_the_six_will_do() {
    let rig = rig_with(&["a"]).await;
    let body = document(&["a"], None);
    let uri = format!("/{BUCKET}?delete");
    let bare = rig.send("POST", &uri, &[], body.as_bytes()).await;
    assert_eq!(bare.status, StatusCode::BAD_REQUEST);
    assert_eq!(bare.code(), "InvalidRequest");
    assert_eq!(rig.get("a", None).await.body, "body");
    for name in super::testing::CHECKSUMS {
        let rig = rig_with(&["a"]).await;
        let reply = call(&rig, name, &body).await;
        assert_eq!(reply.status, StatusCode::OK, "{name}");
        assert_eq!(
            rig.get("a", None).await.status,
            StatusCode::NOT_FOUND,
            "{name}"
        );
    }
}

/// Asserts that a body that is not the one the header names is 400 `BadDigest` and a header that
/// is no hash is 400 `InvalidDigest`, for `Content-MD5` and for a checksum header, and that
/// nothing is deleted; and that a CRC-64/NVMe header that does not match the document is 400
/// `BadDigest`, as it is for a put.
#[tokio::test(start_paused = true)]
async fn a_body_that_does_not_match_its_header_deletes_nothing() {
    let rig = rig_with(&["a"]).await;
    let uri = format!("/{BUCKET}?delete");
    let body = document(&["a"], None);
    for name in [
        "content-md5",
        "x-amz-checksum-crc32",
        "x-amz-checksum-sha256",
    ] {
        let other = checksum(name, b"another body");
        let reply = rig
            .send("POST", &uri, &[(name, other.as_str())], body.as_bytes())
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(reply.code(), "BadDigest", "{name}");
        let short = rig
            .send("POST", &uri, &[(name, "AAAA")], body.as_bytes())
            .await;
        assert_eq!(short.code(), "InvalidDigest", "{name}");
    }
    let nvme = rig
        .send(
            "POST",
            &uri,
            &[("x-amz-checksum-crc64nvme", "AAAAAAAAAAA=")],
            body.as_bytes(),
        )
        .await;
    assert_eq!(nvme.status, StatusCode::BAD_REQUEST);
    assert_eq!(nvme.code(), "BadDigest");
    assert_eq!(rig.get("a", None).await.body, "body");
}

/// Asserts that a document S3 would not take is 400 `MalformedXML` and deletes nothing: text
/// that is not XML, another root, no object, an object with no key, an empty key, a `Quiet`
/// that is neither `true` nor `false`; and that 1000 keys are taken and 1001 are not.
#[tokio::test(start_paused = true)]
async fn a_document_that_is_not_a_delete_is_malformed() {
    let rig = rig_with(&["a"]).await;
    let ns = "xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"";
    let bad = [
        "not xml".to_owned(),
        format!("<Remove {ns}><Object><Key>a</Key></Object></Remove>"),
        format!("<Delete {ns}></Delete>"),
        format!("<Delete {ns}><Object></Object></Delete>"),
        format!("<Delete {ns}><Object><Key></Key></Object></Delete>"),
        format!("<Delete {ns}><Quiet>maybe</Quiet><Object><Key>a</Key></Object></Delete>"),
    ];
    for body in &bad {
        let reply = call(&rig, MD5, body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reply.code(), "MalformedXML", "{body}");
    }
    let names: Vec<String> = (0..1001).map(|n| format!("k{n}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let over = delete(&rig, &refs).await;
    assert_eq!(over.status, StatusCode::BAD_REQUEST);
    assert_eq!(over.code(), "MalformedXML");
    let exact = delete(&rig, &refs[..1000]).await;
    assert_eq!(exact.status, StatusCode::OK);
    assert_eq!(texts(&exact.body, "Deleted").len(), 1000);
    assert_eq!(rig.get("a", None).await.body, "body");
}

/// Asserts that a body larger than 1000 keys of the longest key can fill is 413 `EntityTooLarge`
/// before it is held.
#[tokio::test(start_paused = true)]
async fn a_body_over_what_a_thousand_keys_fill_is_too_large() {
    let rig = rig_with(&[]).await;
    let filler = " ".repeat(1000 * (1024 * 6 + 128) + 1);
    let body = format!("{}{filler}", document(&["a"], None));
    let reply = call(&rig, MD5, &body).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.code(), "EntityTooLarge");
}

/// Asserts that an object with a version id is an `Error` of `NotImplemented` for that key
/// alone, and the other keys are deleted.
#[tokio::test(start_paused = true)]
async fn a_version_id_is_an_error_of_its_key_alone() {
    let rig = rig_with(&["a", "b"]).await;
    let ns = "xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"";
    let body = format!(
        "<Delete {ns}><Object><Key>a</Key><VersionId>v1</VersionId></Object>\
         <Object><Key>b</Key></Object></Delete>"
    );
    let reply = call(&rig, MD5, &body).await;
    assert_eq!(reply.status, StatusCode::OK);
    let listed = errors(&reply.body);
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].0.as_str(), listed[0].1.as_str()),
        ("a", "NotImplemented")
    );
    assert_eq!(rig.get("a", None).await.body, "body");
    assert_eq!(rig.get("b", None).await.status, StatusCode::NOT_FOUND);
}

/// A backend that deletes nothing and counts: it fails the key `bad` and holds each other
/// delete open for a few turns of the runtime, noting how many are open at once.
struct Probing {
    open: AtomicUsize,
    most: AtomicUsize,
}

#[async_trait]
impl Backend for Probing {
    async fn put(&self, key: &str, _body: Bytes) -> Result<(), Error> {
        Err(Error::Full(key.to_owned()))
    }
    async fn put_if_match(&self, key: &str, _body: Bytes, _tag: &str) -> Result<String, Error> {
        Err(Error::NotFound(key.to_owned()))
    }
    async fn get(&self, key: &str, _range: Option<ByteRange>) -> Result<Bytes, Error> {
        Err(Error::NotFound(key.to_owned()))
    }
    async fn head(&self, key: &str) -> Result<Meta, Error> {
        Err(Error::NotFound(key.to_owned()))
    }
    async fn delete(&self, key: &str) -> Result<(), Error> {
        let now = self.open.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        self.open.fetch_sub(1, Ordering::SeqCst);
        if key == "bad" {
            return Err(Error::Unavailable("the store is down".to_owned()));
        }
        Ok(())
    }
    async fn list(&self, _prefix: &str) -> Result<Vec<Listed>, Error> {
        Ok(Vec::new())
    }
}

/// Sends the call for `keys` to a server over `backend`, with `Content-MD5`, and reads the answer.
async fn call_probing(backend: Arc<Probing>, concurrency: usize, keys: &[&str]) -> Reply {
    let clock = Arc::new(crate::TestClock::new());
    let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock);
    config.delete_concurrency = concurrency;
    let server = Server::new(backend, config.clone());
    let body = document(keys, None);
    let sum = checksum(MD5, body.as_bytes());
    let uri = format!("/{BUCKET}?delete");
    let request = sign(
        &config,
        "POST",
        &uri,
        &[(MD5, sum.as_str())],
        body.as_bytes(),
    );
    let response = server.handle(request).await;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("a whole body").to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn idle_probe() -> Arc<Probing> {
    Arc::new(Probing {
        open: AtomicUsize::new(0),
        most: AtomicUsize::new(0),
    })
}

/// Asserts that at most `server.delete_concurrency` deletes are open at once and that they are
/// that many when there are keys enough, with one and with three; and that every key is answered
/// in the order asked whatever the order the deletes end in.
#[tokio::test]
async fn the_deletes_run_as_many_at_once_as_the_setting_says() {
    let names: Vec<String> = (0..12).map(|n| format!("k{n}")).collect();
    let keys: Vec<&str> = names.iter().map(String::as_str).collect();
    for concurrency in [1, 3] {
        let probing = idle_probe();
        let reply = call_probing(Arc::clone(&probing), concurrency, &keys).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(texts(&reply.body, "Key"), names);
        assert_eq!(probing.most.load(Ordering::SeqCst), concurrency);
    }
}

/// Asserts that a delete the backend fails is an `Error` of its key alone, with the code and the
/// fixed text a single delete of it would give (not the backend's own words), and the other keys
/// are `Deleted`.
#[tokio::test]
async fn a_failed_delete_is_an_error_of_its_key_alone() {
    let reply = call_probing(idle_probe(), 4, &["x", "bad", "y"]).await;
    assert_eq!(reply.status, StatusCode::OK);
    let listed = errors(&reply.body);
    assert_eq!(listed.len(), 1);
    let (key, code, message) = &listed[0];
    assert_eq!(key, "bad");
    assert_eq!(code, "ServiceUnavailable");
    assert_eq!(message, "The store did not answer.");
    assert_eq!(texts(&reply.body, "Deleted").len(), 2);
    assert!(!reply.body.contains("the store is down"));
}

/// Asserts who may call it: a read-only key and a scoped key (with the right to delete under
/// `cas/`) are 403 `AccessDenied` and nothing is deleted, and so is a presigned URL of the full
/// key signed for the call.
#[tokio::test(start_paused = true)]
async fn only_the_full_key_may_call_it() {
    let rig = Rig::with(1 << 20, |config| {
        config.read_only_keys = vec![AccessKey::new("AKIDREADER", "reader-secret-0123")];
        let prefixes = Prefixes {
            read: &["cas/"],
            write: &["cas/"],
            delete: &["cas/"],
        };
        let scoped = ScopedKey::new("scoped", "scoped-secret-0123456789", &prefixes);
        config.scoped_keys = vec![scoped.unwrap()];
    })
    .await;
    rig.put("cas/a", b"body").await;
    let body = document(&["cas/a"], None);
    let sum = checksum(MD5, body.as_bytes());
    let uri = format!("/{BUCKET}?delete");
    for key in [
        ("AKIDREADER", "reader-secret-0123"),
        ("scoped", "scoped-secret-0123456789"),
    ] {
        let extra = [(MD5, sum.as_str())];
        let reply = rig
            .send_as(key, "POST", &uri, &extra, body.as_bytes())
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", key.0);
        assert_eq!(reply.code(), "AccessDenied", "{}", key.0);
    }
    let full = AccessKey::new(KEY_ID, SECRET);
    let url = presign_url("POST", &full, &request("", 300), &[("delete", "")]).unwrap();
    let rest = url.split_once("://").unwrap().1;
    let at = rest.find('/').unwrap();
    let presigned = Request::builder()
        .method("POST")
        .uri(&rest[at..])
        .header("host", &rest[..at])
        .header(MD5, sum.as_str())
        .body(Full::new(Bytes::from(body.clone())))
        .unwrap();
    let reply = rig.send_raw(presigned).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(rig.get("cas/a", None).await.body, "body");
}

/// Asserts that `?delete` makes a call only of a POST to the bucket: a DELETE of an object, a
/// GET of the bucket and a POST to an object with it are 501 and change nothing, so a delete of
/// one key cannot be asked for with a parameter that S3 would not take there.
#[tokio::test(start_paused = true)]
async fn the_delete_parameter_is_for_the_post_to_the_bucket_alone() {
    let rig = rig_with(&["a"]).await;
    let object = format!("/{BUCKET}/a?delete");
    let bucket = format!("/{BUCKET}?delete");
    for (method, uri) in [("DELETE", &object), ("POST", &object), ("GET", &bucket)] {
        let reply = rig.send(method, uri, &[], b"").await;
        assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{method} {uri}");
    }
    assert_eq!(rig.get("a", None).await.body, "body");
}
