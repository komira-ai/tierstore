//! The `object_store` crate's S3 client (`AmazonS3`) against the S3 front over loopback, the way
//! a client that commits with a create-only put and reads the tag back (LanceDB, delta-rs and
//! SlateDB, through `object_store`) uses it: a put with `PutMode::Create`, the `e_tag` of its
//! result, then a write conditioned on that tag (`PutMode::Update`). Against a proxy of two tiers,
//! which does not replace on a tag, and against the `tierstore` binary as a build node that writes
//! through its cache to one S3 bucket (the loopback S3 service), which does.

use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::client::{ClientOptions, HttpClient, HttpConnector};
use object_store::path::Path;
use object_store::{
    Error, ObjectStore as _, ObjectStoreExt as _, PutMode, PutPayload, RetryConfig, UpdateVersion,
};

use objstore_s3::s3::fake::FakeS3;
use rustix::process::Signal;

use crate::binary::start;
use crate::files::{BIN_KEY_ID, BIN_SECRET, Files};
use crate::if_match::{BUCKET as NODE_BUCKET, node_file};
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

/// The client of the front at `addr` for `bucket`, with the key `(id, secret)`, which sends each
/// request once: a refusal is what the test looks at, and a retry of a 5xx would only repeat it.
fn store_at(addr: &str, bucket: &str, (id, secret): (&str, &str)) -> AmazonS3 {
    AmazonS3Builder::new()
        .with_endpoint(format!("http://{addr}"))
        .with_allow_http(true)
        .with_virtual_hosted_style_request(false)
        .with_region("us-east-1")
        .with_bucket_name(bucket)
        .with_access_key_id(id)
        .with_secret_access_key(secret)
        .with_http_connector(Loopback)
        .with_retry(RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        })
        .build()
        .expect("a client")
}

/// The client of the full key of `h`'s server.
fn store(h: &Harness) -> AmazonS3 {
    store_at(&h.addr.to_string(), BUCKET, (KEY_ID, SECRET))
}

/// An ETag no object has.
const STALE: &str = "\"0123456789abcdef0123456789abcdef\"";

/// The options of an update conditioned on the ETag `etag` (`If-Match`).
fn update(etag: &str) -> object_store::PutOptions {
    PutMode::Update(UpdateVersion {
        e_tag: Some(etag.to_owned()),
        version: None,
    })
    .into()
}

/// Asserts that a put with `PutMode::Create` gives an `e_tag`, the one a HEAD and a GET then
/// give, and that a second one of the path is `AlreadyExists`; that an update conditioned on a
/// tag the object does not have is `Precondition`; and that one conditioned on the tag the put
/// gave is refused (501, which the client names `Generic`) and leaves the object as it was: a
/// proxy of two tiers does not replace on a tag, since its tag moves when the object moves
/// between its tiers. It fails on a front whose answer to a put carries no `ETag`, which is the
/// failure of every client that commits this way.
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

    let stale = update(STALE);
    let refused = s3
        .put_opts(&path, PutPayload::from_static(b"stale"), stale)
        .await;
    assert!(
        matches!(refused, Err(Error::Precondition { .. })),
        "{refused:?}"
    );

    let current = update(&etag);
    let refused = s3
        .put_opts(&path, PutPayload::from_static(b"second"), current)
        .await;
    assert!(matches!(refused, Err(Error::Generic { .. })), "{refused:?}");
    let read = s3.get(&path).await.unwrap();
    assert_eq!(read.meta.e_tag.as_deref(), Some(&*etag));
    assert_eq!(read.bytes().await.unwrap(), "first");
}

/// Asserts the round trip of a client that commits with `object_store`, through the `tierstore`
/// binary as a build node that writes through its cache to one S3 bucket: a put with
/// `PutMode::Create` gives an `e_tag`, the one a HEAD gives; an update conditioned on it replaces
/// the object and gives the new `e_tag`, which differs and which a HEAD and a GET then give with
/// the new bytes; an update conditioned on a stale tag, and on the tag that was current before,
/// is `Precondition` and leaves the object. The node exits 0 on SIGTERM.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn object_store_replaces_on_the_etag_of_its_create_through_a_node_over_one_bucket() {
    let fake = FakeS3::start().await;
    fake.require_credentials("UPKEY", "up-secret");
    let files = Files::new("bin-object-store");
    let config = node_file(&files, fake.endpoint());
    let flags = ["--config", config.as_str(), "--drain-lb-wait", "50ms"].map(str::to_owned);
    let mut running = start(&flags);
    let s3 = store_at(running.addr(), NODE_BUCKET, (BIN_KEY_ID, BIN_SECRET));
    let path = Path::from("cas/commits/00000000000000000001.json");

    let create: object_store::PutOptions = PutMode::Create.into();
    let made = s3.put_opts(&path, PutPayload::from_static(b"first"), create);
    let first = made.await.expect("a put of a new path").e_tag;
    let first = first.expect("the answer to a put names its ETag");
    assert_eq!(
        s3.head(&path).await.unwrap().e_tag.as_deref(),
        Some(&*first)
    );

    let stale = s3.put_opts(&path, PutPayload::from_static(b"stale"), update(STALE));
    let stale = stale.await;
    assert!(
        matches!(stale, Err(Error::Precondition { .. })),
        "{stale:?}"
    );

    let swapped = s3.put_opts(&path, PutPayload::from_static(b"second"), update(&first));
    let second = swapped.await.expect("an update on the current ETag").e_tag;
    let second = second.expect("the answer to an update names its ETag");
    assert_ne!(second, first);
    assert_eq!(
        s3.head(&path).await.unwrap().e_tag.as_deref(),
        Some(&*second)
    );
    let read = s3.get(&path).await.unwrap();
    assert_eq!(read.meta.e_tag.as_deref(), Some(&*second));
    assert_eq!(read.bytes().await.unwrap(), "second");

    let old = s3.put_opts(&path, PutPayload::from_static(b"third"), update(&first));
    let old = old.await;
    assert!(matches!(old, Err(Error::Precondition { .. })), "{old:?}");
    assert_eq!(
        s3.get(&path).await.unwrap().bytes().await.unwrap(),
        "second"
    );

    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}
