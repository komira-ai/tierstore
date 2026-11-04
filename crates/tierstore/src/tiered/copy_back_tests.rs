//! The reads of a proxy whose format is active: a hot hit and a head read no entry, and a hit in
//! the cold tier copies nothing itself and asks for a copy-back on an entry, create-only, that the
//! reconciler then does.
//!
//! The ask runs in a task of its own, so a test holds its write with a gate (`Post`: the write has
//! reached the store) and never waits for a time.

use futures::StreamExt;

use crate::entry::{Outcome, Step, Take};
use crate::journal::ENTRIES_PREFIX;
use crate::{
    Backend, EntryOp, EntryReconciler, EntryReconcilerConfig, EntryState, Error, Op, Phase,
    ReconcilerConfig,
};

use super::entry_put_tests::{Rig, body};

/// Reads the object `key` of the proxy whole in the way `how` says (0: `open_read`, 1: `get`,
/// 2: `get_stream`) and returns its bytes.
async fn read_whole(rig: &Rig, key: &str, how: usize) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    match how {
        0 => {
            let opened = rig.proxy.open_read(key, None).await?;
            let mut stream = opened.body;
            while let Some(chunk) = stream.next().await {
                bytes.extend_from_slice(&chunk?);
            }
        }
        1 => bytes.extend_from_slice(&rig.proxy.get(key, None).await?),
        _ => {
            let mut stream = rig.proxy.get_stream(key, None).await?;
            while let Some(chunk) = stream.next().await {
                bytes.extend_from_slice(&chunk?);
            }
        }
    }
    Ok(bytes)
}

/// Lets the tasks that are ready run.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

fn copy_back_pending() -> Option<(EntryOp, EntryState, Option<String>)> {
    Some((EntryOp::Cb, EntryState::Pending, None))
}

/// Asserts that a whole read of an object only the cold tier has, by `open_read`, `get` and
/// `get_stream`, gives the object's bytes, writes nothing under the key into the hot tier, and
/// leaves a `(CB, PENDING)` entry with no owner, made by one create-only write.
#[tokio::test]
async fn a_cold_hit_copies_nothing_and_asks_for_a_copy_back_on_an_entry() {
    for how in 0..3 {
        let rig = Rig::active().await;
        rig.cold_mem.put("k", body("cold")).await.unwrap();
        let gate = rig.hot.gate(Op::Put, ENTRIES_PREFIX, Phase::Post);
        assert_eq!(read_whole(&rig, "k", how).await.unwrap(), b"cold");
        gate.parked().await;
        assert!(gate.release());
        settle().await;
        assert_eq!(rig.shape("k").await, copy_back_pending(), "{how}");
        assert!(!rig.hot_has("k").await);
        assert_eq!(rig.hot_faults.calls_on(Op::Put, "k"), 0);
        assert!(rig.cold_has("k").await);
    }
}

/// Asserts that a cold hit of a key that has an entry already (a delete in flight, a copy-back
/// asked for, an eviction) serves the bytes and copies nothing: the ask is one create-only write
/// that does nothing, and the entry is as it was.
#[tokio::test]
async fn a_cold_hit_with_an_open_entry_serves_without_a_copy_back() {
    let cases = [
        (Take::Del, (EntryOp::Del, EntryState::Held, Some("other"))),
        (Take::CopyBack, (EntryOp::Cb, EntryState::Pending, None)),
        (Take::Evict, (EntryOp::Evict, EntryState::Sealed, None)),
    ];
    for (take, (op, st, owner)) in cases {
        let rig = Rig::active().await;
        rig.cold_mem.put("k", body("cold")).await.unwrap();
        rig.other().create("k", take).await.unwrap();
        let before = rig.entry("k").await.unwrap();
        let gate = rig.hot.gate(Op::Put, ENTRIES_PREFIX, Phase::Post);
        assert_eq!(read_whole(&rig, "k", 0).await.unwrap(), b"cold");
        gate.parked().await;
        assert!(gate.release());
        settle().await;
        assert_eq!(rig.entry("k").await, Some(before));
        let shape = rig.shape("k").await;
        assert_eq!(shape, Some((op, st, owner.map(str::to_owned))));
        assert!(!rig.hot_has("k").await);
        assert_eq!(rig.hot_faults.calls_on(Op::Put, "k"), 0);
    }
}

