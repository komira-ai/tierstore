//! Behaviour tests of the proxy and the reconciler, over in-memory backends behind fault
//! injectors, and, with the `s3` feature, of the S3 backend over the loopback S3 service.

mod backends;
mod cache;
#[cfg(feature = "server")]
mod cache_over_proxy;
mod capacity;
#[cfg(feature = "s3")]
mod chain;
mod churn;
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
mod marked_reads;
mod mirror_tags;
mod multipart;
mod one_winner;
mod recheck;
mod reconciler;
mod reconciler_faults;
mod reconciler_races;
mod reconciler_reservations;
mod reconciler_takeover;
mod registration_gc;
mod reservation;
mod reservation_reads;
#[cfg(feature = "s3")]
mod s3;
#[cfg(feature = "s3")]
mod s3_conditional_delete;
#[cfg(feature = "s3")]
mod s3_reservation;
#[cfg(feature = "s3")]
mod s3_streamed;
mod streamed;
mod streaming;
mod support;
mod tiered;
