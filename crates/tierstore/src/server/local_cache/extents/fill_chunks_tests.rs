//! A range fill of many chunks: how many it asks of the upstream at once, the order it takes them
//! in, and what a chunk that fails part way through leaves.

use std::sync::atomic::Ordering;

use tokio::task::JoinHandle;

use super::super::pack_testing::Pack;
use super::super::testing::{Rig, digest, until};
use super::fill::CHUNKS_AT_ONCE;
use super::fill_tests::{data, fill};
use super::{RangeError, RangeWant};
use crate::Op;

/// A pack of 6000 bytes in `rig`, read below in chunks of 500 bytes.
async fn pack_of(rig: &Rig) -> Pack {
    Pack::put(rig, "cas/pack/chunks", &[1000, 2000, 3000]).await
}

/// A fill of the first 5000 bytes of `pack`, under the digest of their true bytes, on a task of
/// its own.
fn spawn_fill(rig: &Rig, pack: &Pack) -> JoinHandle<Result<(), RangeError>> {
    let cache = rig.cache.clone();
    let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[..5000]));
    tokio::spawn(async move {
        let want = RangeWant {
            pack: &key,
            known: None,
            offset: 0,
            len: 5000,
            sha256: &sha,
        };
        cache.fill_range(&want).await
    })
}

/// Lets the task that holds the fill run as far as it can before the test looks again.
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

/// Asserts that a range of ten chunks asks the upstream for four of them at once and no more: with
/// every read held, four GETs are asked and the fifth is not; each chunk that comes back lets one
/// more be asked; and once all are let go the range is kept with the bytes of the pack, under the
/// digest of the whole range, the chunks having been asked in the order of their offsets.
#[tokio::test]
async fn a_range_of_many_chunks_is_read_four_chunks_at_a_time() {
    let rig = Rig::with("tsext-chunks-at-once", |config| config.fill_chunk = 500);
    let pack = pack_of(&rig).await;
    let gate = rig.tamper.hold_key(&pack.key);
    let filling = spawn_fill(&rig, &pack);
    until("four chunks asked", || rig.calls(Op::Get) == 4).await;
    settle().await;
    assert_eq!(CHUNKS_AT_ONCE, 4);
    assert_eq!(rig.calls(Op::Get), 4);
    gate.add_permits(1);
    until("a fifth chunk asked", || rig.calls(Op::Get) == 5).await;
    settle().await;
    assert_eq!(rig.calls(Op::Get), 5);
    gate.add_permits(64);
    assert_eq!(filling.await.unwrap(), Ok(()));
    let asked: Vec<_> = (0..10).map(|n| Some((n * 500, 500))).collect();
    assert_eq!(rig.upstream.get_ranges(), asked);
    let file = std::fs::read(data(&rig, &pack)).unwrap();
    assert_eq!(file[..5000], pack.bytes()[..5000]);
    assert_eq!(rig.cache.extents_held(), (1, 5000));
}

/// Asserts that a chunk that fails while others are in flight fails the fill: the first chunk comes
/// back, the second fails, the fill is `Upstream(503)`, no chunk after the fifth is asked, and
/// nothing is kept, so a later fill of the same range reads it again and keeps it.
#[tokio::test]
async fn a_chunk_that_fails_ends_the_fill_and_keeps_nothing() {
    let rig = Rig::with("tsext-chunks-fail", |config| config.fill_chunk = 500);
    let pack = pack_of(&rig).await;
    let gate = rig.tamper.hold_key(&pack.key);
    let filling = spawn_fill(&rig, &pack);
    until("four chunks asked", || rig.calls(Op::Get) == 4).await;
    gate.add_permits(1);
    until("a fifth chunk asked", || rig.calls(Op::Get) == 5).await;
    rig.tamper.fail_once.store(true, Ordering::SeqCst);
    gate.add_permits(1);
    assert_eq!(filling.await.unwrap(), Err(RangeError::Upstream(503)));
    assert_eq!(rig.calls(Op::Get), 5);
    assert_eq!(rig.cache.extents_held(), (0, 0));
    gate.add_permits(64);
    fill(&rig, &pack, 0, 5000).await.unwrap();
    assert_eq!(rig.cache.extents_held(), (1, 5000));
}
