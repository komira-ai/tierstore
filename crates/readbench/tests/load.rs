//! Many clients at once from one node, and the wait for their start time.

#[cfg(test)]
#[expect(dead_code, reason = "this test file uses part of the fake")]
mod support;
#[cfg(test)]
#[expect(dead_code, reason = "this test file uses part of the trees")]
mod tree;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use readbench::clock::{Clock, MachineClock, ManualClock};
use readbench::load::{LoadPlan, run, wait_until};
use readbench::record::Mode;
use readbench::replay::Knobs;
use support::Kind;
use tierstore_testkit::dir::TestDir;
use tree::{PACK_B, ROOT, ROOT2, Variant, fake, store};

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn knobs() -> Knobs {
    let mut knobs = Knobs::new(1);
    knobs.hash_threads = NonZeroUsize::new(1);
    knobs
}

fn plan(clients: u32, client_base: u32, mode: Mode) -> LoadPlan {
    LoadPlan {
        trees: vec![ROOT.to_owned(), ROOT2.to_owned()],
        clients,
        client_base,
        total_clients: 6,
        mode,
        start_at_wall_ms: None,
        keep: false,
        unverified: false,
    }
}

/// A clock whose wall time moves `step` milliseconds each time it is read.
#[derive(Debug)]
struct Ticking {
    wall: AtomicU64,
    step: u64,
}

impl Clock for Ticking {
    fn now_us(&self) -> u64 {
        0
    }

    fn wall_ms(&self) -> u64 {
        self.wall.fetch_add(self.step, Ordering::SeqCst)
    }
}

#[tokio::test]
async fn a_start_time_that_has_passed_does_not_wait() {
    let clock = ManualClock::new(1_000);
    wait_until(clock.as_ref(), 1_000).await;
    wait_until(clock.as_ref(), 5).await;
}

