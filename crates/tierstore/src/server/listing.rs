//! The walk of a ListObjectsV2 page: which keys are entries of it, and where the next page
//! starts. With a `delimiter` the keys under one prefix fold into one `CommonPrefixes` entry,
//! as they do for S3.

use crate::{Backend, Listed};

use super::reply::S3Error;
use super::request::Query;

/// One entry of a page, which counts one against `max-keys`.
enum Entry {
    Object(Listed),
    /// A common prefix: the prefix of the request and the text up to and with the first
    /// delimiter after it.
    Prefix(String),
}

/// Where a page starts.
enum Start {
    /// After a key (a key that need not exist), as `start-after` and the token of an object do.
    After(String),
    /// After every key that starts with the prefix: the token of a common prefix, so that the
    /// next page does not start with that prefix again.
    PastPrefix(String),
}

/// One page: its objects and common prefixes, each in key order, and the token of the next
/// page if the listing goes on.
pub(super) struct Page {
    pub(super) objects: Vec<Listed>,
    pub(super) prefixes: Vec<String>,
    pub(super) next: Option<String>,
}

/// The mark of the token of a common prefix. It is not a hexadecimal digit, so it tells that
/// token from the token of an object, which is the hexadecimal of its key (the only token
/// before common prefixes were listed, and still the token of an object).
const PREFIX_MARK: char = 'p';

/// The token of the page after `entry`.
fn token(entry: &Entry) -> String {
    match entry {
        Entry::Object(listed) => hex::encode(&listed.key),
        Entry::Prefix(prefix) => format!("{PREFIX_MARK}{}", hex::encode(prefix)),
    }
}

/// Where the page a token stands for starts.
fn untoken(token: &str) -> Result<Start, S3Error> {
    let bad = || S3Error::invalid("the continuation token is not one this server gave");
    let (hexed, prefix) = match token.strip_prefix(PREFIX_MARK) {
        Some(rest) => (rest, true),
        None => (token, false),
    };
    let bytes = hex::decode(hexed).map_err(|_| bad())?;
    let text = String::from_utf8(bytes).map_err(|_| bad())?;
    // A common prefix has the delimiter in it, so it is never empty.
    if prefix && text.is_empty() {
        return Err(bad());
    }
    Ok(if prefix {
        Start::PastPrefix(text)
    } else {
        Start::After(text)
    })
}

/// Where a listing starts: after what its continuation token stands for, else after
/// `start-after`, else at the first key.
fn start(query: &Query) -> Result<Option<Start>, S3Error> {
    match query.get("continuation-token") {
        Some(text) => untoken(text).map(Some),
        None => Ok(query
            .get("start-after")
            .map(|key| Start::After(key.to_owned()))),
    }
}

/// The common prefix of `key` for a request with `prefix` and `delimiter`: `prefix`, then the
/// text of `key` after it up to and with the first `delimiter`; none if that text has no
/// `delimiter`.
fn fold(key: &str, prefix: &str, delimiter: &str) -> Option<String> {
    let rest = key.get(prefix.len()..).unwrap_or_default();
    let end = rest.find(delimiter)? + delimiter.len();
    Some(format!("{prefix}{}", &rest[..end]))
}

/// The page of a listing of `prefix` with at most `max_keys` entries, `batch` keys being read
/// from the backend at a time. (Without a delimiter every key is an entry, and one read of
/// `max_keys + 1` keys is enough; the caller says so with `batch`.)
pub(super) async fn page(
    backend: &dyn Backend,
    query: &Query,
    prefix: &str,
    delimiter: Option<&str>,
    max_keys: usize,
    batch: usize,
) -> Result<Page, S3Error> {
    let (mut after, mut skipping) = match start(query)? {
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
    let next = truncated.then(|| token(&entries[max_keys - 1]));
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
