//! The file names of the keys of a directory tier: how a key is written as a file name and read
//! back.

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
