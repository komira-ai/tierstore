//! Percentiles and rates of a cell's samples.

use serde::Serialize;

/// The summary of a set of samples (milliseconds, unless a caller says otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Summary {
    /// How many samples.
    pub n: usize,
    /// Their mean.
    pub mean: f64,
    /// The median (nearest rank).
    pub p50: f64,
    /// The 99th percentile (nearest rank).
    pub p99: f64,
    /// The largest.
    pub max: f64,
}

/// The nearest-rank `q` quantile (0 < q <= 1) of `sorted`, which is sorted ascending and not
/// empty.
#[must_use]
pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    let rank = ((q * n as f64).ceil() as usize).clamp(1, n);
    sorted[rank - 1]
}

/// The summary of `samples`, or `None` when there are none.
#[must_use]
pub fn summarize(samples: &[f64]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort_by(f64::total_cmp);
    let mean = v.iter().fold(0.0, |a, b| a + b) / v.len() as f64;
    Some(Summary {
        n: v.len(),
        mean,
        p50: quantile(&v, 0.50),
        p99: quantile(&v, 0.99),
        max: v[v.len() - 1],
    })
}

/// Megabytes (10^6 bytes) per second, 0 for no time.
#[must_use]
pub fn mb_per_s(bytes: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        return 0.0;
    }
    let b = bytes as f64;
    b / 1e6 / secs
}

/// `x` rounded to `places` decimals, for tables.
#[must_use]
pub fn round(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_quantiles() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert!((quantile(&v, 0.5) - 50.0).abs() < 1e-9);
        assert!((quantile(&v, 0.99) - 99.0).abs() < 1e-9);
        assert!((quantile(&[7.0], 0.99) - 7.0).abs() < 1e-9);
        assert!((quantile(&[1.0, 2.0], 0.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn summary_of_unsorted_samples_and_of_none() {
        let s = summarize(&[3.0, 1.0, 2.0, 10.0]).unwrap();
        assert_eq!(s.n, 4);
        assert!((s.mean - 4.0).abs() < 1e-9);
        assert!((s.p50 - 2.0).abs() < 1e-9);
        assert!((s.p99 - 10.0).abs() < 1e-9);
        assert!((s.max - 10.0).abs() < 1e-9);
        assert_eq!(summarize(&[]), None);
    }

    #[test]
    fn rates_and_rounding() {
        assert!((mb_per_s(200_000_000, 2.0) - 100.0).abs() < 1e-9);
        assert!(mb_per_s(1, 0.0).abs() < 1e-12);
        assert!((round(1.23456, 2) - 1.23).abs() < 1e-12);
    }
}
