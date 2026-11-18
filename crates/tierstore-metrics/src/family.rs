//! A family: one metric name, its help and kind, and one series per set of label values.
//!
//! The label values of a family are typed ([`Labels`]): `()` for none, one [`LabelValue`], or a
//! pair of them. [`LabelValue`] is sealed: its implementations are the closed enums of
//! [`crate::labels`] and the drive and node ids, so no family can be given a pack, a key or any
//! other free text as a label.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, PoisonError, RwLock};

use crate::cell::{Cell, Counter, Gauge, Histogram, StateCell};

/// The kind of a family, as its `# TYPE` line names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// Goes up and never down while the process runs.
    Counter,
    /// A value that is set; a state set renders as one gauge per state.
    Gauge,
    /// Observations counted into buckets, with their sum and count.
    Histogram,
}

impl Kind {
    /// The word of the `# TYPE` line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// What a family is: its names, help and the bucket bounds of a histogram. Public only so the
/// sealed [`Cell`] trait can take it; it cannot be named outside the crate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spec {
    /// The name in section 12 of the design.
    pub(crate) design: &'static str,
    /// The name it is exported under.
    pub(crate) name: &'static str,
    /// One sentence: what it measures.
    pub(crate) help: &'static str,
    /// The upper bounds of a histogram's buckets, ascending; empty for other kinds.
    pub(crate) buckets: &'static [f64],
}

/// What a registered family is, for a reader of the registry (tests, documentation).
#[derive(Debug, Clone, PartialEq)]
pub struct FamilyInfo {
    /// The name section 12 of the design gives it.
    pub design: &'static str,
    /// The name it is exported under: `tierstore_` and the design's name, except where the
    /// design's name already starts so or does not say what it measures (the per-disk gauges).
    pub name: &'static str,
    /// What it measures.
    pub help: &'static str,
    /// Its kind.
    pub kind: Kind,
    /// The names of its labels, in the order they render (a state set's state label last).
    pub labels: Vec<&'static str>,
}

/// The parts of the label traits a caller of this crate cannot name, so it cannot implement
/// them: the label types are the ones this crate defines.
pub(crate) mod seal {
    use tierstore_format::descriptor::Code;
    use tierstore_format::ids::{DriveId, NodeId};

    use crate::labels::Disk;

    /// The label values a family is given series for before anything is counted: every value of
    /// a closed set, and the ids of the node's configuration.
    #[derive(Debug, Clone, Copy)]
    pub struct Seed<'a> {
        /// The drives of the `{drive}` label of this group of families.
        pub drives: &'a [DriveId],
        /// The peers of the `{node}` label.
        pub nodes: &'a [NodeId],
        /// The disks of the per-disk gauges.
        pub disks: &'a [Disk],
        /// The release, the `version` of `tierstore_build_info`.
        pub version: &'static str,
        /// The hot tier's replica count: `{copies}` runs from 0 to it.
        pub hot_replicas: u8,
        /// The stripe codes of the cold tier's configuration.
        pub codes: &'a [Code],
    }

    /// The seeding of one label value type.
    pub trait Value: Sized {
        /// The values to give series before anything is counted.
        fn seed(seed: &Seed<'_>) -> Vec<Self>;
    }

    /// The seeding and rendering of a set of label values.
    pub trait Set: Sized {
        /// The label names, in order.
        fn names() -> Vec<&'static str>;
        /// Every combination of the seeded values of each label.
        fn seed_all(seed: &Seed<'_>) -> Vec<Self>;
        /// Writes `name="value"` for each label, comma separated, with no braces.
        fn pairs(&self, out: &mut String);
    }
}

/// One label's value: a closed set of names or a drive or node id. Sealed: the implementations
/// are this crate's.
pub trait LabelValue: seal::Value + Copy + Ord + Send + Sync + 'static {
    /// The label's name.
    const NAME: &'static str;
    /// The value as it renders.
    fn text(&self) -> Cow<'static, str>;
}

/// A closed label whose values are the states of a state set ([`crate::StateCell`]): each value
/// is its position in [`State::ALL`]. Sealed.
pub trait State: LabelValue {
    /// Every value, in the order they render.
    const ALL: &'static [Self];
    /// The value's position in [`State::ALL`].
    fn index(self) -> usize;
}

/// The label values of one series of a family: `()`, one [`LabelValue`], or a pair. Sealed.
pub trait Labels: seal::Set + Copy + Ord + Send + Sync + 'static {}

impl<T: seal::Set + Copy + Ord + Send + Sync + 'static> Labels for T {}

impl seal::Set for () {
    fn names() -> Vec<&'static str> {
        Vec::new()
    }

    fn seed_all(_: &seal::Seed<'_>) -> Vec<Self> {
        vec![()]
    }

    fn pairs(&self, _: &mut String) {}
}

impl<T: LabelValue> seal::Set for T {
    fn names() -> Vec<&'static str> {
        vec![T::NAME]
    }

    fn seed_all(seed: &seal::Seed<'_>) -> Vec<Self> {
        T::seed(seed)
    }

    fn pairs(&self, out: &mut String) {
        pair(out, T::NAME, &self.text());
    }
}

impl<A: LabelValue, B: LabelValue> seal::Set for (A, B) {
    fn names() -> Vec<&'static str> {
        vec![A::NAME, B::NAME]
    }

    fn seed_all(seed: &seal::Seed<'_>) -> Vec<Self> {
        let bs = B::seed(seed);
        A::seed(seed)
            .into_iter()
            .flat_map(|a| bs.iter().map(move |b| (a, *b)))
            .collect()
    }

    fn pairs(&self, out: &mut String) {
        pair(out, A::NAME, &self.0.text());
        out.push(',');
        pair(out, B::NAME, &self.1.text());
    }
}

