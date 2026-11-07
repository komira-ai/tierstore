//! The stray sweep: which hot objects are adopted, how often, and how it ends when a call fails.

use super::*;

/// Asserts that a hot object that is old enough and has no cold object and no entry is adopted
/// with a `(PUT, PENDING)` entry that the next step copies; that an object that is young, one that
/// the cold tier has, one that an entry holds, and the reserved objects are not adopted; that the
/// cold tier is asked about one object per hundredth of a second (10 ms) at the default rate of
/// a hundred a second (the waits are the sleeper's record); that the sweep runs once and then not again until
/// its interval has passed on the monotonic clock; and that listings of two objects at a time
/// miss none.
#[tokio::test]
async fn a_stray_is_adopted_once_old_enough_and_the_sweep_repeats_only_after_its_interval() {
    let rig = Rig::new();
    rig.activate().await;
    rig.register("p9").await;
    for key in ["stray", "copied", "held"] {
        rig.hot.put(key, bytes("v")).await.unwrap();
    }
    rig.cold.put("copied", bytes("v")).await.unwrap();
    rig.plant("held", EntryOp::Put, EntryState::Held, Some("p9"))
        .await;
    rig.clock.advance(600_001);
    rig.hot.put("young", bytes("v")).await.unwrap();
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", |config| {
        config.base.page_size = 2;
    });
    let first = r.step().await;
    assert_eq!(
        (first.strays_adopted, first.pinned, first.changed),
        (1, 1, 0)
    );
    assert!(first.progress());
    assert!(first.failed.is_empty());
    assert_eq!(r.strays_adopted(), 1);
    let pause = Duration::from_millis(10);
    assert_eq!(rig.waits.asked(), [pause; 3]);
    assert_eq!(
        rig.shape("stray").await,
        Some((EntryOp::Put, EntryState::Pending, None))
    );
    assert!(!rig.cold_has("young").await);
    assert!(rig.entry("young").await.is_none());

    assert_eq!(
        r.step().await,
        StepReport {
            pinned: 1,
            ..report(1)
        }
    );
    assert!(rig.cold_has("stray").await);
    assert!(rig.entry("stray").await.is_none());
    assert_eq!(rig.waits.asked().len(), 3);

    rig.mono.advance(Duration::from_secs(24 * 3600 - 1));
    r.step().await;
    assert_eq!(rig.waits.asked().len(), 3);
    rig.mono.advance(Duration::from_secs(1));
    let again = r.step().await;
    assert_eq!(again.strays_adopted, 0);
    assert_eq!(rig.waits.asked().len(), 6);
    assert_eq!(r.strays_adopted(), 1);
}

/// Asserts that a failed call ends the sweep with the failure and leaves it due: a listing of the
/// hot tier, a lookup in the cold tier. A failed create of an entry is a failure too, but the
/// sweep goes on and ends: it is not due again until its interval.
#[tokio::test]
async fn a_failed_call_ends_the_sweep_and_leaves_it_due_but_a_failed_create_does_not() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("stray", bytes("v")).await.unwrap();
    rig.clock.advance(600_001);
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let mut r = rig.over(hot.clone(), cold.clone(), "r1", |_| {});
    let unavailable = |what: &str| Error::Unavailable(what.to_owned());

    hot.fail_next_on(Op::List, "", unavailable("list"));
    let mut report = StepReport::default();
    r.sweep(0, &mut report).await;
    assert_eq!(report.failed, [unavailable("list")]);
    assert!(r.sweep_due(0));

    cold.fail_next_on(Op::Head, "stray", unavailable("head"));
    let mut report = StepReport::default();
    r.sweep(0, &mut report).await;
    assert_eq!(report.failed, [unavailable("head")]);
    assert!(r.sweep_due(0));

    hot.fail_next_on(Op::Put, ENTRIES_PREFIX, unavailable("create"));
    let mut report = StepReport::default();
    r.sweep(0, &mut report).await;
    assert_eq!(report.failed, [unavailable("create")]);
    assert_eq!(report.strays_adopted, 0);
    assert!(!r.sweep_due(0));
    assert!(rig.entry("stray").await.is_none());
}

/// Asserts that the pause between two lookups in the cold tier is `1 / stray_rate` seconds rounded
/// up to a whole microsecond: at 3 a second it is 333334 microseconds, not 333333.
#[tokio::test]
async fn the_pause_between_cold_lookups_is_rounded_up_to_a_whole_microsecond() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("stray", bytes("v")).await.unwrap();
    rig.clock.advance(600_001);
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", |config| {
        config.stray_rate = 3;
    });
    r.step().await;
    assert_eq!(rig.waits.asked(), [Duration::from_micros(333_334)]);
}

/// Asserts the defaults of the settings: the departure, the waits of the loop, the retry delays,
/// the comparison range, the stray grace, interval and rate.
#[test]
fn the_settings_default_to_the_values_the_design_names() {
    let config = Rig::new().config("r1");
    let secs = Duration::from_secs;
    assert_eq!(config.departure, secs(60));
    assert_eq!(config.held_ttl, secs(3600));
    assert_eq!((config.idle_min, config.idle_max), (secs(1), secs(10)));
    assert_eq!((config.retry_min, config.retry_max), (secs(1), secs(60)));
    assert_eq!(config.compare_range, 1 << 20);
    assert_eq!(config.stray_grace, secs(600));
    assert_eq!(config.stray_interval, secs(24 * 3600));
    assert_eq!(config.stray_rate, 100);
    assert_eq!(config.abandoned_upload_after, secs(0));
    assert_eq!(config.base.parallel, 16);
}
