//! The walk of one page of a listing: which keys (or open uploads) are entries of it, and where
//! the next page starts. With a delimiter the keys under one prefix fold into one common prefix,
//! as they do for S3. Nothing here is S3's wire: not the XML, not the spelling of a token or a
//! marker, not an error code. The front that answers a listing (the S3 front, in `ops.rs`, with
//! `list_token.rs` and `xml.rs`) reads its request into a prefix, a delimiter, a start and a
//! size, calls [`page`] or [`uploads_page`], and writes the result in its own format, so another
//! front (a JSON listing of a GCS bucket, a directory listing) can use the same walk and give its
//! own tokens.

use crate::{Backend, Error, Listed, OpenUpload};

/// One entry of a page, which counts one against the size of the page: an item (an object or an
/// upload), or a common prefix, the prefix of the request and the text up to and with the first
/// delimiter after it.
enum Entry<T> {
    Item(T),
    Prefix(String),
}

/// Where a page of objects starts.
pub(super) enum Start {
    /// After a key (a key that need not exist), as `start-after` and the token of an object do.
    After(String),
    /// After every key that starts with the prefix: the place after a common prefix, so that the
    /// next page does not start with that prefix again.
    PastPrefix(String),
}

/// One page of objects: the objects and common prefixes, each in key order, and where the next
/// page starts if the listing goes on.
pub(super) struct Page {
    pub(super) objects: Vec<Listed>,
    pub(super) prefixes: Vec<String>,
    pub(super) next: Option<Start>,
}

/// Where the page of open uploads after a page starts: a listing of uploads is started by a key
/// and an upload id (`key-marker` and `upload-id-marker`), so it is the page after one that
/// can start after a common prefix.
pub(super) enum UploadsNext {
    /// After the upload with this id of this key.
    After(String, String),
    /// After every upload of a key that starts with the prefix.
    PastPrefix(String),
}

/// One page of open uploads, as [`Page`] is one of objects.
pub(super) struct UploadsPage {
    pub(super) uploads: Vec<OpenUpload>,
    pub(super) prefixes: Vec<String>,
    pub(super) next: Option<UploadsNext>,
}

/// What a key is to the page being walked.
enum Admit {
    /// Not an entry: it is under a common prefix that is passed or already an entry.
    Skip,
    Item,
    Prefix(String),
}

/// The folding of the keys of a listing, which arrive in key order.
struct Folder<'a> {
    prefix: &'a str,
    delimiter: Option<&'a str>,
    /// The common prefix being passed: keys that start with it are not entries, until one that
    /// does not.
    skipping: Option<String>,
    /// The common prefix last made an entry (keys of one prefix are contiguous in key order).
    last: Option<String>,
}

impl<'a> Folder<'a> {
    fn new(prefix: &'a str, delimiter: Option<&'a str>) -> Self {
        Self {
            prefix,
            delimiter,
            skipping: None,
            last: None,
        }
    }

    /// The common prefix of `key`: `prefix`, then the text of `key` after it up to and with the
    /// first delimiter; none if there is no delimiter, or that text has none.
    fn fold(&self, key: &str) -> Option<String> {
        let (rest, delimiter) = (key.strip_prefix(self.prefix)?, self.delimiter?);
        let end = rest.find(delimiter)? + delimiter.len();
        Some(format!("{}{}", self.prefix, &rest[..end]))
    }

    /// Starts after `key`. A common prefix is left out of a listing that starts at a key the
    /// prefix does not sort after (S3: "`CommonPrefixes` is filtered out from results if it is
    /// not lexicographically greater than" the start), and a prefix is a prefix of the keys under
    /// it, so a start inside a common prefix passes the whole prefix: the keys under it are not
    /// entries. Returns that prefix, which is then the key to read after; none if `key` is in no
    /// common prefix and the listing reads after `key` itself.
    fn start_after(&mut self, key: &str) -> Option<String> {
        self.fold(key).map(|common| self.pass(common))
    }

    /// Starts after every key that starts with `common`, and returns the key to read after.
    fn pass(&mut self, common: String) -> String {
        self.skipping = Some(common.clone());
        common
    }

