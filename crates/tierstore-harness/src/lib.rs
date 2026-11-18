//! The shared fakes and protocol models of the test plan of `docs/design/fetch-cache-serve.md`
//! (revision 7, section 14), task T10a of section 18: everything the table needs that is not a
//! farm.
//!
//! The frozen crates already carry the fakes the interface freeze (T-I) needs: the injected
//! clock and the scripted upstream, engine and entry store of `tierstore-fetch-api`, and the fake
//! storage node of `tierstore-store-api`. This crate adds what the rows of section 14 need beyond
//! them, and the models that make a protocol of the design a test:
//!
//! - [`progress`]: progress barriers: counters, named events and gates that a test awaits
//!   instead of time.
//! - [`net`]: a simulated network between storage nodes: messages in flight that the driver
//!   delivers in any order, loses, or drops at a cut link (symmetric and asymmetric partitions).
//! - [`upstream`]: an `Upstream` whose parts deliver their cells only when the test releases
//!   them, in any order across parts, with per-cell corruption and per-part failures.
//! - [`disk`]: a fake disk with a durable and a volatile layer and crash points, and a fake
//!   drive on the injected clock with scripted service times, hangs, errors and crashes.
//! - [`conformance`]: the `EntryStore` conformance suite, run here on the in-memory store and by
//!   every implementation on its own.
//! - [`models`]: exhaustive models: the 5.1 reader/evictor protocol and the claim and publish of
//!   a cell under loom, and the storage tier's coordinator protocols (majority seal, replication
//!   and the ack rule, the definitive miss) under the breadth-first explorer of
//!   `tierstore-model`.
//!
//! The rows of section 14 that this crate proves, and the fake each other row runs on, are
//! listed in the documentation of [`models`] and [`conformance`].

#![forbid(unsafe_code)]

pub mod conformance;
pub mod disk;
pub mod models;
pub mod net;
pub mod progress;
pub mod upstream;
