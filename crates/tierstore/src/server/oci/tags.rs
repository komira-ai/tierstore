//! Tags: names that point at a manifest, and move.
//!
//! A tag is a directory of versions, `<prefix>tags/<name>/<tag>/<inv>`. The directory of the name
//! is the repository name with each `/` written as `~` (a character no name has), so that no
//! name's directory is inside another's. Each version is a small object whose bytes are the
//! digest of the manifest the tag pointed at then (`sha256:<hex>`), and whose key ends in
//! `<inv>`, the 20 digits of `u64::MAX - n` for the n-th version (the first is `u64::MAX`, each
//! move is one less), so that the newest version sorts first in a listing. The tag is what its
//! newest version holds. The objects are under the front's prefix and not in the blobs'
//! namespace, so no digest names one, and the manifests and blobs stay immutable and named by
//! their content.
//!
//! No stored object is ever overwritten: a tag is moved by writing the next version, a new
//! object, with the store's create-only write, which every store decides for one key with
//! exactly one winner. The proxy therefore needs nothing of its own for tags; its objects stay
//! write-once, and a version is copied to the cold tier and evicted from the hot one as any
//! object is.
//!
//! - `PUT /v2/<name>/manifests/<tag>` stores the manifest by its digest (see `push`), and then
//!   writes the next version of the tag.
//! - `GET` and `HEAD` of `/v2/<name>/manifests/<tag>` read the newest version and answer as the
//!   manifest of its digest does.
//! - `GET /v2/<name>/tags/list` lists the tags of a name, in ascending order, with the `n` and
//!   `last` of the specification for paging (and a `Link` header when a page is not the last).
//!   A name with no tag, and a name nobody has pushed, are the same: 200 and an empty list.
//!   The list reads every version of the name, once, and so costs the versions there are.
//!
//! # Two pushes of one tag
//!
//! A push lists the tag's first version (the newest), reads it, and creates the version after it
//! (the first, if there is none). A push of the digest the newest version holds still writes a
//! version of its own: that version may be one another push has made and not yet acknowledged
//! (through the proxy it is visible before its push is acknowledged and is removed if the push
//! fails), so only a version this push made is evidence that its tag was moved. Of pushes that read the same newest version, the create of one key is won by
//! exactly one: a loser reads the key back, and if the winner holds another digest it is 409
//! `TAG_CONFLICT`, which changes nothing but the manifest it stored by digest, which is harmless
//! and is still pulled by that digest (a client that wants its manifest tagged pushes again; if
//! the winner is then not acknowledged, the 409 was a push to repeat and no tag was lost). If the
//! winner holds the digest the loser pushed, the loser does not answer 201 from the winner's
//! version: through the proxy that version is visible before its push is acknowledged and is
//! removed if the push fails. The loser writes the version after it, with the same create-only
//! write, and is 201 when that is its own. A push whose create's answer is unclear (the store said
//! `Unavailable` and the write may have landed, as an S3 conflict does) reads the key back: the
//! key holding another digest is 409, a key that is not there is 503 (the push may be retried),
//! and a key holding the digest of the push is not taken as the push's own, as the write may be
//! another push's: the push writes the version after it, as a loser of the create does, and is
//! 201 only when that is its own, and the failure it had otherwise. A push that reads after another has finished sees
//! what that push left. A listing that lags a write makes a push find
//! the key of the next version taken, and it is 201 or 409 by the read-back and overwrites
//! nothing; the stores are required to list consistently with the writes before the listing
//! (S3 and MinIO do).
//!
//! Tags are not deleted by the front, and old versions are not either: the specification
//! leaves deletion to the registry, and a tag is moved to another manifest instead.

use std::collections::BTreeSet;

use bytes::Bytes;
use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, LINK};
use hyper::{Response, StatusCode};
use serde_json::{Value, json};

use super::metrics::Outcome;
use super::names::Digest;
use super::push::Answer;
use super::{OciFront, body, echoed, error, header_value, store_failed};
use crate::server::request::query_pairs;
use crate::{Backend, ByteRange, Error};

/// The bytes a version holds: `sha256:` and 64 hex digits.
const TAG_BYTES: u64 = 71;

/// The digits of the suffix of a version's key, which is `u64::MAX - n` and has 20 digits.
const INV_DIGITS: usize = 20;

/// The key of the version `inv` of the tag whose directory is `prefix`.
fn version_key(prefix: &str, inv: u64) -> String {
    format!("{prefix}{inv:0width$}", width = INV_DIGITS)
}

