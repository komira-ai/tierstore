//! The multipart uploads of the proxy: which tier an upload is made in, how the id that a client
//! holds names that tier, and the calls that follow the id.
//!
//! An upload is made in the hot tier whenever the hot tier has room, as a put is, and in the cold
//! tier only when the hot tier is full (the level is `Full`, or the hot tier answers `Full` to the
//! creation). The choice is made once, at the creation, and is written into the id the proxy gives
//! the client: the id of a hot upload is [`HOT_PREFIX`] followed by the hot tier's own id, and any
//! other id is the cold tier's own, unchanged. So an id that a release before this one gave
//! (it never had the prefix) names a cold upload, and an upload made in the cold tier is named as
//! it always was. The parts, the completion and the abort follow the id; no state is kept.
//!
//! The completion of a hot upload is a put on the entry of its key (`entry_put.rs`), like the
//! completion of a cold one, and leaves the entry `(PUT, PENDING)` for the reconciler to copy.

use std::sync::Arc;

use super::entry_put::landing;
use super::{Inner, Level, refuse_reserved};
use crate::journal::is_reserved;
use crate::{Backend, Error, OpenUpload, PartBody, UploadedPart, UploadsAfter};

/// What an upload id of the hot tier starts with, in the id the proxy gives a client. The ids of
/// the stores (S3 services, the directory and memory backends) have no `~`.
pub(super) const HOT_PREFIX: &str = "ts-hot~";

/// The tier an upload id names, and the id the tier knows.
enum Route<'a> {
    Hot(&'a str),
    Cold(&'a str),
}

fn route(id: &str) -> Route<'_> {
    match id.strip_prefix(HOT_PREFIX) {
        Some(inner) => Route::Hot(inner),
        None => Route::Cold(id),
    }
}

/// Where the listing of one tier's open uploads starts.
enum Start {
    /// At the first upload of the prefix.
    First,
    /// After the upload of the key and id, or after every upload of the key where there is no id.
    After(String, Option<String>),
    /// At the first upload of the key, that upload included.
    AtKey(String),
}

/// Where the listings of the cold and the hot tier start for the marker of a client. The listing
/// is in key order, and within a key the uploads of the cold tier come before those of the hot
/// tier, so that the id of the marker says where each tier resumes.
fn starts(after: Option<UploadsAfter<'_>>) -> (Start, Start) {
    let owned = |key: &str, id: Option<&str>| Start::After(key.to_owned(), id.map(str::to_owned));
    match after {
        None => (Start::First, Start::First),
        Some((key, None)) => (owned(key, None), owned(key, None)),
        Some((key, Some(id))) => match route(id) {
            Route::Hot(inner) => (owned(key, None), owned(key, Some(inner))),
            Route::Cold(id) => (owned(key, Some(id)), Start::AtKey(key.to_owned())),
        },
    }
}

impl Inner {
    /// Starts an upload of `key`: in the hot tier unless it is full, where the cold tier takes it.
    pub(super) async fn start_upload(&self, key: &str) -> Result<String, Error> {
        self.admit_write(key).await?;
        if self.level().await < Level::Full {
            match self.hot.create_upload(key).await {
                Ok(id) => return Ok(format!("{HOT_PREFIX}{id}")),
                Err(Error::Full(_)) => self.latch_full(),
                Err(error) => return Err(error),
            }
        }
        self.cold.create_upload(key).await
    }

