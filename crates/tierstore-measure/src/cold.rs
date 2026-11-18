//! Arms C-2 to C-4 and the review's additions: a prototype of the cold tier's read path (a
//! per-drive scheduler serving pack ranges over HTTP), and the loads run against it: one drive
//! streaming whole packs or parts with demand reads arriving, two copies (striped, hedged),
//! local against peer reads, first-byte probes idle and loaded, a mixed read and write load, and
//! slow consumers.

pub mod arms;
pub mod client;
pub mod http;
pub mod sched;
pub mod server;
