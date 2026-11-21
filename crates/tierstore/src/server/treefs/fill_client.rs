//! The client of the fill socket (`server/fill_socket`): one line out, one line back, over
//! connections kept for the next request. The requests of this process: `fill` (a blob inside an
//! upstream object, or the whole object `cas/l/<sha256>` of a manifest), `touch` (not answered),
//! the pin requests, `put` (a staged file of the put directory, answered in JSON), and the
//! questions about the placements of the node (`place`, `mounted`, `image`, `flat`, `span`,
//! `unplace`), and the reports `started`, `attached` and `fetched`, which a thread of its own sends
//! (see [`FillClient::report`]).
//!
//! A request that meets a closed socket, or a connection that went away (the S3 instance of the
//! node is being updated: the old instance keeps answering through its drain, and the new one
//! binds the socket when it takes the lock), is sent again over a new connection, after a pause
//! of a second, until `fetch_timeout` of pauses has passed. The pauses are taken by `pause`, which
//! a test replaces. An answer that does not come in `fetch_timeout` is not asked for again. A
//! connection that cannot be made for lack of descriptors (`EMFILE`, `ENFILE`) is not tried again:
//! the request fails at once with [`AskError::NoDescriptor`], whose words say so.
//!
//! A request may also be given a time to end by ([`Until`]), on a clock: the manifests of one
//! tree are filled under one such end (`core/placed.rs`), and so are the fills of one open of a
//! file, its tries together (`core/blob.rs`). It is then answered by that end at the
//! latest: its answer is waited for no longer than the time left, and no pause is taken that
//! would pass it. Every exchange sets the time its answer is waited for, so a connection kept
//! after a request with an end of its own, or with a wait of its own (`put`), waits the client's
//! own time for the next.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::server::local_cache::trees::FlatTree;
use crate::{Clock, TreeDigest};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::locked;

/// The answer of a fill socket to a request it does not know: a node of an earlier release.
const UNKNOWN: &str = "refused unknown request";

/// How long a request waits between two tries over a socket that is not there.
const PAUSE: Duration = Duration::from_secs(1);

/// The most connections kept for the next request.
const KEPT: usize = 64;

/// The most reports that wait for the thread that sends them; one more is dropped.
pub(super) const REPORTS_QUEUED: usize = 1024;

/// Why a request did not get its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum AskError {
    /// The socket was not there, or went away, for the whole of `fetch_timeout`.
    #[error("the fill socket is not answering")]
    Unavailable,
    /// The socket took the request and gave no answer in `fetch_timeout`.
    #[error("the fill socket gave no answer in time")]
    TimedOut,
    /// The end the request was given passed before it was answered.
    #[error("the fill socket did not answer within the time the request was given")]
    Spent,
    /// No connection could be made: the process, or the system, has no descriptor left. The
    /// number is the errno the system gave (`EMFILE` or `ENFILE`).
    #[error(
        "{NO_DESCRIPTOR}cannot connect to the fill socket: {}",
        std::io::Error::from_raw_os_error(*.0)
    )]
    NoDescriptor(i32),
}

/// What the words of a failure for lack of descriptors begin with, here and in `core/blob.rs`.
pub(super) const NO_DESCRIPTOR: &str = "no descriptor: ";

/// Whether `error` is a call that failed for lack of descriptors, of the process (`EMFILE`) or of
/// the system (`ENFILE`); its errno if so.
pub(super) fn lacks_descriptors(error: &std::io::Error) -> Option<i32> {
    let errno = rustix::io::Errno::from_io_error(error)?;
    [rustix::io::Errno::MFILE, rustix::io::Errno::NFILE]
        .contains(&errno)
        .then(|| errno.raw_os_error())
}

/// When a request is to be answered by, on a clock: what the fills of one tree's manifests share.
#[derive(Clone, Copy)]
pub(super) struct Until<'a> {
    /// The clock the end is on.
    pub clock: &'a dyn Clock,
    /// The end, in milliseconds of `clock`.
    pub end_ms: u64,
}

impl Until<'_> {
    /// The time left before the end; zero once it has passed.
    pub(super) fn left(&self) -> Duration {
        Duration::from_millis(self.end_ms.saturating_sub(self.clock.now()))
    }
}

