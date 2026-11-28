//! Why a run, or the check of a page, ended without a report.

use std::path::{Path, PathBuf};

/// Why a run, or the check of a page, ended without a report. Each says what it was doing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file or directory could not be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// What the system said.
        #[source]
        source: std::io::Error,
    },
    /// A file of the example is not what the runner reads: it does not parse, a value is out of
    /// range, a placeholder is unknown.
    #[error("{}: {why}", file.display())]
    Invalid {
        /// The file.
        file: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// The page does not show the example's files as they are.
    #[error("{}: {}", page.display(), problems.join("; "))]
    Page {
        /// The page.
        page: PathBuf,
        /// What differs, one entry for each file or block.
        problems: Vec<String>,
    },
    /// The work directory exists and holds something: a run starts from an empty one.
    #[error("{}: the work directory is not empty", path.display())]
    WorkNotEmpty {
        /// The work directory.
        path: PathBuf,
    },
    /// The example's client is not installed: it could not be started.
    #[error("the client {program} is not installed: {source}")]
    ClientMissing {
        /// The program the example names.
        program: String,
        /// Why it could not be started.
        #[source]
        source: std::io::Error,
    },
    /// The example's client is installed in another version than the one the example pins.
    #[error("the client {program} is {found:?}, not {expected:?}")]
    ClientVersion {
        /// The program the example names.
        program: String,
        /// The first line the example wants it to print.
        expected: String,
        /// The first line it printed.
        found: String,
    },
    /// tierstore ended before it said where it listens.
    #[error("tierstore did not start ({status}); its log {}:\n{log_text}", log.display())]
    TierstoreStart {
        /// How it ended.
        status: String,
        /// The file of its output.
        log: PathBuf,
        /// What that file holds.
        log_text: String,
    },
    /// A program of a step, or of the closing commands, could not be started.
    #[error("step {step}: {program} could not be started: {source}")]
    Spawn {
        /// The step (`cleanup` for a closing command).
        step: String,
        /// The program.
        program: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// A command of a step ended with a status other than 0.
    #[error("step {step}: `{command}` ended with {status}; its output is in {}", log.display())]
    Command {
        /// The step.
        step: String,
        /// The command's words, joined by spaces.
        command: String,
        /// How it ended.
        status: String,
        /// The file of its standard error (and of its standard output, unless that went to a
        /// file of its own).
        log: PathBuf,
    },
    /// `/metricz` could not be read.
    #[error("reading /metricz of {addr}: {why}")]
    Metrics {
        /// tierstore's S3 front.
        addr: String,
        /// What went wrong.
        why: String,
    },
    /// A check could not be evaluated.
    #[error("check {check}: {why}")]
    Check {
        /// The check.
        check: String,
        /// Why.
        why: String,
    },
}

/// The [`Error::Io`] of `path`, for `map_err`.
pub fn io_at(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// The [`Error::Invalid`] of `file`, for `map_err`.
pub fn invalid_at(file: &Path) -> impl FnOnce(String) -> Error + '_ {
    move |why| Error::Invalid {
        file: file.to_path_buf(),
        why,
    }
}
