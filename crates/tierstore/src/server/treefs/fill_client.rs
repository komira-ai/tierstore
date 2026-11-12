//! The client of the fill socket (`server/fill_socket`): one line out, one line back, over
//! connections kept for the next request. The requests of this process: `fill` (a blob inside an
//! upstream object, or the whole object `cas/l/<sha256>` of a manifest), `touch` (not answered),
//! the pin requests, and `put` (a staged file of the put directory, answered in JSON).
//!
//! A request that meets a closed socket, or a connection that went away (the S3 instance of the
//! node is being updated: the old instance keeps answering through its drain, and the new one
//! binds the socket when it takes the lock), is sent again over a new connection, after a pause
//! of a second, until `fetch_timeout` of pauses has passed. The pauses are taken by `pause`, which
//! a test replaces. An answer that does not come in `fetch_timeout` is not asked for again.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::locked;

/// How long a request waits between two tries over a socket that is not there.
const PAUSE: Duration = Duration::from_secs(1);

/// The most connections kept for the next request.
const KEPT: usize = 64;

/// Why a request did not get its answer.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum AskError {
    /// The socket was not there, or went away, for the whole of `fetch_timeout`.
    #[error("the fill socket is not answering")]
    Unavailable,
    /// The socket took the request and gave no answer in `fetch_timeout`.
    #[error("the fill socket gave no answer in time")]
    TimedOut,
}

/// The answer to a fill that did not put the blob in the cache: the words of the socket
/// (`upstream 404`, `mismatch`, `no space`, ...), or that nobody answered.
pub(super) type FillFailure = String;

/// Why an exchange over a connection failed.
enum Failed {
    /// The connection is closed or broken.
    Gone,
    /// No answer came in time.
    TimedOut,
}

/// A connection and what it has read of the next line.
struct Connection {
    reader: BufReader<UnixStream>,
}

/// The client of one fill socket.
pub(super) struct FillClient {
    path: PathBuf,
    /// The pauses a request may take in all, in whole `PAUSE`s.
    tries: u32,
    /// The time an answer is waited for.
    answer_timeout: Duration,
    idle: Mutex<Vec<Connection>>,
    pause: Arc<dyn Fn(Duration) + Send + Sync>,
}

impl FillClient {
    /// A client of the socket at `path`, that waits `fetch_timeout` for an answer, and for a
    /// socket to answer.
    pub(super) fn new(path: PathBuf, fetch_timeout: Duration) -> Self {
        let tries = u32::try_from(fetch_timeout.as_secs()).unwrap_or(u32::MAX);
        Self::pausing(path, tries, fetch_timeout, Arc::new(std::thread::sleep))
    }

    /// A client that pauses `tries` times at most, by `pause`, and waits `answer_timeout` for an
    /// answer.
    pub(super) fn pausing(
        path: PathBuf,
        tries: u32,
        answer_timeout: Duration,
        pause: Arc<dyn Fn(Duration) + Send + Sync>,
    ) -> Self {
        Self {
            path,
            tries,
            answer_timeout,
            idle: Mutex::new(Vec::new()),
            pause,
        }
    }

    /// A connection: one kept, or a new one.
    fn connection(&self) -> (Option<Connection>, bool) {
        if let Some(kept) = locked(&self.idle).pop() {
            return (Some(kept), true);
        }
        let made = UnixStream::connect(&self.path).ok().map(|stream| {
            // A duration that is not zero is always taken.
            let _ = stream.set_read_timeout(Some(self.answer_timeout));
            Connection {
                reader: BufReader::new(stream),
            }
        });
        (made, false)
    }

    /// Keeps `connection` for the next request.
    fn keep(&self, connection: Connection) {
        let mut idle = locked(&self.idle);
        if idle.len() < KEPT {
            idle.push(connection);
        }
    }

