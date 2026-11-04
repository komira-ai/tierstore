//! The S3 front of the tiering proxy and its binary: an S3 client against the server over
//! loopback, requests signed by hand, the drain, and the `tierstore` process.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod binary;
mod cache_node;
mod client;
mod combined_node;
mod config_file;
mod drain;
mod files;
mod idle;
mod listeners;
mod raw;
mod roles;
mod run;
mod sdk;
mod signer;
mod stalled;
mod support;
