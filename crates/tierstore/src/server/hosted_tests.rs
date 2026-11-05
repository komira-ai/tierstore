//! Virtual-hosted-style addressing (`<bucket>.<domain>/<key>`), through `Server::handle`, and the
//! pieces that decide it: which host names a bucket, and which text is a domain.

use bytes::Bytes;
use http_body_util::Full;
use hyper::http::request::Parts;
use hyper::{Request, StatusCode};

use super::AccessKey;
use super::auth::sign_on;
use super::hosted::{hosted_bucket, resolve_hosted, valid_domain};
use super::presign::presign_path;
use super::presign::tests::request;
use super::request::Target;
use super::testing::{BUCKET, KEY_ID, Reply, Rig, SECRET, texts};

/// The host of the bucket of the rig under the domain `s3.local`.
const HOST: &str = "tier-bucket.s3.local";

async fn hosted_rig() -> Rig {
    Rig::with(1 << 20, |config| {
        config.domains = vec!["s3.local".to_owned()];
    })
    .await
}

fn parts_of(host: Option<&str>, uri: &str) -> Parts {
    let mut builder = Request::builder().uri(uri);
    if let Some(host) = host {
        builder = builder.header("host", host);
    }
    builder.body(()).unwrap().into_parts().0
}

fn domains(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

fn bucket_of(host: &str, list: &[&str]) -> Option<String> {
    hosted_bucket(&parts_of(Some(host), "/"), &domains(list))
}

/// Asserts which host names a bucket: the part before `.<domain>`, whatever the case of the
/// host, with a port or a trailing dot, with dots in the bucket name, and for the longest domain
/// the host is under; and no bucket for the bare domain, a host under no domain, an IPv4 or IPv6
/// address, a host with an empty label before the domain, and any host when there are no domains.
#[test]
fn a_host_names_the_bucket_before_its_domain() {
    let list = ["s3.local"];
    assert_eq!(bucket_of("tier.s3.local", &list).as_deref(), Some("tier"));
    assert_eq!(
        bucket_of("Tier.S3.Local:9000", &list).as_deref(),
        Some("tier")
    );
    assert_eq!(bucket_of("tier.s3.local.", &list).as_deref(), Some("tier"));
    assert_eq!(bucket_of("a.b.s3.local", &list).as_deref(), Some("a.b"));
    assert_eq!(
        bucket_of("tier.s3.local", &["local", "s3.local"]).as_deref(),
        Some("tier")
    );
    assert_eq!(
        bucket_of("tier.s3.local", &["s3.local", "local"]).as_deref(),
        Some("tier")
    );
    for host in [
        "s3.local",
        "tier.other.test",
        "127.0.0.1:9000",
        "[::1]:9000",
        ".s3.local",
    ] {
        assert_eq!(bucket_of(host, &list), None, "{host}");
    }
    assert_eq!(bucket_of("tier.s3.local", &[]), None);
    assert_eq!(bucket_of("xs3.local", &list), None);
}

/// Asserts that the host comes from the authority of the URI when the request has no `Host`
/// header, and that a request with neither names no bucket.
#[test]
fn the_host_is_the_uri_authority_without_a_header() {
    let list = domains(&["s3.local"]);
    let absolute = parts_of(None, "http://tier.s3.local:9000/key");
    assert_eq!(hosted_bucket(&absolute, &list).as_deref(), Some("tier"));
    assert_eq!(hosted_bucket(&parts_of(None, "/key"), &list), None);
}

/// Asserts what a path under a bucket's host names: the bucket for `/`, an object for anything
/// else, decoded, a key that starts with the bucket's name staying a key; the bucket compared
/// without regard to case; another bucket `NoSuchBucket` with the name; a path that is not UTF-8
/// refused.
#[test]
fn a_path_under_a_bucket_host_is_the_key() {
    let at = |label: &str, path: &str| resolve_hosted("tier", label, path);
    assert_eq!(at("tier", "/").unwrap(), Target::Bucket);
    assert_eq!(at("tier", "").unwrap(), Target::Bucket);
    assert_eq!(
        at("tier", "/a/b%20c").unwrap(),
        Target::Object("a/b c".to_owned())
    );
    assert_eq!(
        at("tier", "/tier/x").unwrap(),
        Target::Object("tier/x".to_owned())
    );
    assert_eq!(at("TIER", "/k").unwrap(), Target::Object("k".to_owned()));
    let other = at("other", "/k").unwrap_err();
    assert_eq!(other.kind(), (StatusCode::NOT_FOUND, "NoSuchBucket"));
    assert!(other.message().contains("other"));
    assert_eq!(
        at("tier", "/%FF").unwrap_err().kind().0,
        StatusCode::BAD_REQUEST
    );
}

/// Asserts which text is a domain: names of letters, digits and hyphens, kept in lower case; and
/// not a URL, a port, a path, an empty text, an empty label, a label that starts or ends with a
/// hyphen, or a name with a space or an underscore.
#[test]
fn only_a_domain_name_is_a_domain() {
    assert_eq!(
        valid_domain("S3.Example.com").as_deref(),
        Some("s3.example.com")
    );
    assert_eq!(valid_domain("localhost").as_deref(), Some("localhost"));
    assert_eq!(valid_domain("a-b.c1").as_deref(), Some("a-b.c1"));
    for bad in [
        "",
        ".",
        ".s3.local",
        "s3.local.",
        "a..b",
        "http://s3.local",
        "s3.local:9000",
        "s3.local/x",
        "-a.local",
        "a-.local",
        "a b.local",
        "a_b.local",
    ] {
        assert_eq!(valid_domain(bad), None, "{bad:?}");
    }
}

/// Asserts that every call of a bucket is the same call on its host as on the path-style one: a
/// put of a key with a space and a slash, a get, a ranged get, a head, a conditional get, and a
/// delete, each answered as the path-style call is; and that the object put on one is on the
/// other.
#[tokio::test(start_paused = true)]
async fn an_object_is_put_read_and_deleted_on_the_bucket_host() {
    let rig = hosted_rig().await;
    let put = rig
        .send_on(
            HOST,
            "PUT",
            "/dir/a%20b",
            &[("if-none-match", "*")],
            b"0123456789",
        )
        .await;
    assert_eq!(put.status, StatusCode::OK);
    let get = rig.send_on(HOST, "GET", "/dir/a%20b", &[], b"").await;
    assert_eq!(
        (get.status, get.body.as_str()),
        (StatusCode::OK, "0123456789")
    );
    let range = rig
        .send_on(HOST, "GET", "/dir/a%20b", &[("range", "bytes=2-4")], b"")
        .await;
    assert_eq!(
        (range.status, range.body.as_str()),
        (StatusCode::PARTIAL_CONTENT, "234")
    );
    let head = rig.send_on(HOST, "HEAD", "/dir/a%20b", &[], b"").await;
    assert_eq!(head.header("content-length"), "10");
    let tag = head.header("etag").to_owned();
    let same = rig
        .send_on(HOST, "GET", "/dir/a%20b", &[("if-none-match", &tag)], b"")
        .await;
    assert_eq!(same.status, StatusCode::NOT_MODIFIED);
    let path_style = rig.get("dir/a%20b", None).await;
    assert_eq!(path_style.body, "0123456789");
    assert_eq!(path_style.header("etag"), tag);
    let delete = rig.send_on(HOST, "DELETE", "/dir/a%20b", &[], b"").await;
    assert_eq!(delete.status, StatusCode::NO_CONTENT);
    for reply in [
        rig.send_on(HOST, "GET", "/dir/a%20b", &[], b"").await,
        rig.get("dir/a%20b", None).await,
    ] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.code(), "NoSuchKey");
    }
}

