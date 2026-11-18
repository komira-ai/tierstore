//! Times on the wire: seconds since the epoch and nanoseconds.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NANOS_PER_SEC: u32 = 1_000_000_000;

/// A point in time as FUSE carries it: whole seconds since the epoch (negative before it) and
/// `nsecs` (below 1e9) added to them, so 1.2 s before the epoch is `(-2, 800_000_000)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp {
    /// Whole seconds since the epoch, rounded towards negative infinity.
    pub secs: i64,
    /// Nanoseconds after `secs`.
    pub nsecs: u32,
}

impl Timestamp {
    /// `t`, saturated at `(i64::MAX, 999_999_999)` and `(i64::MIN, 0)` (only a platform whose
    /// `SystemTime` reaches past an `i64` of seconds saturates; Linux's does not).
    #[must_use]
    pub fn from_system_time(t: SystemTime) -> Self {
        const NANOS: i128 = NANOS_PER_SEC as i128;
        const MIN: i128 = i64::MIN as i128 * NANOS;
        const MAX: i128 = i64::MAX as i128 * NANOS + (NANOS - 1);
        let nanos = match t.duration_since(UNIX_EPOCH) {
            Ok(after) => after.as_nanos().cast_signed(),
            Err(before) => -before.duration().as_nanos().cast_signed(),
        };
        let nanos = nanos.clamp(MIN, MAX);
        // In range after the clamp: the quotient fits an i64, the remainder is below 1e9.
        Self {
            secs: nanos.div_euclid(NANOS) as i64,
            nsecs: nanos.rem_euclid(NANOS) as u32,
        }
    }

    /// The time as a `SystemTime`; `None` if `nsecs` is not below 1e9 or the platform's
    /// `SystemTime` cannot hold it.
    #[must_use]
    pub fn to_system_time(self) -> Option<SystemTime> {
        if self.nsecs >= NANOS_PER_SEC {
            return None;
        }
        let whole = if self.secs >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(self.secs.unsigned_abs()))?
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(self.secs.unsigned_abs()))?
        };
        whole.checked_add(Duration::from_nanos(u64::from(self.nsecs)))
    }

    /// The time from its wire fields (`fuse_attr`, `fuse_setattr_in`): the seconds are an
    /// `i64` sent as its two's complement in a `u64`.
    #[must_use]
    pub fn from_wire(secs: u64, nsecs: u32) -> Self {
        Self {
            secs: secs.cast_signed(),
            nsecs,
        }
    }

    /// The seconds as the wire's `u64` (the two's complement of a negative time).
    #[must_use]
    pub fn wire_secs(self) -> u64 {
        self.secs.cast_unsigned()
    }
}

/// The tests of `src/time.rs` of fuser 0.18.0 (MIT), ported, and the conversion back, which
/// the fork leaves wrong for a negative time with nanoseconds.
#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::Timestamp;

    fn ts(secs: i64, nsecs: u32) -> Timestamp {
        Timestamp { secs, nsecs }
    }

    #[test]
    fn test_time_from_system_time_negative() {
        let before_epoch = UNIX_EPOCH - Duration::new(1, 200_000_000);
        assert_eq!(
            Timestamp::from_system_time(before_epoch),
            ts(-2, 800_000_000)
        );
        let whole = UNIX_EPOCH - Duration::from_secs(3);
        assert_eq!(Timestamp::from_system_time(whole), ts(-3, 0));
    }

    #[test]
    fn test_time_from_system_time_i64_min_boundary() {
        let min = UNIX_EPOCH - Duration::new(i64::MAX as u64 + 1, 0);
        assert_eq!(Timestamp::from_system_time(min), ts(i64::MIN, 0));

        let min_plus_eps = UNIX_EPOCH - Duration::new(i64::MAX as u64, 800_000_000);
        assert_eq!(
            Timestamp::from_system_time(min_plus_eps),
            ts(i64::MIN, 200_000_000)
        );

        let min_plus_one = UNIX_EPOCH - Duration::new(i64::MAX as u64, 0);
        assert_eq!(
            Timestamp::from_system_time(min_plus_one),
            ts(i64::MIN + 1, 0)
        );
    }

    #[test]
    fn a_time_after_the_epoch_keeps_seconds_and_nanoseconds() {
        let t = UNIX_EPOCH + Duration::new(0x1234, 0x5678);
        assert_eq!(Timestamp::from_system_time(t), ts(0x1234, 0x5678));
        assert_eq!(Timestamp::from_system_time(UNIX_EPOCH), ts(0, 0));
    }

    #[test]
    fn a_timestamp_converts_back_to_the_same_system_time() {
        for t in [
            UNIX_EPOCH,
            UNIX_EPOCH + Duration::new(0x1234, 0x5678),
            UNIX_EPOCH - Duration::new(1, 200_000_000),
            UNIX_EPOCH - Duration::from_secs(3),
        ] {
            assert_eq!(Timestamp::from_system_time(t).to_system_time(), Some(t));
        }
        // -1.2 s, not the -2.8 s a subtraction of the nanoseconds would give.
        assert_eq!(
            ts(-2, 800_000_000).to_system_time(),
            Some(UNIX_EPOCH - Duration::new(1, 200_000_000))
        );
    }

    /// The cases of the fork's later `test_system_time_from_time_negative` and
    /// `test_time_round_trip` (fuser master, after 0.18.0).
    #[test]
    fn every_timestamp_round_trips_through_system_time() {
        assert_eq!(
            ts(-1, 500_000_000).to_system_time(),
            Some(UNIX_EPOCH - Duration::new(0, 500_000_000))
        );
        assert_eq!(
            ts(-2, 0).to_system_time(),
            Some(UNIX_EPOCH - Duration::new(2, 0))
        );
        for (secs, nsecs) in [
            (0, 0),
            (0, 1),
            (1, 999_999_999),
            (1_700_000_000, 123_456_789),
            (-1, 0),
            (-1, 1),
            (-1, 999_999_999),
            (-2, 800_000_000),
            (-1_000_000_000, 500_000_000),
            (i64::MIN, 0),
            (i64::MAX, 999_999_999),
        ] {
            let t = ts(secs, nsecs).to_system_time().unwrap();
            assert_eq!(Timestamp::from_system_time(t), ts(secs, nsecs));
        }
    }

    #[test]
    fn nanoseconds_of_a_second_or_more_are_not_a_time() {
        assert_eq!(ts(0, 1_000_000_000).to_system_time(), None);
        assert_eq!(ts(-1, u32::MAX).to_system_time(), None);
        assert!(ts(0, 999_999_999).to_system_time().is_some());
    }

    #[test]
    fn the_extremes_of_an_i64_of_seconds_convert_both_ways() {
        for extreme in [ts(i64::MAX, 999_999_999), ts(i64::MIN, 0), ts(i64::MIN, 1)] {
            let t = extreme.to_system_time().unwrap();
            assert_eq!(Timestamp::from_system_time(t), extreme);
        }
    }

    #[test]
    fn wire_seconds_are_the_twos_complement_of_negative_seconds() {
        assert_eq!(ts(-1, 0).wire_secs(), u64::MAX);
        assert_eq!(ts(0x1234, 0).wire_secs(), 0x1234);
        assert_eq!(Timestamp::from_wire(u64::MAX, 7), ts(-1, 7));
        assert_eq!(Timestamp::from_wire(0x1234, 0x5678), ts(0x1234, 0x5678));
        assert_eq!(Timestamp::default(), ts(0, 0));
    }
}
