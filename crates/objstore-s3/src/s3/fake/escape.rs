//! XML text of the fake service's listings.

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
