//! The metrics and the demand-read trace of tierstore, as section 12 of
//! `docs/design/fetch-cache-serve.md` (revision 7) lists them: every wait observable, and no
//! label a pack or a key.
//!
//! [`Metrics`] holds every family of section 12, in groups by the part of tierstore that updates
//! it ([`Metrics::node`], [`Metrics::fetch`], [`Metrics::cache`], [`Metrics::fuse`],
//! [`Metrics::hot`], [`Metrics::front`], [`Metrics::cold`], [`Metrics::movement`],
//! [`Metrics::drives`]). It is made from a [`Topology`], the part of the node's configuration
//! its labels take values from, so one registry serves both deployment profiles of section 21;
//! [`Metrics::render`] writes the families of the node's role in the Prometheus text format.
//!
//! **Labels.** A family's label values are typed ([`Labels`]): the closed enums of [`labels`],
//! the frozen enums that already name a set of values (the class of a fetch, the drive
//! scheduler's classes, the verify priority), and the drive and node ids. The label traits are
//! sealed, so no caller can label a series with a pack, a key or any other text.
//!
//! **Time.** A duration is observed in seconds from the difference of two readings of the
//! injected clock ([`Histogram::observe_ms`]), so tests move a
//! [`tierstore_fetch_api::clock::ManualClock`] and assert counts, never timing.
//!
//! **Trace.** [`trace::DemandTrace`] marks the stages of one demand read on the injected clock
//! and emits one `tracing` event when the read is answered.
//!
//! Nothing counts into the registry or serves its text yet: the subsystems of section 18 count
//! into it as they land, and the server's metrics page writes [`Metrics::render`] once it holds
//! a registry.

#![forbid(unsafe_code)]

mod cell;
mod family;
mod groups;
pub mod labels;
mod topology;
pub mod trace;

pub use cell::{Counter, Gauge, Histogram, StateCell};
pub use family::{Family, FamilyInfo, Kind, LabelValue, Labels, State};
pub use groups::{Cache, Cold, Drives, Fetch, Front, Fuse, Hot, Movement, Node};
pub use topology::{Metrics, Topology};

/// A family of counters, one per value of `L`.
pub type CounterVec<L> = Family<L, Counter>;
/// A family of gauges, one per value of `L`.
pub type GaugeVec<L> = Family<L, Gauge>;
/// A family of histograms, one per value of `L`.
pub type HistogramVec<L> = Family<L, Histogram>;
/// A family of state sets, one per value of `L`, whose states are the values of `S`.
pub type StateSetVec<L, S> = Family<L, StateCell<S>>;
