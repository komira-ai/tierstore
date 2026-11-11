//! Handover: the pins of a hold move to the attachment of a mount at its first pin of a tree the
//! hold lists, and a fill that lands later is pinned for the attachments, not for the hold. The
//! upstream is held at a gate so that each step is made while a fill is in the middle.

use super::holds::{HoldError, Released};
use super::holds_tests::{held_by, pinners, tree};
use super::prefetch::Prefetched;
use super::prefetch_tests::{rig, stored};
use super::testing::{Rig, until};
use crate::Op;

/// The names of the trees the pins file lists, each with the mount and the blobs, as the file has
/// them now.
fn file_of(rig: &Rig) -> serde_json::Value {
    rig.cache.inner.save_pins(true);
    let text = std::fs::read(rig.config.path.join("pins")).unwrap();
    serde_json::from_slice(&text).unwrap()
}

/// Asserts that the first pin of a mount for a tree a hold lists moves every pin of the hold on
/// that tree to the attachment, the blobs the request did not name included: the owners of each
/// blob are then the attachment alone, the pinned bytes do not change, the hold counts the
/// handover once however many pins follow, and the pins file lists the attachment's tree with both
/// blobs and no tree of the hold.
#[tokio::test]
async fn attach_moves_the_holds_pins_to_the_attachment() {
    let rig = rig("thold-handover", 1);
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let t = tree(1);
    for blob in [&a, &b] {
        assert_eq!(rig.cache.fill_blob(&blob.blob()).await, Ok(()));
    }
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    for blob in [&a, &b] {
        assert_eq!(
            rig.cache.hold_blob("job", &t, &blob.blob()),
            Ok(Prefetched::Present)
        );
    }
    assert_eq!(pinners(&rig, &b.sha), vec![held_by("job", &t)]);
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_hold_handovers_total 0\n")
    );

    let attached = (String::from("boot-1"), t.clone());
    let answer = rig
        .cache
        .pin("boot-1", &t, std::slice::from_ref(&a.sha))
        .unwrap();
    assert_eq!(answer.pinned, 1);
    assert_eq!(pinners(&rig, &a.sha), vec![attached.clone()]);
    assert_eq!(pinners(&rig, &b.sha), vec![attached.clone()]);
    assert_eq!(rig.cache.pinned_bytes(), 5000);
    assert_eq!(
        rig.cache
            .pin("boot-1", &t, std::slice::from_ref(&b.sha))
            .unwrap()
            .pinned,
        1
    );
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_hold_handovers_total 1\n")
    );

    let mut blobs = vec![a.sha.clone(), b.sha.clone()];
    blobs.sort();
    let want = serde_json::json!({"trees": [{"mount": "boot-1", "tree": t, "blobs": blobs}]});
    assert_eq!(file_of(&rig), want);
}

/// Asserts that a blob whose fill ends after the tree was attached is pinned for each attachment
/// of the tree, not for the hold, and that a second mount that attaches the tree is pinned for
/// too.
#[tokio::test]
async fn blob_filled_after_attach_is_pinned_for_the_attachment() {
    let rig = rig("thold-after", 1);
    let a = stored(&rig, 1, 3000).await;
    let t = tree(1);
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Queued)
    );
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.pin("boot-1", &t, &[]).unwrap().pinned, 0);
    assert_eq!(rig.cache.pin("boot-2", &t, &[]).unwrap().pinned, 0);

    gate.add_permits(64);
    until("a is pinned when it lands", || {
        rig.cache.pinned_bytes() == 3000
    })
    .await;
    let mut owners = pinners(&rig, &a.sha);
    owners.sort();
    let want = vec![
        ("boot-1".to_owned(), t.clone()),
        ("boot-2".to_owned(), t.clone()),
    ];
    assert_eq!(owners, want);
    let filled = Released {
        trees: 1,
        filled: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("job"), Ok(filled));
}

/// Asserts that the release of a hold after the handover leaves the attachment's pins, that the
/// unpin of the tree (the detach) then ends them, and that a fill that ends after that is pinned
/// for nobody: the hold gave the tree up and the attachment is gone.
#[tokio::test]
async fn release_after_attach_leaves_the_attachments_pins() {
    let rig = rig("thold-release", 1);
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let t = tree(1);
    assert_eq!(rig.cache.fill_blob(&a.blob()).await, Ok(()));
    let gate = rig.tamper.hold_key(&b.key);
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Present)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &b.blob()),
        Ok(Prefetched::Queued)
    );
    assert_eq!(
        rig.cache
            .pin("boot-1", &t, std::slice::from_ref(&a.sha))
            .unwrap()
            .pinned,
        1
    );

    // The release leaves the fill that runs, and the attachment's pin of a.
    let released = Released {
        trees: 1,
        present: 1,
        pending: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("job"), Ok(released));
    assert_eq!(
        pinners(&rig, &a.sha),
        vec![("boot-1".to_owned(), t.clone())]
    );
    assert_eq!(rig.cache.pinned_bytes(), 3000);

    assert_eq!(rig.cache.unpin("boot-1", &t), Ok(1));
    assert_eq!(rig.cache.pinned_bytes(), 0);
    gate.add_permits(64);
    until("b is in the cache", || rig.cache.held() == (5000, 2)).await;
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that a hold opened for a tree that a mount has pinned already pins nothing for itself:
/// the blob the cache holds and the blob that fills later are both pinned for the attachment
/// alone, and no handover is counted, since no pin of the hold moved.
#[tokio::test]
async fn attach_before_hydrate_leaves_the_hold_pinning_nothing() {
    let rig = rig("thold-before", 1);
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let t = tree(1);
    assert_eq!(rig.cache.fill_blob(&a.blob()).await, Ok(()));
    assert_eq!(
        rig.cache
            .pin("boot-1", &t, std::slice::from_ref(&a.sha))
            .unwrap()
            .pinned,
        1
    );
    let gate = rig.tamper.hold_key(&b.key);

    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Present)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &b.blob()),
        Ok(Prefetched::Queued)
    );
    gate.add_permits(64);
    until("b is in the cache", || rig.cache.held() == (5000, 2)).await;
    until("b is pinned", || rig.cache.pinned_bytes() == 5000).await;

    let attached = ("boot-1".to_owned(), t.clone());
    assert_eq!(pinners(&rig, &a.sha), vec![attached.clone()]);
    assert_eq!(pinners(&rig, &b.sha), vec![attached]);
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_hold_handovers_total 0\n")
    );
    let filled = Released {
        trees: 1,
        filled: 1,
        present: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("job"), Ok(filled));
    assert_eq!(rig.cache.pinned_bytes(), 5000);
}

