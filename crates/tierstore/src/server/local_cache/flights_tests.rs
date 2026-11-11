//! One table of running fills for every path: a name has one fill whoever asks, a request that
//! finds one joins it and takes no permit, and the fill's end is read by each path in its own way.
//! The upstream is held at a gate, so each request is made while the fill is in the middle.

use futures::poll;

use super::fill::{Ended, Filled, Stop, s3_answer};
use super::pack_testing::Pack;
use super::source::Blob;
use super::testing::{Rig, digest, pattern, until};
use crate::{Backend as _, Error, Op};

/// The permits of the fill socket's pool and of the S3 front's, free now.
fn free(rig: &Rig) -> (usize, usize) {
    let inner = &rig.cache.inner;
    (
        inner.source_permits.available_permits(),
        inner.permits.available_permits(),
    )
}

/// Asserts that a request of the fill socket that arrives while an S3 read's fill of the same
/// digest runs joins it: it takes no permit of its pool, the upstream is read once, and both are
/// answered with the bytes once the fill ends.
#[tokio::test]
async fn mount_open_during_s3_fill_joins_it() {
    let rig = Rig::new("tflights-mount-joins");
    let body = pattern(2000);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;
    let all = free(&rig);
    let gate = rig.tamper.hold();
    let (cache, name) = (rig.cache.clone(), key.clone());
    let reader = tokio::spawn(async move { cache.get(&name, None).await });
    until("the S3 fill is at the upstream", || rig.calls(Op::Get) == 1).await;

    let blob = Blob {
        sha256: &sha,
        size: 2000,
        from: &key,
        offset: 0,
    };
    let mut request = Box::pin(rig.cache.fill_blob(&blob));
    assert!(poll!(request.as_mut()).is_pending());
    assert_eq!(free(&rig), (all.0, all.1 - 1));

    gate.add_permits(64);
    assert_eq!(request.await, Ok(()));
    assert_eq!(reader.await.unwrap().unwrap(), body);
    assert_eq!(rig.calls_on(Op::Get, &key), 1);
    assert_eq!(rig.cache.held(), (2000, 1));
    assert_eq!(free(&rig), all);
}

/// Asserts that an S3 read that arrives while the fill socket's fill of the same digest runs
/// joins it: it takes no permit of the S3 pool, the upstream is read once, and the read is given
/// the bytes.
#[tokio::test]
async fn s3_get_during_mount_fill_joins_it() {
    let rig = Rig::new("tflights-s3-joins");
    let body = pattern(2000);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;
    let all = free(&rig);
    let gate = rig.tamper.hold();
    let (cache, from, wanted) = (rig.cache.clone(), key.clone(), sha.clone());
    let filling = tokio::spawn(async move {
        let blob = Blob {
            sha256: &wanted,
            size: 2000,
            from: &from,
            offset: 0,
        };
        cache.fill_blob(&blob).await
    });
    until("the fill is at the upstream", || rig.calls(Op::Get) == 1).await;
    assert_eq!(free(&rig), (all.0 - 1, all.1));

    let mut read = Box::pin(rig.get(&key));
    assert!(poll!(read.as_mut()).is_pending());
    assert_eq!(free(&rig), (all.0 - 1, all.1));

    gate.add_permits(64);
    assert_eq!(read.await.unwrap(), body);
    assert_eq!(filling.await.unwrap(), Ok(()));
    assert_eq!(rig.calls_on(Op::Get, &key), 1);
    assert_eq!(rig.cache.held(), (2000, 1));
    assert_eq!(free(&rig), all);
}

/// Asserts that an S3 read does not wait for a fill whose bytes are cut out of a pack: the name is
/// not an object of the upstream, so the read is answered by the upstream (not found, here) while
/// the fill is still in the middle, and the fill ends as an entry that the read still does not
/// see.
#[tokio::test]
async fn s3_get_during_span_fill_does_not_wait_for_it() {
    let rig = Rig::new("tflights-span");
    let pack = Pack::put(&rig, "packs/flying", &[900, 700]).await;
    let key = format!("cas/l/{}", pack.blobs[0].0);
    let gate = rig.tamper.hold_key(&pack.key);
    let (cache, sha, from) = (rig.cache.clone(), pack.blobs[0].0.clone(), pack.key.clone());
    let filling = tokio::spawn(async move {
        let blob = Blob {
            sha256: &sha,
            size: 900,
            from: &from,
            offset: 0,
        };
        cache.fill_blob(&blob).await
    });
    until("the span fill is at the upstream", || {
        rig.calls_on(Op::Get, &pack.key) == 1
    })
    .await;

    assert_eq!(rig.get(&key).await, Err(Error::NotFound(key.clone())));
    assert_eq!(rig.cache.held(), (0, 0));
    gate.add_permits(64);
    assert_eq!(filling.await.unwrap(), Ok(()));
    assert_eq!(rig.cache.held(), (900, 1));
    assert_eq!(rig.get(&key).await, Err(Error::NotFound(key.clone())));
}

/// Asserts that a request of the fill socket for a digest whose place in the table is a keep (a
/// put through the cache) is not answered `no space`: it waits for the keep to leave the table
/// and then fills, and an S3 reader that finds the place is answered from the upstream at once.
#[tokio::test]
async fn a_request_that_finds_a_keep_waits_for_it_and_is_then_filled() {
    let rig = Rig::new("tflights-place");
    let body = pattern(300);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;
    let hex = super::layout::name_of(&key).hex;
    let (place, gate) = super::fill::Pending::keeping();
    drop(super::locked(&rig.cache.inner.flights).insert(hex.clone(), place.clone()));
    assert_eq!(place.clone().await, Ok(Filled::Skipped));

    let cache = rig.cache.clone();
    let asked = tokio::spawn(async move { cache.fill_manifest(&sha).await });
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert!(!asked.is_finished());
    assert_eq!(rig.calls(Op::Get), 0);

    drop(super::locked(&rig.cache.inner.flights).remove(&hex));
    drop(gate);
    assert_eq!(asked.await.unwrap(), Ok(()));
    assert_eq!(rig.cache.held(), (300, 1));
}

/// Asserts what an S3 reader is told by each way a fill can end: the exact answer of
/// [`s3_answer`] for every [`Stop`] (the readers of a fill that did not keep the object go to the
/// upstream; bytes known to be wrong and a fill that did not finish are errors).
#[test]
fn the_s3_front_reads_each_end_of_a_fill_its_own_way() {
    let gone = Error::NotFound("k".to_owned());
    let answer = |ended: Ended| s3_answer(ended);
    assert_eq!(answer(Ok(Filled::Kept)), Ok(Filled::Kept));
    assert_eq!(answer(Err(Stop::Upstream(gone.clone()))), Err(gone));
    for stop in [Stop::Short, Stop::Local, Stop::Invalidated, Stop::Lost] {
        assert_eq!(answer(Err(stop)), Ok(Filled::Skipped));
    }
    assert_eq!(answer(Err(Stop::Changed)), Ok(Filled::Changed));
    let text = |text: &str| Err(Error::Unavailable(text.to_owned()));
    assert_eq!(
        answer(Err(Stop::Mismatch)),
        text("the upstream's bytes do not hash to the name of the object")
    );
    assert_eq!(
        answer(Err(Stop::TooLarge)),
        text("the object is larger than a whole-object fill may be")
    );
    assert_eq!(
        answer(Err(Stop::Aborted)),
        text("the fill of the object did not finish")
    );
}
