//! The S3 front under the placements of node-local trees (`docs/design/node-local-trees.md`,
//! section 6.2 and 6.4; tracking issue komira-ai/tierstore#326).
//!
//! A tree placed at a bucket and a key prefix shows its files as objects of that bucket on this
//! node: a request first asks the tree store whether its key falls under a placement, and only a
//! key under none is an ordinary request. Under a placement:
//!
//! - `GET` and `HEAD` of a file answer as for an object whose bytes are the file's: the length is
//!   its size, the `ETag` its SHA-256 (quoted hex), `Last-Modified` the time the placement was
//!   made, and `x-amz-meta-mode` its mode in four octal digits (`4755`). A `Range` is served as for
//!   an object, and so are the conditions (`If-Match` and the rest) and the `response-*`
//!   overrides. The bytes are a span of a pack in the cache, filled from the upstream (whole, and
//!   checked against the file's SHA-256) by the first read that does not find them there.
//! - A symbolic link is an object of no bytes, its `ETag` the SHA-256 of nothing, with
//!   `x-amz-meta-symlink-target` its target (each byte that is not printable ASCII, and `%`,
//!   written `%XX`).
//! - A directory, the placement's root included, is no object: `404 NoSuchKey`, as is a path that
//!   is not in the tree.
//! - A write (`PUT`, a part, the start or the completion of a multipart upload, `DELETE`, and each
//!   such key of a DeleteObjects) is refused `409 TreeLocationReadOnly`; the abort of an upload
//!   is not, since it changes no object.
//! - A listing merges the keys of the placements it meets with the ordinary objects, in key order,
//!   and leaves out an ordinary object whose key is under a placement (the tree answers that
//!   key on this node).
//!
//! A placement that ends between the lookup and the answer leaves an ordinary key: the request is
//! then answered as one.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt as _, stream};
use hyper::StatusCode;
use hyper::http::request::Parts;

use crate::{Backend, Error, Listed, Meta, ReadBody, TreeDigest, Window};

use super::{asked, conditional, object_headers, overridden, overrides, read_answer};
use crate::server::Server;
use crate::server::listing::Keys;
use crate::server::local_cache::RangeError;
use crate::server::local_cache::trees::{
    FetchError, Located, OpenFile, ReadError, ReadHold, Resolved, TreeStore,
};
use crate::server::reply::{Answer, S3Error, answer};
use crate::server::request::{Query, Target, header};

/// The most bytes read from a pack's file at a time for a response.
pub(super) const CHUNK: u64 = 1 << 20;

/// The refusal of a write under a placement.
pub(in crate::server) fn read_only() -> S3Error {
    S3Error::new(
        StatusCode::CONFLICT,
        "TreeLocationReadOnly",
        "A tree is placed at this key on this node: it cannot be written or deleted.",
    )
}