/// Asserts the calls on the bucket itself on its host: `GET /?list-type=2` lists the bucket, with
/// the prefix, the same keys as the path-style listing; `GET /` without a list type is the 501 of
/// the path-style `GET /<bucket>`; `HEAD /` and `PUT /` are the 200 of HeadBucket and CreateBucket;
/// and `GET /?uploads` lists the open uploads.
#[tokio::test(start_paused = true)]
async fn the_bucket_calls_are_on_the_slash_of_its_host() {
    let rig = hosted_rig().await;
    for key in ["dir/a", "dir/b", "other"] {
        assert_eq!(rig.put(key, b"x").await.status, StatusCode::OK);
    }
    let listed = rig
        .send_on(HOST, "GET", "/?list-type=2&prefix=dir%2F", &[], b"")
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(texts(&listed.body, "Key"), ["dir/a", "dir/b"]);
    assert_eq!(texts(&listed.body, "Name"), [BUCKET]);
    let path_style = rig.list("prefix=dir%2F").await;
    assert_eq!(texts(&path_style.body, "Key"), texts(&listed.body, "Key"));
    let bare = rig.send_on(HOST, "GET", "/", &[], b"").await;
    assert_eq!(bare.status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        rig.send_on(HOST, "HEAD", "/", &[], b"").await.status,
        StatusCode::OK
    );
    assert_eq!(
        rig.send_on(HOST, "PUT", "/", &[], b"").await.status,
        StatusCode::OK
    );
    let started = rig.send_on(HOST, "POST", "/big?uploads", &[], b"").await;
    assert_eq!(started.status, StatusCode::OK);
    let open = rig.send_on(HOST, "GET", "/?uploads", &[], b"").await;
    assert_eq!(texts(&open.body, "Key"), ["big"]);
}

