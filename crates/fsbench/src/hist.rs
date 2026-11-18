//! A latency histogram in nanoseconds: exact below 128 ns, then 64 buckets per power of two (a
//! bucket is at most 1/64 of its value wide, so a percentile is within 1.6% of the value it
//! stands for), and the exact largest value. Histograms of the readers of a run add up into the
//! run's, and they are kept in the results file as the buckets that hold a value, so a later
//! reading can merge or re-cut them.

use serde::{Deserialize, Serialize};

/// The values below this are a bucket each.
const EXACT: u64 = 128;

/// The buckets per power of two above [`EXACT`].
const PER_OCTAVE: u64 = 64;

/// The bucket of `value`.
fn bucket_of(value: u64) -> u32 {
    if value < EXACT {
        return value as u32;
    }
    let top_bit = 63 - u64::from(value.leading_zeros());
    let shift = top_bit - 6;
    let top = value >> shift;
    (EXACT + (shift - 1) * PER_OCTAVE + (top - PER_OCTAVE)) as u32
}

/// The values bucket `bucket` holds: the first and the last.
fn bounds_of(bucket: u32) -> (u64, u64) {
    let bucket = u64::from(bucket);
    if bucket < EXACT {
        return (bucket, bucket);
    }
    let k = bucket - EXACT;
    let shift = k / PER_OCTAVE + 1;
    let top = PER_OCTAVE + k % PER_OCTAVE;
    // Written so the last bucket (whose end is `u64::MAX`) does not overflow.
    (top << shift, (top << shift) + ((1 << shift) - 1))
}

/// Counts of values by bucket.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Histogram {
    /// How many values were added.
    pub count: u64,
    /// The largest value added, 0 for none.
    pub max: u64,
    /// The sum of the values added (to report a mean), saturating at `u64::MAX` (584 years of
    /// nanoseconds).
    pub sum: u64,
    /// `(bucket, count)` for each bucket that holds a value, by bucket.
    pub buckets: Vec<(u32, u64)>,
    /// The counts by bucket while values are being added; `buckets` is made from it.
    #[serde(skip)]
    dense: Vec<u64>,
}

impl Histogram {
    /// Adds one value.
    pub fn add(&mut self, value: u64) {
        let bucket = bucket_of(value) as usize;
        if self.dense.len() <= bucket {
            self.dense.resize(bucket + 1, 0);
        }
        self.dense[bucket] += 1;
        self.count += 1;
        self.max = self.max.max(value);
        self.sum = self.sum.saturating_add(value);
    }

    /// The histogram with `buckets` made from the values added: what is written out.
    #[must_use]
    pub fn sealed(mut self) -> Self {
        let mut merged = Self::default();
        merged.merge(&self);
        self.buckets = merged.buckets;
        self.dense = Vec::new();
        self
    }

    /// Adds every value of `other` (sealed or not) to this one, which keeps its `buckets` form.
    pub fn merge(&mut self, other: &Self) {
        let mut all: std::collections::BTreeMap<u32, u64> = self.buckets.iter().copied().collect();
        for &(bucket, count) in &other.buckets {
            *all.entry(bucket).or_default() += count;
        }
        for (bucket, &count) in other.dense.iter().enumerate() {
            if count > 0 {
                *all.entry(bucket as u32).or_default() += count;
            }
        }
        for (bucket, &count) in self.dense.iter().enumerate() {
            if count > 0 {
                *all.entry(bucket as u32).or_default() += count;
            }
        }
        self.dense = Vec::new();
        self.buckets = all.into_iter().collect();
        self.count += other.count;
        self.max = self.max.max(other.max);
        self.sum = self.sum.saturating_add(other.sum);
    }

    /// The value at quantile `q` (0.5 for the median, 0.99): the middle of the bucket that holds
    /// the `ceil(q * count)`-th value, and never more than the largest value. 0 when empty.
    /// Reads the `buckets` form ([`Histogram::sealed`] or a merge).
    #[must_use]
    pub fn quantile(&self, q: f64) -> u64 {
        let rank = ((q * self.count as f64).ceil() as u64).clamp(1, self.count.max(1));
        let mut seen = 0;
        for &(bucket, count) in &self.buckets {
            seen += count;
            if seen >= rank {
                let (low, high) = bounds_of(bucket);
                return (low + (high - low) / 2).min(self.max);
            }
        }
        0
    }

    /// The mean, 0 when empty.
    #[must_use]
    pub fn mean(&self) -> u64 {
        self.sum.checked_div(self.count).unwrap_or(0)
    }
}

#[cfg(test)]
#[path = "hist_tests.rs"]
mod tests;
