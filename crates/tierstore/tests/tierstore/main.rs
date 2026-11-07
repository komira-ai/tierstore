//! Behaviour tests of the proxy and the entry reconciler, over in-memory backends behind fault
//! injectors, and, with the `s3` feature, of the S3 backend over the loopback S3 service.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod backends;
mod cache;
#[cfg(feature = "server")]
mod cache_over_proxy;
#[cfg(feature = "server")]
mod cache_upstreams;
mod capacity;
mod cold_reads;
mod cold_stream;
mod conformance;
mod explorer;
mod faults;
mod format_pause;
mod hold;
mod killed_put;
mod list_parts;
mod list_uploads;
mod list_uploads_reserved;
mod marks_e2e;
mod multipart;
mod open_read;
mod recheck;
#[cfg(feature = "s3")]
mod s3;
#[cfg(feature = "s3")]
mod s3_conditional_delete;
#[cfg(feature = "server")]
mod s3_keys;
#[cfg(feature = "s3")]
mod s3_streamed;
#[cfg(feature = "s3")]
mod s3_tagged;
mod streamed;
mod streaming;
mod support;
mod tagged_read;
mod tiered;
