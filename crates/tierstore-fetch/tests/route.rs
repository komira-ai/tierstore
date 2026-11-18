//! What `tierstore_fetch::route` proves: which source of a location serves a part (the first
//! choice), where a retry goes (the alternative) and when a demand part may be hedged (design
//! 4.6 and 10.6), and what `DeviceBook` remembers of the devices.
//!
//! Catches: an unhealthy copy chosen over a healthy one; a state (`up`, `drain`, unknown) treated
//! as unhealthy or a decrease state treated as healthy; tiers out of order (promoted copy, hot,
//! cold, degraded); the queue wait or the location order ignored among equals; a
//! source chosen for a byte its extent does not cover; an excluded source chosen; a retry sent
//! to the failed device while another exists, or no retry target when only one source exists; a
//! hedge during a decrease state, to a busier or unhealthy copy, to a cold source, to
//! the same device, of a hot read to a degraded read, of a long range to a degraded read, or to
//! a degraded read whose source drives are not known to be idle; signals that overwrite what an
//! answer left out; a busy deadline that moves backward.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::collections::BTreeSet;

use support::{MIB, cold, degraded, drive, generation, hot, hot_clean, location};
use tierstore_fetch::route::{DeviceBook, DeviceInfo, Routing};
use tierstore_fetch_api::ids::ByteRange;
use tierstore_fetch_api::upstream::{DeviceRef, DriveSignals, DriveState, Location, Source};
use tierstore_format::ids::NodeId;

const LEN: u64 = 64 * MIB;
const DEGRADED_MAX: u64 = 4 * MIB;

fn hot_dev(node: u8) -> DeviceRef {
    DeviceRef::Hot(NodeId(node))
}

fn loc(sources: Vec<Source>) -> Location {
    location(generation(1), LEN, sources)
}

fn wait(ms: u32) -> DriveSignals {
    DriveSignals {
        queue_wait_ms: Some(ms),
        ..DriveSignals::default()
    }
}

fn state(s: DriveState) -> DriveSignals {
    DriveSignals {
        drive_state: Some(s),
        ..DriveSignals::default()
    }
}

fn queue(q: u32) -> DriveSignals {
    DriveSignals {
        drive_queue: Some(q),
        ..DriveSignals::default()
    }
}

fn first(
    l: &Location,
    excluded: &[usize],
    book: &DeviceBook,
    now: u64,
    offset: u64,
) -> Option<usize> {
    let excluded: BTreeSet<usize> = excluded.iter().copied().collect();
    Routing {
        location: l,
        excluded: &excluded,
        book,
        now,
    }
    .first(offset)
}

fn alternative(
    l: &Location,
    excluded: &[usize],
    book: &DeviceBook,
    offset: u64,
    failed: DeviceRef,
) -> Option<usize> {
    let excluded: BTreeSet<usize> = excluded.iter().copied().collect();
    Routing {
        location: l,
        excluded: &excluded,
        book,
        now: 0,
    }
    .alternative(offset, failed)
}

fn hedge(
    l: &Location,
    excluded: &[usize],
    book: &DeviceBook,
    now: u64,
    len: u64,
    current: usize,
) -> Option<usize> {
    let excluded: BTreeSet<usize> = excluded.iter().copied().collect();
    Routing {
        location: l,
        excluded: &excluded,
        book,
        now,
    }
    .hedge(0, len, &l.sources[current], DEGRADED_MAX)
}

/// A copy on a stalled, failed, down, suspect, out or indexing device loses to a healthy copy
/// even when it is first in the location.
#[test]
fn a_copy_on_a_device_in_a_non_serving_state_loses_to_a_healthy_copy() {
    let l = loc(vec![hot(1, LEN), hot(2, LEN)]);
    let states = [
        DriveState::Stalled,
        DriveState::Failed,
        DriveState::Down,
        DriveState::Suspect,
        DriveState::Out,
        DriveState::Indexing,
    ];
    let chosen: Vec<Option<usize>> = states
        .iter()
        .map(|s| {
            let mut book = DeviceBook::default();
            book.signals(hot_dev(1), state(*s));
            first(&l, &[], &book, 0, 0)
        })
        .collect();
    assert_eq!(chosen, vec![Some(1); 6]);
}