    /// Sends `line` and reads one line back; with `answered` false nothing is read.
    fn exchange(connection: &mut Connection, line: &str, answered: bool) -> Result<String, Failed> {
        let stream = connection.reader.get_mut();
        stream
            .write_all(format!("{line}\n").as_bytes())
            .map_err(|_| Failed::Gone)?;
        if !answered {
            return Ok(String::new());
        }
        let mut answer = String::new();
        match connection.reader.read_line(&mut answer) {
            Ok(got) if got > 0 && answer.ends_with('\n') => {
                Ok(answer.trim_end_matches('\n').to_owned())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Err(Failed::TimedOut)
            }
            _ => Err(Failed::Gone),
        }
    }

    /// The answer to `line`, asked again over a new connection when a connection fails, with a
    /// pause when a new connection fails too.
    fn ask_line(&self, line: &str, answered: bool) -> Result<String, AskError> {
        let mut paused = 0;
        loop {
            let (connection, kept) = self.connection();
            if let Some(mut connection) = connection {
                match Self::exchange(&mut connection, line, answered) {
                    Ok(answer) => {
                        self.keep(connection);
                        return Ok(answer);
                    }
                    Err(Failed::TimedOut) => return Err(AskError::TimedOut),
                    // A kept connection that failed is a socket that was replaced: try again.
                    Err(Failed::Gone) if kept => continue,
                    Err(Failed::Gone) => {}
                }
            }
            if paused >= self.tries {
                return Err(AskError::Unavailable);
            }
            paused += 1;
            (self.pause)(PAUSE);
        }
    }

    /// Asks for the whole object `cas/l/<sha256>` to be put in the cache.
    pub(super) fn fill_manifest(&self, sha256: &str) -> Result<(), FillFailure> {
        self.fill(&format!("fill {sha256}"))
    }

    /// Asks for the blob `sha256` of `size` bytes at `offset` of the upstream object `key` to be
    /// put in the cache.
    pub(super) fn fill_blob(
        &self,
        sha256: &str,
        size: u64,
        key: &str,
        offset: u64,
    ) -> Result<(), FillFailure> {
        self.fill(&format!("fill {sha256} {size} from {key} @ {offset}"))
    }

    fn fill(&self, line: &str) -> Result<(), FillFailure> {
        match self.ask_line(line, true) {
            Ok(answer) if answer == "ok" => Ok(()),
            Ok(answer) => Err(answer),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Counts the blobs as used now; no answer is read, and none is lost if the socket is away.
    pub(super) fn touch(&self, sha256: &[&str]) {
        let _ = self.ask_line(&format!("touch {}", sha256.join(" ")), false);
    }

    /// Pins `blobs` for `tree` of `mount`; the answer is the numbers or the words of the refusal.
    pub(super) fn pin(&self, mount: &str, tree: &str, blobs: &[&str]) -> Result<String, AskError> {
        self.ask_line(&format!("pin {mount} {tree} {}", blobs.join(" ")), true)
    }

    /// Takes away the pins of `tree` of `mount`.
    pub(super) fn unpin(&self, mount: &str, tree: &str) -> Result<String, AskError> {
        self.ask_line(&format!("unpin {mount} {tree}"), true)
    }

    /// Asks for the `size` bytes of the file at `path`, which hash to `sha256`, to be put as the
    /// object `key`, create-only; the answer is the line of JSON the socket gives.
    pub(super) fn put(
        &self,
        key: &str,
        sha256: &str,
        size: u64,
        path: &std::path::Path,
    ) -> Result<String, AskError> {
        let line = format!("put {key} {sha256} {size} from {}", path.display());
        self.ask_line(&line, true)
    }

    /// Keeps the pins of `trees` of `mount` and takes away the others.
    pub(super) fn pins_live(&self, mount: &str, trees: &[String]) -> Result<String, AskError> {
        self.ask_line(&format!("pins-live {mount} {}", trees.join(" ")), true)
    }
}

#[cfg(test)]
#[path = "fill_client_tests.rs"]
mod tests;