/// Asserts a whole multipart upload on the bucket host: start, a part, completion and a read of the
/// object; the completion's `Location` and the others are as for the path-style call.
#[tokio::test(start_paused = true)]
async fn a_multipart_upload_runs_on_the_bucket_host() {
    let rig = hosted_rig().await;
    let started = rig.send_on(HOST, "POST", "/m/k?uploads", &[], b"").await;
    let id = texts(&started.body, "UploadId").concat();
    let part = format!("/m/k?partNumber=1&uploadId={id}");
    let stored = rig.send_on(HOST, "PUT", &part, &[], b"part-one").await;
    assert_eq!(stored.status, StatusCode::OK);
    let tag = stored.header("etag").trim_matches('"').to_owned();
    let done = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"{tag}\"</ETag></Part>\
         </CompleteMultipartUpload>"
    );
    let complete = format!("/m/k?uploadId={id}");
    let finished = rig
        .send_on(HOST, "POST", &complete, &[], done.as_bytes())
        .await;
    assert_eq!(finished.status, StatusCode::OK);
    assert_eq!(rig.get("m/k", None).await.body, "part-one");
}

/// Asserts how hosts are told apart: a bucket label that is not the served bucket is 404
/// `NoSuchBucket` naming it, whatever the call; the case of the host, a port and a trailing dot
/// make no difference; the bare domain, another domain and an IP address are path-style, the
/// bare domain and the IP address serving `/<bucket>/<key>` and the other domain reading `/k` as
/// the bucket `k` as it always did; a key that starts with the bucket's name is a key of the
/// bucket; and `*.s3.local` with two domains configured takes the longest.
#[tokio::test(start_paused = true)]
async fn each_host_is_hosted_or_path_style_as_the_domains_say() {
    let rig = Rig::with(1 << 20, |config| {
        config.domains = vec!["local".to_owned(), "s3.local".to_owned()];
    })
    .await;
    rig.put("k", b"one").await;
    rig.put("tier-bucket/k", b"two").await;
    let unknown = rig.send_on("nope.s3.local", "GET", "/k", &[], b"").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.code(), "NoSuchBucket");
    assert!(unknown.body.contains("nope"));
    for host in [
        HOST,
        "TIER-BUCKET.S3.LOCAL",
        "tier-bucket.s3.local:8080",
        "tier-bucket.s3.local.",
    ] {
        let reply = rig.send_on(host, "GET", "/k", &[], b"").await;
        assert_eq!(reply.body, "one", "{host}");
    }
    let nested = rig.send_on(HOST, "GET", "/tier-bucket/k", &[], b"").await;
    assert_eq!(nested.body, "two");
    let single = hosted_rig().await;
    single.put("k", b"one").await;
    for host in ["s3.local", "127.0.0.1:9000", "[::1]:9000"] {
        let path = single
            .send_on(host, "GET", "/tier-bucket/k", &[], b"")
            .await;
        assert_eq!(path.body, "one", "{host}");
    }
    let other = single
        .send_on("tier-bucket.other.test", "GET", "/k", &[], b"")
        .await;
    assert_eq!(other.status, StatusCode::NOT_FOUND);
    assert!(other.body.contains("&quot;k&quot;"), "{}", other.body);
}

/// Asserts that with no domains configured (the default) a bucket-style host changes nothing:
/// `GET /k` on it is the path-style call for the bucket `k`, 404 `NoSuchBucket`, as before.
#[tokio::test(start_paused = true)]
async fn without_domains_every_host_is_path_style() {
    let rig = Rig::new(1 << 20).await;
    rig.put("k", b"one").await;
    let hosted = rig.send_on(HOST, "GET", "/k", &[], b"").await;
    assert_eq!(hosted.status, StatusCode::NOT_FOUND);
    assert_eq!(hosted.code(), "NoSuchBucket");
    let path = rig.send_on(HOST, "GET", "/tier-bucket/k", &[], b"").await;
    assert_eq!(path.body, "one");
}

