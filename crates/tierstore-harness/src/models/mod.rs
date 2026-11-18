//! Exhaustive models of the protocols of the design: each states a protocol of the design as
//! small steps, explores every interleaving of a bounded scenario, and records every violation of
//! the protocol's properties; each also has switches that turn one rule of the design off, and the
//! tests show the model reports the matching violation.
//!
//! - [`evict`]: the 5.1 reader/evictor protocol and the claim and publish of a cell, under loom.
//! - [`seal`]: the hot tier's replication, ack rule and majority seal (10.3), under the
//!   breadth-first explorer of `tierstore-model`, on [`crate::net::SimNet`].
//! - [`locate`]: the definitive-miss rule (10.6), checked over every assignment of node states
//!   and copies, and the `LOCATE` coordinator over a `PeerClient`.

pub mod evict;
pub mod locate;
pub mod seal;