/// Asserts that a hot hit and a head (of a hot object and of a cold one) make no call on any entry
/// and no write at all, and that a cold read reads no entry either: it only creates one.
#[tokio::test]
async fn gets_and_heads_never_read_an_entry() {
    let rig = Rig::active().await;
    rig.hot_mem.put("hot", body("h")).await.unwrap();
    rig.cold_mem.put("cold", body("c")).await.unwrap();
    assert_eq!(read_whole(&rig, "hot", 0).await.unwrap(), b"h");
    assert_eq!(read_whole(&rig, "hot", 1).await.unwrap(), b"h");
    rig.proxy.head("hot").await.unwrap();
    rig.proxy.head("cold").await.unwrap();
    settle().await;
    assert!(rig.hot_faults.call_log().iter().all(|(op, key)| {
        !(key.starts_with(ENTRIES_PREFIX) && matches!(op, Op::Get | Op::Head | Op::Put))
    }));
    assert_eq!(read_whole(&rig, "cold", 1).await.unwrap(), b"c");
    settle().await;
    let log = rig.hot_faults.call_log();
    let on_entries: Vec<_> = log
        .iter()
        .filter(|(_, key)| key.starts_with(ENTRIES_PREFIX))
        .collect();
    assert_eq!(on_entries.len(), 1, "{on_entries:?}");
    assert_eq!(on_entries[0].0, Op::Put);
}

/// Asserts that a read whose ask fails is not failed by it: the bytes are served and no entry is
/// left.
#[tokio::test]
async fn a_failed_ask_does_not_fail_the_read() {
    let rig = Rig::active().await;
    rig.cold_mem.put("k", body("cold")).await.unwrap();
    rig.hot_faults.fail_next_on(
        Op::Put,
        ENTRIES_PREFIX,
        Error::Unavailable("down".to_owned()),
    );
    let gate = rig.hot.gate(Op::Put, ENTRIES_PREFIX, Phase::Post);
    assert_eq!(read_whole(&rig, "k", 0).await.unwrap(), b"cold");
    gate.parked().await;
    assert!(gate.release());
    settle().await;
    assert!(rig.entry("k").await.is_none());
}

/// Asserts that no copy-back is asked for when the hot tier is above the critical mark, and none
/// for a read of a range: the bytes are served and no entry is made.
#[tokio::test]
async fn a_cold_hit_at_the_critical_mark_or_of_a_range_asks_for_no_copy_back() {
    let rig = Rig::active().await;
    rig.cold_mem.put("k", body("cold")).await.unwrap();
    rig.fill_hot().await;
    assert_eq!(read_whole(&rig, "k", 0).await.unwrap(), b"cold");
    assert_eq!(read_whole(&rig, "k", 1).await.unwrap(), b"cold");
    settle().await;
    assert!(rig.entry("k").await.is_none());
    let rig = Rig::active().await;
    rig.cold_mem.put("k", body("cold")).await.unwrap();
    let range = crate::ByteRange::new(1, 2);
    assert_eq!(rig.proxy.get("k", Some(range)).await.unwrap(), body("ol"));
    settle().await;
    assert!(rig.entry("k").await.is_none());
    assert_eq!(rig.hot_faults.calls_on(Op::Put, "k"), 0);
}

/// Asserts the whole of a copy-back: a cold hit asks for it, the reconciler copies the cold object
/// into the hot tier and removes the entry, and a get is then a hot hit. And that a copy-back that
/// a delete amended first copies nothing: the key stays out of the hot tier.
#[tokio::test]
async fn the_reconciler_copies_what_a_cold_hit_asked_for_and_not_what_a_delete_took() {
    let rig = Rig::active().await;
    for key in ["copied", "deleted"] {
        rig.cold_mem.put(key, body(key)).await.unwrap();
        let gate = rig.hot.gate(Op::Put, ENTRIES_PREFIX, Phase::Post);
        assert_eq!(read_whole(&rig, key, 1).await.unwrap(), key.as_bytes());
        gate.parked().await;
        assert!(gate.release());
        settle().await;
        assert_eq!(rig.shape(key).await, copy_back_pending());
    }
    let pending = rig.entry("deleted").await.unwrap();
    let amended = rig.other().advance(&pending, Step::Amend).await.unwrap();
    assert!(matches!(amended, Outcome::Done(_)));
    rig.cold_mem.delete("deleted").await.unwrap();

    let base = ReconcilerConfig::new("r1", 1 << 20, rig.clock.clone());
    let mut config = EntryReconcilerConfig::new(base);
    config.base.mono = rig.mono.clone();
    let mut reconciler = EntryReconciler::new(rig.hot_mem.clone(), rig.cold_mem.clone(), config);
    let step = reconciler.step().await;
    assert!(step.failed.is_empty(), "{:?}", step.failed);
    assert_eq!(
        rig.hot_mem.get("copied", None).await.unwrap(),
        body("copied")
    );
    assert!(rig.entry("copied").await.is_none());
    assert!(!rig.hot_has("deleted").await);
    assert_eq!(rig.shape("deleted").await.map(|s| s.0), Some(EntryOp::Del));
    assert_eq!(read_whole(&rig, "copied", 1).await.unwrap(), b"copied");
}
