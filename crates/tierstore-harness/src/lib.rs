//! The shared fakes and protocol models of the test plan of `docs/design/fetch-cache-serve.md`
//! (revision 7, section 14), task T10a of section 18: what the table needs that is not a farm.
//!
//! The frozen crates already carry the fakes of the interface freeze: the injected clock and the
//! scripted upstream, engine and entry store of `tierstore-fetch-api`, and the fake storage node
//! of `tierstore-store-api`. This crate adds what the rows of section 14 need beyond them, and the
//! models that make a protocol of the design a test:
//!
//! - [`progress`]: progress barriers (counters, named events, gates) that a test awaits instead
//!   of time.
//! - [`net`]: a simulated network between storage nodes: messages in flight that the driver
//!   delivers in any order or loses, and directed cut links (symmetric and asymmetric
//!   partitions).
//! - [`upstream`]: `CellUpstream`, an `Upstream` whose parts yield their cells only when the test
//!   releases them, in any order across parts, with per-cell corruption and per-part failures.
//! - [`disk`]: `FakeDisk`, a disk with a durable and a volatile layer, injected faults and a
//!   driver that crashes a protocol before (and after) each of its operations; and `FakeDrive`,
//!   its operations served on the injected clock with scripted service times, errors, hangs and
//!   crashes.
//! - [`conformance`]: the `EntryStore` conformance suite, run here on the in-memory store of
//!   `tierstore-fetch-api`; the Linux and the in-process implementations each run it on their own
//!   fixture.
//! - [`models`]: exhaustive models: the 5.1 reader/evictor protocol and the claim and publish of
//!   a cell under loom ([`models::evict`]); the hot tier's replication, ack rule and majority seal
//!   under the breadth-first explorer of `tierstore-model` ([`models::seal`]); the definitive-miss
//!   rule, its exhaustive check and the `LOCATE` coordinator ([`models::locate`]).
//!
//! # The rows of section 14
//!
//! Rows proved here, by this crate's tests (the module named holds the test's subject):
//!
//! | Row (first words) | Where |
//! |---|---|
//! | Loom: reader/evictor protocol with per-cell CAS ... | [`models::evict`] |
//! | Reader with a forced loss of its counter | [`models::evict`] |
//! | Live reader stuck beyond `evict_wait` (the protocol: cells restored, nothing journaled, the timeout counted) | [`models::evict`] |
//! | Hedge and original both deliver a cell (the shared token) | [`models::evict`] |
//! | `EntryStore` conformance suite (on the in-memory store) | [`conformance`] |
//! | Mount killed (lifeline hangup); a mount grows its counter memfd; more than 128 holders (the in-process part) | [`conformance`] |
//! | Replication protocol under scripted crashes and message loss | [`models::seal`] |
//! | Two sealers under a partition (deterministic simulation) | [`models::seal`] |
//! | Weaker acknowledgement is documented behaviour | [`models::seal`] |
//! | Seal and roll (the minority part: no seal, 503) | [`models::seal`] |
//! | Definitive miss (rule `N - min_acks + 1`) | [`models::locate`] |
//! | Pack lost mid-job (the coordinator's part: wait while not definitive, lost on a miss) | [`models::locate`] |
//!
//! Every other row is made to pass by the task of section 18 that implements its subject, on
//! these fakes and those of the frozen crates:
//!
//! - the fetch engine, the budget, the verifier and the S3 front (T3, T4, T2, T12: single
//!   flight, claims, reverse-order completion, the 1 GiB span, ramp and grid, budget and
//!   controller, retries and stalls, hedging, demand reserve, fairness, corrupt cell, 412 per
//!   copy, pinned generation, degraded charge, S3-front object): [`upstream::CellUpstream`],
//!   [`progress`] and the injected clock;
//! - the journal, the eviction and the entry store on Linux (T1, T1b, T8: crash orderings,
//!   batched PUNCH, boot id, journal batching, held-only cache, grace window): [`disk`],
//!   [`conformance`];
//! - the mount protocol and client (T5, T6: epoch, wait coalescing, blocked readers): the fake
//!   engine of `tierstore-fetch-api` and [`progress`];
//! - the storage tier (T-S1 to T-S12: group commit, seal triggers, large PUT, create-only, stamps,
//!   index recovery, LIST, placement, erasure code, destage, conversion, backpressure, promote,
//!   degraded read, rebuild, scrub, GC, hot-only mode, the drive scheduler, hung drive, slow
//!   consumer, I/O errors, view states, audits, profiles and migrations): [`disk`] (crash points
//!   and the fake drive), [`net`], the fake storage node of `tierstore-store-api`, and the
//!   explorer the models here run on;
//! - the format rows (T-FMT, T-S2), the FUSE rows (T-F1 to T-F7, which need the kernel), the
//!   CAS service rows (T-K1a) and the macOS run (T-M2) need no fake of this crate.
//!
//! A few small functions carry `#[inline(never)]`: the crate is optimised in test builds (its
//! models explore every interleaving), and inlined into the test binaries their library copy
//! would count as never run in the coverage report.

#![forbid(unsafe_code)]

pub mod conformance;
pub mod disk;
pub mod models;
pub mod net;
pub mod progress;
pub mod upstream;
