//! The control socket of the `fs` interface of the instance that serves: one request line out, one
//! line back, over `std::os::unix::net` (`treefs/control.rs` is the other end).
//!
//! The socket is named by the node's file; its path is taken under the root, as every file of the
//! host is. The write and the read each wait at most the timeout, and an answer is read up to
//! [`MAX_ANSWER`] bytes: one that is longer, or that ends before its newline, is no answer.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::ports::{Control, PortError, PortKind};
use super::host::under;

const KIND: PortKind = PortKind::Instances;
/// The most of an answer that is read: the answers the config hook asks for are one short line.
const MAX_ANSWER: u64 = 64 * 1024;

/// A client of the control sockets of the host under `root`.
pub struct ControlSocket {
    root: PathBuf,
    timeout: Duration,
}

impl ControlSocket {
    /// A client whose write of a request and read of its answer each fail after `timeout`.
    #[must_use]
    pub fn new(root: PathBuf, timeout: Duration) -> ControlSocket {
        ControlSocket { root, timeout }
    }
}

impl Control for ControlSocket {
    fn ask(&self, socket: &Path, line: &str) -> Result<String, PortError> {
        let at = under(&self.root, socket);
        let failed =
            |what: String| PortError::failed(KIND, format!("{line:?} to {}: {what}", at.display()));
        let asked = || -> std::io::Result<String> {
            let stream = UnixStream::connect(&at)?;
            stream.set_write_timeout(Some(self.timeout))?;
            stream.set_read_timeout(Some(self.timeout))?;
            (&stream).write_all(format!("{line}\n").as_bytes())?;
            let mut answer = String::new();
            let _ = BufReader::new((&stream).take(MAX_ANSWER)).read_line(&mut answer)?;
            Ok(answer)
        };
        let answer = asked().map_err(|error| failed(error.to_string()))?;
        answer
            .strip_suffix('\n')
            .map(str::to_owned)
            .ok_or_else(|| failed(format!("no whole line answered ({} bytes)", answer.len())))
    }
}