    /// Stores a part in the tier the id names. A hot tier that has no room for the part answers
    /// [`Error::Full`], which the S3 front makes a 503 `SlowDown`; the part is not moved to the
    /// cold tier, for the parts already stored are in the hot tier. The usage is not asked: an
    /// upload that began with room goes on until the tier itself refuses a part.
    pub(super) async fn send_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.admit_write(key).await?;
        match route(id) {
            Route::Hot(inner) => {
                let sent = self.hot.upload_part_with_len(key, inner, number, len, body);
                self.note_full(sent.await)
            }
            Route::Cold(id) => {
                let sent = self.cold.upload_part_with_len(key, id, number, len, body);
                sent.await
            }
        }
    }

    /// Completes an upload in the tier the id names, on the entry of its key: the object is made by
    /// that tier's own create-only completion. A hot tier that has no room to make the object
    /// answers [`Error::Full`] and leaves the upload open, so that the same completion can be tried
    /// again.
    pub(super) async fn complete_on_entry(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.admit_write(key).await?;
        let (tier, id, hot) = match route(id) {
            Route::Hot(inner) => (&self.hot, inner, true),
            Route::Cold(id) => (&self.cold, id, false),
        };
        let done = || async move {
            let made = tier.complete_upload(key, id, parts).await;
            let made = if hot { self.note_full(made) } else { made };
            made.map_err(landing)
        };
        self.put_on_entry(key, done).await
    }

    /// Drops an upload in the tier the id names.
    pub(super) async fn drop_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        refuse_reserved(key)?;
        match route(id) {
            Route::Hot(inner) => self.hot.abort_upload(key, inner).await,
            Route::Cold(id) => self.cold.abort_upload(key, id).await,
        }
    }

    /// A page of the parts of an open upload, from the tier the id names.
    pub(super) async fn list_upload_parts(
        &self,
        key: &str,
        id: &str,
        after: Option<u32>,
        max_parts: usize,
    ) -> Result<Vec<UploadedPart>, Error> {
        refuse_reserved(key)?;
        match route(id) {
            Route::Hot(inner) => self.hot.list_parts(key, inner, after, max_parts).await,
            Route::Cold(id) => self.cold.list_parts(key, id, after, max_parts).await,
        }
    }

    /// Latches the hot tier full when it answered `Full`; the answer goes on.
    fn note_full<T>(&self, answer: Result<T, Error>) -> Result<T, Error> {
        if matches!(answer, Err(Error::Full(_))) {
            self.latch_full();
        }
        answer
    }

    /// A page of the open uploads of both tiers, in key order (the cold tier's uploads of a key
    /// before the hot tier's), at most `max_uploads`, after the marker. The ids of the hot tier's
    /// uploads are the ids a client holds, with the prefix.
    pub(super) async fn list_open_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        if max_uploads == 0 {
            return Ok(Vec::new());
        }
        let (cold_start, hot_start) = starts(after);
        let cold = self.tier_uploads(&self.cold, prefix, cold_start, max_uploads);
        let mut found = cold.await?;
        let hot = self.tier_uploads(&self.hot, prefix, hot_start, max_uploads);
        found.extend(hot.await?.into_iter().map(|up| OpenUpload {
            id: format!("{HOT_PREFIX}{}", up.id),
            ..up
        }));
        // A stable sort keeps the cold tier's uploads of a key before the hot tier's.
        found.sort_by(|a, b| a.key.cmp(&b.key));
        found.truncate(max_uploads);
        Ok(found)
    }

    /// The first `want` open uploads of `tier` from `start`, without those of a reserved key.
    async fn tier_uploads(
        &self,
        tier: &Arc<dyn Backend>,
        prefix: &str,
        start: Start,
        want: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let after = match start {
            Start::First => None,
            Start::After(key, id) => Some((key, id)),
            Start::AtKey(key) => {
                // The tier's marker excludes the uploads of its key, so those are read first.
                let mut found = Vec::new();
                if key.starts_with(prefix) {
                    found = self.page_uploads(tier, &key, None, want).await?;
                    found.retain(|up| up.key == key);
                }
                if found.len() < want {
                    let rest = Some((key, None));
                    let more = self
                        .page_uploads(tier, prefix, rest, want - found.len())
                        .await?;
                    found.extend(more);
                }
                return Ok(found);
            }
        };
        self.page_uploads(tier, prefix, after, want).await
    }

    /// A page of `want` open uploads of `tier` after the marker, made up from the next pages of the
    /// tier when the filter of reserved keys cut one short, so that a short page is the last.
    async fn page_uploads(
        &self,
        tier: &Arc<dyn Backend>,
        prefix: &str,
        mut after: Option<(String, Option<String>)>,
        want: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let mut found: Vec<OpenUpload> = Vec::new();
        let mut calls = 0;
        while found.len() < want {
            calls += 1;
            let limit = self.config.upload_listing_calls;
            if calls > limit {
                let text = format!("the listing of uploads of a tier took over {limit} calls");
                return Err(Error::Unavailable(text));
            }
            let ask = want - found.len();
            let marker = after
                .as_ref()
                .map(|(key, id)| (key.as_str(), id.as_deref()));
            let page = tier.list_uploads(prefix, marker, ask).await?;
            let last = page.last().map(|up| (up.key.clone(), Some(up.id.clone())));
            let full = page.len() == ask;
            found.extend(page.into_iter().filter(|up| !is_reserved(&up.key)));
            match last {
                Some(last) if full => {
                    // A full page that ends where the last one did would be asked for for ever.
                    if after.as_ref().is_some_and(|before| last <= *before) {
                        let text = "a tier's listing of uploads did not advance";
                        return Err(Error::Unavailable(text.to_owned()));
                    }
                    after = Some(last);
                }
                _ => break,
            }
        }
        Ok(found)
    }
}
