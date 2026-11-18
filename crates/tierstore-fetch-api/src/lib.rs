//! The interfaces of the read path on a build node, as sections 2 to 9 of
//! `docs/design/fetch-cache-serve.md` (revision 7) state them, frozen by the interface-freeze
//! change (section 18, row T-I), with in-tree fakes so every task of section 18 can run against
//! them before the real implementations land.
//!
//! What is here, by module:
//! - [`ids`]: pack identity (`PackId = (key, pack_root)`), holders, jobs, ranks, classes, cell
//!   runs and byte ranges (section 2, 4.5).
//! - [`cell`]: the cell byte (low 3 bits state, bit 7 `durable`) and the transitions the state
//!   table of section 2 allows, each a CAS from a named predecessor.
//! - [`page`]: the layout of the state page a mount maps read-only, the seals that make it
//!   read-only, and the counter regions of a mount's counter memfd (5.1).
//! - [`entry`]: `StateView`, `ReaderCounters`, `HolderCounters` (the server's view of a holder's
//!   counter memfd), `Entry` (cells, journaled `punch`, `counts`, `touch`, `read_cell`, `fds`) and
//!   `EntryStore` (`attach`, `register_holder`, `drop_holder`, `entries`) (5.1, 5.6, 7, 8).
//! - [`list`]: `ListL2` and `ListVersionUnsupported` (section 3).
//! - [`verify`]: `Verifier`, the hash pool's interface (3.3, 4.7).
//! - [`upstream`]: `Upstream` and what a location, a part request and a part response carry
//!   (4.6, 10.6).
//! - [`budget`]: `Budget`, the client side of the three-level flow control (4.3).
//! - [`engine`]: `Engine`: `ensure` and `wait` (4.1, 5.4).
//! - [`wire`]: the mount-to-tierstore protocol: `open-pack` with counter regions, `wait` with
//!   `lost`, `ensure`, `release`, epoch, the descriptors each message carries (5.4).
//! - [`clock`]: the injected clock every timed decision reads, and its manual fake (section 14).
//! - [`fake`]: an upstream with scripted answers, an engine with scripted waits, and an
//!   in-memory entry store with a holder's counters in memory (`MemCounters`).
//!
//! Each trait's documentation names the section 14 rows its implementations must pass.

#![forbid(unsafe_code)]

pub mod budget;
pub mod cell;
pub mod clock;
pub mod engine;
pub mod entry;
pub mod fake;
pub mod ids;
pub mod list;
pub mod page;
pub mod upstream;
pub mod verify;
pub mod wire;

/// The retriable failure cause the mount provider reports to the jobscheduler for every job that
/// holds a tree whose fill was lost (4.6; T-J1 requeues such a job).
pub const TREE_LOST_CAUSE: &str = "tree-lost";
