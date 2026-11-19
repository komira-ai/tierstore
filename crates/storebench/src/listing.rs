//! The listing every offline arm reads: each object of silo's buckets, one line each.
//!
//! ```text
//! <bucket> TAB <key> TAB <size> TAB <modified_ms> TAB <etag>
//! ```
//!
//! `modified_ms` is the time of the object's last write as the listing states it (milliseconds
//! since the Unix epoch); `etag` is without its quotes (a multipart object's tag ends `-<parts>`).
//! [`list`] writes it from silo page by page, so a listing of any size needs memory for one page;
//! [`read`] reads it back.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use objstore_s3::s3::S3Store;
use objstore_s3::{KeyPrefix, ListToken, PageSize};

use crate::Error;

/// One object of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    /// The bucket.
    pub bucket: String,
    /// The key.
    pub key: String,
    /// Its size in bytes.
    pub size: u64,
    /// When it was last written, in milliseconds since the Unix epoch.
    pub modified_ms: u64,
    /// Its entity tag, without quotes.
    pub etag: String,
}

impl Object {
    /// Whether the object was written by a multipart upload (its tag ends `-<parts>`).
    #[must_use]
    pub fn multipart(&self) -> bool {
        self.etag
            .rsplit_once('-')
            .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }
}

/// Writes every object of `store` (the bucket `bucket`) to `out` as listing lines, in key
/// order, and returns how many it wrote.
///
/// # Errors
/// [`Error::Store`] when a page cannot be read; [`Error::Io`] when `out` cannot be written
/// (`path` names it).
pub async fn list(
    store: &S3Store,
    bucket: &str,
    out: &mut dyn Write,
    path: &Path,
) -> Result<u64, Error> {
    let everything = KeyPrefix::new("").map_err(|e| Error::Usage(e.to_string()))?;
    let mut token: Option<ListToken> = None;
    let mut count = 0u64;
    loop {
        let page = store
            .list_stamped(&everything, token.as_ref(), PageSize::MAX)
            .await
            .map_err(|e| Error::store(format!("list {bucket}"), e))?;
        for o in &page.objects {
            writeln!(
                out,
                "{bucket}\t{}\t{}\t{}\t{}",
                o.key.as_str(),
                o.size,
                o.modified_millis,
                o.etag.trim_matches('"')
            )
            .map_err(|e| Error::io("write", path, e))?;
            count += 1;
        }
        match page.next {
            Some(next) => token = Some(next),
            None => return Ok(count),
        }
    }
}

/// Parses one listing line (`path` and `line` name it in an error).
///
/// # Errors
/// [`Error::Parse`] when the line does not have five fields or a number is not one.
pub fn parse_line(text: &str, path: &Path, line: usize) -> Result<Object, Error> {
    let fields: Vec<&str> = text.split('\t').collect();
    let [bucket, key, size, modified, etag] = fields.as_slice() else {
        return Err(Error::parse(
            path,
            line,
            format!("{} fields, not 5", fields.len()),
        ));
    };
    let number = |what: &str, v: &str| {
        v.parse::<u64>()
            .map_err(|_| Error::parse(path, line, format!("{what} {v:?} is not a number")))
    };
    Ok(Object {
        bucket: (*bucket).to_owned(),
        key: (*key).to_owned(),
        size: number("size", size)?,
        modified_ms: number("modified_ms", modified)?,
        etag: (*etag).to_owned(),
    })
}

/// Every object of the listing at `path`, in file order.
///
/// # Errors
/// [`Error::Io`] when the file cannot be read; [`Error::Parse`] for a line that is not one.
pub fn read(path: &Path) -> Result<Vec<Object>, Error> {
    let file = std::fs::File::open(path).map_err(|e| Error::io("open", path, e))?;
    let mut objects = Vec::new();
    for (i, text) in BufReader::new(file).lines().enumerate() {
        let text = text.map_err(|e| Error::io("read", path, e))?;
        if text.is_empty() {
            continue;
        }
        objects.push(parse_line(&text, path, i + 1)?);
    }
    Ok(objects)
}

/// Creates `path` for writing a listing.
///
/// # Errors
/// [`Error::Io`] when it cannot be created.
pub fn create(path: &Path) -> Result<BufWriter<std::fs::File>, Error> {
    std::fs::File::create(path)
        .map(BufWriter::new)
        .map_err(|e| Error::io("create", path, e))
}

#[cfg(test)]
#[path = "listing_tests.rs"]
mod tests;
