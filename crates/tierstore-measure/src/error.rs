//! The one error type of the crate.

use std::path::{Path, PathBuf};

/// Why an arm could not run or finish.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file system call on `path` failed.
    #[error("{what} {path}: {source}", path = path.display())]
    Io {
        /// The call, as a verb: "open", "read", "write".
        what: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// The error the system gave.
        source: std::io::Error,
    },
    /// An argument or an input is not usable; the text says which and why.
    #[error("{0}")]
    Invalid(String),
    /// A program the arm runs failed or could not be started.
    #[error("command `{cmd}`: {detail}")]
    Command {
        /// The program and its arguments.
        cmd: String,
        /// The exit status and the first line of its standard error, or the start error.
        detail: String,
    },
    /// An HTTP or S3 exchange failed.
    #[error("{0}")]
    Net(String),
    /// A result could not be written as JSON.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// An [`Error::Net`] with `e`'s text.
    pub fn net(e: impl std::fmt::Display) -> Self {
        Self::Net(e.to_string())
    }

    /// An [`Error::Io`] for `what` on `path`.
    pub fn io<'a>(what: &'static str, path: &'a Path) -> impl FnOnce(std::io::Error) -> Self + 'a {
        move |source| Self::Io {
            what,
            path: path.to_path_buf(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_names_the_call_and_the_path() {
        let e = Error::io("open", Path::new("/x/y"))(std::io::Error::other("boom"));
        assert_eq!(e.to_string(), "open /x/y: boom");
    }

    #[test]
    fn net_error_keeps_the_text() {
        assert_eq!(Error::net("refused").to_string(), "refused");
    }

    #[test]
    fn command_error_names_the_command() {
        let e = Error::Command {
            cmd: "xfs_info /mnt".into(),
            detail: "exit status 1".into(),
        };
        assert_eq!(e.to_string(), "command `xfs_info /mnt`: exit status 1");
    }
}
