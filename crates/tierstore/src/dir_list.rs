//! The listing of a directory tier: the files of the root and of the subdirectory of `_entries/`,
//! as the keys they stand for. A file with a hashed name (see the module `dir_names`) stands for
//! the key in its header, which costs one more read for each.

use std::io::{ErrorKind, SeekFrom};
use std::path::Path;

use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::{Error, Listed};

use super::entries;
use super::names::{MAX_KEY, decode, encode, hashed_name, is_hashed_shape};
use super::{TAG_LEN, io_error, modified_of};

/// The objects whose key starts with `prefix`, in key order: those of the root, with the files of
/// the previous release for the keys under `_entries/`, and those of the subdirectory, read only
/// if `prefix` can match one of them. A key in both is listed once, from the root, whose file is
/// the key's (see the module `dir_entries`).
pub(super) async fn list(root: &Path, prefix: &str) -> Result<Vec<Listed>, Error> {
    let mut found = Vec::new();
    scan(root, "", prefix, &mut found).await?;
    if entries::PREFIX.starts_with(prefix) || prefix.starts_with(entries::PREFIX) {
        let dir = root.join(entries::DIR);
        scan(&dir, entries::PREFIX, prefix, &mut found).await?;
    }
    found.sort_by(|a, b| a.key.cmp(&b.key));
    found.dedup_by(|later, first| later.key == first.key);
    Ok(found)
}

/// Adds to `found` the objects of the directory `dir` whose key, which is `base` and then what
/// the file name decodes to, starts with `prefix`. A root (`base` empty) takes a file for a key
/// under `_entries/` only where its name is the one the key is written to, and a subdirectory
/// that is not there has no objects.
async fn scan(dir: &Path, base: &str, prefix: &str, found: &mut Vec<Listed>) -> Result<(), Error> {
    let fail = io_error(dir);
    let opened = fs::read_dir(dir).await;
    let mut files = match opened {
        Err(why) if !base.is_empty() && why.kind() == ErrorKind::NotFound => return Ok(()),
        opened => opened.map_err(&fail)?,
    };
    while let Some(entry) = files.next_entry().await.map_err(&fail)? {
        let file = entry.file_name();
        let Some(name) = file.to_str().filter(|name| !name.starts_with('.')) else {
            continue;
        };
        // The key of a hashed name is in the file, so a name of that shape is read; a file that
        // does not hold a key with that hashed name is a plain file.
        let held = match is_hashed_shape(name) {
            true => hashed_key(&entry.path(), name, !base.is_empty()).await,
            false => None,
        };
        let Some((key, header)) = held.or_else(|| plain_key(name, base)) else {
            continue;
        };
        if !key.starts_with(prefix) {
            continue;
        }
        let meta = entry.metadata().await.ok();
        let Some(meta) = meta.filter(std::fs::Metadata::is_file) else {
            continue;
        };
        // The tag is the first bytes of the file, the same ones a HEAD reads; a file that
        // cannot be read through them (gone, or shorter than a tag) is not an object.
        let Some(tag) = read_tag(&entry.path()).await else {
            continue;
        };
        found.push(Listed {
            key,
            size: meta.len().saturating_sub(TAG_LEN as u64 + header),
            modified: modified_of(&meta),
            tag: hex::encode(tag),
        });
    }
    Ok(())
}

/// The key, and the length of the header behind the tag (none), of the plain file `name`, which
/// is `base` and what `name` decodes to. A root (`base` empty) takes a file for a key under
/// `_entries/` only where its name is the one the key is written to.
fn plain_key(name: &str, base: &str) -> Option<(String, u64)> {
    let key = decode(name)?;
    let plain = !base.is_empty() || entries::rest(&key).is_none();
    (plain || encode(&key) == name).then(|| (format!("{base}{key}"), 0))
}

/// The key, and the length of the header behind the tag, held by the file at `path` if `name` is
/// the hashed name of that key and the file is where that key's file is: in the subdirectory of
/// `_entries/` if `in_entries`, else in the root.
async fn hashed_key(path: &Path, name: &str, in_entries: bool) -> Option<(String, u64)> {
    let mut file = fs::File::open(path).await.ok()?;
    file.seek(SeekFrom::Start(TAG_LEN as u64)).await.ok()?;
    let len = file.read_u64().await.ok()?;
    let mut key = Vec::new();
    (&mut file)
        .take(len.min(MAX_KEY as u64))
        .read_to_end(&mut key)
        .await
        .ok()?;
    let key = String::from_utf8(key).ok()?;
    let held = (
        hashed_name(&key),
        key.len() as u64,
        entries::rest(&key).is_some(),
    );
    (held == (Some(name.to_owned()), len, in_entries)).then(|| (key, 8 + len))
}

/// The content tag stored at the start of the file at `path`, or `None` if it cannot be read.
async fn read_tag(path: &Path) -> Option<[u8; TAG_LEN]> {
    let mut file = fs::File::open(path).await.ok()?;
    let mut tag = [0_u8; TAG_LEN];
    file.read_exact(&mut tag).await.ok()?;
    Some(tag)
}
