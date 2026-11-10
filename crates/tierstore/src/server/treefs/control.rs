//! The control socket: how a client attaches a tree for a run, and ends the attachments of a run.
//! A Unix socket in the runtime directory, mode 0660, group `fs.client_group`; one line out, one
//! line of JSON back, in the order of the requests of a connection.
//!
//! ```text
//! attach <sha256> for <run>        {"ok":true,"path":"<mountpoint>/trees/<sha256>"}
//! attach <sha256> for <run> prefetch all   the same; every blob of the tree is also filled in the
//!                                  background, in manifest order, and pinned, without delaying
//!                                  the answer
//! detach <run>                     {"ok":true,"failures":[{"path":..,"digest":..,"reason":..}]}
//! detach-all-except [<run> ...]    {"ok":true,"detached":<number of runs>}
//! attachments                      {"ok":true,"attachments":[{"tree":..,"run":..}]}
//! ```
//!
//! A request that is refused is answered `{"ok":false,"error":"<why>"}`. `attach` is answered when
//! the tree's manifests are read and flattened. `detach` of a run that attached nothing is
//! answered `not attached`.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustix::event::{PollFd, PollFlags, poll};

use super::attach::Failure;
use super::core::{AttachError, Core};

/// The longest request, in bytes: a `detach-all-except` of about a thousand runs.
const MAX_LINE: usize = 64 * 1024;

/// The answer to a request, as a line.
fn answer(value: &serde_json::Value) -> String {
    format!("{value}\n")
}

/// The answer that refuses with `why`.
fn refused(why: &str) -> serde_json::Value {
    serde_json::json!({"ok": false, "error": why})
}

/// The answer to the request `line`.
pub(super) fn handle(core: &Arc<Core>, line: &str) -> serde_json::Value {
    let mut words = line.split(' ');
    match (words.next(), words.collect::<Vec<_>>().as_slice()) {
        (Some("attach"), [digest, "for", run]) => attached(core.attach(digest, run)),
        (Some("attach"), [digest, "for", run, "prefetch", "all"]) => {
            attached(core.attach_asking(digest, run, true))
        }
        (Some("detach"), [run]) => match core.detach(run) {
            Some(failures) => serde_json::json!({"ok": true, "failures": failures_of(&failures)}),
            None => refused("not attached"),
        },
        (Some("detach-all-except"), runs) => {
            let keep: Vec<String> = runs.iter().map(|run| (*run).to_owned()).collect();
            serde_json::json!({"ok": true, "detached": core.detach_all_except(&keep)})
        }
        (Some("attachments"), []) => {
            let listed: Vec<_> = core
                .attachments()
                .iter()
                .map(|(tree, run)| serde_json::json!({"tree": tree.to_string(), "run": run}))
                .collect();
            serde_json::json!({"ok": true, "attachments": listed})
        }
        _ => refused("unknown request"),
    }
}

/// The answer to an attach that ended in `result`.
fn attached(result: Result<PathBuf, AttachError>) -> serde_json::Value {
    match result {
        Ok(path) => serde_json::json!({"ok": true, "path": path}),
        Err(why) => refused(&why.to_string()),
    }
}

/// The failures as JSON.
fn failures_of(failures: &[Failure]) -> serde_json::Value {
    serde_json::json!(failures)
}

/// One connection: the requests it sends, answered in order, until it closes.
fn session(core: &Arc<Core>, stream: UnixStream) {
    let (mut read, mut write) = (BufReader::new(&stream), &stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut limited = (&mut read).take(MAX_LINE as u64 + 1);
        if !matches!(limited.read_until(b'\n', &mut line), Ok(got) if got > 0) {
            return;
        }
        let text = if line.last() == Some(&b'\n') {
            line.pop();
            String::from_utf8_lossy(&line).into_owned()
        } else {
            // A line longer than a request may be, or the end of the stream inside a line.
            if line.len() > MAX_LINE {
                let _ = write.write_all(answer(&refused("the request is too long")).as_bytes());
            }
            return;
        };
        if write
            .write_all(answer(&handle(core, &text)).as_bytes())
            .is_err()
        {
            return;
        }
    }
}

/// The socket and the thread that serves it.
pub(in crate::server) struct Control {
    path: PathBuf,
    /// The device and inode of the socket's file as this bound it: a file at the path of another
    /// is the successor's (`nfs/run.rs`), and is not removed.
    file: (u64, u64),
    /// The end of a pair that wakes the thread, which waits for a connection or for it.
    wake: UnixStream,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Control {
    /// Binds the control socket at `path`, replacing the file a process that is gone left, with
    /// mode 0660 and the group `gid`, and serves it on a thread of its own.
    ///
    /// # Errors
    /// What the system said when the socket cannot be bound or given its mode and group.
    pub(in crate::server) fn serve(
        core: Arc<Core>,
        path: &Path,
        gid: u32,
    ) -> std::io::Result<Self> {
        // A file that stays is found by the bind.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
        rustix::fs::chown(path, None, Some(rustix::fs::Gid::from_raw(gid)))?;
        let meta = std::fs::metadata(path)?;
        let (wake, woken) = UnixStream::pair()?;
        let thread = std::thread::Builder::new()
            .name("fs-control".to_owned())
            .spawn(move || {
                loop {
                    let mut fds = [
                        PollFd::new(&listener, PollFlags::IN),
                        PollFd::new(&woken, PollFlags::IN),
                    ];
                    // A signal that interrupts the wait is retried; any other failure ends the
                    // thread, which a drop then still joins.
                    if rustix::io::retry_on_intr(|| poll(&mut fds, None)).is_err() {
                        return;
                    }
                    if !fds[1].revents().is_empty() {
                        return;
                    }
                    if let Ok((stream, _)) = listener.accept() {
                        let core = Arc::clone(&core);
                        let _ = std::thread::spawn(move || session(&core, stream));
                    }
                }
            })?;
        Ok(Self {
            path: path.to_owned(),
            file: (meta.dev(), meta.ino()),
            wake,
            thread: Some(thread),
        })
    }
}

impl Drop for Control {
    /// Stops accepting and removes the socket file, unless it is another's by now. Connections
    /// that are open finish their request and end when their client closes.
    fn drop(&mut self) {
        // The thread waits for a connection or for this byte.
        let _ = (&self.wake).write_all(b"x");
        let _ = self.thread.take().map(std::thread::JoinHandle::join);
        let ours =
            std::fs::metadata(&self.path).is_ok_and(|meta| (meta.dev(), meta.ino()) == self.file);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
