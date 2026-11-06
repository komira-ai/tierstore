//! The writes of the files of objects in a directory tier that stream a body to a new file: the
//! chunks of a put, the parts joined by the completion of an upload, and the tag written over the
//! room at the start of the file after the last byte.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::{Error, PartBody, TAG_LEN, create_new, finish, io_error, stored_parts};

/// Writes the chunks of `body` to a new file at `path`, and gives their SHA-256.
pub(super) async fn write_stream(
    path: &Path,
    body: &mut PartBody<'_>,
) -> Result<[u8; TAG_LEN], Error> {
    let fail = io_error(path);
    let mut file = create_new(path).await.map_err(&fail)?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(&fail)?;
    }
    let flushed = file.flush().await;
    flushed.and(file.sync_all().await).map_err(&fail)?;
    Ok(finish(hasher))
}

/// The files of the parts of the upload in `dir` that `parts` names, in the order given.
///
/// # Errors
/// [`Error::InvalidPart`] if a part named is not stored, or is stored with another content tag.
pub(super) async fn named_part_files(
    dir: &Path,
    parts: &[(u32, String)],
) -> Result<Vec<PathBuf>, Error> {
    let stored = stored_parts(dir).await?;
    let mut files = Vec::new();
    for (number, tag) in parts {
        let found = stored.iter().find(|part| part.is(*number, tag));
        let Some(found) = found else {
            return Err(Error::InvalidPart(format!("part {number}")));
        };
        files.push(dir.join(&found.name));
    }
    Ok(files)
}

/// Writes the contents of `files`, one after the other, to a new file at `path` behind the
/// content tag of the whole, which is written last, and gives the tag.
pub(super) async fn join_files(
    path: &Path,
    files: &[PathBuf],
    chunk: usize,
) -> Result<[u8; TAG_LEN], Error> {
    let fail = io_error(path);
    let mut out = create_new(path).await.map_err(&fail)?;
    out.write_all(&[0_u8; TAG_LEN]).await.map_err(&fail)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; chunk];
    for file in files {
        let mut input = File::open(file).await.map_err(io_error(file))?;
        loop {
            let count = input.read(&mut buffer).await.map_err(io_error(file))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            out.write_all(&buffer[..count]).await.map_err(&fail)?;
        }
    }
    seal(&mut out, hasher).await.map_err(&fail)
}

/// Writes the chunks of `body` to a new file at `path` behind room for the content tag, then
/// the tag of the whole, which is known only after the last chunk, and syncs the file. A body
/// that ends with an error leaves a file that is not an object (the caller removes it). Gives
/// the tag.
pub(super) async fn write_object_stream(
    path: &Path,
    body: &mut PartBody<'_>,
) -> Result<[u8; TAG_LEN], Error> {
    let fail = io_error(path);
    let mut out = create_new(path).await.map_err(&fail)?;
    out.write_all(&[0_u8; TAG_LEN]).await.map_err(&fail)?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        out.write_all(&chunk).await.map_err(&fail)?;
    }
    seal(&mut out, hasher).await.map_err(&fail)
}

/// Writes the tag of the hash `hasher` has come to over the room at the start of `out`, and
/// syncs the file. Gives the tag.
pub(super) async fn seal(out: &mut File, hasher: Sha256) -> std::io::Result<[u8; TAG_LEN]> {
    let tag = finish(hasher);
    out.seek(SeekFrom::Start(0)).await?;
    out.write_all(&tag).await?;
    out.flush().await?;
    out.sync_all().await?;
    Ok(tag)
}
