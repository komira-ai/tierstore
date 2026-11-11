//! The fill socket of a build node: a Unix socket in the node's state directory, over which the
//! filesystem process (`tierstore fs`) asks the node's tierstore to put blobs in its cache. The
//! requests are in [`request`]; what each does to the cache is in
//! [`local_cache`](super::local_cache) (module `source`).
//!
//! # Who may ask
//!
//! The socket is mode 0600 and its peer's user id must be the instance's own, checked on every
//! connection as the hand-off socket checks its taker (see [`handoff`](super::handoff)); any other
//! peer is answered `refused` and disconnected.
//!
//! # When it is bound
//!
//! Only by the instance that holds the lock of the cache directory, and while it holds it: the
//! instance that finds the lock taken by another binds nothing, and one that takes the lock binds
//! at the tick in which it takes it (the cache says so on a channel, see
//! `LocalCache::watch_mode`), replacing the file the instance before it left. An instance that
//! loses the lock closes the socket and its connections. A hand-over of the S3 instance therefore
//! needs no step of its own: the old instance answers through its drain, and the new one binds
//! when the old one has exited and released the lock. The file is removed by the instance that
//! made it, if the name still leads to it, so that the removal by an instance that is going cannot
//! take the file of the one that took over. A client that finds the socket closed asks again.
//!
//! # The conversation
//!
//! One line per request, one line per answer, in the order of the requests of a connection: a
//! connection runs one request at a time, so a client that wants fills to run at once opens
//! several. `ok` answers a fill that is in the cache; the other answers are the words of
//! `FillError` (`upstream <status>`, `mismatch`, `short`, `no space`, `too large`, `busy`,
//! `read-only`, `io`, `changed`, `aborted`) and `refused <why>` for a line that is not a request.
//! A `touch` is never answered. The pin requests are answered `ok <pinned> <refused> <absent>`
//! (`pin`: the blobs now pinned by the tree, the blobs refused for the cache's `pin_limit`, the
//! blobs the cache does not hold), `ok <blobs>` (`unpin`: the blobs the tree had pinned) and
//! `ok <trees>` (`pins-live`: the trees whose pins were taken away), or `read-only` from an
//! instance that does not hold the lock of the cache directory; the mount id `hold`, which the
//! cache's holds pin under, is refused with its words (`the mount id "hold" is reserved for holds`).

mod request;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use super::ACCEPT_PAUSE;
use super::local_cache::{Blob, FillError};
use super::local_cache::{LocalCache, Mode, PinError};
use request::{MAX_LINE, Request, parse};

/// How long the socket waits after it could not be bound before it tries again.
const REBIND_PAUSE: Duration = Duration::from_secs(1);

/// The socket file this process made, and removed when it is dropped if the name still leads to
/// it.
struct SocketFile {
    path: PathBuf,
    identity: (u64, u64),
}

impl SocketFile {
    /// What the name `path` leads to now: its device and inode.
    fn identity_of(path: &Path) -> Option<(u64, u64)> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        Some((meta.dev(), meta.ino()))
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if Self::identity_of(&self.path) == Some(self.identity) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A socket that is bound, and the connections it has accepted.
struct Listening {
    listener: UnixListener,
    connections: JoinSet<()>,
    /// Dropped after the listener and the connections: the file goes last.
    _file: SocketFile,
}

impl Listening {
    /// Binds `path`, replacing the file an instance that is gone left there, with mode 0600.
    fn bind(path: &Path) -> std::io::Result<Self> {
        match std::fs::remove_file(path) {
            Err(why) if why.kind() != std::io::ErrorKind::NotFound => return Err(why),
            _ => {}
        }
        let listener = UnixListener::bind(path)?;
        let file = SocketFile {
            path: path.to_owned(),
            identity: SocketFile::identity_of(path).unwrap_or_default(),
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            connections: JoinSet::new(),
            _file: file,
        })
    }

    /// Takes the next connection and serves it on a task of its own, or reaps one that ended.
    async fn step(&mut self, cache: &LocalCache, uid: u32) {
        tokio::select! {
            accepted = self.listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let _ = self.connections.spawn(session(cache.clone(), stream, uid));
                }
                Err(why) => {
                    tracing::warn!("tierstore: the fill socket cannot accept: {why}");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                }
            },
            Some(_) = self.connections.join_next() => {}
        }
    }
}

/// One step of `socket`, or never when there is none.
async fn step(socket: Option<&mut Listening>, cache: &LocalCache, uid: u32) {
    match socket {
        Some(socket) => socket.step(cache, uid).await,
        None => std::future::pending().await,
    }
}

