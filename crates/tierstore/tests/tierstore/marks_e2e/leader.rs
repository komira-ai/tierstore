//! The leader: the copy to the cold tier, the eviction, the copy back, and the whole way of a key
//! from the hot tier to the cold one and back.

use std::time::Duration;

use tierstore::{
    CopyBackConfig, CopyBacker, CopyRecords, EvictConfig, GateBackend, LeaderLease, MarkEvictor,
    Phase,
};

use super::*;

/// The key of a test.
const K: &str = "builds/k";

/// The eviction and the copy-back of the leader `r1`, which holds the lease.
struct Passes {
    evictor: MarkEvictor,
    backer: CopyBacker,
    markers: Markers,
    lease: LeaderLease,
}

impl World {
    /// The eviction and the copy-back of the leader `r1`, over the three tiers of the world.
    async fn passes(&self) -> Passes {
        self.passes_over(self.hot.clone()).await
    }

    /// As [`World::passes`] over `hot`, a wrapper of the hot tier of the world.
    async fn passes_over(&self, hot: Arc<dyn Backend>) -> Passes {
        let mut base = ReconcilerConfig::new("r1", 1 << 30, self.clock.clone());
        base.mono = self.mono.clone();
        base.sleeper = Arc::new(Never);
        let lease = LeaderLease::new(self.hot.clone(), &base);
        assert!(lease.take().await, "the lease is free");
        let evictor = MarkEvictor::new(
            hot,
            self.cold.clone(),
            self.markers("r1"),
            lease.clone(),
            Arc::new(CopyRecords::default()),
            self.mono.clone(),
            EvictConfig::default(),
        );
        let backer = CopyBacker::new(
            self.hot.clone(),
            self.cold.clone(),
            self.markers("r1"),
            lease.clone(),
            self.mono.clone(),
            CopyBackConfig::default(),
        );
        Passes {
            evictor,
            backer,
            markers: self.markers("r1"),
            lease,
        }
    }
}

/// Steps `leader` until it holds the lease, moving the clocks past the time another holder's lease
/// takes to be taken over, and gives the step that held it.
async fn work(world: &World, leader: &mut EntryReconciler) -> tierstore::StepReport {
    for _ in 0..4 {
        let step = leader.step().await;
        if step.held {
            return step;
        }
        world.pass(Duration::from_secs(31));
    }
    panic!("the leader never held the lease");
}

/// A wait that never ends.
struct Never;

#[async_trait]
impl Sleeper for Never {
    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await;
    }
}

/// Asserts what the leader's pass asks of the stores to copy a new key, for that key: the marker
/// read before and after the cold HEAD, the hot object streamed once to a create-only cold PUT, and
/// the two writes that end the marker (the compare-and-swap to `CLEARING` and the DELETE). The hot
/// object is left, and the cold object has the bytes.
#[tokio::test]
async fn the_leader_copies_a_new_key_to_cold_by_one_streamed_write_and_clears_its_marker() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    let mut leader = world.leader("r1");
    let mut seen = world.cursor();
    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(step.changed, 1);
    let of_key: Vec<_> = world
        .since(&mut seen)
        .into_iter()
        .filter(|(_, _, key)| key == K)
        .collect();
    assert_eq!(
        of_key,
        [
            req(Store::Hot, Op::Head, K),
            req(Store::Hot, Op::Get, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Marks, Op::PutIfMatch, K),
            req(Store::Marks, Op::Delete, K),
            req(Store::Cold, Op::Head, K),
            req(Store::Cold, Op::Put, K),
        ]
    );
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v1"));
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v1"));
    assert_eq!(world.marker(K).await, None);
}

/// Asserts that a key overwritten after its copy is copied again over the cold object by its
/// tag, that cold is never emptied first (no cold DELETE), and that the marker is cleared.
#[tokio::test]
async fn the_leader_replaces_the_cold_copy_of_an_overwritten_key_by_its_tag() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);
    proxy
        .put_plain(K, tierstore::PutBody::Held(body("v2")), &Default::default())
        .await
        .unwrap();
    released(&world, K).await;
    let deletes = world.cold.calls(Op::Delete);
    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v2"));
    assert_eq!(world.cold.calls(Op::Delete), deletes);
    assert_eq!(world.cold.calls(Op::PutIfMatch), 1);
    assert_eq!(world.marker(K).await, None);
}

/// Asserts that the leader clears the marker of a put that left no object behind only after
/// `orphan_after` has passed on its own clock since it first saw the marker, and not at the first
/// pass: a put in flight is not cleared from under itself.
#[tokio::test]
async fn the_leader_clears_a_marker_with_no_object_only_after_orphan_after() {
    let world = World::new().await;
    world.proxy("p1").await;
    let markers = world.markers("p9");
    markers
        .create("builds/orphan", tierstore::MarkBirth::Dirty)
        .await
        .unwrap();
    let mut leader = world.leader("r1");
    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(
        world.shape("builds/orphan").await,
        Some((MarkerState::Dirty, false))
    );
    world.pass(Duration::from_secs(121));
    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.marker("builds/orphan").await, None);
}

