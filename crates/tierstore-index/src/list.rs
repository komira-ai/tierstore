//! The LIST of the S3 front over several index nodes (`docs/design/fetch-cache-serve.md`, 10.2
//! and the section 14 row "LIST").
//!
//! Every key is held by two or three nodes, so a listing merges the sorted key streams of the
//! nodes it can reach: [`list`] asks each [`ListSource`] for pages (one call is one seek of one
//! node's index), merges them in key order, collapses a key that several nodes hold, and applies
//! the delimiter roll-up. A rolled-up prefix is not walked key by key: once it is emitted, each
//! source that has no key left beyond it in its buffer is asked again after [`seek_past`] the
//! prefix, so the prefix costs one seek per source however many keys it holds.

use std::collections::VecDeque;

use tierstore_store_api::index::{Index, IndexError};

/// The most keys and common prefixes one page of a listing holds.
pub const MAX_LIST_KEYS: usize = 1000;

/// The longest key S3 accepts, in bytes.
pub const MAX_KEY_LEN: usize = 1024;

/// A sorted stream of live keys, one index node's.
pub trait ListSource {
    /// Up to `limit` live keys that start with `prefix` and sort after `after` (when given), in
    /// key order. One call is one seek of one node's index. An index that is being built answers
    /// [`IndexError::Indexing`].
    fn list(
        &mut self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<Vec<u8>>, IndexError>;
}

/// A [`ListSource`] over an [`Index`]: the keys of [`Index::list`].
pub struct IndexSource<'a>(pub &'a dyn Index);

impl ListSource for IndexSource<'_> {
    fn list(
        &mut self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<Vec<u8>>, IndexError> {
        Ok(self
            .0
            .list(prefix, after, limit)?
            .into_iter()
            .map(|(key, _)| key)
            .collect())
    }
}

/// One page of a listing asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRequest {
    /// Only keys that start with this.
    pub prefix: Vec<u8>,
    /// Keys whose rest after the prefix holds the delimiter roll up into a common prefix. An
    /// empty delimiter is none.
    pub delimiter: Option<Vec<u8>>,
    /// The most keys and common prefixes together on the page, at most [`MAX_LIST_KEYS`].
    pub max_keys: usize,
    /// Only keys after this, the `next` of the previous page. One before the prefix starts at the
    /// prefix.
    pub after: Option<Vec<u8>>,
}

/// One page of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListPage {
    /// The keys that do not roll up, in key order.
    pub keys: Vec<Vec<u8>>,
    /// The common prefixes (the prefix, the text up to the delimiter, and the delimiter), in key
    /// order.
    pub common_prefixes: Vec<Vec<u8>>,
    /// The `after` of the following page, when this page is full and more follows.
    pub next: Option<Vec<u8>>,
}

/// The smallest key that sorts after every key of at most [`MAX_KEY_LEN`] bytes that starts with
/// `prefix`: the prefix followed by 0xff bytes up to [`MAX_KEY_LEN`] bytes.
#[must_use]
pub fn seek_past(prefix: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    out.resize(prefix.len().max(MAX_KEY_LEN), 0xff);
    out
}

/// One source with the keys fetched from it and not merged yet.
struct Stream<'a> {
    source: &'a mut dyn ListSource,
    buffered: VecDeque<Vec<u8>>,
    /// The `after` of the next fetch.
    cursor: Option<Vec<u8>>,
    /// The source has no key beyond what it returned.
    done: bool,
}

impl Stream<'_> {
    /// The next key of the stream, fetching a page when the buffer is empty.
    fn head(&mut self, prefix: &[u8], fetch: usize) -> Result<Option<&Vec<u8>>, IndexError> {
        if self.buffered.is_empty() && !self.done {
            let page = self.source.list(prefix, self.cursor.as_deref(), fetch)?;
            self.done = page.len() < fetch;
            if let Some(last) = page.last() {
                self.cursor = Some(last.clone());
            }
            self.buffered = page.into();
        }
        Ok(self.buffered.front())
    }
}

