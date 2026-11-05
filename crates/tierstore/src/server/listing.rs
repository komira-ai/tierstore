//! The walk of one page of a listing of keys: which keys are entries of it, and where the next
//! page starts. With a delimiter the keys under one prefix fold into one common prefix, as they
//! do for S3. Nothing here is S3's wire: not the XML, not the spelling of a token, not an error
//! code. The front that answers a listing (the S3 front, in `ops.rs`, with `list_token.rs` and
//! `xml.rs`) reads its request into a prefix, a delimiter, a [`Start`] and a size, calls
//! [`page`], and writes the [`Page`] in its own format, so another front (a JSON listing of a
//! GCS bucket, a directory listing) can use the same walk and give its own tokens.

use crate::{Backend, Error, Listed};

/// One entry of a page, which counts one against `max-keys`.
enum Entry {
    Object(Listed),
    /// A common prefix: the prefix of the request and the text up to and with the first
    /// delimiter after it.
    Prefix(String),
}

/// Where a page starts.
pub(super) enum Start {
    /// After a key (a key that need not exist), as `start-after` and the token of an object do.
    After(String),
    /// After every key that starts with the prefix: the place after a common prefix, so that the
    /// next page does not start with that prefix again.
    PastPrefix(String),
}

/// One page: its objects and common prefixes, each in key order, and where the next page starts
/// if the listing goes on.
pub(super) struct Page {
    pub(super) objects: Vec<Listed>,
    pub(super) prefixes: Vec<String>,
    pub(super) next: Option<Start>,
}

/// The common prefix of `key` for a request with `prefix` and `delimiter`: `prefix`, then the
/// text of `key` after it up to and with the first `delimiter`; none if that text has no
/// `delimiter`.
fn fold(key: &str, prefix: &str, delimiter: &str) -> Option<String> {
    let rest = key.get(prefix.len()..).unwrap_or_default();
    let end = rest.find(delimiter)? + delimiter.len();
    Some(format!("{prefix}{}", &rest[..end]))
}

/// Where the page after `entry` starts.
fn resume(entry: &Entry) -> Start {
    match entry {
        Entry::Object(listed) => Start::After(listed.key.clone()),
        Entry::Prefix(prefix) => Start::PastPrefix(prefix.clone()),
    }
}

/// The page of a listing of `prefix` with at most `max_keys` entries, `batch` keys being read
/// from the backend at a time. (Without a delimiter every key is an entry, and one read of
/// `max_keys + 1` keys is enough; the caller says so with `batch`.)
pub(super) async fn page(
    backend: &dyn Backend,
    prefix: &str,
    delimiter: Option<&str>,
    start: Option<Start>,
    max_keys: usize,
    batch: usize,
) -> Result<Page, Error> {
    // A page of no entries reads nothing, and is not truncated (as for S3).
    if max_keys == 0 {
        let (objects, prefixes) = (Vec::new(), Vec::new());
        return Ok(Page {
            objects,
            prefixes,
            next: None,
        });
    }
    let (mut after, mut skipping) = match start {
        None => (None, None),
        Some(Start::After(key)) => (Some(key), None),
        Some(Start::PastPrefix(skipped)) => (Some(skipped.clone()), Some(skipped)),
    };
    let mut entries: Vec<Entry> = Vec::new();
    loop {
        let keys = backend.list_page(prefix, after.as_deref(), batch).await?;
        let more = keys.len() == batch;
        after = keys.last().map(|last| last.key.clone());
        for listed in keys {
            if skipping
                .as_deref()
                .is_some_and(|skipped| listed.key.starts_with(skipped))
            {
                continue;
            }
            skipping = None;
            let folded = delimiter.and_then(|text| fold(&listed.key, prefix, text));
            let entry = match folded {
                Some(common) => {
                    if matches!(entries.last(), Some(Entry::Prefix(last)) if *last == common) {
                        continue;
                    }
                    Entry::Prefix(common)
                }
                None => Entry::Object(listed),
            };
            entries.push(entry);
        }
        if entries.len() > max_keys || !more {
            break;
        }
    }
    let truncated = entries.len() > max_keys;
    entries.truncate(max_keys);
    let next = truncated.then(|| resume(&entries[max_keys - 1]));
    let (mut objects, mut prefixes) = (Vec::new(), Vec::new());
    for entry in entries {
        match entry {
            Entry::Object(listed) => objects.push(listed),
            Entry::Prefix(common) => prefixes.push(common),
        }
    }
    Ok(Page {
        objects,
        prefixes,
        next,
    })
}
