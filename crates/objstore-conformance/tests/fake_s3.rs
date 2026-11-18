//! The suite against the loopback fake S3 service through the `S3Store` client: an S3 bucket
//! with conditional writes, which refuses every write to a used key.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use objstore_conformance::{IdenticalPut, LiveSet, run};
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::{Capabilities, KeyPrefix};

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// Asserts that every case passes against the fake S3 service (`412` on any write to a used key,
/// ranges cut at the end, `416` from the end on, HEAD, idempotent DELETE, ListObjectsV2 with
/// continuation tokens), and that the run leaves no object in the bucket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fake_s3_service_passes_every_case() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let root = KeyPrefix::new("conformance/run-1/").unwrap();
    let report = run(&store, &root, IdenticalPut::Refused).await;
    assert!(report.passed(), "{report}");
    assert_eq!(s3.keys(), Vec::<String>::new());
}

/// Asserts that the live keys written through one client are listed exactly through another (a
/// new client is all a restart is to a service that keeps its objects).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_live_keys_are_listed_through_a_new_client() {
    let s3 = FakeS3::start().await;
    let prefix = KeyPrefix::new("conformance/live/").unwrap();
    let set = LiveSet::seed(&s3.store(CONDITIONAL), &prefix)
        .await
        .unwrap();
    set.check(&s3.store(CONDITIONAL)).await.unwrap();
}

/// Asserts that the suite says so when the fake S3 service is run with the wrong declaration of
/// its answer to an identical write (it refuses; `Accepted` was declared).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_declaration_of_the_identical_answer_fails_two_cases() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let root = KeyPrefix::new("conformance/run-2/").unwrap();
    let report = run(&store, &root, IdenticalPut::Accepted).await;
    let failed: Vec<String> = report.violations().map(ToString::to_string).collect();
    assert_eq!(
        failed,
        [
            "identical-put: put_new of conformance/run-2/identical-put/object failed: object \
             conformance/run-2/identical-put/object already exists",
            "create-race: 1 of 8 concurrent writers of conformance/run-2/create-race/same \
             succeeded, expected 8",
        ]
    );
}
