//! The fetch engine of a build node (`docs/design/fetch-cache-serve.md`, section 4), the
//! implementation of the frozen [`tierstore_fetch_api::engine::Engine`] (section 18, row T3).
//!
//! One [`FetchEngine`] serves demand reads, mount Prepare and S3-front misses. What it does, by
//! module:
//! - [`plan`]: part sizing and the grid-relative ramp (4.4).
//! - [`queue`]: the queue of a class: deficit round robin across jobs, then `(rank, offset)`, and
//!   the active-pack window (4.5).
//! - [`route`]: which copy serves a part, a retry and a hedge (4.6, 10.6).
//! - [`baseline`]: the first-byte and network baselines its timeouts are relative to (4.6).
//! - `engine`, `fill`, `resolve` and `op` (private): single flight per pack (4.1), claims and
//!   demand sub-ranges (4.2, 4.5), the `LOCATE` step, the pinned generation and the lost path
//!   (4.6, 10.6), and the attempts of a part: stall retries, hedges, `If-Match`, per-cell
//!   verification failure with failover (4.6).
//! - `pool` and `gate` (private): the pool of 1 MiB cell buffers parts are received into, and the
//!   dirty-byte cap a cell waits on before its publish, both taken `demand` first (4.8).
//! - [`timer`]: the injected clock the engine sleeps on.
//! - [`stats`]: what it counts, and the progress events tests wait on.
//!
//! [`plan`], [`queue`], [`route`] and [`baseline`] are pure (no clock, no I/O) and public, so
//! their rules are tested directly, and so the storage tier's upstream (T-S6) and the farm
//! validation (T10b) use the same copy choice and part plan the engine does.
//!
//! The engine reads every time from the injected [`timer::Timer`]; no decision depends on the
//! machine's clock, so its tests move time by hand.

#![forbid(unsafe_code)]

mod attempt;
pub mod baseline;
pub mod config;
mod engine;
mod fill;
mod gate;
mod op;
pub mod plan;
mod pool;
pub mod queue;
mod resolve;
pub mod route;
mod settle;
pub mod stats;
pub mod timer;

pub use config::EngineConfig;
pub use engine::{EngineParts, FetchEngine};
pub use stats::{GetKind, Stats, TimerKind};
