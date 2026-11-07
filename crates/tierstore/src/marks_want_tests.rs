//! The ask a cold read makes for a copy-back (M10): when it writes a `WANT` marker and when it
//! writes nothing, and that it never waits on or fails the read. Time is the monotonic clock of the
//! rig.

use std::time::Duration;

use crate::marks_leader_tests::{Rig, Shape, shape_of};
use crate::{ColdRead, CopyBackConfig, Error, MarkerState, Op, Skip, WantRequests, Wanted};

const KEY: &str = "models/a.bin";

fn read(size: u64) -> ColdRead {
    ColdRead {
        size,
        critical: false,
        active: true,
    }
}

fn asks(rig: &Rig, tweak: impl FnOnce(&mut CopyBackConfig)) -> WantRequests {
    let mut config = CopyBackConfig::default();
    tweak(&mut config);
    WantRequests::new(config, rig.mono.clone())
}

/// The marker of an ask: `WANT`, nothing owed, nobody's.
fn want_shape() -> Shape {
    (MarkerState::Want, false, false, None, None)
}

/// Asserts the bounds of 5.7: a cold-only key read again and again makes one create attempt a
/// `want_retry` (the retry is allowed at exactly the interval, and then meets the marker it made),
/// none for an object above `copy_back_max_bytes` (one of exactly that size asks), none at the
/// critical level, and none while the format is not `active-marks`; a refusal makes no request and
/// does not count as an ask.
#[tokio::test]
async fn a_cold_only_key_read_many_times_makes_one_want_per_want_retry_and_none_above_the_size_cap_or_at_critical()
 {
    let rig = Rig::new();
    let markers = rig.markers("P");
    let wants = asks(&rig, |config| config.copy_back_max_bytes = 100);

    let inactive = ColdRead {
        active: false,
        ..read(1)
    };
    assert_eq!(
        wants.ask(&markers, KEY, inactive).await,
        Wanted::Skipped(Skip::Inactive)
    );
    let critical = ColdRead {
        critical: true,
        ..read(1)
    };
    assert_eq!(
        wants.ask(&markers, KEY, critical).await,
        Wanted::Skipped(Skip::Critical)
    );
    assert_eq!(
        wants.ask(&markers, KEY, read(101)).await,
        Wanted::Skipped(Skip::TooBig)
    );
    assert_eq!(rig.marks.calls(Op::Put), 0);

    assert_eq!(wants.ask(&markers, KEY, read(100)).await, Wanted::Made);
    assert_eq!(shape_of(&rig, KEY).await, Some(want_shape()));
    for _ in 0..1000 {
        assert_eq!(
            wants.ask(&markers, KEY, read(100)).await,
            Wanted::Skipped(Skip::Recent)
        );
    }
    rig.mono.advance(Duration::from_millis(59_999));
    assert_eq!(
        wants.ask(&markers, KEY, read(100)).await,
        Wanted::Skipped(Skip::Recent)
    );
    assert_eq!(rig.marks.calls(Op::Put), 1);
    rig.mono.advance(Duration::from_millis(1));
    assert_eq!(wants.ask(&markers, KEY, read(100)).await, Wanted::Exists);
    assert_eq!(rig.marks.calls(Op::Put), 2);
}

/// Asserts that a failed create is returned to the caller, who ignores it, and still counts as an
/// ask, so that a store that is down is not asked once per read.
#[tokio::test]
async fn a_failed_ask_is_ignored_by_the_read_and_not_repeated_within_want_retry() {
    let rig = Rig::new();
    let markers = rig.markers("P");
    let wants = asks(&rig, |_| {});
    let down = Error::Unavailable("hot down".into());
    rig.marks.fail_next_with(Op::Put, down.clone());
    rig.marks.fail_next_with(Op::Get, down.clone());
    assert_eq!(
        wants.ask(&markers, KEY, read(1)).await,
        Wanted::Failed(down)
    );
    assert_eq!(
        wants.ask(&markers, KEY, read(1)).await,
        Wanted::Skipped(Skip::Recent)
    );
    assert_eq!(shape_of(&rig, KEY).await, None);
}

/// Asserts that the set of asked keys is bounded by `want_recent_limit`: past it the oldest key is
/// forgotten and may be asked for again at once, and a key asked for again re-enters as the
/// newest.
#[tokio::test]
async fn the_keys_a_proxy_remembers_asking_for_are_bounded() {
    let rig = Rig::new();
    let markers = rig.markers("P");
    let wants = asks(&rig, |config| config.want_recent_limit = 2);
    for key in ["a", "b", "c"] {
        assert_eq!(wants.ask(&markers, key, read(1)).await, Wanted::Made);
    }
    // "a" was forgotten, and its marker is there.
    assert_eq!(wants.ask(&markers, "a", read(1)).await, Wanted::Exists);
    assert_eq!(
        wants.ask(&markers, "c", read(1)).await,
        Wanted::Skipped(Skip::Recent)
    );
    // Asking for "a" made "b" the oldest, and it is forgotten.
    assert_eq!(wants.ask(&markers, "b", read(1)).await, Wanted::Exists);
    assert_eq!(
        wants.ask(&markers, "a", read(1)).await,
        Wanted::Skipped(Skip::Recent)
    );
}