#[tokio::test]
async fn a_start_time_ahead_is_waited_for_by_looking_at_the_clock_again() {
    let clock = Ticking {
        wall: AtomicU64::new(95),
        step: 3,
    };
    wait_until(&clock, 100).await;
    // Read at 95, 98 and 101.
    assert_eq!(clock.wall.load(Ordering::SeqCst), 104);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clients_take_the_trees_in_turn_and_each_tree_is_prepared_once() {
    let fake = fake(Variant::Whole).await;
    let store = Arc::new(store(&fake).await);
    let dir = scratch("load-engine");
    let mut plan = plan(3, 0, Mode::Engine);
    plan.start_at_wall_ms = Some(0);
    let records = run(
        store,
        &plan,
        &knobs(),
        "run1",
        "node1",
        &dir.path().join("cache"),
        &dir.path().join("sidecars"),
        MachineClock::shared(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(
        records
            .iter()
            .map(|r| (r.client, r.tree.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, ROOT), (1, ROOT2), (2, ROOT)]
    );
    assert!(
        records
            .iter()
            .all(|r| r.mode == Mode::Engine && r.clients == 6)
    );
    assert!(records.iter().all(|r| r.run == "run1" && r.node == "node1"));
    // The budget divides the store by the clients of the whole run.
    assert!(records.iter().all(|r| r.config["clients"] == "6"));
    assert!(records.iter().all(|r| r.bytes_fetched >= r.bytes_needed));
    // Each tree's manifests were read once for the three clients (ROOT's two, ROOT2's one).
    assert_eq!(
        fake.filer(0)
            .path_count(&format!("/buckets/kbf-cas/cas/l/{ROOT}")),
        1
    );
    assert_eq!(
        fake.filer(0)
            .path_count(&format!("/buckets/kbf-cas/cas/l/{ROOT2}")),
        1
    );
    // The caches are removed with the run.
    for i in 0..3 {
        assert!(!dir.path().join(format!("cache/c{i}")).exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_numbers_its_clients_from_its_base_and_can_keep_their_caches() {
    let fake = fake(Variant::Whole).await;
    let store = Arc::new(store(&fake).await);
    let dir = scratch("load-base");
    let mut plan = plan(2, 5, Mode::Engine);
    plan.keep = true;
    let records = run(
        store,
        &plan,
        &knobs(),
        "run1",
        "node2",
        &dir.path().join("cache"),
        &dir.path().join("sidecars"),
        MachineClock::shared(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    // Clients 5 and 6 of the run: trees 5 % 2 = 1 and 6 % 2 = 0.
    assert_eq!(
        records
            .iter()
            .map(|r| (r.client, r.tree.as_str()))
            .collect::<Vec<_>>(),
        vec![(5, ROOT2), (6, ROOT)]
    );
    assert!(dir.path().join("cache/c5/packs-v2").is_dir());
    assert!(dir.path().join("cache/c6/packs-v2").is_dir());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shipped_clients_read_through_the_filer_and_leave_no_cache() {
    let fake = fake(Variant::Whole).await;
    let store = Arc::new(store(&fake).await);
    let dir = scratch("load-shipped");
    let mut knobs = knobs();
    knobs.fill_chunk = 1 << 20;
    let records = run(
        store,
        &plan(2, 0, Mode::Shipped),
        &knobs,
        "run1",
        "node1",
        &dir.path().join("cache"),
        &dir.path().join("sidecars"),
        MachineClock::shared(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|r| r.mode == Mode::Shipped));
    assert_eq!(records[0].gets["chunk"], 8);
    assert_eq!(records[1].gets["chunk"], 1);
    assert_eq!(fake.total(Kind::VolumeGet), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tree_whose_pack_the_filer_lacks_fails_the_load_before_any_client_starts() {
    let dir = scratch("load-fail");
    let lost = fake(Variant::Without(PACK_B)).await;
    let mut plan = plan(1, 0, Mode::Shipped);
    plan.trees = vec![ROOT.to_owned()];
    let err = run(
        Arc::new(store(&lost).await),
        &plan,
        &knobs(),
        "run1",
        "node1",
        &dir.path().join("cache"),
        &dir.path().join("sidecars"),
        MachineClock::shared(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("{PACK_B}: the filer has no such pack")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unverified_load_of_a_tree_whose_pack_the_filer_lacks_fails_before_any_client_starts() {
    let dir = scratch("load-unverified-fail");
    let lost = fake(Variant::Without(PACK_B)).await;
    let mut plan = plan(1, 0, Mode::Engine);
    plan.trees = vec![ROOT.to_owned()];
    plan.unverified = true;
    let err = run(
        Arc::new(store(&lost).await),
        &plan,
        &knobs(),
        "run1",
        "node1",
        &dir.path().join("cache"),
        &dir.path().join("sidecars"),
        MachineClock::shared(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("{PACK_B}: the filer has no such pack")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_whose_pack_went_missing_after_the_preparation_fails_naming_itself() {
    let dir = scratch("load-gone");
    let whole = fake(Variant::Whole).await;
    let mut shipped = plan(1, 3, Mode::Shipped);
    shipped.trees = vec![ROOT.to_owned()];
    run(
        Arc::new(store(&whole).await),
        &shipped,
        &knobs(),
        "run1",
        "node1",
        &dir.path().join("cache"),
        &dir.path().join("sidecars"),
        MachineClock::shared(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    // The sidecar is kept; the store that now has no pack B reads the tree's manifests but not the pack.
    let lost = fake(Variant::Without(PACK_B)).await;
    let err = run(
        Arc::new(store(&lost).await),
        &shipped,
        &knobs(),
        "run1",
        "node1",
        &dir.path().join("cache"),
        &dir.path().join("sidecars"),
        MachineClock::shared(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap_err();
    let text = err.to_string();
    assert!(
        text.starts_with(&format!("client: client 3 of tree {ROOT}: ")),
        "{text}"
    );
    assert!(text.ends_with("not found"), "{text}");
}

#[tokio::test]
async fn a_load_without_trees_or_clients_is_a_usage_error() {
    let fake = fake(Variant::Whole).await;
    let store = Arc::new(store(&fake).await);
    let dir = scratch("load-usage");
    for (trees, clients) in [(vec![], 1), (vec![ROOT.to_owned()], 0)] {
        let mut plan = plan(clients, 0, Mode::Engine);
        plan.trees = trees;
        let err = run(
            store.clone(),
            &plan,
            &knobs(),
            "r",
            "n",
            dir.path(),
            dir.path(),
            MachineClock::shared(),
            &tokio::runtime::Handle::current(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "a load needs at least one tree and one client"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tree_named_twice_is_prepared_once() {
    let fake = fake(Variant::Whole).await;
    let store = Arc::new(store(&fake).await);
    let dir = scratch("load-twice");
    let mut plan = plan(2, 0, Mode::Shipped);
    plan.trees = vec![ROOT2.to_owned(), ROOT2.to_owned()];
    let records = run(
        store,
        &plan,
        &knobs(),
        "run1",
        "node1",
        &dir.path().join("cache"),
        &dir.path().join("sidecars"),
        MachineClock::shared(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(
        fake.filer(0)
            .path_count(&format!("/buckets/kbf-cas/cas/l/{ROOT2}")),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unverified_load_prepares_each_tree_without_its_packs_or_a_sidecar() {
    let fake = fake(Variant::Whole).await;
    let store = Arc::new(store(&fake).await);
    let dir = scratch("load-unverified");
    let mut plan = plan(2, 0, Mode::Engine);
    plan.unverified = true;
    let records = run(
        store,
        &plan,
        &knobs(),
        "run1",
        "node1",
        &dir.path().join("cache"),
        &dir.path().join("sidecars"),
        MachineClock::shared(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|r| r.config["verified"] == "false"));
    assert!(records.iter().all(|r| r.config["sidecar_built"] == "false"));
    assert!(!dir.path().join("sidecars").exists());
    // Only the trees' manifests were read through the filer's object GET.
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 3);
}