/// Asserts that a hot object whose copy to cold the leader has not made is never evicted, however
/// old: the eviction takes a lock create-only and any marker at all refuses it.
#[tokio::test]
async fn an_object_the_leader_has_not_copied_is_never_evicted() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    let passes = world.passes().await;
    world.pass(Duration::from_secs(3600));
    let report = passes.evictor.evict_pass(vec![K.to_owned()], 1).await;
    assert_eq!((report.active, report.evicted, report.kept), (true, 0, 1));
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v1"));
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
}

/// Asserts that a key overwritten after its copy is not evicted until the leader has copied it
/// again: its marker is `DIRTY`, so the eviction is refused, and the hot object keeps the newer bytes.
#[tokio::test]
async fn an_object_overwritten_after_its_copy_is_not_evicted_before_it_is_copied_again() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);
    proxy
        .put_plain(K, tierstore::PutBody::Held(body("v2")), &Default::default())
        .await
        .unwrap();
    released(&world, K).await;
    leader.stop().await;
    let passes = world.passes().await;
    world.pass(Duration::from_secs(3600));
    let report = passes.evictor.evict_pass(vec![K.to_owned()], 1).await;
    assert_eq!((report.evicted, report.kept), (0, 1));
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v2"));
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v1"));
}

/// Asserts that a copied key is not evicted while its cold copy is younger than `evict_min_age`,
/// and is, once it is old enough: the hot object is deleted, the marker is gone, the freed bytes
/// are reported, and a read is served by the cold tier with the bytes that were put.
#[tokio::test]
async fn a_copied_object_is_evicted_when_its_cold_copy_is_old_enough_and_is_then_read_from_cold() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);
    leader.stop().await;
    let passes = world.passes().await;

    let report = passes.evictor.evict_pass(vec![K.to_owned()], 1).await;
    assert_eq!((report.evicted, report.young), (0, 1));
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v1"));

    world.pass(Duration::from_secs(601));
    assert!(
        passes.lease.take().await,
        "the leader's step takes the lease before it works"
    );
    let report = passes.evictor.evict_pass(vec![K.to_owned()], 1).await;
    assert_eq!((report.evicted, report.freed), (1, 2), "{report:?}");
    assert_eq!(world.hot_text(K).await, None);
    assert_eq!(world.marker(K).await, None);
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("v1"));
}

/// Asserts that a hot object whose cold copy has other bytes is not evicted but marked `DIRTY` for
/// the copier, so that the newer bytes are never lost: here the hot object was put in behind the
/// leader's back with no marker.
#[tokio::test]
async fn an_object_that_differs_from_its_cold_copy_is_marked_dirty_and_kept() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);
    leader.stop().await;
    world.hot_mem.delete(K).await.unwrap();
    world.hot_mem.put(K, body("v2")).await.unwrap();
    let passes = world.passes().await;
    world.pass(Duration::from_secs(601));
    let report = passes.evictor.evict_pass(vec![K.to_owned()], 1).await;
    assert_eq!((report.evicted, report.dirtied), (0, 1), "{report:?}");
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v2"));
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));

    let mut next = world.leader("r2");
    let step = work(&world, &mut next).await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v2"));
}

/// Asserts the way of a key in both directions, with a read after every step that finds the same
/// bytes: put, copied, evicted from hot, read from cold (which asks for the copy back), copied
/// back by the leader into hot, read from hot again with a single hot request.
#[tokio::test]
async fn a_key_goes_from_hot_to_cold_and_back_and_every_read_on_the_way_finds_its_bytes() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("v1"));
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);
    leader.stop().await;
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("v1"));

    let mut passes = world.passes().await;
    world.pass(Duration::from_secs(601));
    assert!(
        passes.lease.take().await,
        "the leader's step takes the lease before it works"
    );
    let report = passes.evictor.evict_pass(vec![K.to_owned()], 1).await;
    assert_eq!(report.evicted, 1, "{report:?}");
    assert_eq!(world.hot_text(K).await, None);

    assert_eq!(served(&proxy, K).await.as_deref(), Ok("v1"));
    marked(&world, K).await;
    assert_eq!(world.shape(K).await, Some((MarkerState::Want, false)));
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("v1"));

    let wants = vec![passes.markers.read(K).await.unwrap().unwrap()];
    let report = passes.backer.pass(wants, 1).await;
    assert_eq!((report.landed, report.failed.len()), (1, 0), "{report:?}");
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v1"));
    assert_eq!(world.marker(K).await, None);

    let mut seen = world.cursor();
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("v1"));
    assert_eq!(world.since(&mut seen), [req(Store::Hot, Op::Get, K)]);
}

