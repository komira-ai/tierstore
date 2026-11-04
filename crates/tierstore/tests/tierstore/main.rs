//! Behaviour tests of the proxy and the reconciler, over in-memory backends behind fault
//! injectors, and, with the `s3` feature, of the S3 backend over the loopback S3 service.

mod backends;
mod cache;
#[cfg(feature = "server")]
mod cache_over_proxy;
#[cfg(feature = "server")]
mod cache_upstreams;
mod capacity;
#[cfg(feature = "s3")]
mod chain;
mod churn;
mod cold_reads;
mod cold_stream;
mod conformance;
mod faults;
mod format_drain;
mod format_pause;
mod hold;
mod journal;
mod killed_put;
mod lease;
mod lease_takeover;
mod list_uploads;
mod list_uploads_reserved;
mod mirror_tags;
mod multipart;
mod one_winner;
mod open_read;
mod recheck;
mod reconciler;
mod reconciler_copies;
mod reconciler_faults;
mod reconciler_races;
mod reconciler_takeover;
mod registration_gc;
#[cfg(feature = "s3")]
mod s3;
#[cfg(feature = "s3")]
mod s3_conditional_delete;
#[cfg(feature = "s3")]
mod s3_streamed;
#[cfg(feature = "s3")]
mod s3_tagged;
mod streamed;
mod streaming;
mod support;
mod tagged_read;
mod tagged_reads;
mod tiered;