/// Serves the fill socket at `path` for `cache` until `halt` is notified: bound while the cache
/// is read-write, closed while it is not. Requests that are being answered when `halt` comes are
/// dropped with their connection; the fills they started go on.
pub(super) async fn serve(cache: LocalCache, path: &Path, halt: Arc<Notify>) {
    serve_as(cache, path, halt, rustix::process::geteuid().as_raw()).await;
}

/// [`serve`] for peers of the user `uid`.
async fn serve_as(cache: LocalCache, path: &Path, halt: Arc<Notify>, uid: u32) {
    let mut modes = cache.watch_mode();
    let mut socket: Option<Listening> = None;
    loop {
        let writable = *modes.borrow_and_update() == Mode::ReadWrite;
        if !writable {
            socket = None;
        } else if socket.is_none() {
            socket = bind(path);
        }
        let rebind = writable && socket.is_none();
        tokio::select! {
            biased;
            () = halt.notified() => break,
            _ = modes.changed() => {}
            () = pause_if(rebind, REBIND_PAUSE) => {}
            () = step(socket.as_mut(), &cache, uid) => {}
        }
    }
}

/// Binds the socket, saying on stderr why it could not.
fn bind(path: &Path) -> Option<Listening> {
    match Listening::bind(path) {
        Ok(socket) => {
            tracing::info!("tierstore: the fill socket is at {}", path.display());
            Some(socket)
        }
        Err(why) => {
            tracing::warn!(
                "tierstore: the fill socket at {} cannot be bound: {why}",
                path.display()
            );
            None
        }
    }
}

/// Waits `pause` if `wanted`; never otherwise.
async fn pause_if(wanted: bool, pause: Duration) {
    if wanted {
        tokio::time::sleep(pause).await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// One connection: the requests it sends, answered in order, until it closes.
async fn session(cache: LocalCache, stream: UnixStream, uid: u32) {
    // A peer whose credentials cannot be read is as good as another user.
    let user = stream.peer_cred().map(|cred| cred.uid());
    let (read, mut write) = stream.into_split();
    if user.ok() != Some(uid) {
        let _ = write
            .write_all(b"refused not the user of this instance\n")
            .await;
        return;
    }
    let mut read = BufReader::new(read);
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut limited = (&mut read).take(MAX_LINE as u64 + 1);
        if !matches!(limited.read_until(b'\n', &mut line).await, Ok(got) if got > 0) {
            return;
        }
        if line.last() == Some(&b'\n') {
            let _ = line.pop();
        } else {
            // A line longer than a request may be, or the end of the stream inside a line.
            if line.len() > MAX_LINE {
                let _ = write.write_all(b"refused the request is too long\n").await;
            }
            return;
        }
        let answer = match parse(&line) {
            Ok(Request::Touch(digests)) => {
                cache.touch_blobs(digests.iter().map(String::as_str));
                continue;
            }
            Ok(Request::Blob {
                sha256,
                size,
                from,
                offset,
            }) => {
                let blob = Blob {
                    sha256: &sha256,
                    size,
                    from: &from,
                    offset,
                };
                said(cache.fill_blob(&blob).await)
            }
            Ok(Request::Manifest { sha256 }) => said(cache.fill_manifest(&sha256).await),
            Ok(Request::Pin { mount, tree, blobs }) => pinned(
                cache
                    .pin(&mount, &tree, &blobs)
                    .map(|got| format!("{} {} {}", got.pinned, got.refused, got.absent)),
            ),
            Ok(Request::Unpin { mount, tree }) => {
                pinned(cache.unpin(&mount, &tree).map(|count| count.to_string()))
            }
            Ok(Request::PinsLive { mount, trees }) => pinned(
                cache
                    .pins_live(&mount, &trees)
                    .map(|count| count.to_string()),
            ),
            Err(refusal) => format!("refused {refusal}"),
        };
        if write
            .write_all(format!("{answer}\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
    }
}

/// The answer to a pin request: `ok` and the numbers, or the words of the failure. Every digest
/// was checked when the line was parsed, so the failures left are a read-only cache and the mount
/// id of the holds.
fn pinned(done: Result<String, PinError>) -> String {
    match done {
        Ok(numbers) => format!("ok {numbers}"),
        Err(PinError::ReadOnly) => "read-only".to_owned(),
        Err(other) => other.to_string(),
    }
}

/// The answer to a fill: `ok`, or the words of the failure.
fn said(filled: Result<(), FillError>) -> String {
    filled.map_or_else(|error| error.to_string(), |()| "ok".to_owned())
}

#[cfg(test)]
mod serve_tests;
