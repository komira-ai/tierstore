//! Tags: names that point at a manifest, and move.
//!
//! A tag is one small object, `<prefix>tags/<name>/<tag>`, whose bytes are the digest of the
//! manifest it points at (`sha256:<hex>`). The directory is the repository name with each `/`
//! written as `~` (a character no name has), so that no name's directory is inside another's and
//! a directory backend never needs a file and a directory of one path. The objects are under the
//! front's prefix and not in the blobs' namespace, so no digest names one, and the manifests and
//! blobs stay immutable and named by their content.
//!
//! - `PUT /v2/<name>/manifests/<tag>` stores the manifest by its digest (see `push`), and then
//!   points the tag at it, in one step of the store that a second push of the tag cannot share.
//! - `GET` and `HEAD` of `/v2/<name>/manifests/<tag>` read the tag and answer as the manifest of
//!   that digest does.
//! - `GET /v2/<name>/tags/list` lists the tags of a name, in ascending order, with the `n` and
//!   `last` of the specification for paging (and a `Link` header when a page is not the last).
//!   A name with no tag, and a name nobody has pushed, are the same: 200 and an empty list.
//!
//! # Two pushes of one tag
//!
//! A tag that does not exist is made with the store's create-only write; a tag that does is
//! replaced with a write that must find the content tag (`If-Match`) the push read just before.
//! Of any number of pushes that read the same state of a tag, exactly one write is taken and the
//! tag then points at that push's manifest; each of the others is answered 409 `TAG_CONFLICT`
//! and changes nothing but the manifest it stored by digest, which is harmless and is still
//! pulled by that digest. A client that wants its manifest tagged pushes again. A push that
//! starts after another has finished reads the state that push left, and is taken.
//!
//! Tags are not deleted by the front: the specification leaves deletion to the registry, and a
//! tag is moved to another manifest instead.

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

/// The bytes a tag object holds: `sha256:` and 64 hex digits.
const TAG_BYTES: u64 = 71;

/// Points the tag at `key` at the digest in `body`: creates it if it is not there, else replaces
/// it if it still has the content tag that was read.
///
/// # Errors
/// [`Error::PreconditionFailed`] if another write got there first (the tag was created, changed
/// or deleted since it was read); the store's own error otherwise.
pub(super) async fn point(backend: &dyn Backend, key: &str, body: Bytes) -> Result<(), Error> {
    let lost = || Error::PreconditionFailed(key.to_owned());
    let held = match backend.head(key).await {
        Ok(meta) => meta,
        Err(Error::NotFound(_)) => {
            return match backend.put(key, body).await {
                Err(Error::AlreadyExists(_)) => Err(lost()),
                other => other,
            };
        }
        Err(failure) => return Err(failure),
    };
    match backend.put_if_match(key, body, &held.tag).await {
        Ok(_) => Ok(()),
        Err(Error::NotFound(_)) => Err(lost()),
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
    /// The key of the tag `tag` of the repository `name` (both checked against their grammars).
    pub(super) fn tag_key(&self, name: &str, tag: &str) -> String {
        format!("{}{tag}", self.tag_dir(name))
    }

    /// The directory of the tags of `name`.
    fn tag_dir(&self, name: &str) -> String {
        format!("{}tags/{}/", self.prefix.as_str(), name.replace('/', "~"))
    }

    /// The digest the tag points at.
    ///
    /// # Errors
    /// [`Error::NotFound`] if there is no such tag; [`Error::Other`] if the object does not hold
    /// a digest.
    pub(super) async fn tagged(&self, name: &str, tag: &str) -> Result<Digest, Error> {
        let key = self.tag_key(name, tag);
        let range = ByteRange::new(0, TAG_BYTES);
        let held = self.backend.get(&key, Some(range)).await?;
        Digest::parse(&String::from_utf8_lossy(&held))
            .ok_or_else(|| Error::Other(format!("{key} does not hold a digest")))
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
        let mut tags: Vec<String> = listed
            .iter()
            .map(|object| object.key.get(dir.len()..).unwrap_or_default().to_owned())
            .collect();
        tags.sort();
        if let Some(after) = after {
            tags.retain(|tag| tag.as_str() > after);
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
            let last = tags.last().map_or("", String::as_str);
            let count = limit.unwrap_or_default();
            let next = format!("</v2/{name}/tags/list?last={last}&n={count}>; rel=\"next\"");
            headers.insert(LINK, header_value(&next));
        }
        (Outcome::Ok, response)
    }
}