/// The merge of every stream, in key order, without duplicates.
struct Merge<'a> {
    streams: Vec<Stream<'a>>,
    prefix: &'a [u8],
    fetch: usize,
    /// The last common prefix emitted: keys under it are skipped.
    rolled: Option<Vec<u8>>,
}

impl Merge<'_> {
    /// The next key to consider: the smallest head of the streams, taken from every stream that
    /// holds it. A key outside the prefix, longer than [`MAX_KEY_LEN`] or under the last rolled
    /// prefix is dropped.
    fn next_key(&mut self) -> Result<Option<Vec<u8>>, IndexError> {
        loop {
            let mut min: Option<Vec<u8>> = None;
            for stream in &mut self.streams {
                if let Some(head) = stream.head(self.prefix, self.fetch)?
                    && min.as_ref().is_none_or(|m| head < m)
                {
                    min = Some(head.clone());
                }
            }
            let Some(key) = min else {
                return Ok(None);
            };
            for stream in &mut self.streams {
                if stream.buffered.front() == Some(&key) {
                    stream.buffered.pop_front();
                }
            }
            let under_rolled = self.rolled.as_ref().is_some_and(|p| key.starts_with(p));
            if key.len() > MAX_KEY_LEN || !key.starts_with(self.prefix) || under_rolled {
                continue;
            }
            return Ok(Some(key));
        }
    }

    /// Records the emission of the common prefix `common`: buffered keys under it are dropped, and
    /// a stream left with nothing buffered continues after it.
    fn roll_up(&mut self, common: &[u8]) {
        for stream in &mut self.streams {
            while stream
                .buffered
                .front()
                .is_some_and(|k| k.starts_with(common))
            {
                stream.buffered.pop_front();
            }
            if stream.buffered.is_empty() {
                stream.cursor = stream.cursor.take().max(Some(seek_past(common)));
            }
        }
        self.rolled = Some(common.to_vec());
    }
}

/// The common prefix `key` rolls up into: the prefix and the rest up to and including the first
/// delimiter; none when the rest holds no delimiter. `key` starts with `prefix`.
fn common_prefix(key: &[u8], prefix: &[u8], delimiter: &[u8]) -> Option<Vec<u8>> {
    let rest = &key[prefix.len()..];
    let at = rest.windows(delimiter.len()).position(|w| w == delimiter)?;
    Some(key[..prefix.len() + at + delimiter.len()].to_vec())
}

/// One page of the listing of `req` over `sources`. A source's error is returned as it is.
pub fn list(
    sources: &mut [&mut dyn ListSource],
    req: &ListRequest,
) -> Result<ListPage, IndexError> {
    let cap = req.max_keys.min(MAX_LIST_KEYS);
    let mut page = ListPage::default();
    if cap == 0 {
        return Ok(page);
    }
    let cursor = req.after.clone().filter(|a| *a >= req.prefix);
    let delimiter = req.delimiter.as_deref().filter(|d| !d.is_empty());
    let mut merge = Merge {
        streams: sources
            .iter_mut()
            .map(|source| Stream {
                source: &mut **source,
                buffered: VecDeque::new(),
                cursor: cursor.clone(),
                done: false,
            })
            .collect(),
        prefix: &req.prefix,
        fetch: cap,
        rolled: None,
    };
    let mut last = Vec::new();
    while page.keys.len() + page.common_prefixes.len() < cap {
        let Some(key) = merge.next_key()? else {
            return Ok(page);
        };
        match delimiter.and_then(|d| common_prefix(&key, &req.prefix, d)) {
            Some(common) => {
                merge.roll_up(&common);
                last = seek_past(&common);
                page.common_prefixes.push(common);
            }
            None => {
                last.clone_from(&key);
                page.keys.push(key);
            }
        }
    }
    if merge.next_key()?.is_some() {
        page.next = Some(last);
    }
    Ok(page)
}
