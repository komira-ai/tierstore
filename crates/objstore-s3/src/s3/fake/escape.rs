//! XML text of the fake service's listings, and the decoding of the escapes of a request.

/// `text` as the content of an XML element: `&`, `<`, `>` and the quotes as entities, and tab,
/// line feed and carriage return as numeric references (a bare one is read back as a line feed
/// or a space).
pub(super) fn xml_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push_str(&format!("&#{};", u32::from(c))),
            other => out.push(other),
        }
    }
    out
}

/// `text` with its `%XX` escapes decoded (an escape that is not one stays as it is).
pub(super) fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = bytes
            .get(at + 1..at + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .filter(|_| bytes[at] == b'%')
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
