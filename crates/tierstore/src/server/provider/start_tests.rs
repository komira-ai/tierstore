//! `tierstore_request_to_start_seconds` across the scheduler's request and the mount: the first
//! `Prepare` of a run stamps it, a release or a sweep forgets it, and the first look of the mount
//! into one of the run's trees ends its span, end to end through a core of `tierstore fs` over the
//! real tree store and fill socket, on the cache's clock, which only the test moves.

use std::sync::Arc;

use super::Provider;
use super::testing::{AGENT, READ_ONLY, Rig, put_pack_list, settings, source};
use crate::server::local_cache::testing::until;
use crate::server::treefs::live_testing::{Live, walk};

/// The names from the mount's root to the tree of the source `index` of `run`, then `inside`.
fn names(run: &str, index: u32, inside: &[&str]) -> Vec<String> {
    let place = Rig::place_of(run, index);
    let names = place.split('/').chain(inside.iter().copied());
    names.map(str::to_owned).collect()
}

/// Looks up every name of `names` on the core, from the mount's root.
fn walked(core: &crate::server::treefs::core::Core, names: &[String]) {
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let _ = walk(core, &names);
}

/// The lines of the family in the cache's `/metricz` text.
fn family(text: &str) -> Vec<&str> {
    let lines = text.lines();
    lines
        .filter(|line| line.contains("tierstore_request_to_start_seconds"))
        .collect()
}

/// Asserts that the first `Prepare` of a run stamps it and the others leave it as it is (a second
/// source, the same source again), that a refused `Prepare` stamps nothing, and that a release of
/// a source of the run, or a sweep of it, forgets the run.
#[tokio::test]
async fn the_first_prepare_of_a_run_stamps_it_and_a_release_or_a_sweep_forgets_it() {
    let rig = Rig::new("prov-start-stamp").await;
    let cache = &rig.bench.rig.cache;
    let provider = &rig.provider;
    provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    provider
        .prepare(source("js.1", 1), &rig.key, READ_ONLY)
        .unwrap();
    provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    assert_eq!(cache.runs_kept(), 1);
    let refused = provider.prepare(source("js.2", 0), "cas/l/not-a-digest", READ_ONLY);
    assert!(refused.is_err());
    assert_eq!(cache.runs_kept(), 1);
    provider.release(source("js.1", 1)).unwrap();
    assert_eq!(cache.runs_kept(), 0);
    // A release of the run's last source, which forgets nothing more, is a success as before.
    provider.release(source("js.1", 0)).unwrap();
    provider
        .prepare(source("js.3", 0), &rig.key, READ_ONLY)
        .unwrap();
    assert_eq!(cache.runs_kept(), 1);
    assert_eq!(provider.sweep(AGENT, &[]), Ok(1));
    assert_eq!(cache.runs_kept(), 0);
}

/// Asserts that `tierstore_request_to_start_seconds` spans the scheduler and the mount: from the
/// first `Prepare` of a run, which the agent sends the moment it has the job, to the first lookup
/// of `tierstore fs` inside one of the run's trees. The run `js.1` has two sources, prepared
/// 40 ms apart, and `js.2` one, prepared with the second; every tree is mounted, and its root
/// looked up as the agent and the overlay's mount do, which is no start. At 200 ms a lookup in
/// `js.1`'s first tree samples 0.2 s (in the bucket of 0.2 and not that of 0.1); at 500 ms a
/// lookup in `js.1`'s second tree adds nothing (the run is sampled) and a lookup in `js.2`'s
/// tree samples 0.46 s. The mount's reports leave on one thread in order, so once the second
/// sample is in, the report of `js.1`'s second tree was answered before it.
#[tokio::test(flavor = "multi_thread")]
async fn request_to_start_seconds_spans_the_prepare_of_the_scheduler_and_the_first_look_of_the_mount()
 {
    let live = Live::new("prov-start-span").await;
    let cache = live.bench.rig.cache.clone();
    let provider = Provider::with(live.bench.store.clone(), cache.clone(), settings());
    let key = put_pack_list(&live.bench).await;
    let clock = Arc::clone(&live.bench.rig.clock);
    clock.set(1_000_000);
    provider
        .prepare(source("js.1", 0), &key, READ_ONLY)
        .unwrap();
    clock.advance(40);
    provider
        .prepare(source("js.1", 1), &key, READ_ONLY)
        .unwrap();
    provider
        .prepare(source("js.2", 0), &key, READ_ONLY)
        .unwrap();
    let sources = [("js.1", 0), ("js.1", 1), ("js.2", 0)];
    for (run, index) in sources {
        provider.mount(source(run, index)).await.unwrap();
    }
    live.on_core(move |core| {
        for (run, index) in sources {
            walked(core, &names(run, index, &[]));
        }
    })
    .await;
    assert_eq!(cache.runs_kept(), 2);
    clock.advance(160);
    live.on_core(|core| walked(core, &names("js.1", 0, &["bin", "tool"])))
        .await;
    until("the span of js.1 is sampled", || {
        cache
            .metrics()
            .contains("tierstore_request_to_start_seconds_count 1\n")
    })
    .await;
    clock.advance(300);
    live.on_core(|core| {
        walked(core, &names("js.1", 1, &["lib"]));
        walked(core, &names("js.2", 0, &["bin"]));
    })
    .await;
    until("the span of js.2 is sampled", || {
        cache
            .metrics()
            .contains("tierstore_request_to_start_seconds_count 2\n")
    })
    .await;
    let text = cache.metrics();
    let mut want = vec!["# TYPE tierstore_request_to_start_seconds histogram".to_owned()];
    let le = [
        "0.005", "0.01", "0.025", "0.05", "0.1", "0.2", "0.5", "1", "2.5", "5", "10", "30", "60",
        "120", "300", "900",
    ];
    // 0.2 s is in the bucket of 0.2 (the sixth) and above; 0.46 s in that of 0.5 and above.
    let below = [0, 0, 0, 0, 0, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2];
    for (le, below) in le.iter().zip(below) {
        want.push(format!(
            "tierstore_request_to_start_seconds_bucket{{le=\"{le}\"}} {below}"
        ));
    }
    want.extend([
        "tierstore_request_to_start_seconds_bucket{le=\"+Inf\"} 2".to_owned(),
        "tierstore_request_to_start_seconds_sum 0.66".to_owned(),
        "tierstore_request_to_start_seconds_count 2".to_owned(),
    ]);
    assert_eq!(family(&text), want);
}
