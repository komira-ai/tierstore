//! `CopyObject` and `UploadPartCopy` through the S3 front (`docs/design/overwrite.md`, section
//! 5.5): a stock SDK copies an object and the parts of an upload, the source conditions of S3 are
//! answered as S3 answers them, and a copy replaces or creates its destination as a put of the
//! store's format does.

use std::fmt::Debug;

use aws_sdk_s3::config::http::HttpResponse;
use aws_sdk_s3::config::{
    Builder, Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::{ByteStream, DateTime};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::{Client, Config};
use tierstore::Backend;

use crate::raw::call;
use crate::support::{BUCKET, Harness, KEY_ID, SECRET};

/// An SDK client with the key of the server, configured as `sdk.rs` configures it.
fn client(h: &Harness) -> Client {
    let settings: Builder = Config::builder()
        .behavior_version_latest()
        .http_client(aws_smithy_http_client::Builder::new().build_http())
        .endpoint_url(format!("http://{}", h.addr))
        .force_path_style(true)
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(KEY_ID, SECRET, None, None, "static"))
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
    Client::from_conf(settings.build())
}

/// The HTTP status and the S3 error code of a call the server refused.
fn refusal<T: Debug, E: ProvideErrorMetadata + Debug>(
    result: Result<T, SdkError<E, HttpResponse>>,
) -> (u16, String) {
    let error = result.unwrap_err();
    let status = error.raw_response().expect("an answer").status().as_u16();
    (status, error.code().unwrap_or_default().to_owned())
}

/// Puts `body` under `name`, create-only.
async fn put(sdk: &Client, name: &str, body: &[u8]) {
    sdk.put_object()
        .bucket(BUCKET)
        .key(name)
        .if_none_match("*")
        .body(ByteStream::from(body.to_vec()))
        .send()
        .await
        .unwrap();
}

/// The whole body of `name`.
async fn get(sdk: &Client, name: &str) -> Vec<u8> {
    let out = sdk.get_object().bucket(BUCKET).key(name).send().await;
    out.unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes()
        .to_vec()
}

/// The entity tag of `name`, as a head gives it.
async fn tag_of(sdk: &Client, name: &str) -> String {
    let head = sdk.head_object().bucket(BUCKET).key(name).send();
    head.await.unwrap().e_tag().expect("a tag").to_owned()
}

/// The source of a copy: `tier-bucket/<name>`.
fn source(name: &str) -> String {
    format!("{BUCKET}/{name}")
}

/// Asserts that `copy_object` of a small object answers the entity tag the source has, and that
/// the destination then reads back with the bytes of the source while the source is unchanged.
#[tokio::test]
async fn copy_object_makes_an_object_with_the_source_tag_and_bytes() {
    let h = Harness::start(|_| {}).await;
    let sdk = client(&h);
    put(&sdk, "src/a", b"hello copy").await;
    let copied = sdk
        .copy_object()
        .bucket(BUCKET)
        .key("dst/a")
        .copy_source(source("src/a"))
        .send()
        .await
        .unwrap();
    let result = copied.copy_object_result().expect("a copy result");
    let tag = tag_of(&sdk, "src/a").await;
    assert_eq!(result.e_tag(), Some(tag.as_str()));
    assert_eq!(tag_of(&sdk, "dst/a").await, tag);
    assert_eq!(get(&sdk, "dst/a").await, b"hello copy");
    assert_eq!(get(&sdk, "src/a").await, b"hello copy");
}

/// Asserts that a copy whose source does not exist is 404 `NoSuchKey` and writes nothing.
#[tokio::test]
async fn copy_object_of_a_missing_source_is_no_such_key() {
    let h = Harness::start(|_| {}).await;
    let sdk = client(&h);
    let sent = sdk
        .copy_object()
        .bucket(BUCKET)
        .key("dst/a")
        .copy_source(source("src/absent"))
        .send();
    assert_eq!(refusal(sent.await), (404, "NoSuchKey".to_owned()));
    assert!(h.hot_mem.get("dst/a", None).await.is_err());
}

/// Asserts that a copy of an object onto itself is 400 `InvalidRequest` and leaves the object as
/// it was.
#[tokio::test]
async fn copy_object_onto_itself_is_invalid_request() {
    let h = Harness::start(|_| {}).await;
    let sdk = client(&h);
    put(&sdk, "src/a", b"same").await;
    let sent = sdk
        .copy_object()
        .bucket(BUCKET)
        .key("src/a")
        .copy_source(source("src/a"))
        .send();
    assert_eq!(refusal(sent.await), (400, "InvalidRequest".to_owned()));
    assert_eq!(get(&sdk, "src/a").await, b"same");
}

