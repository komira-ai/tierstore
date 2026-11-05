//! The calls on the bucket itself: the bucket is one the server is configured with, so a call
//! about it is answered from the settings, and none of them reaches the backend.

use hyper::StatusCode;

use crate::Backend;
use crate::journal::FORMAT_KEY;

use super::Server;
use super::reply::{Answer, S3Error, xml_answer};
use super::request::{Query, Target};
use super::xml;

/// Whether the request is GetBucketLocation: a `GET` of the bucket with `location` (S3 reads
/// `?location` and `?location=` alike).
pub(super) fn asks_location(method: &str, target: &Target, query: &Query) -> bool {
    method == "GET" && *target == Target::Bucket && query.has("location")
}

/// Whether the request is ListBuckets: a `GET` of the service root.
pub(super) fn asks_buckets(method: &str, target: &Target) -> bool {
    method == "GET" && *target == Target::Root
}

/// The parameters of ListBuckets that this front answers; `x-id` is the name the SDKs add.
const BUCKET_PARAMS: [&str; 5] = [
    "bucket-region",
    "continuation-token",
    "max-buckets",
    "prefix",
    "x-id",
];

/// The most buckets a page of ListBuckets may be asked to have (`max-buckets`, from 1 to this).
const MAX_BUCKETS: usize = 10_000;

/// When the bucket was made, for the `CreationDate` of ListBuckets: the time the store's
/// `_tiering/format` object was last written, which for a store whose format change is done is
/// the time the change was made active, and which the backend gives as the object's modified
/// time; `fallback` where the store has no such object (a store never changed) or does not
/// answer. It is read once, at the start, and nothing about it is stored.
pub(super) async fn created_at(hot: &dyn Backend, fallback: u64) -> u64 {
    match hot.head(FORMAT_KEY).await {
        Ok(meta) => meta.modified,
        Err(_) => fallback,
    }
}

impl Server {
    /// ListBuckets: the one bucket the server serves, narrowed as S3 narrows a list: `prefix`
    /// keeps the bucket if its name starts with it, `bucket-region` if it is in that region.
    /// `max-buckets` is checked and changes nothing, since one bucket never fills a page; the
    /// list is never cut, so the server gives no continuation token and refuses one.
    pub(super) fn list_buckets(&self, query: &Query) -> Result<Answer, S3Error> {
        if !query.only(&BUCKET_PARAMS) {
            return Err(S3Error::unsupported(
                "this query parameter is not supported",
            ));
        }
        let page = query.get("max-buckets").map(str::parse::<usize>);
        if !matches!(page, Some(Ok(1..=MAX_BUCKETS)) | None) {
            return Err(S3Error::invalid(
                "max-buckets must be a whole number from 1 to 10000",
            ));
        }
        if query.has("continuation-token") {
            return Err(S3Error::invalid(
                "the continuation token is not one this server gave",
            ));
        }
        let (bucket, region) = (&self.config.bucket, &self.config.region);
        let named = bucket.starts_with(query.get("prefix").unwrap_or_default());
        let placed = query
            .get("bucket-region")
            .is_none_or(|asked| asked == region);
        Ok(xml_answer(
            StatusCode::OK,
            xml::buckets(bucket, region, self.config.bucket_created, named && placed),
        ))
    }

    /// GetBucketLocation: the region of the server. S3 gives an empty `LocationConstraint` for
    /// `us-east-1`, and the name of the region for every other.
    pub(super) fn location(&self) -> Answer {
        let region = self.config.region.as_str();
        xml_answer(StatusCode::OK, xml::location(region))
    }
}