/// A device that is `up` or `drain`, or one nothing is known of, keeps its place: the first copy
/// of the location is chosen.
#[test]
fn up_and_drain_and_unknown_devices_are_healthy() {
    let l = loc(vec![hot(1, LEN), hot(2, LEN)]);
    let mut chosen: Vec<Option<usize>> = [DriveState::Up, DriveState::Drain]
        .iter()
        .map(|s| {
            let mut book = DeviceBook::default();
            book.signals(hot_dev(1), state(*s));
            first(&l, &[], &book, 0, 0)
        })
        .collect();
    chosen.push(first(&l, &[], &DeviceBook::default(), 0, 0));
    assert_eq!(chosen, vec![Some(0); 3]);
}

/// A device that answered `503` is unhealthy until its busy deadline; at the deadline it is
/// healthy again.
#[test]
fn a_device_that_answered_503_is_skipped_until_its_busy_deadline() {
    let l = loc(vec![hot(1, LEN), hot(2, LEN)]);
    let mut book = DeviceBook::default();
    book.busy(hot_dev(1), 100);
    let chosen = [
        first(&l, &[], &book, 0, 0),
        first(&l, &[], &book, 99, 0),
        first(&l, &[], &book, 100, 0),
    ];
    assert_eq!(chosen, [Some(1), Some(1), Some(0)]);
}

/// Health outranks the tier: a promoted copy on a stalled device loses to a healthy cold copy.
#[test]
fn health_outranks_the_tier() {
    let l = loc(vec![hot_clean(1, LEN), cold(2, 1, 0, LEN)]);
    let mut book = DeviceBook::default();
    book.signals(hot_dev(1), state(DriveState::Stalled));
    assert_eq!(first(&l, &[], &book, 0, 0), Some(1));
}

/// Among healthy copies the promoted copy comes before hot, hot before cold, cold before
/// degraded; each excluded in turn leaves the next.
#[test]
fn the_tiers_are_promoted_then_hot_then_cold_then_degraded() {
    let l = loc(vec![
        degraded(5, &[drive(1, 1), drive(2, 1)], 0, LEN),
        cold(2, 1, 0, LEN),
        hot(3, LEN),
        hot_clean(4, LEN),
    ]);
    let book = DeviceBook::default();
    let chosen = [
        first(&l, &[], &book, 0, 0),
        first(&l, &[3], &book, 0, 0),
        first(&l, &[3, 2], &book, 0, 0),
        first(&l, &[3, 2, 1], &book, 0, 0),
        first(&l, &[3, 2, 1, 0], &book, 0, 0),
    ];
    assert_eq!(chosen, [Some(3), Some(2), Some(1), Some(0), None]);
}

/// A cold shard on a stalled drive goes to the degraded source (the reconstructor).
#[test]
fn a_cold_shard_on_a_stalled_drive_goes_to_the_degraded_source() {
    let l = loc(vec![
        cold(1, 1, 0, LEN),
        degraded(5, &[drive(2, 1), drive(3, 1)], 0, LEN),
    ]);
    let mut book = DeviceBook::default();
    book.signals(drive(1, 1), state(DriveState::Stalled));
    assert_eq!(first(&l, &[], &book, 0, 0), Some(1));
}

/// Among equals the lowest advertised queue wait wins, a device that advertised none counts as
/// idle, and equal waits fall back to the location's order.
#[test]
fn among_equals_the_lowest_queue_wait_wins_then_the_location_order() {
    let l = loc(vec![hot(1, LEN), hot(2, LEN), hot(3, LEN)]);
    let mut book = DeviceBook::default();
    book.signals(hot_dev(1), wait(30));
    book.signals(hot_dev(2), wait(10));
    book.signals(hot_dev(3), wait(10));
    let mut unknown = DeviceBook::default();
    unknown.signals(hot_dev(1), wait(5));
    assert_eq!(
        [first(&l, &[], &book, 0, 0), first(&l, &[], &unknown, 0, 0)],
        [Some(1), Some(1)]
    );
}

