//! The reconciler over a mirror entry that names no tag, or a tag that is not the hot object's.

use tierstore::{Backend, Error, JournalOp};

use crate::reconciler_reservations::{entry, with_journal};
use crate::support::{assert_absent, body, read};

/// Asserts that a mirror entry that names no tag deletes nothing: it owns no object, so the hot
/// copy stays, and nothing is copied.
#[tokio::test(start_paused = true)]
async fn a_putmirror_entry_without_a_tag_leaves_the_hot_copy() {
    let rig = with_journal(&[entry(JournalOp::PutMirror, "k", None)]).await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    assert_eq!(read(&*rig.hot_mem, "k").await, body("hot!!"));
    assert_absent(&*rig.cold_mem, "k").await;
}

/// Asserts that a mirror entry whose tag is not the hot object's leaves the hot object when the
/// cold tier holds nothing: the object is not the mirror's.
#[tokio::test(start_paused = true)]
async fn a_putmirror_entry_does_not_delete_a_hot_object_of_another_tag() {
    let rig = with_journal(&[entry(JournalOp::PutMirror, "k", Some("old"))]).await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.hot_mem, "k").await, body("hot!!"));
}
