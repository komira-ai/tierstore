//! The create-only put that names the SHA-256 of its body, against a real S3 store: a MinIO
//! server the test starts on a loopback address of its own, with its data in a scratch directory.
//! These tests are what shows which checks the store makes, so they are not about the loopback
//! service of the other tests, which only does what it was written to do.
//!
//! What they find against MinIO:
//!
//! - A single PUT whose bytes are not its `x-amz-checksum-sha256` is refused and stores nothing;
//!   the store keeps the checksum of a single PUT (`FULL_OBJECT`) and a HEAD gives it back.
//! - A multipart upload is checked part by part only: the checksum kept for it is of the parts
//!   (`COMPOSITE`), and a whole-object digest that is not its bytes' is stored as metadata
//!   without a word. So the backend checks the body itself, and records the digest as metadata,
//!   which is what a put sent again is compared with for such an object.
//! - A body cut in the middle leaves no object (and no open upload), and the put sent again makes
//!   it; a third is answered by the HEAD as the same object; other bytes are refused.
//!
//! They need the `minio` server binary on the `PATH`, which the per-PR checks do not have, so
//! they are ignored there; run them with `cargo test -p tierstore --features s3 --test
//! upstream_put -- --ignored`.

#![cfg(feature = "s3")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt as _, stream};
use objstore_s3::s3::{S3Store, StoredSha256};
use objstore_s3::{KeyPrefix, ObjectKey, ObjectStoreError, PageSize};
use tierstore::s3::{Capabilities, Credentials, S3Config, S3Tuning};
use tierstore::{Backend, Error, PartBody, PutOutcome, S3Backend, TreeDigest};
use tierstore_testkit::dir::TestDir;
use tierstore_testkit::port::own_loopback_port;
use tierstore_testkit::reaped::{Reaped, spawn};

/// The reason the tests are not run by default.
const NEEDS_MINIO: &str = "needs the `minio` server binary on the PATH";

/// The test's own key pair for the server it starts.
const USER: &str = "tierstore-test";
const SECRET: &str = "tierstore-test-secret";

/// How long the server is given to answer once started.
const START: Duration = Duration::from_secs(60);

/// A MinIO server the test started, its scratch directory, and a bucket on it.
struct Minio {
    _process: Reaped,
    _dir: TestDir,
    endpoint: String,
}

impl Minio {
    /// Starts a server on a loopback address of this test's own and makes the bucket `tierstore`.
    async fn start(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let (address, console) = (own_loopback_port(), own_loopback_port());
        let mut command = Command::new("minio");
        command
            .args(["server", "--quiet", "--address", &address.to_string()])
            .args(["--console-address", &console.to_string()])
            .arg(dir.path().join("data"))
            .env("MINIO_ROOT_USER", USER)
            .env("MINIO_ROOT_PASSWORD", SECRET)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let process = spawn(&mut command, 3).expect(NEEDS_MINIO);
        let minio = Self {
            _process: process,
            _dir: dir,
            endpoint: format!("http://{address}"),
        };
        let store = minio.store(S3Tuning::default());
        store.wait_answering(START).await.unwrap();
        store.create_bucket().await.unwrap();
        minio
    }

    fn config(&self) -> S3Config {
        S3Config {
            endpoint: self.endpoint.clone(),
            region: "us-east-1".to_owned(),
            bucket: "tierstore".to_owned(),
            credentials: Credentials::new(USER, SECRET),
            capabilities: Capabilities {
                conditional_put: true,
                object_lock: false,
            },
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            min_body_rate: None,
        }
    }

    /// The client of the store, for what the backend does not ask.
    fn store(&self, tuning: S3Tuning) -> S3Store {
        S3Store::new_tuned(self.config(), tuning).unwrap()
    }

    /// The backend under test.
    fn backend(&self, tuning: S3Tuning) -> S3Backend {
        S3Backend::new_tuned(self.config(), tuning).unwrap()
    }
}

/// A tuning that sends a body over 1 MiB as a multipart upload of 5 MiB parts.
fn parts() -> S3Tuning {
    S3Tuning {
        single_put_max: 1 << 20,
        part_bytes: 5 << 20,
        ..S3Tuning::default()
    }
}

/// `len` bytes that are not all zeros, different for each `seed`.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i % 251) as u8 ^ seed)
        .collect::<Vec<u8>>()
}