/// How long an answer is waited for: `wait`, or the time `left` before the request's end where
/// that is shorter.
fn within(wait: Duration, left: Option<Duration>) -> Duration {
    left.map_or(wait, |left| wait.min(left))
}

/// The answer to a fill that did not put the blob in the cache: the words of the socket
/// (`upstream 404`, `mismatch`, `no space`, ...), or that nobody answered.
pub(super) type FillFailure = String;

/// Where the mount reads the tree of a placement from, as the node answers `image`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PlacedTree {
    /// The tree is the image of this digest, in the tree's directory of the cache directory.
    Image {
        /// The digest of the tree.
        tree: TreeDigest,
        /// The fs locations of the placement.
        paths: Vec<String>,
    },
    /// The tree, flattened.
    Flat(FlatTree),
}

/// Where a path of the mount is among the placements of the node: the answer to `place`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// The fs location of the placement with this id, which the node now holds for the mount.
    Tree(String),
    /// A directory above a location.
    Dir,
    /// Nothing is there.
    Nothing,
}

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
    /// The queue of the thread that sends the reports, made with the first; none if the thread
    /// could not be started.
    reports: OnceLock<Option<SyncSender<String>>>,
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
            reports: OnceLock::new(),
        }
    }

    /// A connection: one kept, or a new one; none where the socket is not there.
    ///
    /// # Errors
    /// [`AskError::NoDescriptor`] for a connection that cannot be made for lack of descriptors.
    fn connection(&self) -> Result<(Option<Connection>, bool), AskError> {
        if let Some(kept) = locked(&self.idle).pop() {
            return Ok((Some(kept), true));
        }
        let made = match UnixStream::connect(&self.path) {
            Ok(stream) => stream,
            Err(error) => {
                return lacks_descriptors(&error).map_or(Ok((None, false)), |errno| {
                    Err(AskError::NoDescriptor(errno))
                });
            }
        };
        // A duration that is not zero is always taken.
        let _ = made.set_read_timeout(Some(self.answer_timeout));
        let made = Connection {
            reader: BufReader::new(made),
        };
        Ok((Some(made), false))
    }

    /// The time an answer is waited for: `fs.fetch_timeout`.
    pub(super) fn answer_timeout(&self) -> Duration {
        self.answer_timeout
    }

    /// Keeps `connection` for the next request.
    fn keep(&self, connection: Connection) {
        let surplus = {
            let mut idle = locked(&self.idle);
            if idle.len() < KEPT {
                idle.push(connection);
                None
            } else {
                Some(connection)
            }
        };
        // A connection that is not kept is closed with the lock of the idle ones released.
        drop(surplus);
    }

    /// Sends `line` and reads one line back, waiting `wait` for it; with `answered` false nothing
    /// is read. The wait is set on the connection for each exchange, so none is left over from
    /// the one before.
    fn exchange(
        connection: &mut Connection,
        line: &str,
        (answered, wait): (bool, Duration),
    ) -> Result<String, Failed> {
        let stream = connection.reader.get_mut();
        // A duration that is not zero is always taken.
        let _ = stream.set_read_timeout(Some(wait.max(Duration::from_millis(1))));
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
        self.ask(line, (answered, None), None)
    }

    /// [`FillClient::ask_line`], waiting `wait` for the answer where given instead of the
    /// client's own time, and answered by `until` where given: [`AskError::Spent`] once it has
    /// passed, or when a pause would pass it before the client's own pauses are used up.
    fn ask(
        &self,
        line: &str,
        (answered, wait): (bool, Option<Duration>),
        until: Option<Until<'_>>,
    ) -> Result<String, AskError> {
        let mut paused = 0;
        loop {
            let left = until.map(|until| until.left());
            if left.is_some_and(|left| left.is_zero()) {
                return Err(AskError::Spent);
            }
            let wait = within(wait.unwrap_or(self.answer_timeout), left);
            let (connection, kept) = self.connection()?;
            if let Some(mut connection) = connection {
                match Self::exchange(&mut connection, line, (answered, wait)) {
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
            if left.is_some_and(|left| left < PAUSE) {
                return Err(AskError::Spent);
            }
            paused += 1;
            (self.pause)(PAUSE);
        }
    }

    /// Asks for the whole object `cas/l/<sha256>` to be put in the cache, answered by `until`.
    pub(super) fn fill_manifest(&self, sha256: &str, until: Until<'_>) -> Result<(), FillFailure> {
        self.fill(&format!("fill {sha256}"), Some(until))
    }

    /// Asks for the blob `sha256` of `size` bytes at `offset` of the upstream object `key` to be
    /// put in the cache, answered by `until` where given.
    pub(super) fn fill_blob(
        &self,
        (sha256, size): (&str, u64),
        (key, offset): (&str, u64),
        until: Option<Until<'_>>,
    ) -> Result<(), FillFailure> {
        self.fill(
            &format!("fill {sha256} {size} from {key} @ {offset}"),
            until,
        )
    }

    fn fill(&self, line: &str, until: Option<Until<'_>>) -> Result<(), FillFailure> {
        match self.ask(line, (true, None), until) {
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
    /// object `key`, create-only, waiting at most `wait` for the answer; the answer is the line of
    /// JSON the socket gives.
    #[cfg(any(target_os = "linux", test))]
    pub(super) fn put(
        &self,
        (key, sha256, size): (&str, &str, u64),
        path: &std::path::Path,
        wait: Duration,
    ) -> Result<String, AskError> {
        let line = format!("put {key} {sha256} {size} from {}", path.display());
        self.ask(&line, (true, Some(wait)), None)
    }

    /// Where the path `path` (names joined by `/`) is among the placements of the node; a
    /// placement found there is held for `mount` until [`FillClient::unplace`]. A node of a
    /// release before the placements, which does not know the request, has none. The error is the
    /// answer that is none of these, or that nobody answered.
    pub(super) fn place(&self, mount: &str, path: &str) -> Result<Place, String> {
        let answer = self
            .ask_line(&format!("place {mount} {path}"), true)
            .map_err(|error| error.to_string())?;
        match answer.split_once(' ') {
            Some(("tree", id)) => Ok(Place::Tree(id.to_owned())),
            None if answer == "dir" => Ok(Place::Dir),
            None if answer == "none" => Ok(Place::Nothing),
            _ if answer == UNKNOWN => Ok(Place::Nothing),
            _ => Err(answer),
        }
    }

    /// The paths of the fs locations of the placements of the node: none from a node of a release
    /// before the placements.
    pub(super) fn mounted(&self) -> Result<Vec<String>, String> {
        let answer = self
            .ask_line("mounted", true)
            .map_err(|error| error.to_string())?;
        if answer == UNKNOWN {
            return Ok(Vec::new());
        }
        serde_json::from_str(&answer).map_err(|_| answer)
    }

    /// Where the tree of the placement `id` is read from: the image of the tree in the cache
    /// directory, by its digest, or the tree flattened. A node of a release before the images,
    /// which does not know the request, is asked for the tree flattened. The error is the answer
    /// (`none` for a placement that is gone, `damaged`, `too large`) or that nobody answered.
    pub(super) fn image(&self, id: &str) -> Result<PlacedTree, String> {
        let answer = self
            .ask_line(&format!("image {id}"), true)
            .map_err(|error| error.to_string())?;
        if answer == UNKNOWN {
            return self.flat(id).map(PlacedTree::Flat);
        }
        if let Some(rest) = answer.strip_prefix("image ") {
            let read = rest.split_once(' ').and_then(|(tree, paths)| {
                let tree = TreeDigest::from_hex(tree)?;
                let paths = serde_json::from_str(paths).ok()?;
                Some(PlacedTree::Image { tree, paths })
            });
            return read.ok_or(answer);
        }
        serde_json::from_str(&answer)
            .map(PlacedTree::Flat)
            .map_err(|_| answer)
    }

    /// The tree of the placement `id`, flattened. The error is the answer (`none` for a
    /// placement that is gone, `damaged`, `too large`) or that nobody answered.
    pub(super) fn flat(&self, id: &str) -> Result<FlatTree, String> {
        let answer = self
            .ask_line(&format!("flat {id}"), true)
            .map_err(|error| error.to_string())?;
        serde_json::from_str(&answer).map_err(|_| answer)
    }

    /// The file of the cache that holds the `size` bytes at `offset` of the pack `pack`, which
    /// hash to `sha256`, and the offset of their first byte in it: filled first if the cache does
    /// not hold them, answered by `until` where given. The error is the words of the answer, or
    /// that nobody answered.
    pub(super) fn span(
        &self,
        (sha256, size): (&str, u64),
        (pack, offset): (&str, u64),
        until: Option<Until<'_>>,
    ) -> Result<(u64, PathBuf), String> {
        let line = format!("span {sha256} {size} from {pack} @ {offset}");
        let answer = self
            .ask(&line, (true, None), until)
            .map_err(|e| e.to_string())?;
        let held = answer
            .strip_prefix("ok ")
            .and_then(|rest| rest.split_once(' '));
        match held.map(|(base, path)| (base.parse::<u64>(), path)) {
            Some((Ok(base), path)) => Ok((base, PathBuf::from(path))),
            _ => Err(answer),
        }
    }

    /// Tells the node that the mount first looked into the tree at the fs location `path`
    /// (`started <path>`), for `tierstore_request_to_start_seconds`; the caller does not wait.
    pub(super) fn report_started(&self, path: &str) {
        self.report(format!("started {path}"));
    }

    /// Sends the report `line` to the node (`started`, `attached`, `fetched`: the node's metrics,
    /// `local_cache/starts.rs` and `local_cache/attaches.rs`); the caller does not wait. A report
    /// is sent by a thread of its own, over connections of its own, so that the request it tells
    /// of, such as the first of a job's command, never waits on the socket. A report that finds
    /// [`REPORTS_QUEUED`] reports waiting is dropped, and one the socket does not take is lost, as
    /// is one a node of a release before it refuses: the metrics are advice.
    pub(super) fn report(&self, line: String) {
        let queue = self.reports.get_or_init(|| self.start_reporting());
        if let Some(queue) = queue {
            let _ = queue.try_send(line);
        }
    }

    /// Starts the thread that sends the reports, with a client of the same socket and times; its
    /// queue. The thread ends when this client is dropped.
    fn start_reporting(&self) -> Option<SyncSender<String>> {
        let (queue, reports) = sync_channel::<String>(REPORTS_QUEUED);
        let pause = Arc::clone(&self.pause);
        let sender = Self::pausing(self.path.clone(), self.tries, self.answer_timeout, pause);
        let thread = std::thread::Builder::new()
            .name("fs-reports".to_owned())
            .spawn(move || {
                while let Ok(line) = reports.recv() {
                    let _ = sender.ask_line(&line, true);
                }
            });
        thread.ok().map(|_| queue)
    }

    /// Ends the hold of `mount` on the placement `id`; nothing is lost if the socket is away.
    pub(super) fn unplace(&self, mount: &str, id: &str) {
        let _ = self.ask_line(&format!("unplace {mount} {id}"), true);
    }

    /// Ends the holds of `mount` on every placement but `ids`; the answer is the node's words, which
    /// a node that does not know the request gives too.
    pub(super) fn places_live(&self, mount: &str, ids: &[String]) -> Result<String, AskError> {
        self.ask_line(&format!("places-live {mount} {}", ids.join(" ")), true)
    }

    /// Waits as long as a request waits between two tries over a socket that is not there.
    pub(super) fn rest(&self) {
        (self.pause)(PAUSE);
    }

    /// [`FillClient::rest`], if that leaves time before `until` for another try; whether it did.
    pub(super) fn rest_before(&self, until: Until<'_>) -> bool {
        // A pause that leaves no time would be followed by a try that is not sent.
        if until.left() <= PAUSE {
            return false;
        }
        (self.pause)(PAUSE);
        true
    }

    /// Keeps the pins of `trees` of `mount` and takes away the others.
    pub(super) fn pins_live(&self, mount: &str, trees: &[String]) -> Result<String, AskError> {
        self.ask_line(&format!("pins-live {mount} {}", trees.join(" ")), true)
    }
}

#[cfg(test)]
#[path = "fill_client_tests.rs"]
mod tests;
