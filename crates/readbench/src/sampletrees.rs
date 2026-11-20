//! A sample of the trees a store holds, from the filer's listing of the directory `cas/l/`
//! (JSON, in pages): the listing is read from just after a key, at most a number of entries
//! deep, keeping the entries that are recent and of a size that can be a manifest, and of those
//! the ones whose manifest parses and has enough directories. The listing is in name order, so
//! the same store gives the same sample.

use serde::Deserialize;

use crate::Error;
use crate::manifest::{Child, Fetch, manifest_key, parse};
use crate::swfs::FilerFetch;

/// The most entries asked of the filer in one page.
const PAGE: u64 = 1000;

/// Which trees to keep.
#[derive(Debug, Clone)]
pub struct Query {
    /// List the entries named after this key (empty: from the start).
    pub after: String,
    /// Look at most this many entries of the listing.
    pub scan: u64,
    /// Keep entries whose `Mtime` is not before this RFC 3339 time (compared as text; empty keeps
    /// all).
    pub since: String,
    /// Keep entries of at least this many bytes.
    pub min_bytes: u64,
    /// Keep entries of at most this many bytes.
    pub max_bytes: u64,
    /// Keep manifests with at least this many directories among their children.
    pub min_dirs: usize,
    /// Stop after this many trees.
    pub count: usize,
}

/// What a sampling found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Sampled {
    /// The roots kept, in listing order.
    pub roots: Vec<String>,
    /// Entries of the listing looked at.
    pub scanned: u64,
    /// Entries that passed the time and size bounds but are not manifests (loose blobs).
    pub unparsed: u64,
}

/// One page of the filer's answer.
#[derive(Deserialize)]
struct Page {
    #[serde(rename = "Entries")]
    entries: Option<Vec<Listed>>,
    #[serde(rename = "ShouldDisplayLoadMore", default)]
    more: bool,
}

/// One entry of a page.
#[derive(Deserialize)]
struct Listed {
    #[serde(rename = "FullPath")]
    full_path: String,
    #[serde(rename = "FileSize", default)]
    size: u64,
    #[serde(rename = "Mtime", default)]
    mtime: String,
}

impl Listed {
    /// The entry's name: the last part of its path.
    fn name(&self) -> &str {
        self.full_path.rsplit('/').next().unwrap_or_default()
    }

    /// Whether the entry is recent enough and of a size the query allows.
    fn passes(&self, q: &Query) -> bool {
        self.mtime.as_str() >= q.since.as_str()
            && self.size >= q.min_bytes
            && self.size <= q.max_bytes
    }
}

/// Reads one page of the listing: up to `limit` entries named after `after`.
async fn page(fetch: &FilerFetch, limit: u64, after: &str) -> Result<(Vec<Listed>, bool), Error> {
    let url = format!("{}/cas/l/?limit={limit}&lastFileName={after}", fetch.base);
    let json = fetch.http.get_bytes(&url, true).await?;
    let page: Page = serde_json::from_slice(&json).map_err(|e| Error::parse(url, e.to_string()))?;
    Ok((page.entries.unwrap_or_default(), page.more))
}

/// Samples the store's trees as `q` says.
///
/// # Errors
/// A page of the listing or the manifest of a kept entry could not be read.
pub async fn sample_trees(fetch: &FilerFetch, q: &Query) -> Result<Sampled, Error> {
    let mut found = Sampled::default();
    let mut after = q.after.clone();
    while found.roots.len() < q.count && found.scanned < q.scan {
        let (entries, more) = page(fetch, (q.scan - found.scanned).min(PAGE), &after).await?;
        for entry in &entries {
            if found.roots.len() >= q.count {
                break;
            }
            found.scanned += 1;
            if !entry.passes(q) {
                continue;
            }
            let key = manifest_key(entry.name());
            match parse(&key, &fetch.get(&key).await?) {
                Ok(children) => {
                    let dirs = children
                        .iter()
                        .filter(|c| matches!(c, Child::Dir { .. }))
                        .count();
                    if dirs >= q.min_dirs {
                        found.roots.push(entry.name().to_owned());
                    }
                }
                Err(_) => found.unparsed += 1,
            }
        }
        let Some(last) = entries.last().filter(|_| more) else {
            break;
        };
        after = last.name().to_owned();
    }
    Ok(found)
}
