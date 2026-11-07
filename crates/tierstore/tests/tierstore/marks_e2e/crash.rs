//! A proxy that dies in the middle of an operation: what each death leaves in the stores, that the
//! key reads right meanwhile, and that the leader brings the stores back to agree.
//!
//! A death is a call parked at a gate while the future that made it is dropped before the call
//! goes on (`die_at`), or, for a death that runs no destructor, forgotten (`vanish`). Nothing
//! sleeps: the leader's clock is moved by hand.

use std::mem::ManuallyDrop;
use std::time::Duration;

use tierstore::StraySweep;
use tierstore::{CopyRecords, Gate, GateBackend, Phase, PutBody, PutClaim, SweepConfig, SweepTime};

use super::*;

/// The key of a test.
const K: &str = "builds/k";

/// Starts `put` on a task, waits until `gate` holds a call of it, and drops the task: the process
/// died with that call not yet made. Everything the task held is dropped with it.
async fn die_at<F>(gate: &Gate, put: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let task = tokio::spawn(put);
    gate.parked().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

/// Parks `put` at `gate` and forgets it without dropping it: the process was killed, and no drop
/// guard of it runs.
async fn vanish<F>(gate: &Gate, put: F) -> ManuallyDrop<std::pin::Pin<Box<F>>>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let mut put = ManuallyDrop::new(Box::pin(put));
    tokio::select! {
        biased;
        () = &mut *put => panic!("the call ended instead of parking"),
        () = gate.parked() => {}
    }
    put
}

/// Asserts the first crash window of a put: the object landed and the marker did not. The key reads
/// right (hot wins), the leader copies nothing (no marker), and the stray sweep adopts the object
/// once it is older than `stray_grace`, so that the next pass of the leader copies it to cold.
#[tokio::test]
async fn a_put_that_dies_after_its_object_landed_leaves_it_unmarked_and_the_sweep_adopts_it() {
    let world = World::new().await;
    let marks = Arc::new(GateBackend::new(world.marks.clone()));
    let mut config = world.config("p1");
    config.marks = Some(marks.clone());
    let p1 = world.proxy_with(config).await;
    let p2 = world.proxy("p2").await;
    let gate = marks.gate(Op::Put, K, Phase::Pre);
    die_at(&gate, async move {
        let _ = p1.put(K, body("v1")).await;
    })
    .await;

    assert_eq!(world.hot_text(K).await.as_deref(), Some("v1"));
    assert_eq!(world.marker(K).await, None);
    assert_eq!(served(&p2, K).await.as_deref(), Ok("v1"));
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 0);
    assert_eq!(world.cold_text(K).await, None);

    world.pass(Duration::from_secs(601));
    let mut sweep = StraySweep::new(
        world.hot.clone(),
        world.cold.clone(),
        world.marks.clone(),
        world.markers("r1"),
        Arc::new(CopyRecords::default()),
        SweepTime {
            clock: world.clock.clone(),
            mono: world.mono.clone(),
            waiter: Arc::new(Ticking {
                mono: world.mono.clone(),
            }),
        },
        SweepConfig::default(),
    );
    let report = sweep.run().await;
    assert_eq!((report.active, report.adopted), (true, 1), "{report:?}");
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));

    assert_eq!(leader.step().await.changed, 1);
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v1"));
    assert_eq!(world.marker(K).await, None);
    assert_eq!(served(&p2, K).await.as_deref(), Ok("v1"));
}

