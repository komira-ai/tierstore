//! The one error type of the harness: what failed, on which file or process, and why.

use std::path::{Path, PathBuf};

/// A failure of the harness. Each names the file, process or line it is about.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file or directory could not be read, written or opened.
    #[error("{what} {}: {source}", path.display())]
    Io {
        /// What was being done (`read`, `open`, `write`, `walk`, ...).
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
    /// A child process could not be started, or said something the protocol does not have, or
    /// ended without its answer.
    #[error("{process}: {why}")]
    Child {
        /// The child (`reader 3`, `burner 0`, `null-fs`).
        process: String,
        /// What went wrong.
        why: String,
    },
    /// The arguments ask for something that cannot be done.
    #[error("{0}")]
    Usage(String),
}

impl Error {
    /// An [`Error::Io`] of `what` on `path`.
    pub(crate) fn io(what: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            what,
            path: path.to_path_buf(),
            source,
        }
    }

    /// An [`Error::Child`] of `process`.
    pub(crate) fn child(process: &str, why: impl Into<String>) -> Self {
        Self::Child {
            process: process.to_owned(),
            why: why.into(),
        }
    }

    /// For `map_err`: an [`Error::Io`] of `what` on `path`.
    pub(crate) fn at<'a>(
        what: &'static str,
        path: &'a Path,
    ) -> impl FnOnce(std::io::Error) -> Self + 'a {
        move |source| Self::io(what, path, source)
    }

    /// For `map_err`: an [`Error::Child`] of `process`, saying that `what` failed and why.
    pub(crate) fn of<'a, E: std::fmt::Display>(
        process: &'a str,
        what: &'a str,
    ) -> impl FnOnce(E) -> Self + 'a {
        move |e| Self::child(process, format!("{what}: {e}"))
    }
}