fn sha(bytes: &[u8]) -> TreeDigest {
    TreeDigest::of(bytes)
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

/// `bytes` in chunks of 64 KiB.
fn body(bytes: &[u8]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = bytes
        .chunks(64 << 10)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    stream::iter(items).boxed()
}

/// The first `upto` bytes of `bytes` in chunks of 64 KiB, and then the error of a writer that
/// went away: the request is cut in the middle of its body.
fn cut(bytes: &[u8], upto: usize) -> PartBody<'static> {
    let gone = Error::Unavailable("the writer went away".to_owned());
    let mut items: Vec<Result<Bytes, Error>> = bytes[..upto]
        .chunks(64 << 10)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    items.push(Err(gone));
    stream::iter(items).boxed()
}

/// The multipart uploads the store holds open.
async fn open_uploads(store: &S3Store) -> usize {
    let prefix = KeyPrefix::new("").unwrap();
    let page = PageSize::new(1000).unwrap();
    let listed = store.list_multipart_uploads(&prefix, None, page).await;
    listed.unwrap().uploads.len()
}

/// Asserts that the store checks the SHA-256 of a single PUT: bytes that are not the checksum
/// sent are refused (400, `BadDigest` or MinIO's `XAmzContentChecksumMismatch`) and leave nothing,
/// and the right ones are stored with the checksum kept as the whole object's and the digest
/// recorded, both of which a HEAD gives back.
#[tokio::test]
#[ignore = "needs the `minio` server binary on the PATH; run with --ignored"]
async fn the_store_checks_the_checksum_of_a_single_put_and_keeps_it() {
    let minio = Minio::start("ts-minio-single").await;
    let store = minio.store(S3Tuning::default());
    let bytes = pattern(100_000, 1);
    let len = bytes.len() as u64;
    let wrong = sha(b"other bytes");
    let refused = store
        .put_stream_new_sha256(&key("single"), len, body_stream(&bytes), wrong.as_bytes())
        .await
        .unwrap_err();
    let ObjectStoreError::Service { status, code, .. } = &refused else {
        panic!("a refusal of the store, not {refused:?}");
    };
    assert_eq!(*status, 400, "{refused}");
    assert!(
        ["BadDigest", "XAmzContentChecksumMismatch"].contains(&code.as_str()),
        "{refused}"
    );
    let missing = store.head_sha256(&key("single")).await.unwrap_err();
    assert!(
        matches!(missing, ObjectStoreError::NotFound(_)),
        "{missing}"
    );
    let digest = sha(&bytes);
    store
        .put_stream_new_sha256(&key("single"), len, body_stream(&bytes), digest.as_bytes())
        .await
        .unwrap();
    let stored = store.head_sha256(&key("single")).await.unwrap();
    let want = StoredSha256 {
        size: len,
        checksum: Some(*digest.as_bytes()),
        recorded: Some(*digest.as_bytes()),
    };
    assert_eq!(stored, want);
}

/// Asserts that the store checks a multipart upload part by part only: an upload whose parts are
/// right and whose whole-object digest is not its bytes' is completed, its kept checksum is not a
/// whole-object one, and the wrong digest is what its metadata records. This is why the backend
/// checks the body itself before the completion.
#[tokio::test]
#[ignore = "needs the `minio` server binary on the PATH; run with --ignored"]
async fn the_store_checks_only_the_parts_of_a_multipart_upload() {
    let minio = Minio::start("ts-minio-parts").await;
    let store = minio.store(parts());
    let bytes = pattern(6 << 20, 2);
    let len = bytes.len() as u64;
    let wrong = sha(b"not these bytes");
    store
        .put_stream_new_sha256(&key("parts"), len, body_stream(&bytes), wrong.as_bytes())
        .await
        .unwrap();
    let stored = store.head_sha256(&key("parts")).await.unwrap();
    let want = StoredSha256 {
        size: len,
        checksum: None,
        recorded: Some(*wrong.as_bytes()),
    };
    assert_eq!(stored, want);
}