/// The tier outranks the queue wait: an idle cold copy does not beat a busy hot copy.
#[test]
fn the_tier_outranks_the_queue_wait() {
    let l = loc(vec![cold(1, 1, 0, LEN), hot(2, LEN)]);
    let mut book = DeviceBook::default();
    book.signals(hot_dev(2), wait(500));
    assert_eq!(first(&l, &[], &book, 0, 0), Some(1));
}

/// A source whose extent does not hold the offset is skipped (the sources of a chain object, one
/// per block); an offset no extent holds has no source; an extent that runs to the end of the
/// address space holds every later offset.
#[test]
fn a_source_is_chosen_only_for_offsets_its_extent_covers() {
    let book = DeviceBook::default();
    let chain = loc(vec![cold(1, 1, 0, 100), cold(1, 2, 100, 100)]);
    let chosen = [
        first(&chain, &[], &book, 0, 0),
        first(&chain, &[], &book, 0, 99),
        first(&chain, &[], &book, 0, 100),
        first(&chain, &[], &book, 0, 199),
        first(&chain, &[], &book, 0, 200),
    ];
    assert_eq!(chosen, [Some(0), Some(0), Some(1), Some(1), None]);

    let open_ended = loc(vec![Source {
        extent: ByteRange {
            offset: 10,
            len: u64::MAX,
        },
        ..hot(1, 0)
    }]);
    assert_eq!(
        [
            first(&open_ended, &[], &book, 0, 9),
            first(&open_ended, &[], &book, 0, 10),
            first(&open_ended, &[], &book, 0, u64::MAX - 1),
        ],
        [None, Some(0), Some(0)]
    );
}

/// A location without sources, and one whose sources are all excluded, have no first choice.
#[test]
fn no_source_is_chosen_from_an_empty_or_fully_excluded_location() {
    let book = DeviceBook::default();
    let two = loc(vec![hot(1, LEN), hot(2, LEN)]);
    assert_eq!(
        [
            first(&loc(vec![]), &[], &book, 0, 0),
            first(&two, &[0, 1], &book, 0, 0)
        ],
        [None, None]
    );
}

/// An excluded copy is never chosen even when it is the best one.
#[test]
fn an_excluded_copy_is_skipped_even_when_it_is_the_best() {
    let l = loc(vec![hot_clean(1, LEN), hot(2, LEN)]);
    assert_eq!(first(&l, &[0], &DeviceBook::default(), 0, 0), Some(1));
}

/// A retry goes to a source on another device, by the same order as the first choice.
#[test]
fn a_retry_goes_to_the_best_source_on_another_device() {
    let l = loc(vec![hot(1, LEN), cold(2, 1, 0, LEN), hot(3, LEN)]);
    let book = DeviceBook::default();
    assert_eq!(alternative(&l, &[], &book, 0, hot_dev(1)), Some(2));
}

/// With no other device to go to, a retry takes the first choice again: the failed device on a
/// fresh connection.
#[test]
fn a_retry_with_no_other_device_goes_to_the_same_source_again() {
    let l = loc(vec![hot(1, LEN)]);
    let book = DeviceBook::default();
    assert_eq!(alternative(&l, &[], &book, 0, hot_dev(1)), Some(0));
}

/// A retry does not pick an excluded source on another device, nor one that does not cover the
/// offset: it falls back to the failed device's own source.
#[test]
fn a_retry_skips_excluded_and_non_covering_sources_of_other_devices() {
    let book = DeviceBook::default();
    let excluded = loc(vec![hot(1, LEN), hot(2, LEN)]);
    let chain = loc(vec![cold(1, 1, 0, 100), cold(2, 2, 100, 100)]);
    assert_eq!(
        [
            alternative(&excluded, &[1], &book, 0, hot_dev(1)),
            alternative(&chain, &[], &book, 0, drive(1, 1)),
        ],
        [Some(0), Some(0)]
    );
}

