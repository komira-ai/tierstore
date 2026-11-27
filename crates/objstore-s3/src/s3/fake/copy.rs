//! CopyObject and UploadPartCopy of the fake service: a PUT that carries `x-amz-copy-source`.
//!
//! The source is `<bucket>/<key>` (a leading `/` allowed, the key percent-encoded); the bucket is
//! not checked, as the service holds one. A source the service does not hold is 404 `NoSuchKey`.
//! The four source conditions are S3's: `x-amz-copy-source-if-match` must name the source's tag
//! (`*` names any), and without it `-if-unmodified-since` must not be before the source's time of
//! last write; `-if-none-match` must not name the tag, and without it `-if-modified-since` must be
//! before that time. A date that is not an HTTP date is ignored, as S3 ignores it. A condition
//! that does not hold is 412 `PreconditionFailed`, and nothing is written.
//!
//! A copy of an object writes the key with no condition, as a plain PUT, with the bytes and the
//! metadata of the source; the new object's tag is that of its bytes, as for one PUT, and the
//! answer is a `CopyObjectResult` with that tag and the time of the write. A copy of a part
//! stores the part of the upload (404 `NoSuchUpload` for an upload the service does not hold)
//! from the source, or from the bytes `x-amz-copy-source-range: bytes=<first>-<last>` names (400
//! `InvalidArgument` for a range of another form or past the end of the source), and answers a
//! `CopyPartResult`; such a part carries no checksum. A test can make the next copy of an object
//! answer 200 with an `<Error>`, as S3 does when a copy fails after its answer started
//! ([`Failure::CopyLate`]), or the next copy answer 501, as a store that does not copy does
//! ([`Failure::CopyUnsupported`]).

use super::dates::{iso_date, system_seconds};
use super::escape::unescape;
use super::kept::Kept;
use super::{Failure, Request, State, answer, error, etag_of, part_tag};
use crate::s3::times::http_date_millis;

/// The answer to a PUT that carries `x-amz-copy-source` (`source`, as it came): a copy into the
/// object `key`, or, with an upload id and a part number, into that part.
pub(super) fn copy(
    state: &mut State,
    key: &str,
    source: &str,
    upload: Option<(String, String)>,
    request: &Request,
) -> Vec<u8> {
    if state.take(Failure::CopyUnsupported) {
        return error("501 Not Implemented", "NotImplemented");
    }
    let decoded = unescape(source);
    let path = decoded.trim_start_matches('/');
    let from = path.split_once('/').map_or("", |(_bucket, key)| key);
    let Some(stored) = state.objects.get(from) else {
        return error("404 Not Found", "NoSuchKey");
    };
    if !holds(request, &stored.etag(), stored.modified) {
        return error("412 Precondition Failed", "PreconditionFailed");
    }
    let (kept, sums) = (stored.kept.clone(), stored.sums.clone());
    let now = state.time.unwrap_or_else(system_seconds);
    match upload {
        None => {
            if state.take(Failure::CopyLate) {
                let late = "<Error><Code>InternalError</Code><Message>late</Message></Error>";
                return answer("200 OK", &[], late.as_bytes());
            }
            let etag = etag_of(&kept);
            state.store(key.to_owned(), kept, None, sums);
            let body = format!(
                "<CopyObjectResult><LastModified>{}</LastModified><ETag>{}</ETag>\
                 </CopyObjectResult>",
                iso_date(now),
                etag.replace('"', "&quot;")
            );
            answer("200 OK", &[], body.as_bytes())
        }
        Some((id, number)) => copy_part(state, key, &kept, &id, &number, request, now),
    }
}

/// UploadPartCopy of the bytes of `source` (all, or the range the request names) into the part
/// `number` of the upload `id` of `key`.
fn copy_part(
    state: &mut State,
    key: &str,
    source: &Kept,
    id: &str,
    number: &str,
    request: &Request,
    now: u64,
) -> Vec<u8> {
    let number: u32 = number.parse().unwrap_or(0);
    let range = request.headers.get("x-amz-copy-source-range");
    let span = match range {
        None => Some((0, source.len)),
        Some(text) => text
            .strip_prefix("bytes=")
            .and_then(|range| range.split_once('-'))
            .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
            .filter(|(first, last)| (*first..source.len).contains(last))
            .map(|(first, last)| (first, last + 1)),
    };
    let Some((from, to)) = span else {
        return error("400 Bad Request", "InvalidArgument");
    };
    let upload = state.uploads.get_mut(id).filter(|up| up.key == key);
    let Some(upload) = upload else {
        return error("404 Not Found", "NoSuchUpload");
    };
    let kept = Kept::of(&source.read(from, to));
    let etag = part_tag(&kept);
    upload.stored_at.insert(number, now);
    upload.parts.insert(number, (etag.clone(), kept));
    state.stats.parts += 1;
    let body = format!(
        "<CopyPartResult><LastModified>{}</LastModified><ETag>{}</ETag></CopyPartResult>",
        iso_date(now),
        etag.replace('"', "&quot;")
    );
    answer("200 OK", &[], body.as_bytes())
}

/// Whether the source conditions of `request` hold of an object with the entity tag `etag`
/// (quotes included) written at `modified` (seconds).
fn holds(request: &Request, etag: &str, modified: u64) -> bool {
    let header = |name: &str| request.headers.get(name).map(String::as_str);
    let names = |list: &str| {
        let bare = etag.trim_matches('"');
        list.split(',')
            .map(|item| item.trim().trim_matches('"'))
            .any(|item| item == "*" || item == bare)
    };
    let date = |name: &str| {
        header(name)
            .and_then(http_date_millis)
            .map(|millis| millis / 1000)
    };
    let matched = match header("x-amz-copy-source-if-match") {
        Some(list) => names(list),
        None => date("x-amz-copy-source-if-unmodified-since").is_none_or(|at| modified <= at),
    };
    let fresh = match header("x-amz-copy-source-if-none-match") {
        Some(list) => !names(list),
        None => date("x-amz-copy-source-if-modified-since").is_none_or(|at| modified > at),
    };
    matched && fresh
}
