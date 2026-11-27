//! [`Backend::put_stream_digested`](crate::Backend::put_stream_digested) of the S3 backend: the
//! create-only put of `put_stream_answered` and the plain put of `put_stream_replacing`, each
//! sending the digests the request named on its PUT for the store to check (`Content-MD5`,
//! `x-amz-checksum-sha256`), so that a store that checks them refuses a body cut short; and
//! [`Backend::short_objects`](crate::Backend::short_objects), the counts of the store's look at
//! the key after a PUT that got no answer and of its aborts that failed.

use objstore_s3::s3::{Cleanups, CutPuts, PutChecksums};

use super::{S3Backend, map_error, object_key, put_answer, streamed};
use crate::{BodyDigests, Error, PartBody, PutAnswer, PutKind, ShortObjects};

/// The put (module docs). A backend with one place to put an object writes
/// [`PutKind::CreatesHotOnly`] as [`PutKind::Creates`].
pub(super) async fn put(
    backend: &S3Backend,
    key: &str,
    len: Option<u64>,
    body: PartBody<'_>,
    kind: PutKind<'_>,
    digests: BodyDigests,
) -> Result<Option<PutAnswer>, Error> {
    let object = object_key(key)?;
    let (store, object) = (&backend.store, &object);
    let checksums = PutChecksums {
        md5: digests.md5,
        sha256: digests.sha256,
    };
    let replaces = matches!(kind, PutKind::Replaces(_));
    let sending = |len, source| async move {
        if replaces {
            store
                .put_stream_replacing_checked(object, len, source, checksums)
                .await
        } else {
            store
                .put_stream_new_checked(object, len, source, checksums)
                .await
        }
    };
    let sent = streamed(body, len, backend.chunks_waiting, sending).await?;
    let answer = sent.map_err(|e| map_error(e, key))?;
    Ok(put_answer(answer))
}

/// The counts of the store's looks and failed aborts (module docs).
pub(super) fn short_objects(backend: &S3Backend) -> ShortObjects {
    let CutPuts {
        removed,
        left,
        failed,
    } = backend.store.cut_puts();
    let Cleanups {
        looks_skipped,
        failed_aborts,
        aborts_dropped,
    } = backend.store.cleanups();
    ShortObjects {
        removed,
        left,
        failed,
        skipped: looks_skipped,
        failed_aborts,
        aborts_dropped,
    }
}

impl S3Backend {
    /// Waits until no task of the store's work after failed writes (the look after a PUT that
    /// got no answer, the abort of a failed upload) runs: for a test that must see what that work
    /// came to.
    pub async fn cleanups_idle(&self) {
        self.store.cleanups_idle().await;
    }
}
