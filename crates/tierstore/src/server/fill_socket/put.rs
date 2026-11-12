//! The `put` request of the fill socket: the filesystem process hands over a file it staged, and
//! the cache puts it to the upstream as a create-only object that names its SHA-256 (see
//! [`put_stream_sha256`](crate::Backend::put_stream_sha256)), through the cache as a client's
//! create-only put goes.
//!
//! The file is opened without following a link and must be a regular file of the size the
//! request names; it is read once, in chunks, and hashed as it passes to the upstream, never
//! held whole. The answer is one line of JSON:
//!
//! ```text
//! {"ok":true,"created":true}           the object was made
//! {"ok":true,"created":false}          the key held the same bytes (size and SHA-256) already
//! {"ok":false,"error":"<word>"}        nothing was made by this request
//! ```
//!
//! The words: `exists_different` (the key holds other bytes), `checksum` (the file is not the
//! size or the SHA-256 named, or the upstream refused its checksum), `toolarge` (the upstream
//! refused the object for its size), `stage` (the file cannot be opened or read, or is not a
//! regular file) and `upstream` (any other failure: the upstream did not answer, refused, or the
//! object was deleted while the answer was decided; a retry may succeed).

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::BytesMut;
use futures::StreamExt as _;
use futures::stream;
use rustix::fs::{CWD, Mode, OFlags, openat};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::server::local_cache::LocalCache;
use crate::{Backend as _, Error, PartBody, PutOutcome, TreeDigest};

/// The bytes read from the file at a time.
const CHUNK: usize = 256 << 10;

/// The answer to a put that made the object.
const CREATED: &str = r#"{"ok":true,"created":true}"#;

/// The answer to a put that found the same bytes under the key.
const EXISTED: &str = r#"{"ok":true,"created":false}"#;

/// The answer that a put made nothing, for the reason `word`.
fn failed(word: &str) -> String {
    format!(r#"{{"ok":false,"error":"{word}"}}"#)
}

/// The word of the answer for a put that failed with `error`.
fn word_of(error: &Error) -> &'static str {
    match error {
        Error::AlreadyExists(_) => "exists_different",
        Error::BadDigest(_) => "checksum",
        Error::TooLarge(_) => "toolarge",
        _ => "upstream",
    }
}

/// Why the staged file cannot be put.
enum Unfit {
    /// It cannot be opened, or is not a regular file.
    Stage(String),
    /// It is not the size named.
    Size(u64),
}

/// The file at `path`, opened without following a link and checked to be a regular file of
/// `size` bytes.
fn open(path: &Path, size: u64) -> Result<File, Unfit> {
    // `NONBLOCK` so that a FIFO put in its place does not hold the request; a regular file reads
    // as it would without it.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let fd =
        openat(CWD, path, flags, Mode::empty()).map_err(|why| Unfit::Stage(why.to_string()))?;
    let file = File::from(fd);
    let meta = file
        .metadata()
        .map_err(|why| Unfit::Stage(why.to_string()))?;
    if !meta.is_file() {
        return Err(Unfit::Stage("not a regular file".to_owned()));
    }
    if meta.len() != size {
        return Err(Unfit::Size(meta.len()));
    }
    Ok(file)
}

/// The bytes of `file` as a body, read once in chunks of [`CHUNK`]. A read that fails ends the
/// body with an error and sets `broken`.
fn chunks<'a, R>(file: R, broken: &'a AtomicBool) -> PartBody<'a>
where
    R: AsyncRead + Unpin + Send + 'a,
{
    stream::unfold(Some(file), move |file| async move {
        let mut file = file?;
        let mut chunk = BytesMut::with_capacity(CHUNK);
        match file.read_buf(&mut chunk).await {
            Ok(0) => None,
            Ok(_) => Some((Ok(chunk.freeze()), Some(file))),
            Err(why) => {
                broken.store(true, Ordering::Relaxed);
                let text = format!("the staged file cannot be read: {why}");
                Some((Err(Error::Other(text)), None))
            }
        }
    })
    .boxed()
}

/// Puts the file at `path` as the object `key` (module docs) and gives the answer.
pub(super) async fn answer(
    cache: &LocalCache,
    key: &str,
    sha256: TreeDigest,
    size: u64,
    path: &Path,
) -> String {
    let file = match open(path, size) {
        Ok(file) => file,
        Err(Unfit::Stage(why)) => {
            let shown = path.display();
            tracing::warn!("tierstore: the put of {key} cannot open {shown}: {why}");
            return failed("stage");
        }
        Err(Unfit::Size(found)) => {
            let shown = path.display();
            tracing::warn!("tierstore: the put of {key}: {shown} is {found} bytes, not {size}");
            return failed("checksum");
        }
    };
    let file = tokio::fs::File::from_std(file);
    send(cache, key, sha256, size, file).await
}

/// Puts the `size` bytes `file` gives as the object `key` and gives the answer: `stage` if a read
/// of the file failed, whatever the put then said.
async fn send<R>(cache: &LocalCache, key: &str, sha256: TreeDigest, size: u64, file: R) -> String
where
    R: AsyncRead + Unpin + Send,
{
    let broken = AtomicBool::new(false);
    let body = chunks(file, &broken);
    let put = cache.put_stream_sha256(key, size, body, sha256).await;
    match put {
        Ok(PutOutcome::Created) => CREATED.to_owned(),
        Ok(PutOutcome::Existed) => EXISTED.to_owned(),
        Err(error) => {
            tracing::warn!("tierstore: the put of {key} failed: {error}");
            if broken.load(Ordering::Relaxed) {
                failed("stage")
            } else {
                failed(word_of(&error))
            }
        }
    }
}

#[cfg(test)]
#[path = "put_tests.rs"]
mod tests;
