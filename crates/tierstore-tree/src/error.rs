//! [`TreeError`]: why a tree could not be opened.

use std::io;

use tierstore_fetch_api::list::ListVersionUnsupported;
use tierstore_list::Malformed;
use tierstore_mount_client::ClientError;

/// Why a tree could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// The image file could not be read or mapped.
    #[error("tree image: {what}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// The list is not one this release reads (a version 1 or 2 list, an unknown version or
    /// digest function): the tree is not mountable through tierstore, and its caller takes the
    /// miss path (3.5).
    #[error(transparent)]
    ListVersionUnsupported(#[from] ListVersionUnsupported),
    /// The list's bytes break a rule of the format, or are of another tree.
    #[error("tree image: {0}")]
    Malformed(#[from] Malformed),
    /// A pack of the tree could not be opened through the mount protocol.
    #[error("tree pack {pack}: {source}")]
    Open {
        /// The number of the pack in the list.
        pack: u32,
        /// The mount client's answer.
        #[source]
        source: ClientError,
    },
}