/// Asserts the second crash window of a put: the marker landed and the object did not. The put was
/// never acknowledged, so the key reads as absent; the leader leaves the marker at its first pass
/// and clears it after `orphan_after`; and the key can be created again.
#[tokio::test]
async fn a_put_that_dies_after_its_marker_landed_leaves_a_marker_the_leader_clears_after_orphan_after()
 {
    let world = World::new().await;
    let hot = Arc::new(GateBackend::new(world.hot.clone()));
    let gate = hot.gate(Op::Put, K, Phase::Pre);
    let p1 = world.proxy_over(hot, world.config("p1")).await;
    let p2 = world.proxy("p2").await;
    die_at(&gate, async move {
        let _ = p1.put(K, body("v1")).await;
    })
    .await;

    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
    assert_eq!(world.hot_text(K).await, None);
    assert_eq!(served(&p2, K).await, Err(Error::NotFound(K.to_owned())));
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 0);
    assert!(world.marker(K).await.is_some());

    world.pass(Duration::from_secs(121));
    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.marker(K).await, None);
    p2.put(K, body("v2")).await.unwrap();
    assert_eq!(served(&p2, K).await.as_deref(), Ok("v2"));
}

/// Asserts that a proxy killed while it holds the lock of an overwrite, after the lock was taken
/// and before the replace, leaves the old bytes readable and the key locked; that another proxy's
/// put waits for `lock_wait` and is answered `Unavailable` (the lock's 503) and writes nothing; that
/// the leader reaps the lock once it stood unchanged for `lock_lease`; and that the same put then
/// succeeds.
#[tokio::test]
async fn a_proxy_killed_holding_an_overwrite_lock_is_reaped_and_the_key_is_writable_again() {
    let world = World::new().await;
    let hot = Arc::new(GateBackend::new(world.hot.clone()));
    let p1 = world.proxy_over(hot.clone(), world.config("p1")).await;
    let p2 = world.proxy("p2").await;
    p1.put(K, body("v1")).await.unwrap();
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);

    let gate = hot.gate(Op::PutIfMatch, K, Phase::Pre);
    let victim = Arc::clone(&p1);
    let _dead = vanish(&gate, async move {
        let _ = victim
            .put_plain(K, PutBody::Held(body("v2")), &PutClaim::default())
            .await;
    })
    .await;
    assert_eq!(world.shape(K).await, Some((MarkerState::Lock, false)));
    assert_eq!(served(&p2, K).await.as_deref(), Ok("v1"));

    let refused = p2
        .put_plain(K, PutBody::Held(body("v3")), &PutClaim::default())
        .await;
    assert!(matches!(refused, Err(Error::Unavailable(_))), "{refused:?}");
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v1"));

    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.shape(K).await, Some((MarkerState::Lock, false)));
    world.converge(&mut leader, K).await;
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v1"));

    p2.put_plain(K, PutBody::Held(body("v3")), &PutClaim::default())
        .await
        .unwrap();
    assert_eq!(served(&p2, K).await.as_deref(), Ok("v3"));
}

/// Asserts that a proxy killed in the middle of a delete, with `gone` written and the hot DELETE not
/// made, leaves a key that was not acknowledged as deleted: the object is read while the lock
/// stands, and after the leader reaped the lock the key reads the same from both proxies and the
/// stores agree after the copy.
#[tokio::test]
async fn a_proxy_killed_in_the_middle_of_a_delete_leaves_the_key_as_it_was_and_the_stores_agree() {
    let world = World::new().await;
    let hot = Arc::new(GateBackend::new(world.hot.clone()));
    let p1 = world.proxy_over(hot.clone(), world.config("p1")).await;
    let p2 = world.proxy("p2").await;
    p1.put(K, body("v1")).await.unwrap();
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);

    let gate = hot.gate(Op::Delete, K, Phase::Pre);
    let victim = Arc::clone(&p1);
    let _dead = vanish(&gate, async move {
        let _ = victim.delete(K).await;
    })
    .await;
    assert_eq!(world.shape(K).await, Some((MarkerState::Lock, true)));
    assert_eq!(served(&p2, K).await.as_deref(), Ok("v1"));

    world.converge(&mut leader, K).await;
    let seen = served(&p2, K).await;
    let p3 = world.proxy("p3").await;
    assert_eq!(seen, served(&p3, K).await);
    assert_eq!(seen.as_deref(), Ok("v1"));
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v1"));
    assert_eq!(world.marker(K).await, None);
}
