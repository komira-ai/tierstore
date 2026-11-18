//! The interfaces of the tierstore storage tier, as section 10 of
//! `docs/design/fetch-cache-serve.md` (revision 7) states them, frozen by the interface-freeze
//! change (section 18, row T-I), with a fake storage node so every task of the storage tier, and
//! the read path's `StoreUpstream`, can run against them before the real nodes exist.
//!
//! The rules of section 10 are part of the interfaces: content-derived generation, the cluster
//! stamp in every record and the liveness rule, create-only by the index under a key lock with the
//! all-node check (`Index`, `HotStore`); the replication ballots, the majority seal and the ack
//! rule (`HotStore`); the placement invariant and stripe descriptors (`Placement`, `ColdStore`);
//! the watchdog, scheduler liveness, read-ahead and slot release, the write share, the small lane
//! and threads = streams x depth (`DriveSched`); `LOCATE`, per-copy `If-Match` and locations that
//! carry generations (`PeerClient`).
//!
//! What is here, by module:
//! - [`view`]: the membership view (nodes, drives, their states, the cluster settings) and the
//!   quorum rules (definitive miss, majority commit).
//! - [`placement`]: `Placement`: node rank by rendezvous hashing and the stripe placement.
//! - [`index`]: `Index`: the per-node ordered map of keys, the liveness rule, replay.
//! - [`hot`]: `HotStore`: append and group commit, large files, the majority seal, replication.
//! - [`cold`]: `ColdStore`: shard writes, range and degraded reads, the stripe free.
//! - [`sched`]: `DriveSched`: the per-drive I/O scheduler.
//! - [`proto`]: the peer protocol: routes, headers, and the messages of every route.
//! - [`peer`]: `PeerClient`: the typed client of the peer protocol.
//! - [`fake`]: a fake storage node with scripted service times, hangs, errors and crashes, on an
//!   injected clock.
//!
//! Each trait's documentation names the section 14 rows its implementations must pass.

#![forbid(unsafe_code)]

pub mod cold;
pub mod fake;
pub mod hot;
pub mod index;
pub mod peer;
pub mod placement;
pub mod proto;
pub mod sched;
pub mod view;
