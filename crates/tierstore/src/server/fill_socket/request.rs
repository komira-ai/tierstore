//! The requests of the fill socket: one line each, parsed here and run by [`serve`](super::serve).
//!
//! ```text
//! fill <sha256> <size> from <key> @ <offset>   a blob inside the upstream object <key>
//! fill <sha256>                                the whole object cas/l/<sha256>
//! touch <sha256> [<sha256> ...]                these blobs were used now (no answer)
//! ```
//!
//! A digest is sixty-four lower-case hex digits; a number is decimal digits only. `<key>` is the
//! rest of the line between `from ` and the last ` @ `, so a key may hold spaces; it is not
//! under a prefix the proxy keeps for itself.

use crate::journal::is_reserved;
use crate::server::local_cache::layout::is_digest;

/// The longest line a request may be, in bytes: a `touch` of about a thousand digests.
pub(super) const MAX_LINE: usize = 64 * 1024;

/// The longest key of an S3 object, in bytes.
const MAX_KEY: usize = 1024;

/// A request that is understood.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Request {
    /// Fill the blob of `size` bytes at `offset` of the upstream object `from`.
    Blob {
        sha256: String,
        size: u64,
        from: String,
        offset: u64,
    },
    /// Fill the whole object `cas/l/<sha256>`.
    Manifest { sha256: String },
    /// Count these blobs as used now; only the digests that are digests are kept.
    Touch(Vec<String>),
}

/// Why a line is not a request. The text follows `refused ` in the answer.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum Refusal {
    /// The line is not text.
    #[error("the request is not text")]
    NotText,
    /// The first word is not one the socket answers.
    #[error("unknown request")]
    Unknown,
    /// A request that has not the words of its form.
    #[error("malformed request")]
    Malformed,
    /// A digest that is not sixty-four lower-case hex digits.
    #[error("not a sha256 digest")]
    Digest,
    /// A size or an offset that is not decimal digits that fit 64 bits.
    #[error("not a number")]
    Number,
    /// A blob of no bytes: an empty file needs no fill.
    #[error("size is zero")]
    Empty,
    /// The blob ends past what 64 bits count.
    #[error("offset and size pass 2^64")]
    Overflow,
    /// A key that is empty or longer than an S3 key.
    #[error("not a usable key")]
    Key,
    /// A key under a prefix the proxy keeps for itself.
    #[error("reserved key")]
    Reserved,
}

/// The digest `text`, or why it is not one.
fn digest(text: &str) -> Result<String, Refusal> {
    if is_digest(text) {
        Ok(text.to_owned())
    } else {
        Err(Refusal::Digest)
    }
}

/// The number `text` writes in decimal digits (no sign, no space).
fn number(text: &str) -> Result<u64, Refusal> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Refusal::Number);
    }
    text.parse().map_err(|_| Refusal::Number)
}

/// The request a line is, without its newline.
///
/// # Errors
/// Why the line is not a request: see [`Refusal`]. A `touch` is never refused: it is not
/// answered, so what is not a digest in it is passed over.
pub(super) fn parse(line: &[u8]) -> Result<Request, Refusal> {
    let text = std::str::from_utf8(line).map_err(|_| Refusal::NotText)?;
    let (verb, rest) = text.split_once(' ').unwrap_or((text, ""));
    match verb {
        "fill" => fill(rest),
        "touch" => Ok(Request::Touch(
            rest.split(' ')
                .filter(|word| is_digest(word))
                .map(str::to_owned)
                .collect(),
        )),
        _ => Err(Refusal::Unknown),
    }
}

/// The request `fill <rest>` is.
fn fill(rest: &str) -> Result<Request, Refusal> {
    let Some((sha256, source)) = rest.split_once(' ') else {
        return Ok(Request::Manifest {
            sha256: digest(rest)?,
        });
    };
    let sha256 = digest(sha256)?;
    let (size, source) = source.split_once(' ').ok_or(Refusal::Malformed)?;
    let source = source.strip_prefix("from ").ok_or(Refusal::Malformed)?;
    let (from, offset) = source.rsplit_once(" @ ").ok_or(Refusal::Malformed)?;
    let (size, offset) = (number(size)?, number(offset)?);
    if size == 0 {
        return Err(Refusal::Empty);
    }
    if offset.checked_add(size).is_none() {
        return Err(Refusal::Overflow);
    }
    if from.is_empty() || from.len() > MAX_KEY {
        return Err(Refusal::Key);
    }
    if is_reserved(from) {
        return Err(Refusal::Reserved);
    }
    Ok(Request::Blob {
        sha256,
        size,
        from: from.to_owned(),
        offset,
    })
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
