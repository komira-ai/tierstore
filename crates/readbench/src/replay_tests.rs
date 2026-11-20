//! The verifier of an unverified run.

use bytes::Bytes;
use tierstore_fetch_api::verify::{CellJob, Verdict, Verifier as _, VerifyPriority};

use super::{BTreeMap, NoVerifier, placeholder};

#[tokio::test]
async fn the_verifier_of_an_unverified_run_refuses_a_cell_it_is_handed() {
    let job = CellJob {
        cell: 0,
        expected: [0; 32],
        bytes: Bytes::from_static(b"x"),
        priority: VerifyPriority::Fill,
    };
    let (verdict, back) = NoVerifier.verify(job).await;
    assert_eq!(verdict, Verdict::Mismatch);
    assert_eq!(&back[..], b"x");
}

#[test]
fn a_placeholder_has_one_zero_digest_per_started_cell() {
    let sizes = BTreeMap::from([("k".to_owned(), (2 << 20) + 1)]);
    let info = placeholder(&sizes, "k").unwrap();
    assert_eq!(info.cells.size, (2 << 20) + 1);
    assert_eq!(info.cells.digests, vec![[0u8; 32]; 3]);
    assert!(placeholder(&sizes, "other").is_none());
}
