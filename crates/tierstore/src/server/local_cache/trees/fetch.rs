//! Making a file of a placement readable: the span of the cached pack that holds its bytes, filled
//! from the upstream first if the cache does not hold it. Every interface reads a file this way,
//! the S3 front by placement and path ([`TreeStore::open_file`]) and the mount by the span it
//! already knows ([`TreeStore::fetch_span`]), so both answer with the same bytes.

use super::super::{RangeError, RangeWant, Span};
use super::read::{FileSpan, ReadError, Resolved};
use super::{PlacementId, ReadHold, TreeStore};
use crate::{Mode, TreeDigest};

/// Why a file of a placement was not made readable.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The path was not answered: see [`ReadError`].
    #[error(transparent)]
    Read(#[from] ReadError),
    /// The path is a directory or a symbolic link, which have no bytes to read; what it is.
    #[error("not a file")]
    NotAFile(Resolved),
    /// The cache could not fill the range from the upstream: see [`RangeError`].
    #[error("fill: {0}")]
    Fill(#[from] RangeError),
    /// The range was filled, and was gone from the cache when it was looked for (evicted, or
    /// replaced by a write through the cache, in between).
    #[error("the filled range is no longer in the cache")]
    Lost,
}

/// A file of a placement, readable: its bytes are `size` bytes of `span`'s file from its offset.
#[derive(Debug)]
pub struct OpenFile {
    /// The file's mode.
    pub mode: Mode,
    /// Its length.
    pub size: u64,
    /// The SHA-256 of its bytes.
    pub sha256: TreeDigest,
    /// Where its bytes are in the cache; none exactly when the file is empty.
    pub span: Option<Span>,
    /// The hold of the placement: it is not taken away while this is kept.
    pub hold: ReadHold,
}

impl TreeStore {
    /// The file `path` of the placement `id`, with its bytes in the cache: the placement is held
    /// from before the path is resolved, and the span is filled if the cache does not hold it.
    ///
    /// # Errors
    /// [`FetchError::Read`] with [`ReadError::NoPlacement`] for a placement that is not alive (and
    /// the other [`ReadError`]s for the path), [`FetchError::NotAFile`] for a directory or a link,
    /// and what [`fetch_span`](Self::fetch_span) gives.
    pub async fn open_file(&self, id: &PlacementId, path: &str) -> Result<OpenFile, FetchError> {
        let hold = self
            .open_read(id)
            .map_err(|_| FetchError::Read(ReadError::NoPlacement))?;
        let resolved = self.resolve(id, path)?;
        let Resolved::File {
            mode,
            size,
            sha256,
            span,
        } = resolved
        else {
            return Err(FetchError::NotAFile(resolved));
        };
        let span = match span {
            Some(span) => Some(self.fetch_span(&span, size, &sha256).await?),
            None => None,
        };
        Ok(OpenFile {
            mode,
            size,
            sha256,
            span,
            hold,
        })
    }

    /// Where the `size` bytes (above 0) at `span` are in the cache, filled from the upstream and
    /// checked against `sha256` first if the cache does not hold them.
    ///
    /// # Errors
    /// [`FetchError::Fill`] if the fill failed, [`FetchError::Lost`] if the range was gone from
    /// the cache once filled.
    pub async fn fetch_span(
        &self,
        span: &FileSpan,
        size: u64,
        sha256: &TreeDigest,
    ) -> Result<Span, FetchError> {
        let cache = &self.shared.cache;
        if let Some(held) = cache.span_of(&span.pack, span.offset, size) {
            return Ok(held);
        }
        let hex = sha256.to_string();
        let want = RangeWant {
            pack: &span.pack,
            known: None,
            offset: span.offset,
            len: size,
            sha256: &hex,
        };
        cache.fill_range(&want).await?;
        cache
            .span_of(&span.pack, span.offset, size)
            .ok_or(FetchError::Lost)
    }
}