/// Asserts that a key deleted while a read of it asked for the copy back is not brought back: the
/// delete takes the `WANT` marker over, the copy-back finds the marker is no `WANT` any more and
/// writes nothing into hot, and the key stays absent.
#[tokio::test]
async fn a_delete_after_the_ask_for_a_copy_back_stops_the_copy_back_and_the_key_stays_absent() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    world.cold_mem.put(K, body("cold")).await.unwrap();
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("cold"));
    marked(&world, K).await;
    let mut passes = world.passes().await;
    let wants = vec![passes.markers.read(K).await.unwrap().unwrap()];

    proxy.delete(K).await.unwrap();
    released(&world, K).await;
    let report = passes.backer.pass(wants, 1).await;
    assert_eq!((report.landed, report.lost), (0, 1), "{report:?}");
    assert_eq!(world.hot_text(K).await, None);
    assert_eq!(served(&proxy, K).await, Err(Error::NotFound(K.to_owned())));
}

/// Asserts that an overwrite that arrives while the leader holds the lock of an eviction (its hot
/// DELETE not yet made) waits for it, and that the new bytes are the key's afterwards: the eviction
/// deletes the old hot object, the overwrite finds the key gone from hot, creates it anew, and the
/// leader copies the new bytes over the old cold copy. The acknowledged write is not lost.
#[tokio::test]
async fn an_overwrite_that_meets_the_lock_of_an_eviction_waits_and_its_bytes_survive() {
    let world = World::new().await;
    let (config, waits) = world.config_counting("p1");
    let proxy = world.proxy_with(config).await;
    proxy.put(K, body("old")).await.unwrap();
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);
    leader.stop().await;
    let held = Arc::new(GateBackend::new(world.hot.clone()));
    let gate = held.gate(Op::DeleteIfMatch, K, Phase::Pre);
    let passes = world.passes_over(held).await;
    world.pass(Duration::from_secs(601));
    assert!(passes.lease.take().await);
    let evicting =
        tokio::spawn(async move { passes.evictor.evict_pass(vec![K.to_owned()], 1).await });
    gate.parked().await;
    assert_eq!(world.shape(K).await, Some((MarkerState::Lock, false)));

    let writer = tokio::spawn({
        let proxy = Arc::clone(&proxy);
        async move {
            let claim = tierstore::PutClaim::default();
            proxy
                .put_plain(K, tierstore::PutBody::Held(body("new")), &claim)
                .await
        }
    });
    waiting(&waits).await;
    assert!(gate.release());
    let report = evicting.await.unwrap();
    assert_eq!(report.evicted, 1, "{report:?}");
    writer.await.unwrap().unwrap();
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("new"));
    released(&world, K).await;
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));

    let mut next = world.leader("r2");
    let step = work(&world, &mut next).await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.cold_text(K).await.as_deref(), Some("new"));
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("new"));
}

/// Asserts that a put that lands while the leader is copying the old bytes is not lost: the copy
/// writes the old bytes to cold, but its compare-and-swap that would clear the marker fails because
/// the put changed the marker, so the marker stands `DIRTY` and the next pass copies the new
/// bytes over.
#[tokio::test]
async fn an_overwrite_during_the_copy_leaves_the_marker_and_the_next_pass_copies_the_new_bytes() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("old")).await.unwrap();
    let held = Arc::new(GateBackend::new(world.cold.clone()));
    let gate = held.gate(Op::Put, K, Phase::Post);
    let mut base = ReconcilerConfig::new("r1", 1 << 30, world.clock.clone());
    base.mono = world.mono.clone();
    base.sleeper = Arc::new(Never);
    let config = EntryReconcilerConfig::new(base);
    let mut leader = EntryReconciler::new(world.hot.clone(), held, config)
        .with_marks(world.marks.clone(), BUCKET)
        .unwrap();
    let mut copying = Box::pin(leader.step());
    tokio::select! {
        biased;
        _ = &mut copying => panic!("the copy ended instead of parking"),
        () = gate.parked() => {}
    }
    assert_eq!(world.cold_text(K).await.as_deref(), Some("old"));

    let claim = tierstore::PutClaim::default();
    let put = proxy.put_plain(K, tierstore::PutBody::Held(body("newer")), &claim);
    put.await.unwrap();
    released(&world, K).await;
    assert!(gate.release());
    let step = copying.await;
    assert_eq!(step.changed, 0, "{step:?}");
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("newer"));

    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.cold_text(K).await.as_deref(), Some("newer"));
    assert_eq!(world.marker(K).await, None);
}