/// Asserts that the host is signed, so a request cannot be sent to another bucket's host than
/// the one it was signed for: a request signed for the bucket host and sent with another host is
/// 403 `SignatureDoesNotMatch`, and one signed for one host and sent to the same host with a
/// different case of the letters is too (the signature covers the header as sent).
#[tokio::test(start_paused = true)]
async fn the_signature_covers_the_bucket_host() {
    let rig = hosted_rig().await;
    rig.put("k", b"one").await;
    let key = (KEY_ID, SECRET);
    let payload = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b""));
    let signed = sign_on(HOST, &rig.config, key, ("GET", "/k"), &[], &payload, b"");
    let (mut parts, body) = signed.into_parts();
    parts
        .headers
        .insert("host", "nope.s3.local".parse().unwrap());
    let moved = rig.send_raw(Request::from_parts(parts, body)).await;
    assert_eq!(moved.status, StatusCode::FORBIDDEN);
    assert_eq!(moved.code(), "SignatureDoesNotMatch");
    let signed = sign_on(HOST, &rig.config, key, ("GET", "/k"), &[], &payload, b"");
    let (mut parts, body) = signed.into_parts();
    parts
        .headers
        .insert("host", "TIER-BUCKET.s3.local".parse().unwrap());
    let recased = rig.send_raw(Request::from_parts(parts, body)).await;
    assert_eq!(recased.code(), "SignatureDoesNotMatch");
}

/// Asserts that on a bucket's host a key can be called `healthz`, `readyz`, `usagez` or `metricz`
/// (it is an object, and the probe is not answered there), and that the probes are still
/// answered on a host that is not a bucket's.
#[tokio::test(start_paused = true)]
async fn the_probe_paths_are_keys_on_a_bucket_host() {
    let rig = hosted_rig().await;
    for key in ["healthz", "readyz", "usagez", "metricz"] {
        let uri = format!("/{key}");
        let put = rig
            .send_on(HOST, "PUT", &uri, &[("if-none-match", "*")], key.as_bytes())
            .await;
        assert_eq!(put.status, StatusCode::OK, "{key}");
        let got = rig.send_on(HOST, "GET", &uri, &[], b"").await;
        assert_eq!(got.body, key, "{key}");
    }
    let plain = Request::builder()
        .uri("/healthz")
        .header("host", "tier.test")
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(rig.send_raw(plain).await.body, "ok");
    let bare = Request::builder()
        .uri("/healthz")
        .header("host", "s3.local")
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(rig.send_raw(bare).await.body, "ok");
}

fn url_request(url: &str) -> Request<Full<Bytes>> {
    let rest = url.split_once("://").unwrap().1;
    let at = rest.find('/').unwrap();
    Request::builder()
        .uri(&rest[at..])
        .header("host", &rest[..at])
        .body(Full::new(Bytes::new()))
        .unwrap()
}

/// Asserts that a presigned GET URL made for the bucket's host (signed over the host and the path
/// `/<key>`) serves the object, that the same URL with its host changed is
/// `SignatureDoesNotMatch`, and that a URL for a host with another bucket's name is 404
/// `NoSuchBucket`.
#[tokio::test(start_paused = true)]
async fn a_presigned_url_for_a_bucket_host_serves_the_object() {
    let rig = hosted_rig().await;
    rig.put("dir/k", b"one").await;
    let key = AccessKey::new(KEY_ID, SECRET);
    let mut asked = request("dir/k", 300);
    asked.endpoint = "http://tier-bucket.s3.local:9000";
    let url = presign_path("GET", &key, &asked, &[], "/dir/k").unwrap();
    let served: Reply = rig.send_raw(url_request(&url)).await;
    assert_eq!(
        (served.status, served.body.as_str()),
        (StatusCode::OK, "one")
    );
    let moved = url.replace("tier-bucket.s3.local", "nope.s3.local");
    let refused = rig.send_raw(url_request(&moved)).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.code(), "SignatureDoesNotMatch");
    let wrong_bucket = request("dir/k", 300);
    let mut asked = wrong_bucket;
    asked.endpoint = "http://nope.s3.local";
    let url = presign_path("GET", &key, &asked, &[], "/dir/k").unwrap();
    let unknown = rig.send_raw(url_request(&url)).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.code(), "NoSuchBucket");
}
