//! The file names of the keys of a directory tier: how a key is written as a file name and read
//! back.

use std::time::{SystemTime, UNIX_EPOCH};

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
