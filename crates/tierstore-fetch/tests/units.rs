//! What the small modules of `tierstore_fetch` prove on their own: the baselines the timeouts
//! are relative to, the timers (manual and tokio), the default configuration (the design's
//! starting values) and the counters' accessors.
//!
//! Catches: a quantile that is not the nearest rank, or one taken over samples that should have
//! been dropped; a size class above the last that does not clamp; a timeout that is not
//! `max(floor, factor x baseline)`, or that overflows; a manual sleep that completes before the
//! clock reaches its deadline, or does not complete after; a timer that does not follow or share
//! its clock; a tokio sleep that does not complete for a deadline already passed; a default
//! constant that drifts from the design; a counter accessor that reads the wrong slot.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use futures::FutureExt;
use tierstore_fetch::baseline::{Baselines, SAMPLES, timeout};
use tierstore_fetch::timer::{ManualTimer, Timer, TokioTimer};
use tierstore_fetch::{EngineConfig, GetKind, Stats, TimerKind};
use tierstore_fetch_api::clock::{Clock, ManualClock};

const MIB: u64 = 1 << 20;

/// With no sample every quantile is absent, in every size class.
#[test]
fn a_baseline_with_no_samples_has_no_quantile() {
    let b = Baselines::default();
    let all: Vec<Option<u64>> = (0..3)
        .flat_map(|c| [b.first_byte_p50(c), b.first_byte_p99(c), b.ms_per_mib(c)])
        .collect();
    assert_eq!(all, vec![None; 9]);
}

/// The p50 and p99 of the samples 1 to 100 are 50 and 99 (nearest rank), whatever the order they
/// arrived in.
#[test]
fn first_byte_quantiles_are_the_nearest_rank_of_the_samples() {
    let mut b = Baselines::default();
    for ms in (1..=100).rev() {
        b.first_byte(0, ms);
    }
    assert_eq!(
        (b.first_byte_p50(0), b.first_byte_p99(0)),
        (Some(50), Some(99))
    );
}

/// One sample is every quantile.
#[test]
fn a_single_sample_is_every_quantile() {
    let mut b = Baselines::default();
    b.first_byte(1, 42);
    b.network(1, 17);
    assert_eq!(
        (b.first_byte_p50(1), b.first_byte_p99(1), b.ms_per_mib(1)),
        (Some(42), Some(42), Some(17))
    );
}

/// Only the last `SAMPLES` samples count: after 200 samples the oldest 72 are gone.
#[test]
fn only_the_last_samples_are_kept() {
    let mut b = Baselines::default();
    for ms in 1..=200 {
        b.first_byte(0, ms);
        b.network(0, ms);
    }
    assert_eq!(SAMPLES, 128);
    assert_eq!(
        (b.first_byte_p50(0), b.first_byte_p99(0), b.ms_per_mib(0)),
        (Some(136), Some(199), Some(136))
    );
}

/// The network median of a class is the nearest-rank median of its samples.
#[test]
fn the_network_baseline_is_the_median_of_its_samples() {
    let mut b = Baselines::default();
    for ms in [30, 10, 20, 50, 40] {
        b.network(2, ms);
    }
    assert_eq!(b.ms_per_mib(2), Some(30));
}

/// The size classes are separate, and a sample of one signal does not show in the other.
#[test]
fn classes_and_signals_are_kept_apart() {
    let mut b = Baselines::default();
    b.first_byte(0, 7);
    b.network(1, 9);
    assert_eq!(
        (
            b.first_byte_p50(0),
            b.first_byte_p50(1),
            b.first_byte_p50(2),
            b.ms_per_mib(0),
            b.ms_per_mib(1),
            b.ms_per_mib(2),
        ),
        (Some(7), None, None, None, Some(9), None)
    );
}

/// A size class above the last shares the last class's samples.
#[test]
fn size_classes_above_two_clamp_to_the_last_class() {
    let mut b = Baselines::default();
    b.first_byte(2, 11);
    b.network(200, 13);
    assert_eq!(
        (
            b.first_byte_p50(3),
            b.first_byte_p99(255),
            b.ms_per_mib(2),
            b.first_byte_p50(1),
        ),
        (Some(11), Some(11), Some(13), None)
    );
}

/// A timeout is the larger of its floor and the factor times the baseline; with no baseline it
/// is the floor; a huge baseline saturates instead of wrapping.
#[test]
fn a_timeout_is_the_larger_of_the_floor_and_the_factor_times_the_baseline() {
    assert_eq!(
        [
            timeout(2_000, 8, None),
            timeout(2_000, 8, Some(10)),
            timeout(2_000, 8, Some(250)),
            timeout(2_000, 8, Some(300)),
            timeout(500, 0, Some(1_000)),
            timeout(500, 8, Some(u64::MAX)),
            timeout(500, 8, Some(u64::MAX / 8 + 1)),
        ],
        [2_000, 2_000, 2_000, 2_400, 500, u64::MAX, u64::MAX]
    );
}

/// A manual sleep stays pending until the clock reaches its deadline and completes when it does;
/// the clock counts it as a sleeper while it waits.
#[test]
fn a_manual_sleep_completes_only_when_the_clock_reaches_its_deadline() {
    let clock = ManualClock::new();
    let timer = ManualTimer::new(clock.clone());
    let mut sleep = timer.sleep_until(10);
    let before = (&mut sleep).now_or_never();
    let sleepers_waiting = clock.sleepers();
    clock.advance(9);
    let one_short = (&mut sleep).now_or_never();
    clock.advance(1);
    let at_deadline = (&mut sleep).now_or_never();
    assert_eq!(
        (
            before,
            sleepers_waiting,
            one_short,
            at_deadline,
            clock.sleepers()
        ),
        (None, 1, None, Some(()), 0)
    );
}

