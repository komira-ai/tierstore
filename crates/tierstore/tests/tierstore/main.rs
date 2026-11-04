//! Behaviour tests of the proxy and the entry reconciler, over in-memory backends behind fault
//! injectors, and, with the `s3` feature, of the S3 backend over the loopback S3 service.

mod backends;
mod cache;
#[cfg(feature = "server")]
mod cache_over_proxy;
mod capacity;
mod churn;
mod cold_stream;
mod conformance;
mod explorer;
mod faults;
mod format_pause;
mod killed_put;
mod list_uploads;
mod list_uploads_reserved;
mod multipart;
mod open_read;
mod recheck;
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
