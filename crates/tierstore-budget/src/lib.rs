//! The budget of a build node (`docs/design/fetch-cache-serve.md`, 4.3), the implementation of
//! the frozen [`tierstore_fetch_api::budget::Budget`] (section 18, row T4).
//!
//! The budget is flow control against what each device and each storage node advertise, not a
//! model of them: admission is the storage nodes' (10.7). It keeps three levels of state:
//! - **per drive** ([`DeviceView`]): `per_drive_max[d]`, decreased only by signals drive d
//!   produced (its `503`s, its first-byte times, its reported queue wait, the load a `LOCATE`
//!   answer gives for it), with its own ladder: permits `2 -> 1`, then pacing of part starts up to
//!   one per 2 s;
//! - **per storage node** ([`NodeView`]): the parts in flight to the node, decreased only by
//!   evidence about the node as a whole (over a third of its drives with traffic in a decrease
//!   state, or its network time per MiB above twice its baseline for two windows);
//! - **the client ceiling**: the parts in flight in all, from the quantities of 4.3
//!   ([`Quantities`]), decreased only by client-wide evidence (its own network signal, or over a
//!   third of all drives with traffic in a decrease state); its ladder is fewer parts, then `P`
//!   toward 16 MiB.
//!
//! A degraded read is charged `k` parts against the ceiling and its node and one permit on each
//! source drive. The demand reserve (`max(1 part, 25%)` of the ceiling) is held back from
//! `prepare` and `background`, and one permit of a drive is held for `demand` while demand waits
//! for it. The jobscheduler's hint of active clients (`Observation::ClientsActive`) divides the
//! share while it is fresh (10 s); without it the share divides by `N`.
//!
//! A client sends no pings: every signal arrives on an answer to a request it made, and a device
//! it has no traffic with has no state.
//!
//! By module:
//! - [`config`]: the topology and the constants, and their validation.
//! - [`quantities`]: the table of 4.3, pure.
//! - [`signal`]: windows, medians and windowed-minimum baselines, pure.
//! - `controller`, `device`, `level` (private): the three levels, their ladders and the admission
//!   rule, a pure state machine fed the time.
//! - [`ClientBudget`]: the controller behind a lock and the injected timer, as `Budget`.

#![forbid(unsafe_code)]

mod budget;
pub mod config;
mod controller;
mod device;
mod level;
pub mod quantities;
pub mod signal;
mod snapshot;

pub use budget::ClientBudget;
pub use config::{BudgetConfig, CEILING_FLOOR, ConfigError, Topology};
pub use device::DriveStep;
pub use level::LevelStep;
pub use quantities::Quantities;
pub use snapshot::{DeviceView, NodeView, Snapshot};