/// Asserts that a copy whose `x-amz-copy-source-if-match` names another tag is 412
/// `PreconditionFailed` and writes nothing, and that one that names the source's tag is made.
#[tokio::test]
async fn copy_source_if_match_holds_only_for_the_tag_of_the_source() {
    let h = Harness::start(|_| {}).await;
    let sdk = client(&h);
    put(&sdk, "src/a", b"tagged").await;
    let wrong = sdk
        .copy_object()
        .bucket(BUCKET)
        .key("dst/a")
        .copy_source(source("src/a"))
        .copy_source_if_match("\"00000000000000000000000000000000\"")
        .send();
    assert_eq!(refusal(wrong.await), (412, "PreconditionFailed".to_owned()));
    assert!(h.hot_mem.get("dst/a", None).await.is_err());
    let tag = tag_of(&sdk, "src/a").await;
    sdk.copy_object()
        .bucket(BUCKET)
        .key("dst/a")
        .copy_source(source("src/a"))
        .copy_source_if_match(tag)
        .send()
        .await
        .unwrap();
    assert_eq!(get(&sdk, "dst/a").await, b"tagged");
}

/// Asserts that a copy whose `x-amz-copy-source-if-none-match` names the source's tag is 412 and
/// writes nothing, and that one that names another tag is made.
#[tokio::test]
async fn copy_source_if_none_match_fails_for_the_tag_of_the_source() {
    let h = Harness::start(|_| {}).await;
    let sdk = client(&h);
    put(&sdk, "src/a", b"tagged").await;
    let tag = tag_of(&sdk, "src/a").await;
    let same = sdk
        .copy_object()
        .bucket(BUCKET)
        .key("dst/a")
        .copy_source(source("src/a"))
        .copy_source_if_none_match(tag)
        .send();
    assert_eq!(refusal(same.await), (412, "PreconditionFailed".to_owned()));
    assert!(h.hot_mem.get("dst/a", None).await.is_err());
    sdk.copy_object()
        .bucket(BUCKET)
        .key("dst/a")
        .copy_source(source("src/a"))
        .copy_source_if_none_match("\"00000000000000000000000000000000\"")
        .send()
        .await
        .unwrap();
    assert_eq!(get(&sdk, "dst/a").await, b"tagged");
}

/// The time of last write of `name`, from a head.
async fn modified_of(sdk: &Client, name: &str) -> DateTime {
    let head = sdk.head_object().bucket(BUCKET).key(name).send();
    head.await
        .unwrap()
        .last_modified()
        .copied()
        .expect("a time")
}

/// Asserts that `x-amz-copy-source-if-modified-since` passes for a date before the source's time
/// of last write and is 412 for a date after it, and that the failed copy writes nothing.
#[tokio::test]
async fn copy_source_if_modified_since_compares_with_the_time_of_last_write() {
    let h = Harness::start(|_| {}).await;
    let sdk = client(&h);
    put(&sdk, "src/a", b"timed").await;
    let at = modified_of(&sdk, "src/a").await.secs();
    let after = sdk
        .copy_object()
        .bucket(BUCKET)
        .key("dst/late")
        .copy_source(source("src/a"))
        .copy_source_if_modified_since(DateTime::from_secs(at + 3600))
        .send();
    assert_eq!(refusal(after.await), (412, "PreconditionFailed".to_owned()));
    assert!(h.hot_mem.get("dst/late", None).await.is_err());
    sdk.copy_object()
        .bucket(BUCKET)
        .key("dst/early")
        .copy_source(source("src/a"))
        .copy_source_if_modified_since(DateTime::from_secs(at - 3600))
        .send()
        .await
        .unwrap();
    assert_eq!(get(&sdk, "dst/early").await, b"timed");
}

/// Asserts that `x-amz-copy-source-if-unmodified-since` passes for a date after the source's time
/// of last write and is 412 for a date before it, and that the failed copy writes nothing.
#[tokio::test]
async fn copy_source_if_unmodified_since_compares_with_the_time_of_last_write() {
    let h = Harness::start(|_| {}).await;
    let sdk = client(&h);
    put(&sdk, "src/a", b"timed").await;
    let at = modified_of(&sdk, "src/a").await.secs();
    let before = sdk
        .copy_object()
        .bucket(BUCKET)
        .key("dst/early")
        .copy_source(source("src/a"))
        .copy_source_if_unmodified_since(DateTime::from_secs(at - 3600))
        .send();
    assert_eq!(
        refusal(before.await),
        (412, "PreconditionFailed".to_owned())
    );
    assert!(h.hot_mem.get("dst/early", None).await.is_err());
    sdk.copy_object()
        .bucket(BUCKET)
        .key("dst/late")
        .copy_source(source("src/a"))
        .copy_source_if_unmodified_since(DateTime::from_secs(at + 3600))
        .send()
        .await
        .unwrap();
    assert_eq!(get(&sdk, "dst/late").await, b"timed");
}