/// A retry with nothing left to read from has no target.
#[test]
fn a_retry_with_every_source_excluded_has_no_target() {
    let l = loc(vec![hot(1, LEN)]);
    assert_eq!(
        alternative(&l, &[0], &DeviceBook::default(), 0, hot_dev(1)),
        None
    );
}

/// No hedge is made while the current device is in a decrease state; at the busy deadline the
/// hedge is allowed again.
#[test]
fn no_hedge_while_the_current_device_is_in_a_decrease_state() {
    let l = loc(vec![hot(1, LEN), hot(2, LEN)]);
    let mut book = DeviceBook::default();
    book.busy(hot_dev(1), 100);
    assert_eq!(
        [
            hedge(&l, &[], &book, 50, MIB, 0),
            hedge(&l, &[], &book, 100, MIB, 0),
        ],
        [None, Some(1)]
    );
}

/// A hot hedge goes to a copy whose queue wait is not longer than the current device's (equal is
/// allowed) and to none whose wait is longer.
#[test]
fn a_hot_hedge_goes_to_a_copy_with_a_queue_wait_not_longer_than_the_current() {
    let l = loc(vec![hot(1, LEN), hot(2, LEN)]);
    let waits = [(10, 10), (10, 11), (10, 9), (0, 1)];
    let chosen: Vec<Option<usize>> = waits
        .iter()
        .map(|(current, other)| {
            let mut book = DeviceBook::default();
            book.signals(hot_dev(1), wait(*current));
            book.signals(hot_dev(2), wait(*other));
            hedge(&l, &[], &book, 0, MIB, 0)
        })
        .collect();
    assert_eq!(chosen, [Some(1), None, Some(1), None]);
}

/// Of several hot copies fit to hedge to, the best by the first-choice order is chosen: the
/// shortest wait, and at equal waits a promoted copy before a plain hot one.
#[test]
fn a_hot_hedge_goes_to_the_best_of_the_fit_copies() {
    let l = loc(vec![hot(1, LEN), hot(2, LEN), hot(3, LEN)]);
    let mut book = DeviceBook::default();
    book.signals(hot_dev(1), wait(20));
    book.signals(hot_dev(2), wait(15));
    book.signals(hot_dev(3), wait(5));
    let with_clean = loc(vec![hot(1, LEN), hot(2, LEN), hot_clean(3, LEN)]);
    assert_eq!(
        [
            hedge(&l, &[], &book, 0, MIB, 0),
            hedge(&with_clean, &[], &DeviceBook::default(), 0, MIB, 0),
        ],
        [Some(2), Some(2)]
    );
}

/// A promoted copy being read can be hedged to a hot copy.
#[test]
fn a_promoted_copy_being_read_can_be_hedged_to_a_hot_copy() {
    let l = loc(vec![hot_clean(1, LEN), hot(2, LEN)]);
    assert_eq!(hedge(&l, &[], &DeviceBook::default(), 0, MIB, 0), Some(1));
}

/// A hedge never goes to an unhealthy copy, an excluded copy, a copy on a device in a decrease
/// state, or one whose extent does not hold the offset.
#[test]
fn a_hedge_never_goes_to_an_unhealthy_excluded_or_non_covering_copy() {
    let l = loc(vec![hot(1, LEN), hot(2, LEN)]);
    let mut stalled = DeviceBook::default();
    stalled.signals(hot_dev(2), state(DriveState::Stalled));
    let mut busy = DeviceBook::default();
    busy.busy(hot_dev(2), 10);
    let elsewhere = loc(vec![
        hot(1, LEN),
        Source {
            extent: ByteRange {
                offset: 100,
                len: 100,
            },
            ..hot(2, LEN)
        },
    ]);
    let none = DeviceBook::default();
    assert_eq!(
        [
            hedge(&l, &[], &stalled, 0, MIB, 0),
            hedge(&l, &[], &busy, 5, MIB, 0),
            hedge(&l, &[1], &none, 0, MIB, 0),
            hedge(&elsewhere, &[], &none, 0, MIB, 0),
        ],
        [None, None, None, None]
    );
}