/// The newest version of the tag whose directory is `prefix`: its suffix and its key, or `None`
/// if the tag has no version. The listing is in ascending key order, so the first key is the
/// one with the smallest suffix, which is the newest version (on the proxy, wherever it is).
async fn newest(backend: &dyn Backend, prefix: &str) -> Result<Option<(u64, String)>, Error> {
    let page = backend.list_page(prefix, None, 1).await?;
    let Some(first) = page.into_iter().next() else {
        return Ok(None);
    };
    let suffix = first.key.get(prefix.len()..).unwrap_or_default();
    let digits = suffix.len() == INV_DIGITS && suffix.bytes().all(|byte| byte.is_ascii_digit());
    match suffix.parse::<u64>() {
        Ok(inv) if digits => Ok(Some((inv, first.key))),
        _ => Err(Error::Other(format!(
            "{} is not a version of a tag: its name does not end in {INV_DIGITS} digits",
            first.key
        ))),
    }
}

/// The digest the version at `key` holds.
async fn held(backend: &dyn Backend, key: &str) -> Result<Digest, Error> {
    let range = ByteRange::new(0, TAG_BYTES + 1);
    let bytes = backend.get(key, Some(range)).await?;
    let not_a_digest = || Error::Other(format!("{key} does not hold a digest"));
    if bytes.len() as u64 != TAG_BYTES {
        return Err(not_a_digest());
    }
    Digest::parse(&String::from_utf8_lossy(&bytes)).ok_or_else(not_a_digest)
}

/// The digest the newest version of the tag holds, or `None` if it has none.
async fn current(backend: &dyn Backend, prefix: &str) -> Result<Option<Digest>, Error> {
    match newest(backend, prefix).await? {
        Some((_, key)) => held(backend, &key).await.map(Some),
        None => Ok(None),
    }
}

/// The suffix of the version before `inv`.
fn before(inv: u64, key: &str) -> Result<u64, Error> {
    let before = inv.checked_sub(1);
    before.ok_or_else(|| Error::Other(format!("{key} is the last version of a tag")))
}

/// Points the tag whose directory is `prefix` at `digest`: writes the version after the newest,
/// with the create-only write, also when the newest already holds `digest`.
///
/// A create that finds the key taken by a version that holds `digest` is not taken as done: on
/// the proxy a version is visible in the hot tier before the push that made it is acknowledged,
/// and is removed if that push then fails, so the loser writes the version after it, which it
/// owns, and which is the newest whatever becomes of the other. A taken key that holds another
/// digest is lost.
///
/// # Errors
/// [`Error::PreconditionFailed`] if another push made the version first and it holds another
/// digest; the store's own error otherwise, which for [`Error::Unavailable`] after a create
/// that may not have landed and is not followed by a version of this push's own, so that the
/// push can be tried again.
pub(super) async fn point(
    backend: &dyn Backend,
    prefix: &str,
    digest: &Digest,
) -> Result<(), Error> {
    let mut inv = match newest(backend, prefix).await? {
        None => u64::MAX,
        Some((inv, key)) => {
            held(backend, &key).await?;
            before(inv, &key)?
        }
    };
    loop {
        let key = version_key(prefix, inv);
        match backend.put(&key, Bytes::from(digest.text())).await {
            Err(Error::AlreadyExists(_)) => match held(backend, &key).await {
                Ok(found) if found == *digest => inv = before(inv, &key)?,
                Ok(_) | Err(Error::NotFound(_)) => {
                    return Err(Error::PreconditionFailed(key));
                }
                Err(failure) => return Err(failure),
            },
            Err(unclear @ Error::Unavailable(_)) => {
                settle(backend, &key, digest, unclear).await?;
                inv = before(inv, &key)?;
            }
            other => return other,
        }
    }
}

/// Reads back a create of `key` answered [`Error::Unavailable`] (`unclear`), which does not say
/// whether the write landed or whose it is. `Ok` if the object holds `digest`: that is not
/// this push's write, as far as can be told (it may be another push's, not yet acknowledged and
/// removed if that push fails), so the caller writes the version after it. Lost if it holds
/// another digest, and still `unclear` if it is not there (the write did not land, as far as
/// can be told).
async fn settle(
    backend: &dyn Backend,
    key: &str,
    digest: &Digest,
    unclear: Error,
) -> Result<(), Error> {
    match held(backend, key).await {
        Ok(found) if found == *digest => Ok(()),
        Ok(_) => Err(Error::PreconditionFailed(key.to_owned())),
        Err(Error::NotFound(_)) => Err(unclear),
        Err(failure) => Err(failure),
    }
}