/// Asserts the retry of a commit over a single PUT: a body cut in the middle leaves no object,
/// the put sent again makes it (`Created`), a third finds it (`Existed`, by the checksum the HEAD
/// reads back), other bytes under the key are `AlreadyExists`, and a body that is not its digest
/// is `BadDigest` and leaves nothing.
#[tokio::test]
#[ignore = "needs the `minio` server binary on the PATH; run with --ignored"]
async fn a_cut_put_leaves_nothing_and_the_put_sent_again_makes_the_object() {
    let minio = Minio::start("ts-minio-cut").await;
    let backend = minio.backend(S3Tuning::default());
    let store = minio.store(S3Tuning::default());
    let bytes = pattern(3 << 20, 3);
    let (len, digest) = (bytes.len() as u64, sha(&bytes));
    let k = "cas/b/1/n/pack";
    let failed = backend.put_stream_sha256(k, len, cut(&bytes, 1 << 20), digest);
    assert!(matches!(failed.await, Err(Error::Unavailable(_))));
    let missing = backend.head(k).await.unwrap_err();
    assert_eq!(missing, Error::NotFound(k.to_owned()));
    let made = backend.put_stream_sha256(k, len, body(&bytes), digest);
    assert_eq!(made.await.unwrap(), PutOutcome::Created);
    let again = backend.put_stream_sha256(k, len, body(&bytes), digest);
    assert_eq!(again.await.unwrap(), PutOutcome::Existed);
    let other = pattern(3 << 20, 4);
    let differs = backend.put_stream_sha256(k, len, body(&other), sha(&other));
    assert_eq!(
        differs.await.unwrap_err(),
        Error::AlreadyExists(k.to_owned())
    );
    assert_eq!(backend.get(k, None).await.unwrap(), bytes);
    let bad = backend.put_stream_sha256("cas/b/1/n/L0", len, body(&bytes), sha(b"x"));
    assert_eq!(
        bad.await.unwrap_err(),
        Error::BadDigest("cas/b/1/n/L0".to_owned())
    );
    assert_eq!(open_uploads(&store).await, 0);
    let absent = backend.head("cas/b/1/n/L0").await.unwrap_err();
    assert!(matches!(absent, Error::NotFound(_)), "{absent}");
}

/// Asserts the same over a multipart upload: a body cut after its first part leaves no object and
/// no open upload, the put sent again makes it, a third finds it by the digest recorded in its
/// metadata (the store keeps no whole-object checksum for it), and other bytes of the same length
/// are `AlreadyExists`.
#[tokio::test]
#[ignore = "needs the `minio` server binary on the PATH; run with --ignored"]
async fn a_cut_multipart_put_leaves_nothing_and_the_put_sent_again_makes_the_object() {
    let minio = Minio::start("ts-minio-cut-parts").await;
    let backend = minio.backend(parts());
    let store = minio.store(parts());
    let bytes = pattern(11 << 20, 5);
    let (len, digest) = (bytes.len() as u64, sha(&bytes));
    let k = "cas/b/1/n/L1";
    let failed = backend.put_stream_sha256(k, len, cut(&bytes, 7 << 20), digest);
    assert!(matches!(failed.await, Err(Error::Unavailable(_))));
    assert_eq!(open_uploads(&store).await, 0, "the upload was aborted");
    let missing = backend.head(k).await.unwrap_err();
    assert_eq!(missing, Error::NotFound(k.to_owned()));
    let made = backend.put_stream_sha256(k, len, body(&bytes), digest);
    assert_eq!(made.await.unwrap(), PutOutcome::Created);
    let stored = store.head_sha256(&key(k)).await.unwrap();
    assert_eq!(
        (stored.checksum, stored.recorded),
        (None, Some(*digest.as_bytes()))
    );
    let again = backend.put_stream_sha256(k, len, body(&bytes), digest);
    assert_eq!(again.await.unwrap(), PutOutcome::Existed);
    let mut other = bytes.clone();
    other[len as usize - 1] ^= 1;
    let differs = backend.put_stream_sha256(k, len, body(&other), sha(&other));
    assert_eq!(
        differs.await.unwrap_err(),
        Error::AlreadyExists(k.to_owned())
    );
    assert_eq!(open_uploads(&store).await, 0);
}

/// `bytes` as a body of the client of the store.
fn body_stream(bytes: &[u8]) -> objstore_s3::BodyStream {
    let items: Vec<Result<Bytes, ObjectStoreError>> = bytes
        .chunks(64 << 10)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    stream::iter(items).boxed()
}
