//! The one error type of the harness: what failed, on which file, address or object, and why.

use std::path::PathBuf;

/// A failure of the harness. Each names the file, URL or object it is about.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file or directory could not be read, written or made.
    #[error("{what} {}: {source}", path.display())]
    Io {
        /// What was being done (`read`, `write`, `create`, ...).
        what: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The error of the system.
        source: std::io::Error,
    },
    /// A request could not be sent, or its answer was not what was asked for.
    #[error("{url}: {why}")]
    Http {
        /// The URL asked.
        url: String,
        /// What went wrong (the status, or the transport's error).
        why: String,
    },
    /// Bytes or text that are not what their format says (a manifest, a status page, a line
    /// of a results file).
    #[error("{what}: {why}")]
    Parse {
        /// What was being read (a key, a file and line, a URL).
        what: String,
        /// What is wrong with it.
        why: String,
    },
    /// The read path refused or failed what was asked of it (a plan, the engine, the entry
    /// store, the hash pool, the L2 writer).
    #[error("{what}: {why}")]
    ReadPath {
        /// The step.
        what: &'static str,
        /// Its error.
        why: String,
    },
    /// The arguments ask for something that cannot be done.
    #[error("{0}")]
    Usage(String),
}

impl Error {
    /// An [`Error::Io`] of `what` on `path`.
    pub fn io(what: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            what,
            path: path.into(),
            source,
        }
    }

    /// An [`Error::Parse`].
    pub fn parse(what: impl Into<String>, why: impl Into<String>) -> Self {
        Self::Parse {
            what: what.into(),
            why: why.into(),
        }
    }
}
