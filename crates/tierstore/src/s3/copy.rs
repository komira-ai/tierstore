//! [`Backend::copy_in_store`](crate::Backend::copy_in_store) of the S3 backend: the store's own
//! CopyObject or UploadPartCopy, so the bytes never leave the store, with the source's
//! conditions sent for the store to test against the object it copies.
//!
//! A copy that moves more than `server.s3_single_put_max` (5 GiB by default, S3's limit for one
//! copy as for one PUT) is not made here, nor one the store answers 501 `NotImplemented` (a store
//! that does not copy): the caller copies it with a read and a put, which the backend sends as a
//! multipart upload when it is long. A copy of an object writes the key with no condition on it,
//! as a plain PUT does.

use objstore_s3::ObjectStoreError;
use objstore_s3::s3::{CopyAnswer, CopyConditions, CopySource};

use super::{S3Backend, client_range, map_error, object_key, service_code, unquoted, upload_error};
use crate::{CopyTarget, Error, PutAnswer, StoreCopy};

/// The copy (module docs).
pub(super) async fn copy(
    backend: &S3Backend,
    copy: &StoreCopy<'_>,
) -> Result<Option<PutAnswer>, Error> {
    if copy.moved() > backend.store.single_put_max() {
        return Ok(None);
    }
    let source_key = object_key(copy.source)?;
    let key = object_key(copy.key)?;
    let said = copy.conditions;
    let conditions = CopyConditions {
        if_match: said.if_match.clone(),
        if_none_match: said.if_none_match.clone(),
        if_modified_since: said.if_modified_since.clone(),
        if_unmodified_since: said.if_unmodified_since.clone(),
    };
    let source = CopySource {
        key: &source_key,
        size: copy.source_size,
        conditions: &conditions,
    };
    let (made, id) = match copy.target {
        CopyTarget::Object => (backend.store.copy_object(&source, &key).await, None),
        CopyTarget::Part {
            upload_id,
            number,
            range,
        } => {
            let range = range.and_then(client_range);
            let store = &backend.store;
            let made = store.upload_part_copy(&source, &key, upload_id, number, range);
            (made.await, Some(upload_id))
        }
    };
    match made {
        Ok(answer) => Ok(Some(answered(answer))),
        Err(ObjectStoreError::Service { status: 501, .. }) => Ok(None),
        Err(error) => Err(refused(error, copy, id)),
    }
}

/// What the store's answer says of the object or the part made: its tag without quotes, and
/// its time if the answer names one.
fn answered(answer: CopyAnswer) -> PutAnswer {
    PutAnswer {
        tag: unquoted(&answer.etag),
        modified: answer.modified_millis,
    }
}

/// What a refused copy means: a source the store does not hold is [`Error::NotFound`] naming the
/// source; anything else is what it is for a write of the key, or of the part of the upload `id`.
fn refused(error: ObjectStoreError, copy: &StoreCopy<'_>, id: Option<&str>) -> Error {
    if service_code(&error) == "NoSuchKey" {
        return Error::NotFound(copy.source.to_owned());
    }
    match id {
        Some(id) => upload_error(error, copy.key, id),
        None => map_error(error, copy.key),
    }
}
