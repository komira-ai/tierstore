//! The listing of a directory tier: the files of the root and of the subdirectory of `_entries/`,
//! as the keys they stand for.

use std::io::ErrorKind;
use std::path::Path;

use tokio::fs;

use crate::{Error, Listed};

use super::entries;
use super::names::{decode, encode};
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
        let key = file
            .to_str()
            .filter(|name| !name.starts_with('.'))
            .and_then(|name| {
                let key = decode(name)?;
                let plain = !base.is_empty() || entries::rest(&key).is_none();
                (plain || encode(&key) == name).then_some(key)
            })
            .map(|key| format!("{base}{key}"))
            .filter(|key| key.starts_with(prefix));
        let Some(key) = key else {
            continue;
        };
        let meta = entry.metadata().await.ok();
        let Some(meta) = meta.filter(std::fs::Metadata::is_file) else {
            continue;
        };
        found.push(Listed {
            key,
            size: meta.len().saturating_sub(TAG_LEN as u64),
            modified: modified_of(&meta),
        });
    }
    Ok(())
}
