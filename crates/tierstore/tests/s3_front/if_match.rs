//! `If-Match` from a stock S3 SDK (aws-sdk-rust) against the `tierstore` binary started as a
//! build node that writes through its cache to one S3 bucket (the loopback S3 service of the
//! tests): the conditional update of a client nobody here wrote, through the process as it is
//! deployed over a single upstream store.

use aws_sdk_s3::config::{
    Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::{Client, Config};
use objstore_s3::s3::fake::FakeS3;
use rustix::process::Signal;

use crate::binary::start;
use crate::files::{BIN_KEY_ID, BIN_SECRET, Files};

pub(crate) const BUCKET: &str = "tier-bucket";

/// The file of a build node that writes through a cache of `cas/` to the bucket of `endpoint`
/// (also the node of `object_store_client.rs`).
pub(crate) fn node_file(files: &Files, endpoint: &str) -> String {
    let upstream_key = files.secret("upstream.key", "UPKEY\nup-secret\n");
    let text = format!(
        r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "bin-if-match"
bucket          = "{BUCKET}"
access_key_file = "{key}"

[local_cache]
path          = "{cache}"
budget        = "1MiB"
write_through = true

[cold_store]
kind             = "s3"
bucket           = "tierstore-fake"
endpoint         = "{endpoint}"
credentials_file = "{upstream_key}"

[[policy]]
prefix = "cas/"
cache  = true
"#,
        key = files.key.display(),
        cache = files.key.with_file_name("cache").display(),
        upstream_key = upstream_key.display(),
    );
    files.secret("node.toml", &text).display().to_string()
}

/// A stock SDK client of the front at `addr`, with the node's key and no flexible checksums.
fn client(addr: &str) -> Client {
    let config = Config::builder()
        .behavior_version_latest()
        .http_client(aws_smithy_http_client::Builder::new().build_http())
        .endpoint_url(format!("http://{addr}"))
        .force_path_style(true)
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            BIN_KEY_ID, BIN_SECRET, None, None, "static",
        ))
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .build();
    Client::from_conf(config)
}

/// A put of `body` with `If-Match: <etag>`: the ETag of the new object, or the status and the
/// S3 code of the refusal.
async fn swap(client: &Client, key: &str, etag: &str, body: &str) -> Result<String, (u16, String)> {
    let sent = client
        .put_object()
        .bucket(BUCKET)
        .key(key)
        .if_match(etag)
        .body(ByteStream::from(body.as_bytes().to_vec()))
        .send()
        .await;
    match sent {
        Ok(out) => Ok(out.e_tag().expect("the ETag of the new object").to_owned()),
        Err(error) => {
            let status = error.raw_response().expect("an answer").status().as_u16();
            Err((status, error.code().unwrap_or_default().to_owned()))
        }
    }
}

async fn etag(client: &Client, key: &str) -> String {
    let head = client.head_object().bucket(BUCKET).key(key).send().await;
    head.unwrap().e_tag().unwrap().to_owned()
}

async fn read(client: &Client, key: &str) -> String {
    let out = client.get_object().bucket(BUCKET).key(key).send().await;
    let bytes = out.unwrap().body.collect().await.unwrap().into_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Asserts the conditional update of a stock SDK through a build node over one S3 bucket: a put
/// with a stale ETag is 412 and leaves the bytes, also after the node read them into its cache;
/// one with the current ETag replaces the object and answers the new ETag, which a HEAD then
/// gives, and a GET the new bytes; the ETag that was current is stale after; a key with no object
/// is 404 `NoSuchKey`; of two puts that race with one ETag exactly one wins; and the completion of
/// a multipart upload with `If-Match` of the current ETag replaces the object. The node exits 0 on
/// SIGTERM.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_node_over_one_bucket_replaces_on_if_match_from_a_stock_sdk() {
    let fake = FakeS3::start().await;
    fake.require_credentials("UPKEY", "up-secret");
    let files = Files::new("bin-if-match");
    let config = node_file(&files, fake.endpoint());
    let flags = ["--config", config.as_str(), "--drain-lb-wait", "50ms"].map(str::to_owned);
    let mut running = start(&flags);
    let client = client(running.addr());
    let key = "cas/obj";

    let first = client.put_object().bucket(BUCKET).key(key);
    first
        .body(ByteStream::from_static(b"old"))
        .send()
        .await
        .unwrap();
    let old = etag(&client, key).await;
    assert_eq!(read(&client, key).await, "old");
    let failed = Err((412, "PreconditionFailed".to_owned()));
    let stale = "\"0123456789abcdef0123456789abcdef\"";
    assert_eq!(swap(&client, key, stale, "x").await, failed);
    assert_eq!(read(&client, key).await, "old");

    let new = swap(&client, key, &old, "new").await.unwrap();
    assert_ne!(new, old);
    assert_eq!(etag(&client, key).await, new);
    assert_eq!(read(&client, key).await, "new");
    assert_eq!(swap(&client, key, &old, "late").await, failed);
    let missing = swap(&client, "cas/none", &old, "x").await;
    assert_eq!(missing, Err((404, "NoSuchKey".to_owned())));

    let (a, b) = tokio::join!(
        swap(&client, key, &new, "alpha"),
        swap(&client, key, &new, "bravo")
    );
    let winner = match (a.is_ok(), b.is_ok()) {
        (true, false) => (a.unwrap(), b, "alpha"),
        (false, true) => (b.unwrap(), a, "bravo"),
        other => panic!("not exactly one winner: {other:?}"),
    };
    assert_eq!(winner.1, failed);
    assert_eq!(read(&client, key).await, winner.2);
    assert_eq!(etag(&client, key).await, winner.0);

    let started = client.create_multipart_upload().bucket(BUCKET).key(key);
    let id = started
        .send()
        .await
        .unwrap()
        .upload_id()
        .unwrap()
        .to_owned();
    let sent = client.upload_part().bucket(BUCKET).key(key).upload_id(&id);
    let part = sent.part_number(1).body(ByteStream::from_static(b"parts"));
    let part = part.send().await.unwrap().e_tag().unwrap().to_owned();
    let parts = CompletedMultipartUpload::builder()
        .parts(CompletedPart::builder().part_number(1).e_tag(part).build())
        .build();
    let completing = client.complete_multipart_upload().bucket(BUCKET).key(key);
    let completing = completing.upload_id(&id).multipart_upload(parts);
    let done = completing.if_match(&winner.0).send().await.unwrap();
    let made = done.e_tag().unwrap().to_owned();
    assert_eq!(etag(&client, key).await, made);
    assert_eq!(read(&client, key).await, "parts");

    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}
