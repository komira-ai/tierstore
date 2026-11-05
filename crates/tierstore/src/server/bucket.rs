//! The calls on the bucket itself: the bucket is one the server is configured with, so a call
//! about it is answered from the settings, and none of them reaches the backend.

use hyper::StatusCode;

use super::Server;
use super::reply::{Answer, xml_answer};
use super::request::{Query, Target};
use super::xml;

/// Whether the request is GetBucketLocation: a `GET` of the bucket with `location` (S3 reads
/// `?location` and `?location=` alike).
pub(super) fn asks_location(method: &str, target: &Target, query: &Query) -> bool {
    method == "GET" && *target == Target::Bucket && query.has("location")
}

impl Server {
    /// GetBucketLocation: the region of the server. S3 gives an empty `LocationConstraint` for
    /// `us-east-1`, and the name of the region for every other.
    pub(super) fn location(&self) -> Answer {
        let region = self.config.region.as_str();
        xml_answer(StatusCode::OK, xml::location(region))
    }
}
