//! The file names of the keys of a directory tier: how a key is written as a file name and read
//! back.
//!
//! A key whose encoded name is at most 255 bytes (the longest file name) is that name, and its
//! file holds the content tag and then the body. A key whose name is longer has the file name
//! `<the first 100 bytes of the name>~<the SHA-256 of the whole key, in lower-case hex>` (165
//! bytes), and its file holds the content tag, then the length of the key as 8 bytes
//! (big-endian), the key, and then the body. The tag is still the first 32 bytes, so a listing
//! reads it as it does for any file; the name does not hold the key, so a listing, which has
//! only names, reads the key from the file, and a call that has the key checks that the file
//! holds it. Which of the two a file is follows from the key, not from the name: a plain file
//! can have a name of the shape of a hashed one (the key `a/<64 hex digits>` is such a name), and
//! a listing takes a file of that shape for a hashed one only if its header holds a key whose
//! hashed name it is.

use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::entries;

/// The longest file name, in bytes, that a key may be encoded to without being hashed.
pub(super) const NAME_LIMIT: usize = 255;

/// The longest key, in bytes, whose file holds it: the S3 front takes keys of at most 1024, and
/// a listing reads at most this many bytes of a file to see whether it holds a key.
pub(super) const MAX_KEY: usize = 1 << 16;

/// The bytes of the encoded name that a hashed name keeps.
const KEPT: usize = 100;

/// The number of hex digits of the SHA-256 that ends a hashed name.
const DIGITS: usize = 64;

/// Nanoseconds since the epoch, or 0 before it: part of a name no other open or upload shares,
/// not a time anyone judges.
pub(super) fn unique_nanos() -> u128 {
    #[expect(
        clippy::disallowed_methods,
        reason = "a name no other open or upload shares, not a time anyone judges"
    )]
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |elapsed| elapsed.as_nanos())
}

/// The file name of a key. A `.` at the start of a key is written as `%2E`, so no file name made
/// here starts with `.`: those names are the backend's own (`.uploads`, `.entries`, temporary
/// files).
pub(super) fn encode(key: &str) -> String {
    let mut name = String::with_capacity(key.len());
    for (at, byte) in key.bytes().enumerate() {
        match byte {
            b'/' => name.push('~'),
            b'.' if at > 0 => name.push('.'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => name.push(char::from(byte)),
            _ => name.push_str(&format!("%{byte:02X}")),
        }
    }
    name
}

/// The part of `key` that `encode` writes as its file name: what follows `_entries/`, if the
/// key starts with it, else the key.
fn named_part(key: &str) -> &str {
    entries::rest(key).unwrap_or(key)
}

/// The hashed file name of `key`, if its encoded name is over [`NAME_LIMIT`] bytes.
pub(super) fn hashed_name(key: &str) -> Option<String> {
    let name = encode(named_part(key));
    // The name is ASCII, so it can be cut at any byte.
    let kept = name.get(..KEPT).filter(|_| name.len() > NAME_LIMIT)?;
    Some(format!("{kept}~{}", hex::encode(Sha256::digest(key))))
}

/// The file name of `key` in its directory, or `None` if it would be empty or the key is over
/// [`MAX_KEY`] bytes.
pub(super) fn file_name(key: &str) -> Option<String> {
    let name = hashed_name(key).unwrap_or_else(|| encode(named_part(key)));
    (!name.is_empty() && key.len() <= MAX_KEY).then_some(name)
}

/// What the file of `key` holds behind the content tag, before the body: nothing if the file
/// has the plain name, else the length of the key and the key.
pub(super) fn header(key: &str) -> Vec<u8> {
    if hashed_name(key).is_none() {
        return Vec::new();
    }
    let mut header = (key.len() as u64).to_be_bytes().to_vec();
    header.extend_from_slice(key.as_bytes());
    header
}

/// Whether `name` has the shape of a hashed name: [`KEPT`] bytes, a `~` and 64 lower-case hex
/// digits.
pub(super) fn is_hashed_shape(name: &str) -> bool {
    let digits = name.get(KEPT..).and_then(|tail| tail.strip_prefix('~'));
    digits.is_some_and(|digits| {
        digits.len() == DIGITS
            && digits
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// The key a file name stands for, if the name is one [`encode`] makes.
pub(super) fn decode(name: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(name.len());
    let mut rest = name.bytes();
    while let Some(byte) = rest.next() {
        match byte {
            b'~' => bytes.push(b'/'),
            b'%' => {
                let pair = [rest.next()?, rest.next()?];
                bytes.extend(hex::decode(pair).ok()?);
            }
            other => bytes.push(other),
        }
    }
    String::from_utf8(bytes).ok()
}

/// Whether the byte can be in the id of an upload, which names a directory.
pub(super) fn is_id_byte(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'z' | b'-')
}

/// The name of the file of a part of an upload: its number and its content tag.
pub(super) fn part_name(number: u32, tag: &str) -> String {
    format!("{number:05}-{tag}")
}

/// The number and the content tag a file name of a part has, if it is one.
pub(super) fn parse_part(name: &str) -> Option<(u32, &str)> {
    let (number, tag) = name.split_once('-')?;
    Some((number.parse().ok()?, tag))
}