/// Asserts that an upload whose parts are copied from ranges of one large object (two of the
/// minimum part size and a short last one) completes with the `CopyPartResult` tags, and that the
/// object it makes holds the bytes of the source.
#[tokio::test]
async fn upload_part_copy_by_ranges_makes_an_object_equal_to_the_source() {
    let h = Harness::start(|_| {}).await;
    let sdk = client(&h);
    let part = 5 * 1024 * 1024;
    let whole: Vec<u8> = (0..2 * part + 1000)
        .map(|at| u8::try_from(at % 251).unwrap())
        .collect();
    put(&sdk, "src/big", &whole).await;
    let created = sdk
        .create_multipart_upload()
        .bucket(BUCKET)
        .key("dst/big")
        .send()
        .await
        .unwrap();
    let id = created.upload_id().expect("an upload id").to_owned();
    let last = whole.len() - 1;
    let ranges = [
        (1, 0, part - 1),
        (2, part, 2 * part - 1),
        (3, 2 * part, last),
    ];
    let mut parts = CompletedMultipartUpload::builder();
    for (number, first, end) in ranges {
        let copied = sdk
            .upload_part_copy()
            .bucket(BUCKET)
            .key("dst/big")
            .upload_id(&id)
            .part_number(number)
            .copy_source(source("src/big"))
            .copy_source_range(format!("bytes={first}-{end}"))
            .send()
            .await
            .unwrap();
        let tag = copied
            .copy_part_result()
            .and_then(|result| result.e_tag())
            .expect("a tag for the copied part");
        parts = parts.parts(
            CompletedPart::builder()
                .part_number(number)
                .e_tag(tag)
                .build(),
        );
    }
    sdk.complete_multipart_upload()
        .bucket(BUCKET)
        .key("dst/big")
        .upload_id(&id)
        .multipart_upload(parts.build())
        .send()
        .await
        .unwrap();
    let head = sdk.head_object().bucket(BUCKET).key("dst/big").send();
    assert_eq!(
        head.await.unwrap().content_length(),
        Some(i64::try_from(whole.len()).unwrap())
    );
    assert!(get(&sdk, "dst/big").await == whole);
}

/// Asserts that a part copied from a range that ends past the source is 400 `InvalidArgument`.
#[tokio::test]
async fn upload_part_copy_of_a_range_past_the_source_is_invalid_argument() {
    let h = Harness::start(|_| {}).await;
    let sdk = client(&h);
    put(&sdk, "src/a", b"short").await;
    let created = sdk
        .create_multipart_upload()
        .bucket(BUCKET)
        .key("dst/a")
        .send()
        .await
        .unwrap();
    let id = created.upload_id().expect("an upload id").to_owned();
    let sent = sdk
        .upload_part_copy()
        .bucket(BUCKET)
        .key("dst/a")
        .upload_id(id)
        .part_number(1)
        .copy_source(source("src/a"))
        .copy_source_range("bytes=0-99")
        .send();
    assert_eq!(refusal(sent.await), (400, "InvalidArgument".to_owned()));
}

/// Asserts that, over a store that runs on markers, where a plain put replaces, a copy onto a key
/// that exists is 200 and the key then holds the bytes of the source.
#[tokio::test]
async fn copy_onto_existing_key_replaces_it_under_marks() {
    let h = Harness::start_marks(|_| {}, |_| {}).await;
    assert_eq!(
        call(&h, "PUT", "a", &[], &[], b"from a").await.status(),
        200
    );
    assert_eq!(
        call(&h, "PUT", "b", &[], &[], b"from b").await.status(),
        200
    );
    let copy = [("x-amz-copy-source", "/tier-bucket/a")];
    let copied = call(&h, "PUT", "b", &[], &copy, b"").await;
    assert_eq!(copied.status(), 200);
    let body = copied.text().await.unwrap();
    assert!(body.contains("<CopyObjectResult"), "{body}");
    let read = call(&h, "GET", "b", &[], &[], b"").await;
    assert_eq!(read.text().await.unwrap(), "from a");
}

/// Asserts that, over a store that runs on entries, a copy onto a key that exists is 412 with the
/// key as it was, and that a copy onto a new key is 200.
#[tokio::test]
async fn copy_onto_existing_key_is_412_under_entries() {
    let h = Harness::start_fixed(|_| {}).await;
    let create = [("if-none-match", "*")];
    call(&h, "PUT", "a", &[], &create, b"from a").await;
    call(&h, "PUT", "b", &[], &create, b"from b").await;
    let copy = [("x-amz-copy-source", "/tier-bucket/a")];
    let refused = call(&h, "PUT", "b", &[], &copy, b"").await;
    assert_eq!(refused.status(), 412);
    let body = refused.text().await.unwrap();
    assert!(body.contains("<Code>PreconditionFailed</Code>"), "{body}");
    assert_eq!(h.hot_mem.get("b", None).await.unwrap(), "from b");
    let fresh = call(&h, "PUT", "c", &[], &copy, b"").await;
    assert_eq!(fresh.status(), 200);
    assert_eq!(h.hot_mem.get("c", None).await.unwrap(), "from a");
}