/// A cold source is never a hedge target of a hot read, and a hot copy is not offered as a hedge
/// to a cold read.
#[test]
fn a_hot_read_is_not_hedged_to_cold_and_a_cold_read_is_not_hedged_to_hot() {
    let book = DeviceBook::default();
    let hot_then_cold = loc(vec![hot(1, LEN), cold(2, 1, 0, LEN), cold(3, 1, 0, LEN)]);
    let cold_then_hot = loc(vec![cold(2, 1, 0, LEN), hot(1, LEN)]);
    assert_eq!(
        [
            hedge(&hot_then_cold, &[], &book, 0, MIB, 0),
            hedge(&cold_then_hot, &[], &book, 0, MIB, 0),
        ],
        [None, None]
    );
}

fn cold_with_degraded() -> Location {
    loc(vec![
        cold(1, 1, 0, LEN),
        degraded(5, &[drive(2, 1), drive(3, 1)], 0, LEN),
    ])
}

fn idle_sources() -> DeviceBook {
    let mut book = DeviceBook::default();
    book.signals(drive(2, 1), queue(0));
    book.signals(drive(3, 1), queue(0));
    book
}

/// A cold read of at most the degraded maximum is hedged to a degraded read when every source
/// drive advertises an empty queue; one byte over the maximum is not.
#[test]
fn a_short_cold_read_is_hedged_to_a_degraded_read_when_every_source_drive_is_idle() {
    let l = cold_with_degraded();
    let book = idle_sources();
    assert_eq!(
        [
            hedge(&l, &[], &book, 0, DEGRADED_MAX, 0),
            hedge(&l, &[], &book, 0, DEGRADED_MAX + 1, 0),
            hedge(&l, &[], &book, 0, 1, 0),
        ],
        [Some(1), None, Some(1)]
    );
}

/// A degraded hedge needs every source drive to advertise an empty queue: a queued request on
/// one, or a drive that has said nothing, forbids it.
#[test]
fn a_degraded_hedge_needs_every_source_drive_to_advertise_an_empty_queue() {
    let l = cold_with_degraded();
    let mut one_queued = DeviceBook::default();
    one_queued.signals(drive(2, 1), queue(0));
    one_queued.signals(drive(3, 1), queue(1));
    let mut one_unknown = DeviceBook::default();
    one_unknown.signals(drive(2, 1), queue(0));
    let unknown = DeviceBook::default();
    assert_eq!(
        [
            hedge(&l, &[], &one_queued, 0, MIB, 0),
            hedge(&l, &[], &one_unknown, 0, MIB, 0),
            hedge(&l, &[], &unknown, 0, MIB, 0),
        ],
        [None, None, None]
    );
}

/// A degraded hedge is not made when the reconstructor's device is unhealthy, when the source is
/// excluded, or when it would be the device already being read.
#[test]
fn a_degraded_hedge_is_not_made_to_an_unhealthy_excluded_or_the_same_device() {
    let l = cold_with_degraded();
    let mut stalled = idle_sources();
    stalled.signals(hot_dev(5), state(DriveState::Stalled));
    let same_device = loc(vec![
        cold(1, 1, 0, LEN),
        Source {
            device: drive(1, 1),
            ..degraded(5, &[drive(2, 1), drive(3, 1)], 0, LEN)
        },
    ]);
    assert_eq!(
        [
            hedge(&l, &[], &stalled, 0, MIB, 0),
            hedge(&l, &[1], &idle_sources(), 0, MIB, 0),
            hedge(&same_device, &[], &idle_sources(), 0, MIB, 0),
        ],
        [None, None, None]
    );
}

/// A degraded read is offered as a hedge only to a cold read, not to a hot read.
#[test]
fn a_degraded_hedge_is_offered_only_to_a_cold_read() {
    let hot_read = loc(vec![
        hot(1, LEN),
        degraded(5, &[drive(2, 1), drive(3, 1)], 0, LEN),
    ]);
    let book = idle_sources();
    assert_eq!(hedge(&hot_read, &[], &book, 0, MIB, 0), None);
}

