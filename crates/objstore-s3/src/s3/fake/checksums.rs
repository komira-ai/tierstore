//! The SHA-256 checksums and the metadata of the fake service, as S3 keeps them.
//!
//! - A PUT that carries `x-amz-checksum-sha256` is refused 400 `BadDigest` unless the base64
//!   SHA-256 of its body is that value, and the object keeps it (type `FULL_OBJECT`).
//! - CreateMultipartUpload with `x-amz-checksum-algorithm: SHA256` makes an upload whose every
//!   part must carry `x-amz-checksum-sha256` (400 `InvalidRequest` without one, 400 `BadDigest`
//!   for a wrong one); the object it completes to keeps the checksum of the part checksums, the
//!   base64 SHA-256 of their bytes in order, a dash and the number of parts (type `COMPOSITE`).
//! - Every `x-amz-meta-*` header of a PUT or a CreateMultipartUpload is kept with the object and
//!   given back by a HEAD.
//! - A HEAD gives the checksum only when asked (`x-amz-checksum-mode: ENABLED`).
//!
//! A test can make the service a store that does no checksums
//! ([`FakeS3::ignore_checksums`](super::FakeS3::ignore_checksums)): it takes the headers, checks
//! nothing and keeps no checksum; it still keeps the metadata.

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use super::{Request, error};

/// What an object, or an upload, keeps of the headers it was written with.
#[derive(Clone, Debug, Default)]
pub(super) struct Sums {
    /// The `x-amz-meta-*` headers, names in lower case.
    pub(super) meta: Vec<(String, String)>,
    /// The checksum kept and its type.
    pub(super) sha256: Option<(String, &'static str)>,
}

/// The base64 SHA-256 of `bytes`.
fn base64_sha256(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
}

/// The `x-amz-meta-*` headers of `request`.
fn meta_of(request: &Request) -> Vec<(String, String)> {
    let meta = request.headers.iter();
    let meta = meta.filter(|(name, _)| name.starts_with("x-amz-meta-"));
    meta.map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// What a PUT of a whole object keeps, or the answer that refuses it for a checksum its body does
/// not have.
pub(super) fn of_put(request: &Request, ignore: bool) -> Result<Sums, Vec<u8>> {
    let meta = meta_of(request);
    let said = request.headers.get("x-amz-checksum-sha256");
    let sha256 = match said.filter(|_| !ignore) {
        Some(said) if *said != base64_sha256(&request.body) => {
            return Err(error("400 Bad Request", "BadDigest"));
        }
        Some(said) => Some((said.clone(), "FULL_OBJECT")),
        None => None,
    };
    Ok(Sums { meta, sha256 })
}

/// What a CreateMultipartUpload asks of its upload: the metadata of the object it completes to,
/// with a checksum type (and so a checksum on every part) if it names `SHA256`.
pub(super) fn of_create(request: &Request, ignore: bool) -> Sums {
    let algorithm = request.headers.get("x-amz-checksum-algorithm");
    let sha256 = algorithm.is_some_and(|name| name == "SHA256") && !ignore;
    Sums {
        meta: meta_of(request),
        sha256: sha256.then(|| (String::new(), "COMPOSITE")),
    }
}

/// The SHA-256 of a part of an upload that `wants` a checksum, or the answer that refuses the
/// part for a checksum it lacks or its body does not have. `None` for an upload that wants none.
pub(super) fn of_part(request: &Request, wants: bool) -> Result<Option<Vec<u8>>, Vec<u8>> {
    if !wants {
        return Ok(None);
    }
    let Some(said) = request.headers.get("x-amz-checksum-sha256") else {
        return Err(error("400 Bad Request", "InvalidRequest"));
    };
    let digest = Sha256::digest(&request.body).to_vec();
    if *said != base64::engine::general_purpose::STANDARD.encode(&digest) {
        return Err(error("400 Bad Request", "BadDigest"));
    }
    Ok(Some(digest))
}

/// What the object an upload completes to keeps: the upload's metadata, and the checksum of the
/// SHA-256 digests of `parts` (in order) if the upload has a checksum type.
pub(super) fn completed<'a>(
    begun: &Sums,
    parts: impl Iterator<Item = Option<&'a Vec<u8>>>,
) -> Sums {
    let mut joined = Vec::new();
    let mut count = 0;
    for digest in parts.flatten() {
        joined.extend_from_slice(digest);
        count += 1;
    }
    let composite = format!("{}-{count}", base64_sha256(&joined));
    let sha256 = begun.sha256.as_ref().map(|(_, kind)| (composite, *kind));
    Sums {
        meta: begun.meta.clone(),
        sha256,
    }
}

/// The headers a HEAD of an object that keeps `sums` adds: its metadata, and its checksum and
/// type if `request` asks for them.
pub(super) fn head_headers<'a>(sums: &'a Sums, request: &Request) -> Vec<(&'a str, String)> {
    let mut headers: Vec<(&str, String)> = sums
        .meta
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect();
    let asked = request.headers.get("x-amz-checksum-mode");
    let asked = asked.is_some_and(|mode| mode.eq_ignore_ascii_case("ENABLED"));
    if let Some((sum, kind)) = sums.sha256.as_ref().filter(|_| asked) {
        headers.push(("x-amz-checksum-sha256", sum.clone()));
        headers.push(("x-amz-checksum-type", (*kind).to_owned()));
    }
    headers
}