/// 409: another push moved the tag first.
pub(super) fn conflict(tag: &str) -> Answer {
    let message = "another push moved this tag first: the manifest is stored by its digest, \
                   push the tag again to move it";
    let detail = json!({ "tag": echoed(tag) });
    let response = error(StatusCode::CONFLICT, "TAG_CONFLICT", message, detail);
    (Outcome::Conflict, response)
}

/// 400: `n` of a listing is not a number.
fn bad_count(text: &str) -> Answer {
    let message = "n must be a whole number";
    let detail = json!({ "n": echoed(text) });
    let response = error(
        StatusCode::BAD_REQUEST,
        "PAGINATION_NUMBER_INVALID",
        message,
        detail,
    );
    (Outcome::Invalid, response)
}

impl OciFront {
    /// The directory of the versions of the tag `tag` of the repository `name` (both checked
    /// against their grammars).
    pub(super) fn tag_prefix(&self, name: &str, tag: &str) -> String {
        format!("{}{tag}/", self.tag_dir(name))
    }

    /// The directory of the tags of `name`.
    fn tag_dir(&self, name: &str) -> String {
        format!("{}tags/{}/", self.prefix.as_str(), name.replace('/', "~"))
    }

    /// The digest the tag points at.
    ///
    /// A newest version that is listed and then not there is looked for once more, as the
    /// listing then gives the one that is.
    ///
    /// # Errors
    /// [`Error::NotFound`] if there is no such tag; [`Error::Other`] if the newest version does
    /// not hold a digest.
    pub(super) async fn tagged(&self, name: &str, tag: &str) -> Result<Digest, Error> {
        let prefix = self.tag_prefix(name, tag);
        let backend = self.backend.as_ref();
        let found = match current(backend, &prefix).await {
            Err(Error::NotFound(_)) => current(backend, &prefix).await,
            other => other,
        };
        found?.ok_or(Error::NotFound(prefix))
    }

    /// `GET` or `HEAD` of `/v2/<name>/tags/list`.
    pub(super) async fn tag_list(&self, head: bool, name: &str, query: Option<&str>) -> Answer {
        if !super::names::valid_name(name) {
            return super::invalid_name(name);
        }
        let pairs = query_pairs(query).unwrap_or_default();
        let find = |key: &str| {
            let found = pairs.iter().find(|(asked, _)| asked == key);
            found.map(|(_, value)| value.as_str())
        };
        let limit = match find("n").map(str::parse::<usize>) {
            None => None,
            Some(Ok(count)) => Some(count),
            Some(Err(_)) => return bad_count(find("n").unwrap_or_default()),
        };
        let after = find("last");
        let dir = self.tag_dir(name);
        let listed = match self.backend.list(&dir).await {
            Ok(listed) => listed,
            Err(failure) => return store_failed(&failure, &dir, "NAME_UNKNOWN", Value::Null),
        };
        // The keys sort by `<tag>/`, and `-` and `.` sort before `/`, so the tags are not in
        // order in the listing (`a-b` comes before `a`): they are collected and sorted here.
        let distinct: BTreeSet<&str> = listed
            .iter()
            .map(|object| {
                let rest = object.key.get(dir.len()..).unwrap_or_default();
                rest.split('/').next().unwrap_or_default()
            })
            .collect();
        let mut tags: Vec<&str> = distinct.into_iter().collect();
        if let Some(after) = after {
            tags.retain(|tag| *tag > after);
        }
        let more = limit.is_some_and(|count| count > 0 && tags.len() > count);
        if let Some(count) = limit {
            tags.truncate(count);
        }
        let document = json!({ "name": name, "tags": tags });
        let bytes = Bytes::from(document.to_string());
        let mut response = Response::new(if head {
            body::empty()
        } else {
            body::full(bytes.clone())
        });
        let headers = response.headers_mut();
        let json = HeaderValue::from_static("application/json; charset=utf-8");
        headers.insert(CONTENT_TYPE, json);
        headers.insert(CONTENT_LENGTH, HeaderValue::from(bytes.len()));
        if more {
            let last = tags.last().copied().unwrap_or_default();
            let count = limit.unwrap_or_default();
            let next = format!("</v2/{name}/tags/list?last={last}&n={count}>; rel=\"next\"");
            headers.insert(LINK, header_value(&next));
        }
        (Outcome::Ok, response)
    }
}
