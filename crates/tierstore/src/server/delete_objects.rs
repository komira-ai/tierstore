//! DeleteObjects (`POST /<bucket>?delete`): up to 1000 keys in one call, each deleted by the
//! single-key path ([`Backend::delete`](crate::Backend::delete)), with a result for each.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::iter;
use hyper::StatusCode;
use hyper::body::Body;
use hyper::header::HeaderMap;
use hyper::http::request::Parts;

use crate::Error;
use crate::journal::is_reserved;

use super::Server;
use super::ops::detached;
use super::reply::{Answer, S3Error, xml_answer};
use super::request::{BoxError, Check, Reader};
use super::xml::{self, Outcome};

/// The most keys one call may name; S3's own limit.
pub(super) const DELETE_KEYS: usize = 1000;

/// The most bytes of the document of a call: 1000 keys of the longest key S3 takes (1024 bytes,
/// each byte at worst the six bytes of `&quot;`), with room for the elements around each.
pub(super) const DELETE_BODY_LIMIT: usize = DELETE_KEYS * (1024 * 6 + 128);

/// The headers that carry a hash of the document, one of which S3 needs on this call.
const INTEGRITY: [&str; 6] = [
    "content-md5",
    "x-amz-checksum-crc32",
    "x-amz-checksum-crc32c",
    "x-amz-checksum-crc64nvme",
    "x-amz-checksum-sha1",
    "x-amz-checksum-sha256",
];

/// What the call says to do with one key of its document, once the keys that cannot be deleted
/// are told from the rest.
enum Plan {
    Delete,
    Refuse(Outcome),
}

/// The outcome of the delete of one key.
async fn delete_one(backend: &dyn crate::Backend, key: &str) -> Outcome {
    match backend.delete(key).await {
        Ok(()) => Outcome::Deleted,
        Err(error) => failed(error),
    }
}

/// The outcome a failed call is told as: the code and the text a single delete of the key gives.
/// What the store said goes to the log, as it does for a single call.
fn failed(error: Error) -> Outcome {
    let refusal = S3Error::from(error);
    if let Some(why) = refusal.detail() {
        tracing::warn!("tierstore: DeleteObjects: a key failed: {why}");
    }
    Outcome::Failed {
        code: refusal.code(),
        message: refusal.message().to_owned(),
    }
}

/// Whether the request has a header that carries a hash of its body.
fn has_integrity(headers: &HeaderMap) -> bool {
    INTEGRITY.iter().any(|name| headers.contains_key(*name))
}

fn malformed() -> S3Error {
    let text = "The XML you provided was not well-formed or did not validate against our \
                published schema.";
    S3Error::new(StatusCode::BAD_REQUEST, "MalformedXML", text)
}

impl Server {
    /// Deletes the keys of the document, up to [`ServerConfig::delete_concurrency`] at a time,
    /// and answers with the `DeleteResult`: a `Deleted` for each key that is deleted (a key that
    /// is not there is deleted, as S3 has it), left out when the call is `Quiet`, and an `Error`
    /// for each key that is not, with the code of the failure of a single delete of it. A key of
    /// the proxy (`_journal/`, `_tiering/`, `_entries/`) is `AccessDenied` and a key with a
    /// version id is `NotImplemented`; neither is passed to the backend. A key named twice is
    /// deleted once and answered twice. The deletes run on a task of their own, as a single
    /// delete does, so that a client that goes away does not leave the batch half done.
    ///
    /// The body is checked against its hashes as it is read, and the call needs one of them
    /// (`Content-MD5` or an `x-amz-checksum-*` header), as S3 does.
    ///
    /// [`ServerConfig::delete_concurrency`]: super::ServerConfig::delete_concurrency
    pub(super) async fn delete_objects<B>(
        &self,
        parts: &Parts,
        reader: Reader<B>,
        check: Check,
    ) -> Result<Answer, S3Error>
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        if !has_integrity(&parts.headers) {
            let text = "Missing required header for this request: Content-MD5 or \
                        x-amz-checksum-*.";
            return Err(S3Error::new(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                text,
            ));
        }
        let body = reader.read_all(check).await?;
        let asked = xml::parse_delete(&body).ok_or_else(malformed)?;
        if asked.objects.is_empty() || asked.objects.len() > DELETE_KEYS {
            return Err(malformed());
        }
        let plans: Vec<Plan> = asked.objects.iter().map(plan).collect();
        let mut seen = HashSet::new();
        let doomed: Vec<String> = asked
            .objects
            .iter()
            .zip(&plans)
            .filter(|(_, plan)| matches!(plan, Plan::Delete))
            .filter(|(object, _)| seen.insert(object.key.clone()))
            .map(|(object, _)| object.key.clone())
            .collect();
        let backend = Arc::clone(&self.backend);
        let width = self.config.delete_concurrency.max(1);
        let done = detached(async move {
            let deletes = doomed.into_iter().map(|key| {
                let backend = Arc::clone(&backend);
                async move {
                    let outcome = delete_one(backend.as_ref(), &key).await;
                    (key, outcome)
                }
            });
            let outcomes: HashMap<String, Outcome> = iter(deletes)
                .buffer_unordered(width)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect();
            Ok(outcomes)
        })
        .await
        .map_err(S3Error::from)?;
        let results: Vec<(&str, Outcome)> = asked
            .objects
            .iter()
            .zip(plans)
            .map(|(object, plan)| {
                let outcome = match plan {
                    Plan::Refuse(outcome) => outcome,
                    Plan::Delete => done[&object.key].clone(),
                };
                (object.key.as_str(), outcome)
            })
            .collect();
        let xml = xml::delete_result(&results, asked.quiet);
        Ok(xml_answer(StatusCode::OK, xml))
    }
}

/// What the call does with `object`: a key of the proxy or a version id is refused for that key
/// alone.
fn plan(object: &xml::DeleteObject) -> Plan {
    if is_reserved(&object.key) {
        return Plan::Refuse(failed(Error::Reserved(object.key.clone())));
    }
    if object.versioned {
        return Plan::Refuse(Outcome::Failed {
            code: "NotImplemented",
            message: "versions are not supported: this bucket has none.".to_owned(),
        });
    }
    Plan::Delete
}
