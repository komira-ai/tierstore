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
//! answered `not attached`. These are the attachments of runs: a tree a lookup attached with no run
//! (`core/implicit.rs`) is not listed, and no request here ends it.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
#[cfg(any(target_os = "linux", test))]
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

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
            None if core.is_held() => refused(&AttachError::Replaced.to_string()),
            None => refused("not attached"),
        },
        (Some("detach-all-except"), runs) => {
            let keep: Vec<String> = runs.iter().map(|run| (*run).to_owned()).collect();
            let detached = core.detach_all_except(&keep);
            if core.is_held() {
                return refused(&AttachError::Replaced.to_string());
            }
            serde_json::json!({"ok": true, "detached": detached})
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

/// Whether requests are being answered, and how many are being answered now: a hand-over closes
/// the gate and waits for the requests that passed it.
#[derive(Default)]
struct Gate {
    /// (closed, requests being answered).
    state: Mutex<(bool, usize)>,
    idle: Condvar,
}

/// A request that passed the gate; the gate's count falls when it is dropped.
struct Passed<'a>(&'a Gate);

impl Gate {
    /// Passes a request, or none if the gate is closed.
    fn enter(&self) -> Option<Passed<'_>> {
        let mut state = super::locked(&self.state);
        if state.0 {
            return None;
        }
        state.1 += 1;
        Some(Passed(self))
    }

    /// Closes the gate and returns when no request is being answered.
    #[cfg(any(target_os = "linux", test))]
    fn close_and_drain(&self) {
        let mut state = super::locked(&self.state);
        state.0 = true;
        while state.1 > 0 {
            state = self
                .idle
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Opens the gate again.
    #[cfg(any(target_os = "linux", test))]
    fn open(&self) {
        super::locked(&self.state).0 = false;
    }
}

impl Drop for Passed<'_> {
    fn drop(&mut self) {
        let mut state = super::locked(&self.0.state);
        state.1 -= 1;
        if state.1 == 0 {
            self.0.idle.notify_all();
        }
    }
}

/// One connection: the requests it sends, answered in order, until it closes or the socket is
/// handed over.
fn session(core: &Arc<Core>, gate: &Gate, stream: UnixStream) {
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
        let Some(_passed) = gate.enter() else {
            let _ = write.write_all(answer(&refused(HANDED_OVER)).as_bytes());
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

/// What a request is answered while the socket is being handed over.
const HANDED_OVER: &str = "handed over";

/// The thread that accepts, and the socket that stops it.
struct Acceptor {
    stop: UnixStream,
    thread: std::thread::JoinHandle<()>,
}

/// The socket and the thread that serves it.
pub(in crate::server) struct Control {
    path: PathBuf,
    /// The device and inode of the socket's file as this bound it: a file at the path of another
    /// is the successor's (`nfs/run.rs`), and is not removed.
    file: (u64, u64),
    listener: Arc<UnixListener>,
    core: Arc<Core>,
    gate: Arc<Gate>,
    acceptor: Option<Acceptor>,
    /// Whether the file at `path` is this instance's to remove.
    owns_path: bool,
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
        Self::serve_on(core, listener, path)
    }

    /// Serves the listening socket `listener`, which is bound at `path`: one an earlier instance
    /// gave this one in a hand-over, with the mode and group it made it with.
    ///
    /// # Errors
    /// What the system said when the thread or its pipe cannot be made.
    pub(in crate::server) fn serve_on(
        core: Arc<Core>,
        listener: UnixListener,
        path: &Path,
    ) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        let mut control = Self {
            path: path.to_owned(),
            file: (meta.dev(), meta.ino()),
            listener: Arc::new(listener),
            core,
            gate: Arc::new(Gate::default()),
            acceptor: None,
            owns_path: true,
        };
        control.accept()?;
        Ok(control)
    }

    /// Starts the thread that accepts.
    fn accept(&mut self) -> std::io::Result<()> {
        self.listener.set_nonblocking(true)?;
        let (stopped, stop) = UnixStream::pair()?;
        let listener = Arc::clone(&self.listener);
        let (core, gate) = (Arc::clone(&self.core), Arc::clone(&self.gate));
        let thread = std::thread::Builder::new()
            .name("fs-control".to_owned())
            .spawn(move || {
                loop {
                    let mut fds = [
                        PollFd::new(&*listener, PollFlags::IN),
                        PollFd::new(&stopped, PollFlags::IN),
                    ];
                    // A wait that fails (a signal) is a wait that ends with nothing to read: the
                    // accept below then finds no connection.
                    let _ = poll(&mut fds, None);
                    if !fds[1].revents().is_empty() {
                        return;
                    }
                    // A connection that went away between the wait and the accept is not one.
                    listener.accept().into_iter().for_each(|(stream, _)| {
                        // The listener does not block; on BSD and macOS a connection accepted from
                        // it does not either, and the session reads and waits.
                        let _ = stream.set_nonblocking(false);
                        let (core, gate) = (Arc::clone(&core), Arc::clone(&gate));
                        let _ = std::thread::spawn(move || session(&core, &gate, stream));
                    });
                }
            })?;
        self.acceptor = Some(Acceptor { stop, thread });
        Ok(())
    }

    /// A copy of the listening socket's descriptor, for the successor of a hand-over.
    ///
    /// # Errors
    /// What the system said.
    #[cfg(any(target_os = "linux", test))]
    pub(in crate::server) fn listener_fd(&self) -> std::io::Result<OwnedFd> {
        self.listener.as_fd().try_clone_to_owned()
    }

    /// Stops answering: no connection is accepted from here on (one that comes waits in the
    /// socket's backlog), the request being answered is finished, and the next request of a
    /// connection that is open is refused. The socket's file stays.
    #[cfg(any(target_os = "linux", test))]
    pub(in crate::server) fn pause(&mut self) {
        self.gate.close_and_drain();
        if let Some(Acceptor { stop, thread }) = self.acceptor.take() {
            // The thread polls the read end; a byte on the write end wakes it.
            let _ = rustix::io::write(&stop, &[0]);
            let _ = thread.join();
        }
    }

    /// Answers again after [`Control::pause`].
    ///
    /// # Errors
    /// What the system said when the thread cannot be started.
    #[cfg(any(target_os = "linux", test))]
    pub(in crate::server) fn resume(&mut self) -> std::io::Result<()> {
        self.gate.open();
        self.accept()
    }

    /// Gives up the socket's file to a successor: it is not removed when this is dropped.
    #[cfg(any(target_os = "linux", test))]
    pub(in crate::server) fn leave_path(&mut self) {
        self.owns_path = false;
    }
}

impl Drop for Control {
    /// Stops accepting and removes the socket file, unless it was given to a successor or is
    /// another's by now. Connections that are open finish their request and end when their client
    /// closes.
    fn drop(&mut self) {
        if let Some(Acceptor { stop, thread }) = self.acceptor.take() {
            let _ = rustix::io::write(&stop, &[0]);
            let _ = thread.join();
        }
        let ours =
            std::fs::metadata(&self.path).is_ok_and(|meta| (meta.dev(), meta.ino()) == self.file);
        if self.owns_path && ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
