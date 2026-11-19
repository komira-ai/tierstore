//! The one error type of the measurements: what failed, on which file, object or line, and why.

use std::path::PathBuf;

/// A failure of a measurement. Each names the file, object or line it is about.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file or directory could not be read, written or opened.
    #[error("{what} {}: {source}", path.display())]
    Io {
        /// What was being done (`read`, `create`, `write`, `walk`, ...).
        what: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The error of the system.
        source: std::io::Error,
    },
    /// A file had a line that is not what its format says.
    #[error("{}:{line}: {why}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The line, from 1.
        line: usize,
        /// What is wrong with it.
        why: String,
    },
    /// The object store refused or failed a request.
    #[error("{what}: {source}")]
    Store {
        /// The request (`list kbf-cas`, `GET kbf-cas/cas/...`).
        what: String,
        /// The store's error.
        source: objstore_s3::ObjectStoreError,
    },
    /// The arguments ask for something that cannot be done.
    #[error("{0}")]
    Usage(String),
}

impl Error {
    /// An [`Error::Io`] of `what` on `path`.
    pub(crate) fn io(what: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            what,
            path: path.into(),
            source,
        }
    }

    /// An [`Error::Parse`] of line `line` of `path`.
    pub(crate) fn parse(path: impl Into<PathBuf>, line: usize, why: impl Into<String>) -> Self {
        Self::Parse {
            path: path.into(),
            line,
            why: why.into(),
        }
    }

    /// An [`Error::Store`] of the request `what`.
    pub(crate) fn store(what: impl Into<String>, source: objstore_s3::ObjectStoreError) -> Self {
        Self::Store {
            what: what.into(),
            source,
        }
    }
}
