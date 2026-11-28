//! The bytes of a file as the body of a put: read once, in chunks, never held whole. The put
//! ingest reads its committed files so, and so does the put of a file the put directory of the
//! file system staged (`treefs/in_process/put.rs`).

use std::sync::atomic::{AtomicBool, Ordering};

use bytes::BytesMut;
use futures::StreamExt as _;
use futures::stream;
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::{Error, PartBody};

/// The bytes read from the file at a time.
const CHUNK: usize = 256 << 10;

/// The bytes of `file` as a body, read once in chunks of [`CHUNK`]. A read that fails ends the
/// body with an error and sets `broken`.
pub(in crate::server) fn chunks<'a, R>(file: R, broken: &'a AtomicBool) -> PartBody<'a>
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