/// Writes `name="value"`, the value escaped as the text format requires.
pub(crate) fn pair(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push_str("=\"");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A family of series of the cell type `C` (a [`crate::Counter`], [`crate::Gauge`],
/// [`crate::Histogram`] or [`crate::StateCell`]), one per value of `L`.
///
/// A series is made the first time its labels are used, and the seeded ones (every value of a
/// closed label, the drives and peers of the configuration of each component that registers
/// the family) exist from its registration, at zero. A series is
/// never removed. [`Family::with`] returns the series itself, which a hot path may keep and
/// update without the family's lock.
pub struct Family<L: Labels, C: Cell> {
    spec: Spec,
    series: RwLock<BTreeMap<L, Arc<C>>>,
}

impl<L: Labels, C: Cell> Family<L, C> {
    /// The family of `spec`, with no series yet.
    pub(crate) fn new(spec: Spec) -> Self {
        Self {
            spec,
            series: RwLock::new(BTreeMap::new()),
        }
    }

    /// Makes a series at zero for each seeded label value that has none yet.
    pub(crate) fn seed(&self, seed: &seal::Seed<'_>) {
        for labels in L::seed_all(seed) {
            let _ = self.with(labels);
        }
    }

    /// The series of `labels`, made now if it does not exist yet.
    pub fn with(&self, labels: L) -> Arc<C> {
        if let Some(cell) = self.get(labels) {
            return cell;
        }
        let mut series = self.series.write().unwrap_or_else(PoisonError::into_inner);
        series
            .entry(labels)
            .or_insert_with(|| Arc::new(C::new(&self.spec)))
            .clone()
    }

    /// The series of `labels`, if it exists.
    pub fn get(&self, labels: L) -> Option<Arc<C>> {
        let series = self.series.read().unwrap_or_else(PoisonError::into_inner);
        series.get(&labels).cloned()
    }

    /// The label values of every series, in the order they render.
    pub fn labels(&self) -> Vec<L> {
        let series = self.series.read().unwrap_or_else(PoisonError::into_inner);
        series.keys().copied().collect()
    }
}

impl<L: Labels> Family<L, Counter> {
    /// Adds one to the series of `labels`.
    pub fn inc(&self, labels: L) {
        self.with(labels).inc();
    }

    /// Adds `by` to the series of `labels`.
    pub fn add(&self, labels: L, by: u64) {
        self.with(labels).add(by);
    }
}

impl<L: Labels> Family<L, Gauge> {
    /// Sets the series of `labels`.
    pub fn set(&self, labels: L, value: f64) {
        self.with(labels).set(value);
    }

    /// Adds `by` (negative to subtract) to the series of `labels`.
    pub fn add(&self, labels: L, by: f64) {
        self.with(labels).add(by);
    }

    /// Adds one to the series of `labels`.
    pub fn inc(&self, labels: L) {
        self.with(labels).inc();
    }

    /// Subtracts one from the series of `labels`.
    pub fn dec(&self, labels: L) {
        self.with(labels).dec();
    }
}

impl<L: Labels> Family<L, Histogram> {
    /// Counts one observation of `value` into the series of `labels`.
    pub fn observe(&self, labels: L, value: f64) {
        self.with(labels).observe(value);
    }

    /// Counts one observation of `ms` milliseconds, in seconds, into the series of `labels`.
    pub fn observe_ms(&self, labels: L, ms: u64) {
        self.with(labels).observe_ms(ms);
    }
}

impl<L: Labels, S: State> Family<L, StateCell<S>> {
    /// Makes `state` the current state of the series of `labels`.
    pub fn set(&self, labels: L, state: S) {
        self.with(labels).set(state);
    }
}

/// A family as the registry sees it, whatever its label and cell types.
pub(crate) trait Registered {
    /// What the family is.
    fn info(&self) -> FamilyInfo;
    /// Appends the family's `# HELP` and `# TYPE` lines and every series to `out`.
    fn render(&self, out: &mut String);
}

impl<L: Labels, C: Cell> Registered for Family<L, C> {
    fn info(&self) -> FamilyInfo {
        let mut labels = L::names();
        labels.extend(C::extra_label());
        FamilyInfo {
            design: self.spec.design,
            name: self.spec.name,
            help: self.spec.help,
            kind: C::KIND,
            labels,
        }
    }

    fn render(&self, out: &mut String) {
        let name = self.spec.name;
        let _ = writeln!(out, "# HELP {name} {}", self.spec.help);
        let _ = writeln!(out, "# TYPE {name} {}", C::KIND.as_str());
        let series = self.series.read().unwrap_or_else(PoisonError::into_inner);
        let mut pairs = String::new();
        for (labels, cell) in series.iter() {
            pairs.clear();
            labels.pairs(&mut pairs);
            cell.render(name, &pairs, out);
        }
    }
}

/// Appends one sample line: `name{pairs} value`, or `name value` with no labels.
pub(crate) fn sample(out: &mut String, name: &str, suffix: &str, pairs: &str, value: &str) {
    out.push_str(name);
    out.push_str(suffix);
    if !pairs.is_empty() {
        out.push('{');
        out.push_str(pairs);
        out.push('}');
    }
    out.push(' ');
    out.push_str(value);
    out.push('\n');
}