/// Asserts that the attachments of a handed tree follow the mounts: a mount that unpins the tree
/// stops being pinned for while another mount of it still is, and a mount that sweeps the tree
/// away with `pins_live` stops being pinned for too.
#[tokio::test]
async fn a_fill_pins_for_the_attachments_that_are_left() {
    let rig = rig("thold-left", 1);
    let (a, b) = (stored(&rig, 1, 300).await, stored(&rig, 2, 200).await);
    let t = tree(1);
    let (gate_a, gate_b) = (rig.tamper.hold_key(&a.key), rig.tamper.hold_key(&b.key));
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Queued)
    );
    for mount in ["boot-1", "boot-2"] {
        assert_eq!(rig.cache.pin(mount, &t, &[]).unwrap().pinned, 0);
    }
    assert_eq!(rig.cache.unpin("boot-1", &t), Ok(0));
    gate_a.add_permits(64);
    until("a is pinned", || rig.cache.pinned_bytes() == 300).await;
    assert_eq!(
        pinners(&rig, &a.sha),
        vec![("boot-2".to_owned(), t.clone())]
    );

    // The tree is still listed by boot-2: a sweep that names it keeps it.
    assert_eq!(
        rig.cache.pins_live("boot-2", std::slice::from_ref(&t)),
        Ok(0)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &b.blob()),
        Ok(Prefetched::Queued)
    );
    gate_b.add_permits(64);
    until("b is pinned", || rig.cache.pinned_bytes() == 500).await;
    assert_eq!(
        pinners(&rig, &b.sha),
        vec![("boot-2".to_owned(), t.clone())]
    );

    assert_eq!(rig.cache.unpin("boot-2", &t), Ok(2));
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// A sweep by one mount takes the pins of every other mount, so it takes their attachments too:
/// a blob that fills after a new mount swept is pinned for nobody, and the pins file has none of
/// it. A mount that pins the tree again is the attachment from then on.
#[tokio::test]
async fn a_sweep_by_another_mount_forgets_the_attachments_it_dropped() {
    let rig = rig("thold-sweep-other", 1);
    let (a, b) = (stored(&rig, 1, 300).await, stored(&rig, 2, 200).await);
    let t = tree(1);
    let (gate_a, gate_b) = (rig.tamper.hold_key(&a.key), rig.tamper.hold_key(&b.key));
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Queued)
    );
    assert_eq!(rig.cache.pin("boot-a", &t, &[]).unwrap().pinned, 0);
    assert_eq!(rig.cache.pins_live("boot-b", &[]), Ok(0));
    gate_a.add_permits(64);
    until("a is in the cache", || rig.cache.held() == (300, 1)).await;
    assert!(pinners(&rig, &a.sha).is_empty());
    assert_eq!(rig.cache.pinned_bytes(), 0);

    assert_eq!(rig.cache.pin("boot-b", &t, &[]).unwrap().pinned, 0);
    assert_eq!(
        rig.cache.hold_blob("job", &t, &b.blob()),
        Ok(Prefetched::Queued)
    );
    gate_b.add_permits(64);
    until("b is pinned", || rig.cache.pinned_bytes() == 200).await;
    assert_eq!(
        pinners(&rig, &b.sha),
        vec![("boot-b".to_owned(), t.clone())]
    );
}

/// Asserts that the handover leaves the pins file as it was: the file has the same keys, one
/// entry for the attachment's tree with its blobs and none for the hold, so the previous release
/// reads it as the pins of an attachment, and a cache opened over the directory again pins those
/// blobs for the attachment and has no hold.
#[tokio::test]
async fn handed_over_pins_are_in_the_pins_file_and_a_restart_keeps_them() {
    let rig = rig("thold-compat", 1);
    let a = stored(&rig, 1, 3000).await;
    let t = tree(1);
    assert_eq!(rig.cache.fill_blob(&a.blob()).await, Ok(()));
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Present)
    );
    assert_eq!(file_of(&rig), serde_json::json!({"trees": []}));
    assert_eq!(rig.cache.pin("boot-1", &t, &[]).unwrap().pinned, 0);
    let want = serde_json::json!({"trees": [{"mount": "boot-1", "tree": t, "blobs": [a.sha]}]});
    assert_eq!(file_of(&rig), want);

    let restarted = rig.reopen();
    assert_eq!(restarted.pinned_bytes(), 3000);
    assert_eq!(
        restarted.release_hold("job"),
        Err(HoldError::NoSuchHold("job".to_owned()))
    );
    assert_eq!(restarted.unpin("boot-1", &t), Ok(1));
    assert_eq!(restarted.pinned_bytes(), 0);
}
