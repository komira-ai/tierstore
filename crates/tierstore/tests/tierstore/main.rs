//! Behaviour tests of the proxy and the reconciler, over in-memory backends behind fault
//! injectors, and, with the `s3` feature, of the S3 backend over the loopback S3 service.

mod backends;
mod cache;
mod capacity;
#[cfg(feature = "s3")]
mod chain;
mod conformance;
mod faults;
mod hold;
mod journal;
mod killed_put;
mod lease;
mod lease_takeover;
mod list_uploads;
mod list_uploads_reserved;
mod multipart;
mod one_winner;
mod recheck;
mod reconciler;
mod reconciler_faults;
#[cfg(feature = "s3")]
mod s3;
mod streaming;
mod support;
mod tiered;
