use super::{Histogram, bounds_of, bucket_of};

/// Every value falls in the bucket whose bounds hold it, the buckets are in the order of their
/// values with no gap, and a bucket is at most 1/64 of its first value wide, from 0 to
/// `u64::MAX`.
#[test]
fn every_value_is_in_a_bucket_that_holds_it_and_the_buckets_tile_the_range() {
    let mut values: Vec<u64> = (0..2048).collect();
    for shift in 11..64 {
        let base = 1u64 << shift;
        values.extend([base - 1, base, base + 1, base + base / 3]);
    }
    values.push(u64::MAX);
    for value in values {
        let bucket = bucket_of(value);
        let (low, high) = bounds_of(bucket);
        assert!(
            low <= value && value <= high,
            "{value} in {bucket}: {low}..={high}"
        );
        assert!((high - low) <= low / 64, "{bucket} is {low}..={high}");
    }
    let mut next = 0u64;
    for bucket in 0..=bucket_of(u64::MAX) {
        let (low, high) = bounds_of(bucket);
        assert_eq!(low, next, "bucket {bucket}");
        next = high.wrapping_add(1);
    }
    assert_eq!(next, 0, "the last bucket ends at u64::MAX");
}

/// The quantiles of 1..=1000 ns: the median and the 99th are within a bucket of 500 and 990,
/// the largest is exact, the mean is exact, and a quantile never passes the largest value.
#[test]
fn quantiles_of_a_known_set_are_within_a_bucket_and_the_max_is_exact() {
    let mut h = Histogram::default();
    for value in (1..=1000).rev() {
        h.add(value);
    }
    let h = h.sealed();
    assert_eq!((h.count, h.max, h.mean()), (1000, 1000, 500));
    let near = |got: u64, want: u64| got.abs_diff(want) <= want / 64 + 1;
    assert!(near(h.quantile(0.5), 500), "{}", h.quantile(0.5));
    assert!(near(h.quantile(0.99), 990), "{}", h.quantile(0.99));
    assert_eq!(h.quantile(1.0), 1000);
    assert_eq!(h.quantile(0.0), 1, "the first value");
    let mut one = Histogram::default();
    one.add(1_000_000);
    assert_eq!(one.sealed().quantile(0.5), 1_000_000, "capped at the max");
}

/// An empty histogram answers 0 for every quantile and for the mean.
#[test]
fn an_empty_histogram_reads_zero() {
    let h = Histogram::default().sealed();
    assert_eq!(
        (h.quantile(0.5), h.quantile(0.99), h.mean(), h.max),
        (0, 0, 0, 0)
    );
    assert!(h.buckets.is_empty());
}

/// Two histograms merged, sealed or not, count the values of both: the same as one histogram
/// of every value; and the sealed form survives a JSON round trip unchanged.
#[test]
fn merged_histograms_equal_one_of_every_value_and_round_trip_through_json() {
    let (mut a, mut b, mut both) = (
        Histogram::default(),
        Histogram::default(),
        Histogram::default(),
    );
    for value in [5, 200, 7_000, 7_001, 3_000_000] {
        a.add(value);
        both.add(value);
    }
    for value in [5, 90_000, 12] {
        b.add(value);
        both.add(value);
    }
    let both = both.sealed();
    let mut sealed_a = a.clone().sealed();
    sealed_a.merge(&b);
    assert_eq!(sealed_a, both, "sealed plus raw");
    let mut raw_a = a;
    raw_a.merge(&b.sealed());
    assert_eq!(raw_a, both, "raw plus sealed");
    let text = serde_json::to_string(&both).unwrap();
    let back: Histogram = serde_json::from_str(&text).unwrap();
    assert_eq!(back, both);
}
