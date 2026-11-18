//! The `ObjectStore` conformance suite (`objstore-conformance`) against the S3 front, through the
//! `S3Store` client the CAS service uses (docs/design/fetch-cache-serve.md 10.9, the "S3 subset"
//! row of section 14): the tierstore target of the suite. The entry reconciler runs beside the front,
//! as it does on a node: a DELETE hands the removal of its entry to it, and a second DELETE or a
//! PUT of the key waits until it has.

use objstore_conformance::{IdenticalPut, LiveSet, run};
use objstore_s3::KeyPrefix;

use crate::client::CONDITIONAL;
use crate::support::Harness;

/// Asserts that the S3 front passes every case of the suite: create-only (`412` on different
/// bytes, whole or streamed, and on the same bytes: this front refuses every write to a used
/// key), one winner among concurrent writers, streamed writes that end short, run long or fail
/// leaving nothing, byte-exact ranges cut at the end and `416` from the end on, HEAD, idempotent
/// DELETE and a re-PUT after it, and a LIST of exactly the live keys in pages; and that the run
/// leaves no key listed.
#[tokio::test]
async fn the_s3_front_passes_the_object_store_conformance_suite() {
    let h = Harness::start(|_| {}).await;
    let store = h.store_default(CONDITIONAL);
    let root = KeyPrefix::new("conformance/run-1/").unwrap();
    let report = tokio::select! {
        report = run(&store, &root, IdenticalPut::Refused) => report,
        () = h.reconcile_forever() => unreachable!("the reconciler runs until the suite ends"),
    };
    assert!(report.passed(), "{report}");
}

/// Asserts that a LIST of a prefix gives exactly the live keys after PUTs, DELETEs and re-PUTs
/// once the node has restarted (a new server over the same tiers), and that each still holds its
/// bytes (10.9: "including across a node restart").
#[tokio::test]
async fn the_live_keys_are_listed_exactly_after_a_restart() {
    let h = Harness::start(|_| {}).await;
    let prefix = KeyPrefix::new("conformance/live/").unwrap();
    let store = h.store_default(CONDITIONAL);
    let set = tokio::select! {
        set = LiveSet::seed(&store, &prefix) => set.unwrap(),
        () = h.reconcile_forever() => unreachable!("the reconciler runs until the writes end"),
    };
    let h = h.restart().await;
    set.check(&h.store_default(CONDITIONAL)).await.unwrap();
}
