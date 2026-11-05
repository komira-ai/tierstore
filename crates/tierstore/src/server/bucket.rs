//! The calls on the bucket itself: the bucket is one the server is configured with, so a call
//! about it is answered from the settings, and none of them reaches the backend.

use hyper::StatusCode;

use super::Server;
use super::reply::{Answer, S3Error, xml_answer};
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
const UNSET: [(&str, Unset); 17] = [
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
            xml::buckets(bucket, region, named && placed),
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
}
