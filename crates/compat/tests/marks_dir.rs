//! The markers of a `dir:` hot tier, in the directory beside it that holds its marks bucket: the
//! lock that makes their compare-and-swap atomic between processes, and that the release before
//! reads and replaces the objects this branch wrote there as the plain objects they are. The
//! release before is the real `tierstore` library of its commit (`tierstore_prev`), not a copy of
//! its behaviour; it has no lock for such keys, so the processes that race here are this branch's.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::Arc;

use bytes::Bytes;
use tierstore::{Backend, DirBackend, Error, Marker, MarkerState, decode_marker, encode_marker};
use tierstore_prev::{Backend as PrevBackend, DirBackend as PrevDir};
use tierstore_testkit::dir::TestDir;

/// A key with a `/`, a space and multi-byte text: the marker is named by it exactly.
const KEY: &str = "models/a b/é.bin";

/// The body of a marker of `KEY` with this state and nonce.
fn marker(st: MarkerState, nonce: usize) -> Bytes {
    encode_marker(&Marker {
        v: 1,
        key: KEY.to_owned(),
        st,
        gone: false,
        dirty: true,
        why: None,
        owner: None,
        nonce: format!("{nonce:032x}"),
        extra: Default::default(),
    })
}

/// The nonce a marker body carries.
fn nonce_of(body: &[u8]) -> String {
    decode_marker(body).unwrap().nonce
}

/// Asserts that of eight create-only puts of one marker, made through two backends over one
/// directory (two processes, as far as the lock can tell), exactly one lands and the others are
/// refused as existing; that of eight replacements by one tag, again through the two, exactly one
/// lands and the others are refused for the tag; and that the body left each time is the winner's
/// whole, which the release before reads and replaces as an object: it gives the same bytes and tag,
/// lists the key once and not the lock's subdirectory, and the replacement it makes by the tag is
/// read back by this branch as the marker that was written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_processes_of_one_dir_tier_share_create_only_and_if_match_on_a_marker() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-marks-dir");
    let backends = [
        Arc::new(DirBackend::open(dir.path()).unwrap().with_locked_keys()),
        Arc::new(DirBackend::open(dir.path()).unwrap().with_locked_keys()),
    ];

    let mut creating = Vec::new();
    for round in 0..8 {
        let backend = backends[round % 2].clone();
        let body = marker(MarkerState::Dirty, round);
        creating.push(tokio::spawn(async move {
            (round, backend.put(KEY, body).await)
        }));
    }
    let mut landed = Vec::new();
    for task in creating {
        match task.await.unwrap() {
            (round, Ok(())) => landed.push(round),
            (_, Err(refused)) => assert_eq!(refused, Error::AlreadyExists(KEY.to_owned())),
        }
    }
    assert_eq!(landed.len(), 1, "{landed:?}");
    let created = backends[0].get(KEY, None).await.unwrap();
    assert_eq!(nonce_of(&created), format!("{:032x}", landed[0]));
    let tag = backends[1].head(KEY).await.unwrap().tag;

    let mut swapping = Vec::new();
    for round in 0..8 {
        let backend = backends[round % 2].clone();
        let body = marker(MarkerState::Lock, 100 + round);
        let tag = tag.clone();
        let swap = async move { (round, backend.put_if_match(KEY, body, &tag).await) };
        swapping.push(tokio::spawn(swap));
    }
    let mut won = Vec::new();
    for task in swapping {
        match task.await.unwrap() {
            (round, Ok(_)) => won.push(round),
            (_, Err(refused)) => assert_eq!(refused, Error::PreconditionFailed(KEY.to_owned())),
        }
    }
    assert_eq!(won.len(), 1, "{won:?}");
    let swapped = backends[0].get(KEY, None).await.unwrap();
    let left = decode_marker(&swapped).unwrap();
    assert_eq!(left.st, MarkerState::Lock);
    assert_eq!(left.nonce, format!("{:032x}", 100 + won[0]));

    let before = PrevDir::open(dir.path()).unwrap();
    assert_eq!(before.get(KEY, None).await.unwrap(), swapped);
    let seen = before.head(KEY).await.unwrap();
    let ours = backends[0].head(KEY).await.unwrap();
    assert_eq!(
        (seen.size, seen.tag.as_str()),
        (ours.size, ours.tag.as_str())
    );
    let listed: Vec<String> = before
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    assert_eq!(listed, [KEY]);

    let clearing = marker(MarkerState::Clearing, 200);
    before
        .put_if_match(KEY, clearing.clone(), &seen.tag)
        .await
        .unwrap();
    assert_eq!(backends[1].get(KEY, None).await.unwrap(), clearing);
    assert_eq!(decode_marker(&clearing).unwrap().st, MarkerState::Clearing);
}

/// Asserts that a marker the release before wrote into a marks directory as a plain object (a
/// release that has no marker, so the object is only bytes in the shape of one) is read by this
/// branch, replaced by its tag under the lock, and deleted by the tag, and the release before then
/// finds the key gone.
#[tokio::test]
async fn this_branch_replaces_and_deletes_by_tag_a_marker_object_the_previous_release_wrote() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-marks-dir-old");
    let before = PrevDir::open(dir.path()).unwrap();
    let now = DirBackend::open(dir.path()).unwrap().with_locked_keys();

    before.put(KEY, marker(MarkerState::Want, 1)).await.unwrap();
    let tag = now.head(KEY).await.unwrap().tag;
    assert_eq!(
        nonce_of(&now.get(KEY, None).await.unwrap()),
        format!("{:032x}", 1)
    );
    let replaced = now
        .put_if_match(KEY, marker(MarkerState::Dirty, 2), &tag)
        .await
        .unwrap();
    assert_eq!(before.head(KEY).await.unwrap().tag, replaced);
    now.delete_if_match(KEY, &replaced).await.unwrap();
    assert!(before.list("").await.unwrap().is_empty());
}
