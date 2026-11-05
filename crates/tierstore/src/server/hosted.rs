//! Virtual-hosted-style addressing: a request to `<bucket>.<domain>` names the bucket in its
//! `Host`, and its whole path is the key (`GET http://tier.s3.example.com/a/b` reads the key
//! `a/b` of the bucket `tier`). The domains are `server.domains`; a host that is under none of
//! them, and any request when there are none, is path-style (`/<bucket>/<key>`) as before.

use hyper::http::request::Parts;
use hyper::http::uri::Authority;

use super::reply::S3Error;
use super::request::{Target, decode, header};

/// The `Host` of a request: the header, else the authority of its URI.
fn host_of(parts: &Parts) -> Option<&str> {
    header(&parts.headers, "host").or_else(|| parts.uri.authority().map(Authority::as_str))
}

/// The bucket a virtual-hosted-style request names, in lower case: the part of its `Host`
/// before `.<domain>` for the longest of `domains` the host is under (the port, a trailing dot
/// and the case of the host do not matter; a bucket name may hold dots). `None` for a host that
/// is under none of them, a host that is only a domain, an IP address, and when there are no
/// domains.
pub(crate) fn hosted_bucket(parts: &Parts, domains: &[String]) -> Option<String> {
    if domains.is_empty() {
        return None;
    }
    let host = host_of(parts)?;
    if host.starts_with('[') {
        return None;
    }
    let name = host.rsplit_once(':').map_or(host, |(name, _port)| name);
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let under = |domain: &String| {
        let label = name.strip_suffix(domain.as_str())?.strip_suffix('.')?;
        (!label.is_empty()).then(|| (domain.len(), label.to_owned()))
    };
    let found = domains.iter().filter_map(under).max_by_key(|(len, _)| *len);
    found.map(|(_, label)| label)
}

/// What a virtual-hosted-style path names: the bucket (`/`) or an object of it (`/<key>`),
/// decoded.
///
/// # Errors
/// `NoSuchBucket` when `label` is not the bucket the server serves (names compare without regard
/// to case, as the host does); a path that is not percent-encoded UTF-8.
pub(crate) fn resolve_hosted(bucket: &str, label: &str, path: &str) -> Result<Target, S3Error> {
    let decoded = decode(path).ok_or_else(S3Error::invalid_uri)?;
    if !label.eq_ignore_ascii_case(bucket) {
        return Err(S3Error::no_such_bucket(label));
    }
    let key = decoded.strip_prefix('/').unwrap_or(&decoded);
    Ok(match key {
        "" => Target::Bucket,
        key => Target::Object(key.to_owned()),
    })
}

/// A domain of `server.domains` as the server keeps it (lower case), if `text` is one: dot
/// separated labels of letters, digits and hyphens, none empty and none starting or ending with a
/// hyphen. Not a URL, a port or a path.
pub(crate) fn valid_domain(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let label = |label: &str| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    };
    lower.split('.').all(label).then_some(lower)
}