/// The text of a symbolic link's target as a header value: each byte that is not printable
/// ASCII, and `%`, as `%XX`.
fn escaped(target: &str) -> String {
    let mut out = String::with_capacity(target.len());
    for byte in target.bytes() {
        if (b' '..=b'~').contains(&byte) && byte != b'%' {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// What a key under a placement is, as an object.
struct Shown {
    meta: Meta,
    header: (&'static str, String),
    file: bool,
}

/// The answer to a key that is not in a placement's tree, or is a directory of it.
fn no_such_key(key: &str) -> S3Error {
    S3Error::from(Error::NotFound(key.to_owned()))
}

/// The answer to a read whose path the store could not answer; none when the placement ended.
pub(super) fn unanswered(key: &str, error: ReadError) -> Option<S3Error> {
    match error {
        ReadError::NoPlacement => None,
        ReadError::NotFound | ReadError::NotADirectory => Some(no_such_key(key)),
        ReadError::Corrupt(why) => Some(S3Error::from(Error::Other(format!(
            "the tree placed at {key}: {why}"
        )))),
    }
}

/// The answer to a read whose bytes could not be filled: `503 SlowDown` for what a retry can
/// mend (the cache busy, read-only, full or changed under the fill, the upstream failing), and
/// `500 InternalError` for a source that does not hold the file's bytes.
pub(super) fn unfilled(key: &str, error: &RangeError) -> S3Error {
    let retry = match error {
        RangeError::Upstream(status) => *status >= 500,
        RangeError::Busy
        | RangeError::ReadOnly
        | RangeError::NoSpace
        | RangeError::Io
        | RangeError::Changed
        | RangeError::Aborted => true,
        RangeError::NotKept
        | RangeError::Empty
        | RangeError::Past
        | RangeError::Mismatch
        | RangeError::Short => false,
    };
    if retry {
        return slow_down();
    }
    S3Error::from(Error::Other(format!(
        "the bytes of the tree file {key} cannot be read from its source: {error}"
    )))
}

/// `503 SlowDown`: the bytes are not in the cache and cannot be had now.
fn slow_down() -> S3Error {
    let text = "The bytes of this tree file could not be fetched now; retry after a moment.";
    S3Error::new(StatusCode::SERVICE_UNAVAILABLE, "SlowDown", text).with("retry-after", "1")
}

/// `len` bytes of the file at `path` from `offset`, a chunk at a time, holding `hold` until the
/// last is given or the body is dropped.
pub(super) fn span_body(
    path: &Path,
    offset: u64,
    len: u64,
    hold: ReadHold,
) -> io::Result<ReadBody> {
    let file = Arc::new(File::open(path)?);
    let start = (file, offset, offset + len, hold);
    let body = stream::try_unfold(start, |(file, at, end, hold)| async move {
        if at >= end {
            return Ok(None);
        }
        let len = (end - at).min(CHUNK);
        let reading = Arc::clone(&file);
        let read = tokio::task::spawn_blocking(move || {
            // `len` is at most a chunk, which memory holds.
            let mut bytes = vec![0; usize::try_from(len).unwrap_or(usize::MAX)];
            reading
                .read_exact_at(&mut bytes, at)
                .map(|()| Bytes::from(bytes))
        })
        .await;
        let bytes = read
            .map_err(io::Error::other)
            .and_then(std::convert::identity)
            .map_err(|why| Error::Other(format!("reading a tree file: {why}")))?;
        Ok(Some((bytes, (file, at + len, end, hold))))
    });
    Ok(body.boxed())
}

/// The bytes `[start, start + len)` of the file `open` gives; none if the placement ended.
pub(super) fn file_body(
    key: &str,
    open: Result<OpenFile, FetchError>,
    start: u64,
    len: u64,
) -> Result<Option<ReadBody>, S3Error> {
    let open = match open {
        Ok(open) => open,
        Err(FetchError::Read(error)) => return unanswered(key, error).map_or(Ok(None), Err),
        Err(FetchError::NotAFile(_)) => return Err(no_such_key(key)),
        Err(FetchError::Fill(error)) => return Err(unfilled(key, &error)),
        Err(FetchError::Lost) => return Err(slow_down()),
    };
    let Some(span) = open.span else {
        return Ok(Some(stream::empty().boxed()));
    };
    let body = span_body(&span.path, span.offset + start, len, open.hold);
    // The cache's file went between the fill and the open: it was evicted or replaced.
    body.map(Some).map_err(|_| slow_down())
}

impl Server {
    /// The tree store and the placement `key` falls under, if any.
    fn located(&self, key: &str) -> Option<(&TreeStore, Located)> {
        let store = self.config.trees.as_ref()?;
        let located = store.locate_s3(&self.config.bucket, key)?;
        Some((store, located))
    }

    /// Whether `key` falls under a placement on this node.
    pub(in crate::server) fn placed(&self, key: &str) -> bool {
        self.located(key).is_some()
    }

    /// Refuses a request that writes an object under a placement: a put, a part, the start or
    /// the completion of an upload, a delete. The abort of an upload goes on.
    ///
    /// # Errors
    /// [`read_only`].
    pub(in crate::server) fn refuse_placed_write(
        &self,
        method: &str,
        target: &Target,
        query: &Query,
    ) -> Result<(), S3Error> {
        let Target::Object(key) = target else {
            return Ok(());
        };
        let aborts = method == "DELETE" && query.has("uploadId");
        let writes = matches!(method, "PUT" | "POST" | "DELETE") && !aborts;
        if writes && self.placed(key) {
            return Err(read_only());
        }
        Ok(())
    }

    /// The answer to a `GET` (`body`) or a `HEAD` of `key` if it falls under a placement; none
    /// for an ordinary key.
    ///
    /// # Errors
    /// `404 NoSuchKey` for a directory or a path not in the tree; a failed condition or range as
    /// for an object; what [`unfilled`] says of a fill that failed.
    pub(in crate::server) async fn read_placed(
        &self,
        parts: &Parts,
        query: &Query,
        key: &str,
        body: bool,
    ) -> Result<Option<Answer>, S3Error> {
        let Some((store, located)) = self.located(key) else {
            return Ok(None);
        };
        self.read_located(parts, query, key, store, &located, body)
            .await
    }

    /// [`read_placed`](Self::read_placed) of `key`, which falls under the placement `located`
    /// of `store`: none if the placement ended since it was located.
    ///
    /// # Errors
    /// As [`read_placed`](Self::read_placed).
    pub(super) async fn read_located(
        &self,
        parts: &Parts,
        query: &Query,
        key: &str,
        store: &TreeStore,
        located: &Located,
        body: bool,
    ) -> Result<Option<Answer>, S3Error> {
        let Some(info) = store.placement(&located.placement) else {
            return Ok(None);
        };
        let resolved = match store.resolve(&located.placement, &located.path) {
            Ok(resolved) => resolved,
            Err(error) => return unanswered(key, error).map_or(Ok(None), Err),
        };
        let modified = info.created_ms;
        let shown = match resolved {
            Resolved::Dir { .. } => return Err(no_such_key(key)),
            Resolved::File {
                mode, size, sha256, ..
            } => Shown {
                meta: Meta {
                    size,
                    tag: sha256.to_string(),
                    modified,
                },
                header: ("x-amz-meta-mode", format!("{:04o}", mode.bits())),
                file: true,
            },
            Resolved::Symlink { target } => Shown {
                meta: Meta {
                    size: 0,
                    tag: TreeDigest::of(b"").to_string(),
                    modified,
                },
                header: ("x-amz-meta-symlink-target", escaped(&target)),
                file: false,
            },
        };
        let set = overrides(query)?;
        if let Some(early) = conditional(parts, key, &shown.meta)? {
            return Ok(Some(early));
        }
        let mut headers = object_headers(&shown.meta);
        headers.push(shown.header);
        let size = shown.meta.size;
        if !body {
            headers.push(("content-length", size.to_string()));
            let head = answer(StatusCode::OK, &headers, Bytes::new());
            return Ok(Some(overridden(head, set)));
        }
        let (status, start, len) = match Window::of(asked(header(&parts.headers, "range")), size) {
            Window::Whole => (StatusCode::OK, 0, size),
            Window::Part(range) => {
                let (start, len) = (range.offset(), range.size());
                let last = start + len - 1;
                headers.push(("content-range", format!("bytes {start}-{last}/{size}")));
                (StatusCode::PARTIAL_CONTENT, start, len)
            }
            Window::Unsatisfiable => return Err(S3Error::invalid_range(size)),
        };
        let bytes = if shown.file && size > 0 {
            let open = store.open_file(&located.placement, &located.path).await;
            let Some(read) = file_body(key, open, start, len)? else {
                return Ok(None);
            };
            read
        } else {
            stream::empty().boxed()
        };
        let read = read_answer(status, &headers, len, bytes);
        Ok(Some(overridden(read, set)))
    }

    /// What a listing reads its keys from: the backend, merged with the placements of the bucket.
    pub(in crate::server) fn keys(&self) -> Merged<'_> {
        Merged {
            backend: self.backend.as_ref(),
            trees: self.config.trees.as_ref(),
            bucket: &self.config.bucket,
        }
    }
}

/// The keys of a listing: the backend's, with the keys under a placement taken out, merged with
/// the keys the placements show.
pub(in crate::server) struct Merged<'a> {
    backend: &'a dyn Backend,
    trees: Option<&'a TreeStore>,
    bucket: &'a str,
}

#[async_trait]
impl Keys for Merged<'_> {
    async fn list_page(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Listed>, Error> {
        let trees = self.trees.filter(|trees| trees.places_in(self.bucket));
        let Some(trees) = trees else {
            return self.backend.list_page(prefix, after, limit).await;
        };
        let placed = trees
            .s3_keys(self.bucket, prefix, after, limit)
            .map_err(|why| {
                Error::Other(format!("listing the trees placed under {prefix}: {why}"))
            })?;
        // The backend's keys up to the `limit`-th one not under a placement, or to its end: the
        // first `limit` keys of the merge are then among those and the placements' first `limit`.
        // Once the placements give `limit` keys, a backend key after the last of them cannot be
        // among the first `limit`, so the backend is read no further than that key.
        let bound = placed
            .get(limit.wrapping_sub(1))
            .map(|last| last.key.clone());
        let mut keys = Vec::new();
        let mut cursor = after.map(str::to_owned);
        loop {
            let page = self
                .backend
                .list_page(prefix, cursor.as_deref(), limit)
                .await?;
            let more = !page.is_empty() && page.len() >= limit;
            if let Some(last) = page.last() {
                cursor = Some(last.key.clone());
            }
            let shown = page
                .into_iter()
                .filter(|listed| trees.locate_s3(self.bucket, &listed.key).is_none());
            keys.extend(shown);
            let passed = bound
                .as_ref()
                .is_some_and(|bound| cursor.as_ref() >= Some(bound));
            if !more || keys.len() >= limit || passed {
                break;
            }
        }
        keys.extend(placed.into_iter().map(|key| Listed {
            key: key.key,
            size: key.size,
            modified: key.created_ms,
            tag: key.sha256.to_string(),
        }));
        keys.sort_by(|a, b| a.key.cmp(&b.key));
        keys.truncate(limit);
        Ok(keys)
    }
}
