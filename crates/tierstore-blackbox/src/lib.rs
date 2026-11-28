//! The black-box test of a released `tierstore` (README.md): the binary of the package under test,
//! started with a node file of the farm's shape and driven only from outside, through its
//! mount-provider socket, its mount, its `/metricz`, its process and its files, as the job agent
//! drives it (Prepare, Mount, a read of the tree through the mount, Release).
//!
//! Two programs forward here: `tierstore-blackbox`, the driver ([`driver_main`]), and `fake-s3`,
//! the upstream the node is pointed at ([`fake_main`]). The modules the tests of this crate drive
//! with those programs are public: [`os`] (a process the rig starts and stops, its memory, a full
//! disk), [`provider`] (a client of the provider's socket), [`rig`] (what a case asks of a node),
//! [`live`] (that rig over a running node and its upstream), [`tree`] (a tree as a CAS service
//! writes it) and [`metrics`].
//! Everything else is the driver's own.

#![cfg(target_os = "linux")]

mod cases;
mod clock;
mod config;
mod driver;
mod fake;
mod http;
pub mod live;
pub mod metrics;
pub mod os;
pub mod provider;
pub mod rig;
pub mod tree;

pub use driver::main as driver_main;
pub use fake::main as fake_main;
