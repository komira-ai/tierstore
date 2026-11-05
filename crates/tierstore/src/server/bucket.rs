//! The calls on the bucket itself: the bucket is one the server is configured with, so a call
//! about it is answered from the settings, and none of them reaches the backend.

use bytes::Bytes;
use hyper::StatusCode;

use crate::Backend;
use crate::journal::FORMAT_KEY;

use super::Server;
use super::reply::{Answer, S3Error, answer, xml_answer};
use super::request::{Query, Target};
use super::xml;

/// How S3 answers a read of a setting of a bucket on which nothing is set.
pub(super) enum Unset {
    /// 200 and an empty document with this root element.
    Empty(&'static str),
    /// 200 and the payer, the bucket owner.
    Payer,
    /// 200 and the ACL that gives the owner full control.
    Acl,
    /// 404 with this S3 error code and text.
    Missing(&'static str, &'static str),
}

/// The settings of a bucket that can be read (`GET /<bucket>?<name>`), and what S3 answers for a
/// bucket on which none is set. This server keeps none: the bucket is made by configuration and
/// a setting cannot be written (a write of one is `NotImplemented`), so every read gets this.
/// The encryption is among them on purpose: S3 now encrypts every bucket with its own keys and
/// says so, but tierstore does not encrypt what it stores, and does not say it does.
pub(super) const UNSET: [(&str, Unset); 17] = [
    ("versioning", Unset::Empty("VersioningConfiguration")),
    ("notification", Unset::Empty("NotificationConfiguration")),
    ("accelerate", Unset::Empty("AccelerateConfiguration")),
    ("logging", Unset::Empty("BucketLoggingStatus")),
    ("requestPayment", Unset::Payer),
    ("acl", Unset::Acl),
    (
        "object-lock",
        Unset::Missing(
            "ObjectLockConfigurationNotFoundError",
            "Object Lock configuration does not exist for this bucket",
        ),
    ),
    (
        "policy",
        Unset::Missing("NoSuchBucketPolicy", "The bucket policy does not exist"),
    ),
    (
        "policyStatus",
        Unset::Missing("NoSuchBucketPolicy", "The bucket policy does not exist"),
    ),
    (
        "lifecycle",
        Unset::Missing(
            "NoSuchLifecycleConfiguration",
            "The lifecycle configuration does not exist",
        ),
    ),
    (
        "cors",
        Unset::Missing(
            "NoSuchCORSConfiguration",
            "The CORS configuration does not exist",
        ),
    ),
    (
        "encryption",
        Unset::Missing(
            "ServerSideEncryptionConfigurationNotFoundError",
            "The server side encryption configuration was not found",
        ),
    ),
    (
        "tagging",
        Unset::Missing("NoSuchTagSet", "The TagSet does not exist"),
    ),
    (
        "replication",
        Unset::Missing(
            "ReplicationConfigurationNotFoundError",
            "The replication configuration was not found",
        ),
    ),
    (
        "website",
        Unset::Missing(
            "NoSuchWebsiteConfiguration",
            "The specified bucket does not have a website configuration",
        ),
    ),
    (
        "ownershipControls",
        Unset::Missing(
            "OwnershipControlsNotFoundError",
            "The bucket ownership controls were not found",
        ),
    ),
    (
        "publicAccessBlock",
        Unset::Missing(
            "NoSuchPublicAccessBlockConfiguration",
            "The public access block configuration was not found",
        ),
    ),
];

/// The setting of the bucket that a request reads.
pub(super) enum Setting {
    Location,
    Unset(&'static Unset),
}

/// The setting of the bucket that the request reads, if it is a `GET` of the bucket with
/// `location` (GetBucketLocation) or one of the names of [`UNSET`], with or without a value, as
/// S3 reads them. When it names more than one, `location` comes first and then the order of
/// [`UNSET`].
pub(super) fn asks_setting(method: &str, target: &Target, query: &Query) -> Option<Setting> {
    if method != "GET" || *target != Target::Bucket {
        return None;
    }
    if query.has("location") {
        return Some(Setting::Location);
    }
    let found = UNSET.iter().find(|(name, _)| query.has(name));
    found.map(|(_, unset)| Setting::Unset(unset))
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

    /// A read of a setting of the bucket: GetBucketLocation, which is the region of the server
    /// (S3 gives an empty `LocationConstraint` for `us-east-1`, and the name of the region for
    /// every other), or what S3 answers for a setting that is not set.
    pub(super) fn setting(&self, setting: &Setting) -> Result<Answer, S3Error> {
        let unset = match setting {
            Setting::Location => {
                let region = self.config.region.as_str();
                return Ok(xml_answer(StatusCode::OK, xml::location(region)));
            }
            Setting::Unset(unset) => unset,
        };
        match unset {
            Unset::Empty(root) => Ok(xml_answer(StatusCode::OK, xml::empty_setting(root))),
            Unset::Payer => Ok(xml_answer(StatusCode::OK, xml::payer())),
            Unset::Acl => Ok(xml_answer(StatusCode::OK, xml::acl())),
            Unset::Missing(code, text) => Err(S3Error::new(StatusCode::NOT_FOUND, code, *text)),
        }
    }

    /// HeadBucket: the bucket is there, and `x-amz-bucket-region` says where, which the SDKs
    /// read to find the endpoint of a bucket.
    pub(super) fn head_bucket(&self) -> Answer {
        let headers = [("x-amz-bucket-region", self.config.region.clone())];
        answer(StatusCode::OK, &headers, Bytes::new())
    }

    /// CreateBucket of the bucket the server serves (a bucket of another name is
    /// `NoSuchBucket` before this): it exists, so, as S3 does for a bucket one owns, it is
    /// 200 with `Location` in `us-east-1` and 409 `BucketAlreadyOwnedByYou` in every other
    /// region. A `LocationConstraint` that is not the region of the server (or empty, which
    /// is `us-east-1`'s way of saying nothing) is 400 `IllegalLocationConstraintException`, and a
    /// body that is not a `CreateBucketConfiguration` is `MalformedXML`.
    pub(super) fn create_bucket(&self, body: &[u8]) -> Result<Answer, S3Error> {
        let Some(asked) = xml::parse_create(body) else {
            let text = "The XML you provided was not well-formed or did not validate against \
                        our published schema.";
            return Err(S3Error::new(StatusCode::BAD_REQUEST, "MalformedXML", text));
        };
        let region = self.config.region.as_str();
        if !asked.is_empty() && asked != region {
            let text = format!(
                "The {asked} location constraint is incompatible for the region specific \
                 endpoint this request was sent to."
            );
            let code = "IllegalLocationConstraintException";
            return Err(S3Error::new(StatusCode::BAD_REQUEST, code, text));
        }
        if region != "us-east-1" {
            let text = "Your previous request to create the named bucket succeeded and you \
                        already own it.";
            let code = "BucketAlreadyOwnedByYou";
            return Err(S3Error::new(StatusCode::CONFLICT, code, text));
        }
        let headers = [("location", format!("/{}", self.config.bucket))];
        Ok(answer(StatusCode::OK, &headers, Bytes::new()))
    }

    /// DeleteBucket: S3 refuses a bucket that has objects, 409 `BucketNotEmpty`, and removes one
    /// that has none. This bucket is set up by the configuration of the server, so a call does
    /// not remove it: an empty bucket is 403 `AccessDenied` and stays. Nothing is deleted.
    pub(super) async fn delete_bucket(&self) -> Result<Answer, S3Error> {
        let found = self.backend.list_page("", None, 1).await?;
        if found.is_empty() {
            let text = "The bucket is set up by the configuration of this server and is not \
                        deleted by a call.";
            return Err(S3Error::new(StatusCode::FORBIDDEN, "AccessDenied", text));
        }
        let text = "The bucket you tried to delete is not empty.";
        Err(S3Error::new(StatusCode::CONFLICT, "BucketNotEmpty", text))
    }
}
