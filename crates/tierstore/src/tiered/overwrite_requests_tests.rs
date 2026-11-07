//! What a plain put asks of the stores and how it meets other writers: the requests of a put of a
//! new key and of a same-bytes put, and the overwrites of one key that overlap.
//!
//! Nothing waits for time. The overlapping puts run on the paused clock of the runtime, the stores
//! delay each call by a few milliseconds of it so that the writers meet, and the waits of the
//! proxies are timers of the same clock (`Rig::timed`).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::Either;

use super::overwrite::{PutBody, PutClaim};
use super::plain_put_tests::{MARKS_FORMAT, Rig, body, held, marker, settle};
use crate::{Backend, Error, MarkerState, Op, Phase};

fn log(calls: &[(Op, &str)]) -> Vec<(Op, String)> {
    calls
        .iter()
        .map(|(op, key)| (*op, (*key).to_owned()))
        .collect()
}

/// Asserts that a plain put of a key the hot tier does not hold asks the hot tier for one HEAD
/// before it writes (the design's 3 requests: the HEAD, then the object and the marker together),
/// and reads no marker: its create is create-only, so a marker that exists is found by it.
#[tokio::test]
async fn a_plain_put_of_a_new_key_asks_hot_once_then_writes_the_object_and_the_marker_and_never_cold()
 {
    for stream in [false, true] {
        let rig = Rig::new().await;
        rig.put_kind("new", "v", stream).await.unwrap();
        let hot = log(&[(Op::Head, "new"), (Op::Put, "new")]);
        assert_eq!(rig.hot_log(), hot, "stream {stream}");
        assert_eq!(rig.marks_log(), log(&[(Op::Put, "new")]), "stream {stream}");
        assert!(rig.cold_faults.call_log().is_empty());
    }
}

/// Asserts that a put whose claim names the tag and size of the object the key holds asks for the
/// HEAD and nothing else: it reads no marker, since it locks nothing and writes nothing.
#[tokio::test]
async fn a_same_bytes_put_asks_one_head_and_reads_no_marker() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "same").await;
    let claim = PutClaim {
        tags: vec![rig.hot_tag("k").await],
    };
    let put = rig
        .proxy
        .put_plain("k", held("same"), &claim)
        .await
        .unwrap();
    assert!(!put.wrote);
    assert_eq!(rig.hot_log(), log(&[(Op::Head, "k")]));
    assert_eq!(rig.marks_log(), []);
}

/// Asserts that an overwrite reads the marker once the HEAD found the object: the read is not part
/// of a put of a new key, and is made before the lock is taken so that a marker that exists is
/// swapped and not created over.
#[tokio::test]
async fn an_overwrite_reads_the_marker_after_the_head_has_found_the_object() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let head = rig.hot.gate(Op::Head, "k", Phase::Pre);
    let mut put = Box::pin(rig.put("k", "new"));
    super::plain_put_tests::until_parked(&head, &mut put).await;
    assert_eq!(rig.marks_log(), []);
    assert!(head.release());
    put.await.unwrap();
    settle().await;
    assert_eq!(rig.marks_log().first(), Some(&(Op::Get, "k".to_owned())));
}

/// Asserts that a swap lost to another writer's lock is a wait and not a failure: with
/// `entry_rounds` of one, the writer that lost the swap finds the marker locked, waits for the
/// lock to end (the pause of the first poll) and then takes the lock and replaces the object.
#[tokio::test]
async fn a_lost_swap_that_finds_the_marker_locked_waits_for_the_lock() {
    let rig = Rig::with(MARKS_FORMAT, |config| config.entry_rounds = 1).await;
    rig.seed_hot("k", "old").await;
    rig.seed_marker(&marker("k", MarkerState::Dirty, false, None))
        .await;
    rig.pacer.hold(Duration::from_millis(20));
    let swap = rig.marks.gate(Op::PutIfMatch, "k", Phase::Pre);
    let mut put = Box::pin(rig.put("k", "new"));
    super::plain_put_tests::until_parked(&swap, &mut put).await;
    // Another writer takes the lock first.
    let theirs = marker("k", MarkerState::Lock, false, Some("them"));
    rig.marks_mem.delete("k").await.unwrap();
    rig.seed_marker(&theirs).await;
    assert!(swap.release());
    let slept = Box::pin(rig.pacer.slept_for(Duration::from_millis(20)));
    if let Either::Right((ended, _)) = futures::future::select(slept, &mut put).await {
        panic!("the put ended instead of waiting for the lock: {ended:?}");
    }
    assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 0);
    // It lets go.
    let released = marker("k", MarkerState::Dirty, false, None);
    rig.marks_mem.delete("k").await.unwrap();
    rig.seed_marker(&released).await;
    rig.pacer.open();
    let done = put.await.unwrap();
    settle().await;
    assert!(done.wrote);
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
}

/// The stores of `rig` take 3 ms of the runtime's clock for each call of the kinds a put makes.
fn slow_stores(rig: &Rig) {
    for op in [Op::Head, Op::Get, Op::Put, Op::PutIfMatch] {
        rig.hot_faults.set_delay(op, Some(Duration::from_millis(3)));
        rig.marks_faults
            .set_delay(op, Some(Duration::from_millis(3)));
    }
}

fn locked_by(guard: &Mutex<Vec<usize>>) -> MutexGuard<'_, Vec<usize>> {
    guard.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Asserts that eight overwrites of one key that overlap, four from each of two proxies, are all
/// acknowledged, and that the object is the bytes of the one acknowledged last: the writers wait
/// for each other's lock, as S3's last writer wins, and none is told that the marker keeps
/// changing. The runs are with a marker that exists (`DIRTY`) and with none.
#[tokio::test(start_paused = true)]
async fn eight_overlapping_overwrites_from_two_proxies_are_all_acknowledged_and_the_last_one_wins()
{
    for marked in [true, false] {
        let rig = Rig::timed().await;
        let other = rig.another_proxy("p2").await;
        rig.seed_hot("k", "v").await;
        if marked {
            rig.seed_marker(&marker("k", MarkerState::Dirty, false, None))
                .await;
        }
        slow_stores(&rig);

        let acknowledged = Arc::new(Mutex::new(Vec::new()));
        let mut puts = Vec::new();
        for n in 0..8 {
            let proxy = if n % 2 == 0 { &rig.proxy } else { &other };
            let (proxy, order) = (Arc::clone(proxy), Arc::clone(&acknowledged));
            puts.push(tokio::spawn(async move {
                let body = PutBody::Held(body(&format!("v{n}")));
                let put = proxy.put_plain("k", body, &PutClaim::default()).await;
                if put.is_ok() {
                    locked_by(&order).push(n);
                }
                put
            }));
        }
        for put in puts {
            let put: Result<_, Error> = put.await.unwrap();
            assert!(put.unwrap().wrote, "marked {marked}");
        }
        // The releases are tasks of their own, and each store call takes time of the clock.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let order = locked_by(&acknowledged).clone();
        assert_eq!(order.len(), 8, "marked {marked}");
        let last = format!("v{}", order[7]);
        assert_eq!(rig.hot_text("k").await, Some(last), "marked {marked}");
        let marker = rig.marker("k").await.unwrap();
        assert_eq!(marker.st, MarkerState::Dirty, "marked {marked}");
    }
}