/// A later answer that leaves a field out keeps the earlier value of that field; a field it
/// carries replaces the old one.
#[test]
fn signals_keep_the_fields_a_later_answer_leaves_out() {
    let mut book = DeviceBook::default();
    let d = hot_dev(1);
    book.signals(
        d,
        DriveSignals {
            queue_wait_ms: Some(5),
            service_ms: Some(9),
            drive_queue: Some(2),
            drive_state: Some(DriveState::Up),
        },
    );
    book.signals(d, queue(7));
    let after_queue = book.get(d);
    book.signals(d, state(DriveState::Drain));
    book.signals(d, wait(1));
    assert_eq!(
        [after_queue, book.get(d)],
        [
            DeviceInfo {
                queue_wait_ms: Some(5),
                drive_queue: Some(7),
                state: Some(DriveState::Up),
                busy_until: 0,
            },
            DeviceInfo {
                queue_wait_ms: Some(1),
                drive_queue: Some(7),
                state: Some(DriveState::Drain),
                busy_until: 0,
            },
        ]
    );
}

/// A device nothing is known of reads as the empty record, and a device's signals do not leak
/// to another.
#[test]
fn an_unheard_device_reads_as_the_empty_record() {
    let mut book = DeviceBook::default();
    book.signals(hot_dev(1), wait(5));
    assert_eq!(
        [book.get(hot_dev(2)), book.get(drive(1, 1))],
        [DeviceInfo::default(), DeviceInfo::default()]
    );
}

/// A later busy deadline replaces an earlier one; an earlier one does not shorten it.
#[test]
fn a_busy_deadline_never_moves_backward() {
    let mut book = DeviceBook::default();
    let d = hot_dev(1);
    book.busy(d, 100);
    book.busy(d, 50);
    let after_earlier = book.get(d).busy_until;
    book.busy(d, 200);
    assert_eq!([after_earlier, book.get(d).busy_until], [100, 200]);
}

/// A device is healthy exactly in the states `up` and `drain` (and when none is known), and not
/// in the others.
#[test]
fn healthy_follows_the_device_state() {
    let states = [
        DriveState::Up,
        DriveState::Drain,
        DriveState::Indexing,
        DriveState::Stalled,
        DriveState::Suspect,
        DriveState::Down,
        DriveState::Failed,
        DriveState::Out,
    ];
    let healthy: Vec<bool> = states
        .iter()
        .map(|s| {
            let mut book = DeviceBook::default();
            book.signals(hot_dev(1), state(*s));
            book.healthy(hot_dev(1), 0)
        })
        .collect();
    assert_eq!(
        (healthy, DeviceBook::default().healthy(hot_dev(1), 0)),
        (
            vec![true, true, false, false, false, false, false, false],
            true
        )
    );
}

/// At the busy deadline the device is healthy and no longer decreasing; one millisecond before
/// it is neither. A device that never answered `503` is never decreasing.
#[test]
fn a_device_is_healthy_and_not_decreasing_at_its_busy_deadline() {
    let mut book = DeviceBook::default();
    let d = hot_dev(1);
    book.busy(d, 100);
    assert_eq!(
        [
            (book.healthy(d, 99), book.decreasing(d, 99)),
            (book.healthy(d, 100), book.decreasing(d, 100)),
            (book.healthy(d, 101), book.decreasing(d, 101)),
            (book.healthy(hot_dev(2), 0), book.decreasing(hot_dev(2), 0)),
        ],
        [(false, true), (true, false), (true, false), (true, false)]
    );
}

/// A device in a bad state that is also past its busy deadline is still unhealthy: the state and
/// the deadline are both required.
#[test]
fn a_bad_state_keeps_a_device_unhealthy_after_its_busy_deadline() {
    let mut book = DeviceBook::default();
    let d = hot_dev(1);
    book.busy(d, 10);
    book.signals(d, state(DriveState::Down));
    assert_eq!(
        [book.healthy(d, 50), book.decreasing(d, 50)],
        [false, false]
    );
}
