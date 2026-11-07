//! The completion of a multipart upload that replaces a key that exists, where the format says
//! `active-marks` (`docs/design/put-without-lock.md`, 5.1 and 5.3): S3's `CompleteMultipartUpload`
//! makes the object whatever the key held, and the S3 front calls
//! [`Backend::complete_upload_replacing`](crate::Backend::complete_upload_replacing) for it. The
//! rules are those of the plain put of `plain_put.rs`, with the parts in place of the body:
//!
//! 1. The hot HEAD alone; the marker is read after it, where the key is found (a key the hot tier
//!    does not hold reads no marker, as in `plain_put.rs`).
//! 2. A key the hot tier does not hold is made by the fast path (the object is the hot tier's
//!    create-only completion, the marker is written beside it); a completion that loses a race to
//!    another writer's creation goes on as a replace of what the winner made.
//! 3. A key it holds is replaced under the lock on its marker: the hot tier's completion with
//!    `If-Match` on the tag read, tried again with the tag read again when the tag is refused.
//!    The parts and the upload stay open until a completion has made the object, so the tries
//!    are made with the same parts.
//!
//! An upload begun in the cold tier is refused, as `complete_marked` does, and a hot tier with no
//! room answers [`Error::Full`] with the upload left open; no completion lands in the cold tier.
//! Under every other format the completion is the create-only one of the trait.

use super::Inner;
use super::entry_put::{landing, retry};
use super::marked_put::cold_upload;
use super::pause::WriteRoute;
use super::uploads::{Route as UploadRoute, route};
use crate::{Error, MarkMove, MarkerWhy};

impl Inner {
    /// The completion of the upload `id` of `key`, replacing the key if the format allows.
    pub(super) async fn complete_replacing(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        let WriteRoute::Marks(markers) = self.route_write(key).await? else {
            return self.complete_on_entry(key, id, parts).await;
        };
        let UploadRoute::Hot(upload) = route(id) else {
            return Err(cold_upload(key));
        };
        self.require_live(key)?;
        let (meta, marker) = match self.head_hot(key).await? {
            Some(meta) => (meta, markers.read(key).await?),
            None => {
                let landed = self
                    .land_marked(
                        markers,
                        key,
                        || async {
                            let made = self.hot.complete_upload(key, upload, parts).await;
                            made.map_err(landing)
                        },
                        false,
                    )
                    .await;
                match landed {
                    Err(Error::AlreadyExists(_)) => match self.head_hot(key).await? {
                        Some(meta) => (meta, markers.read(key).await?),
                        None => {
                            let why = "the key was made and removed while the upload completed";
                            return Err(retry(key, why));
                        }
                    },
                    done => return done,
                }
            }
        };
        let mut lock = self
            .take_lock(markers, key, MarkerWhy::Overwrite, marker, false)
            .await?;
        let write = |tag: Option<String>| async move {
            match tag {
                Some(tag) => {
                    let made = self.hot.complete_upload_if_match(key, upload, parts, &tag);
                    made.await
                }
                None => self.hot.complete_upload(key, upload, parts).await,
            }
        };
        let tag = self.replace_with(&mut lock, key, meta.tag, write).await?;
        lock.release(MarkMove::Release { changed: false });
        Ok(tag)
    }
}
