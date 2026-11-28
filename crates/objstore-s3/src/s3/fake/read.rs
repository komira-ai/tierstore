//! The reads of an object by the fake service: a GET of the whole object or of one
//! `bytes=first-last` range, refused when it carries `If-Match` and the object has another entity
//! tag (412) or there is no object (404), and kept in the order they came ([`FakeRead`]).

use super::dates::http_date;
use super::{Request, State, answer, error, if_match_refusal};

/// One GET of an object the service took, as it came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeRead {
    /// The key read.
    pub key: String,
    /// The `Range` header, if the read had one (`bytes=first-last`).
    pub range: Option<String>,
    /// The `If-Match` header, if the read had one, as it was sent.
    pub if_match: Option<String>,
}

/// A GET of the whole object, or of one `bytes=first-last` range; the answer carries the entity
/// tag and the time of last write. With `If-Match`, the answer is 412 if the object has another
/// tag and 404 if there is none, as for a write.
pub(super) fn get(state: &mut State, key: &str, request: &Request) -> Vec<u8> {
    let range = request.headers.get("range");
    state.reads.push(FakeRead {
        key: key.to_owned(),
        range: range.cloned(),
        if_match: request.headers.get("if-match").cloned(),
    });
    if let Some(refused) = if_match_refusal(state, key, request) {
        return refused;
    }
    let Some(stored) = state.objects.get(key) else {
        return error("404 Not Found", "NoSuchKey");
    };
    let object = &stored.kept;
    let Some(range) = range else {
        let headers = [
            ("etag", stored.etag()),
            ("last-modified", http_date(stored.modified)),
        ];
        return answer("200 OK", &headers, &object.read(0, object.len));
    };
    let parsed = range
        .strip_prefix("bytes=")
        .and_then(|r| r.split_once('-'))
        .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)));
    let Some((first, last)) = parsed.filter(|(f, l)| f < &object.len && f <= l) else {
        return error("416 Range Not Satisfiable", "InvalidRange");
    };
    let end = (last + 1).min(object.len);
    let content_range = format!("bytes {first}-{}/{}", end - 1, object.len);
    let bytes = object.read(first, end);
    let headers = [
        ("etag", stored.etag()),
        ("last-modified", http_date(stored.modified)),
        ("content-range", content_range),
    ];
    answer("206 Partial Content", &headers, &bytes)
}