    /// What `key` is to the page.
    fn admit(&mut self, key: &str) -> Admit {
        if let Some(skipped) = &self.skipping {
            if key.starts_with(skipped.as_str()) {
                return Admit::Skip;
            }
            self.skipping = None;
        }
        match self.fold(key) {
            None => Admit::Item,
            Some(common) if self.last.as_ref() == Some(&common) => Admit::Skip,
            Some(common) => {
                self.last = Some(common.clone());
                Admit::Prefix(common)
            }
        }
    }
}

/// Cuts `entries` to `size`, and says whether there were more.
fn cut<T>(entries: &mut Vec<Entry<T>>, size: usize) -> bool {
    let more = entries.len() > size;
    entries.truncate(size);
    more
}

/// The items and the common prefixes of `entries`, each in order.
fn split<T>(entries: Vec<Entry<T>>) -> (Vec<T>, Vec<String>) {
    let (mut items, mut prefixes) = (Vec::new(), Vec::new());
    for entry in entries {
        match entry {
            Entry::Item(item) => items.push(item),
            Entry::Prefix(common) => prefixes.push(common),
        }
    }
    (items, prefixes)
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
    let mut folder = Folder::new(prefix, delimiter);
    let mut after = match start {
        None => None,
        Some(Start::After(key)) => Some(folder.start_after(&key).unwrap_or(key)),
        Some(Start::PastPrefix(common)) => Some(folder.pass(common)),
    };
    let mut entries: Vec<Entry<Listed>> = Vec::new();
    loop {
        let keys = backend.list_page(prefix, after.as_deref(), batch).await?;
        let more = keys.len() == batch;
        after = keys.last().map(|last| last.key.clone());
        for listed in keys {
            match folder.admit(&listed.key) {
                Admit::Skip => {}
                Admit::Item => entries.push(Entry::Item(listed)),
                Admit::Prefix(common) => entries.push(Entry::Prefix(common)),
            }
        }
        if entries.len() > max_keys || !more {
            break;
        }
    }
    let truncated = cut(&mut entries, max_keys);
    let next = truncated.then(|| match &entries[max_keys - 1] {
        Entry::Item(listed) => Start::After(listed.key.clone()),
        Entry::Prefix(common) => Start::PastPrefix(common.clone()),
    });
    let (objects, prefixes) = split(entries);
    Ok(Page {
        objects,
        prefixes,
        next,
    })
}

/// The page of a listing of the open uploads under `prefix` with at most `max_uploads` entries
/// (an upload or a common prefix), `batch` uploads being read from the backend at a time, after
/// the upload with the id (if there is one) of the key `start`.
pub(super) async fn uploads_page(
    backend: &dyn Backend,
    prefix: &str,
    delimiter: Option<&str>,
    start: Option<(String, Option<String>)>,
    max_uploads: usize,
    batch: usize,
) -> Result<UploadsPage, Error> {
    if max_uploads == 0 {
        let (uploads, prefixes) = (Vec::new(), Vec::new());
        return Ok(UploadsPage {
            uploads,
            prefixes,
            next: None,
        });
    }
    let mut folder = Folder::new(prefix, delimiter);
    let mut after = start.map(|(key, id)| match folder.start_after(&key) {
        Some(common) => (common, None),
        None => (key, id),
    });
    let mut entries: Vec<Entry<OpenUpload>> = Vec::new();
    loop {
        let marker = after
            .as_ref()
            .map(|(key, id)| (key.as_str(), id.as_deref()));
        let uploads = backend.list_uploads(prefix, marker, batch).await?;
        let more = uploads.len() == batch;
        after = uploads
            .last()
            .map(|last| (last.key.clone(), Some(last.id.clone())));
        for upload in uploads {
            match folder.admit(&upload.key) {
                Admit::Skip => {}
                Admit::Item => entries.push(Entry::Item(upload)),
                Admit::Prefix(common) => entries.push(Entry::Prefix(common)),
            }
        }
        if entries.len() > max_uploads || !more {
            break;
        }
    }
    let truncated = cut(&mut entries, max_uploads);
    let next = truncated.then(|| match &entries[max_uploads - 1] {
        Entry::Item(upload) => UploadsNext::After(upload.key.clone(), upload.id.clone()),
        Entry::Prefix(common) => UploadsNext::PastPrefix(common.clone()),
    });
    let (uploads, prefixes) = split(entries);
    Ok(UploadsPage {
        uploads,
        prefixes,
        next,
    })
}
