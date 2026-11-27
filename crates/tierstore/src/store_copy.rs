//! What a copy that a store makes of its own object names ([`Backend::copy_in_store`]): the
//! source, its conditions, and the object or the part it writes.
//!
//! [`Backend::copy_in_store`]: crate::Backend::copy_in_store

use crate::ByteRange;

/// The conditions a copy sets on its source, each the text of its header as the client sent it
/// (`x-amz-copy-source-if-match` and the rest). `None` names none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceConditions {
    /// `x-amz-copy-source-if-match`: the entity tags of which the source must have one.
    pub if_match: Option<String>,
    /// `x-amz-copy-source-if-none-match`: the entity tags the source must not have.
    pub if_none_match: Option<String>,
    /// `x-amz-copy-source-if-modified-since`: an HTTP date the source must be written after.
    pub if_modified_since: Option<String>,
    /// `x-amz-copy-source-if-unmodified-since`: an HTTP date the source must not be written after.
    pub if_unmodified_since: Option<String>,
}

/// What a copy writes under its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyTarget<'a> {
    /// The object: replaced if the key has one, as a plain put replaces it.
    Object,
    /// A part of an open multipart upload of the key.
    Part {
        /// The upload.
        upload_id: &'a str,
        /// The number of the part, 1 to 10000.
        number: u32,
        /// The bytes of the source the part is made of; all of them for `None`.
        range: Option<ByteRange>,
    },
}

/// A copy of the object `source` into `key`, as [`CopyTarget`] says.
#[derive(Clone, Copy, Debug)]
pub struct StoreCopy<'a> {
    /// The key of the source.
    pub source: &'a str,
    /// The size of the source as the caller read it, which says how many bytes the copy moves.
    pub source_size: u64,
    /// The conditions on the source, for the store to test against the object it copies.
    pub conditions: &'a SourceConditions,
    /// The key the copy writes.
    pub key: &'a str,
    /// The object or the part it writes.
    pub target: CopyTarget<'a>,
}

impl StoreCopy<'_> {
    /// The number of bytes the copy moves: the length of the range of a part, or the size of the
    /// source.
    #[must_use]
    pub fn moved(&self) -> u64 {
        match self.target {
            CopyTarget::Part {
                range: Some(range), ..
            } => range.size(),
            CopyTarget::Object | CopyTarget::Part { range: None, .. } => self.source_size,
        }
    }
}