/// A manual sleep to a deadline that has already passed completes at once.
#[test]
fn a_manual_sleep_to_a_deadline_in_the_past_completes_on_first_poll() {
    let timer = ManualTimer::new(ManualClock::new());
    timer.clock().advance(50);
    assert_eq!(
        [
            timer.sleep_until(50).now_or_never(),
            timer.sleep_until(0).now_or_never()
        ],
        [Some(()), Some(())]
    );
}

/// The timer reads the time of its clock, and the clock it hands out is the one it was made over
/// (clones of the timer share it too).
#[test]
fn a_manual_timer_follows_and_shares_its_clock() {
    let clock = ManualClock::new();
    let timer = ManualTimer::new(clock.clone());
    let clone = timer.clone();
    let start = timer.now_ms();
    clock.advance(5);
    timer.clock().advance(7);
    assert_eq!(
        (start, timer.now_ms(), clone.now_ms(), clock.now_ms()),
        (0, 12, 12, 12)
    );
}

/// Two sleeps wake independently: advancing to the first deadline wakes only that one.
#[test]
fn advancing_the_clock_wakes_only_the_sleeps_that_are_due() {
    let timer = ManualTimer::default();
    let mut early = timer.sleep_until(5);
    let mut late = timer.sleep_until(20);
    assert!((&mut early).now_or_never().is_none() && (&mut late).now_or_never().is_none());
    timer.clock().advance(5);
    let first = ((&mut early).now_or_never(), (&mut late).now_or_never());
    timer.clock().advance(15);
    assert_eq!(
        (first, (&mut late).now_or_never()),
        ((Some(()), None), Some(()))
    );
}

/// The production timer's time never goes backward.
#[tokio::test]
async fn the_tokio_timer_time_does_not_go_backward() {
    let timer = TokioTimer::new();
    let a = timer.now_ms();
    let b = timer.now_ms();
    let c = timer.now_ms();
    assert_eq!(
        (a <= b, b <= c),
        (true, true),
        "now_ms is monotonic non-decreasing"
    );
}

/// A tokio sleep to a deadline already passed completes on its first poll.
#[tokio::test]
async fn a_tokio_sleep_to_a_deadline_already_passed_completes_on_first_poll() {
    let timer = TokioTimer::new();
    assert_eq!(timer.sleep_until(0).now_or_never(), Some(()));
}

/// A tokio sleep to a deadline already passed completes when awaited.
#[tokio::test]
async fn a_tokio_sleep_to_a_deadline_already_passed_completes_when_awaited() {
    TokioTimer::new().sleep_until(0).await;
}

/// A tokio sleep to a deadline a moment ahead completes when awaited.
#[tokio::test]
async fn a_tokio_sleep_to_a_deadline_ahead_completes_when_awaited() {
    let timer = TokioTimer::new();
    timer.sleep_until(timer.now_ms() + 1).await;
}

/// The default configuration holds the design's starting values (4.4 to 4.6).
#[test]
fn the_default_configuration_is_the_designs_starting_values() {
    let c = EngineConfig::default();
    assert_eq!(
        (
            c.small_pack_max,
            (c.first_piece_min_cells, c.first_piece_max_cells),
            c.ramp_first_cells,
            c.sub_range_max_cells,
            c.window_packs,
            c.max_attempts,
        ),
        (32 * MIB, (2, 4), 4, 4, 3, 3)
    );
    assert_eq!(
        (
            (c.stall_floor_ms, c.stall_factor),
            (c.first_byte_floor_ms, c.first_byte_factor),
            (c.hedge_floor_ms, c.hedge_factor),
        ),
        ((500, 8), (2_000, 8), (30, 2))
    );
    assert_eq!(
        (
            c.degraded_hedge_max,
            c.hedge_permits,
            c.stall_retry_window_ms,
            c.unavailable_wait_ms,
            c.bad_cell_ms,
        ),
        (4 * MIB, 1, 2_000, 600_000, 60_000)
    );
}

/// A configuration equals its clone and differs when one value does.
#[test]
fn a_configuration_compares_by_value() {
    let c = EngineConfig::default();
    let changed = EngineConfig {
        max_attempts: 4,
        ..c.clone()
    };
    assert_eq!((c.clone() == c, changed == c), (true, false));
}

/// Each kind of GET reads its own slot of the counters, and the total is their sum.
#[test]
fn gets_are_counted_by_kind_and_summed() {
    let s = Stats {
        gets: [1, 20, 300, 4_000],
        ..Stats::default()
    };
    assert_eq!(
        (
            s.gets(GetKind::GridPart),
            s.gets(GetKind::DemandSub),
            s.gets(GetKind::Retry),
            s.gets(GetKind::Hedge),
            s.total_gets(),
        ),
        (1, 20, 300, 4_000, 4_321)
    );
}

/// Each kind of timer reads its own slot of the armed timers; a new `Stats` has none armed and
/// no GET.
#[test]
fn armed_timers_are_read_by_kind() {
    let s = Stats {
        timers: [1, 2, 3, 4, 5, 6, 7, 8],
        ..Stats::default()
    };
    let kinds = [
        TimerKind::FirstByte,
        TimerKind::Stall,
        TimerKind::Hedge,
        TimerKind::Backoff,
        TimerKind::Locate,
        TimerKind::BadCell,
        TimerKind::LostTtl,
        TimerKind::EvictPoll,
    ];
    let armed: Vec<u32> = kinds.iter().map(|k| s.armed(*k)).collect();
    let fresh = Stats::default();
    assert_eq!(
        (
            armed,
            kinds.iter().map(|k| fresh.armed(*k)).sum::<u32>(),
            fresh.total_gets()
        ),
        (vec![1, 2, 3, 4, 5, 6, 7, 8], 0, 0)
    );
}
