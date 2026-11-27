//! The `object_store` crate's S3 client (`AmazonS3`) against the S3 front over loopback, the way
//! a client that commits with a create-only put and reads the tag back (LanceDB, delta-rs and
//! SlateDB, through `object_store`) uses it: a put with `PutMode::Create`, the `e_tag` of its
//! result, then a write conditioned on that tag (`PutMode::Update`).

use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::client::{ClientOptions, HttpClient, HttpConnector};
use object_store::path::Path;
use object_store::{
    Error, ObjectStore as _, ObjectStoreExt as _, PutMode, PutPayload, RetryConfig, UpdateVersion,
};

use crate::support::{BUCKET, Harness, KEY_ID, SECRET};

/// Connects the client with the tests' HTTP client: plain HTTP to the loopback front, and the
/// TLS configuration of `tierstore-testkit` (ring), so that no process-wide provider is needed.
#[derive(Debug)]
struct Loopback;

impl HttpConnector for Loopback {
    fn connect(&self, _options: &ClientOptions) -> object_store::Result<HttpClient> {
        let client = tierstore_testkit::http::builder()
            .build()
            .expect("a client");
        Ok(HttpClient::new(client))
    }
}

/// The client of the full key of `h`'s server, which sends each request once: a refusal is
/// what the test looks at, and a retry of a 5xx would only repeat it.
fn store(h: &Harness) -> AmazonS3 {
    AmazonS3Builder::new()
        .with_endpoint(format!("http://{}", h.addr))
        .with_allow_http(true)
        .with_virtual_hosted_style_request(false)
        .with_region("us-east-1")
        .with_bucket_name(BUCKET)
        .with_access_key_id(KEY_ID)
        .with_secret_access_key(SECRET)
        .with_http_connector(Loopback)
        .with_retry(RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        })
        .build()
        .expect("a client")
}

/// Asserts that a put with `PutMode::Create` gives an `e_tag`, the one a HEAD and a GET then
/// give, and that a second one of the path is `AlreadyExists`; that an update conditioned on a
/// tag the object does not have is `Precondition`; and that one conditioned on the tag the put
/// gave is refused and leaves the object as it was: the front does not replace an object on
/// `If-Match` yet (komira-ai/tierstore#526), and this test says what it answers until it does.
/// It fails on a front whose answer to a put carries no `ETag`, which is the failure of every
/// client that commits this way.
#[tokio::test]
async fn object_store_reads_the_etag_of_a_create_and_conditions_an_update_on_it() {
    let h = Harness::start(|_| {}).await;
    let s3 = store(&h);
    let path = Path::from("commits/00000000000000000001.json");

    let made = s3
        .put_opts(
            &path,
            PutPayload::from_static(b"first"),
            PutMode::Create.into(),
        )
        .await
        .expect("a put of a new path");
    let etag = made.e_tag.expect("the answer to a put names its ETag");
    assert_eq!(s3.head(&path).await.unwrap().e_tag.as_deref(), Some(&*etag));
    let read = s3.get(&path).await.unwrap();
    assert_eq!(read.meta.e_tag.as_deref(), Some(&*etag));
    assert_eq!(read.bytes().await.unwrap(), "first");

    let again = s3
        .put_opts(
            &path,
            PutPayload::from_static(b"other"),
            PutMode::Create.into(),
        )
        .await;
    assert!(
        matches!(again, Err(Error::AlreadyExists { .. })),
        "{again:?}"
    );

    let stale = UpdateVersion {
        e_tag: Some("\"0123456789abcdef0123456789abcdef\"".to_owned()),
        version: None,
    };
    let refused = s3
        .put_opts(
            &path,
            PutPayload::from_static(b"stale"),
            PutMode::Update(stale).into(),
        )
        .await;
    assert!(
        matches!(refused, Err(Error::Precondition { .. })),
        "{refused:?}"
    );

    let current = UpdateVersion {
        e_tag: Some(etag.clone()),
        version: None,
    };
    let update = s3
        .put_opts(
            &path,
            PutPayload::from_static(b"second"),
            PutMode::Update(current).into(),
        )
        .await;
    assert!(matches!(update, Err(Error::Generic { .. })), "{update:?}");
    let read = s3.get(&path).await.unwrap();
    assert_eq!(read.meta.e_tag.as_deref(), Some(&*etag));
    assert_eq!(read.bytes().await.unwrap(), "first");
}
