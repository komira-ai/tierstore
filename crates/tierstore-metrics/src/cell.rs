//! The series of a family: a counter, a gauge, a histogram, or a state set's current state.
//!
//! Every update is one or a few relaxed atomic operations, so a series may be updated from any
//! thread without a lock.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::family::{Kind, Spec, State, pair, sample};

/// One series of a family. Sealed with the families (the trait cannot be named outside the
/// crate).
pub trait Cell: Send + Sync + 'static {
    /// The family's kind.
    const KIND: Kind;
    /// A series at zero.
    fn new(spec: &Spec) -> Self;
    /// The name of the label the cell adds itself (a state set's state label).
    fn extra_label() -> Option<&'static str> {
        None
    }
    /// Appends the series' sample lines, under `name`, with the family's label `pairs`.
    fn render(&self, name: &str, pairs: &str, out: &mut String);
}

/// A count that goes up and never down while the process runs.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    /// Adds one.
    pub fn inc(&self) {
        self.add(1);
    }

    /// Adds `by`.
    pub fn add(&self, by: u64) {
        let _ = self.0.fetch_add(by, Ordering::Relaxed);
    }

    /// The count.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Cell for Counter {
    const KIND: Kind = Kind::Counter;

    fn new(_: &Spec) -> Self {
        Self::default()
    }

    fn render(&self, name: &str, pairs: &str, out: &mut String) {
        sample(out, name, "", pairs, &self.get().to_string());
    }
}

/// A value that is set, or moved up and down: a level, a ratio, a duration.
#[derive(Debug, Default)]
pub struct Gauge(AtomicU64);

impl Gauge {
    /// Sets the value.
    pub fn set(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }

    /// Adds `by` (negative to subtract).
    pub fn add(&self, by: f64) {
        add_f64(&self.0, by);
    }

    /// Adds one.
    pub fn inc(&self) {
        self.add(1.0);
    }

    /// Subtracts one.
    pub fn dec(&self) {
        self.add(-1.0);
    }

    /// The value.
    #[must_use]
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

impl Cell for Gauge {
    const KIND: Kind = Kind::Gauge;

    fn new(_: &Spec) -> Self {
        Self::default()
    }

    fn render(&self, name: &str, pairs: &str, out: &mut String) {
        sample(out, name, "", pairs, &number(self.get()));
    }
}

/// Observations counted into the buckets of the family's bounds, with their sum and count.
#[derive(Debug)]
pub struct Histogram {
    bounds: &'static [f64],
    /// The observations at or below each bound and above the one before; the last slot holds
    /// those above every bound.
    counts: Box<[AtomicU64]>,
    sum: AtomicU64,
}

impl Histogram {
    /// Counts one observation of `value` (seconds, bytes or a count, as the family says). A NaN
    /// is not counted, so it can neither land in a bucket nor make the sum NaN for good.
    pub fn observe(&self, value: f64) {
        if value.is_nan() {
            return;
        }
        let slot = self.bounds.partition_point(|bound| *bound < value);
        let _ = self.counts[slot].fetch_add(1, Ordering::Relaxed);
        add_f64(&self.sum, value);
    }

    /// Counts one observation of `ms` milliseconds, in seconds: the difference of two readings
    /// of the injected clock ([`tierstore_fetch_api::clock::Clock`]).
    pub fn observe_ms(&self, ms: u64) {
        // A u64 of milliseconds is exact in an f64 up to 2^53 ms (285 000 years).
        self.observe(ms as f64 / 1000.0);
    }

    /// The observations, all buckets together.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    /// The sum of the observations.
    #[must_use]
    pub fn sum(&self) -> f64 {
        f64::from_bits(self.sum.load(Ordering::Relaxed))
    }

    /// The cumulative count at each bound, ascending, and last the count of all (`+Inf`).
    #[must_use]
    pub fn cumulative(&self) -> Vec<u64> {
        let mut total = 0;
        self.counts
            .iter()
            .map(|c| {
                total += c.load(Ordering::Relaxed);
                total
            })
            .collect()
    }
}

impl Cell for Histogram {
    const KIND: Kind = Kind::Histogram;

    fn new(spec: &Spec) -> Self {
        Self {
            bounds: spec.buckets,
            counts: (0..=spec.buckets.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            sum: AtomicU64::new(0),
        }
    }

    fn render(&self, name: &str, pairs: &str, out: &mut String) {
        let cumulative = self.cumulative();
        let mut labels = String::new();
        let les = self.bounds.iter().map(|b| number(*b));
        for (le, count) in les.chain(["+Inf".to_owned()]).zip(&cumulative) {
            with_label(&mut labels, pairs, "le", &le);
            sample(out, name, "_bucket", &labels, &count.to_string());
        }
        sample(out, name, "_sum", pairs, &number(self.sum()));
        // The count is the `+Inf` bucket's, so one scrape's count equals its last bucket.
        let count = cumulative.last().copied().unwrap_or(0);
        sample(out, name, "_count", pairs, &count.to_string());
    }
}

/// The current state of one series of a state set: one of the values of `S`, or none yet.
/// It renders one gauge per value of `S`, labelled with it: 1 for the current state, 0 for the
/// others (all 0 before a state is set).
#[derive(Debug)]
pub struct StateCell<S> {
    /// The index of the current state in the seeded values of `S`, or `usize::MAX` for none.
    current: AtomicUsize,
    states: PhantomData<fn() -> S>,
}

impl<S: State> StateCell<S> {
    /// Makes `state` the current state.
    pub fn set(&self, state: S) {
        self.current.store(state.index(), Ordering::Relaxed);
    }

    /// The current state, if one was set.
    #[must_use]
    pub fn get(&self) -> Option<S> {
        let index = self.current.load(Ordering::Relaxed);
        S::ALL.get(index).copied()
    }
}

impl<S: State> Cell for StateCell<S> {
    const KIND: Kind = Kind::Gauge;

    fn new(_: &Spec) -> Self {
        Self {
            current: AtomicUsize::new(usize::MAX),
            states: PhantomData,
        }
    }

    fn extra_label() -> Option<&'static str> {
        Some(S::NAME)
    }

    fn render(&self, name: &str, pairs: &str, out: &mut String) {
        let current = self.get();
        let mut labels = String::new();
        for state in S::ALL.iter().copied() {
            with_label(&mut labels, pairs, S::NAME, &state.text());
            let value = if current == Some(state) { "1" } else { "0" };
            sample(out, name, "", &labels, value);
        }
    }
}

/// Sets `labels` to the family's label `pairs` and one more label, `name="value"`. Not generic,
/// so its branch is measured once for every cell type that adds a label.
fn with_label(labels: &mut String, pairs: &str, name: &str, value: &str) {
    labels.clear();
    if !pairs.is_empty() {
        labels.push_str(pairs);
        labels.push(',');
    }
    pair(labels, name, value);
}

/// Adds `by` to the f64 stored as bits in `cell`.
fn add_f64(cell: &AtomicU64, by: f64) {
    let _ = cell.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
        Some((f64::from_bits(bits) + by).to_bits())
    });
}

/// A sample value as the text format writes it: `NaN`, `+Inf`, `-Inf`, or the shortest decimal
/// that reads back as the same f64.
pub(crate) fn number(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "+Inf" } else { "-Inf" }.to_owned()
    } else {
        format!("{value}")
    }
}
