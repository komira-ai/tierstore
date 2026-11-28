//! What the rig asks of the operating system: a program it starts and stops ([`Process`]), the
//! resident memory of that process (`/proc/<pid>/status`), the size of a file system, and a file
//! system filled to its last byte ([`fill`]).
//!
//! A started program's standard output is copied to `<logs>/<name>.out` as it comes and its
//! standard error goes to `<logs>/<name>.err`, so a failed run leaves both for a person to read.
//! It counts as started once it has printed its start line, the first line of its standard
//! output with a word `listen=<address>`: tierstore's `tierstore <version> listen=<address> ...`
//! and `fake-s3 listen=<address> ...`. A program that ends first is an error naming its exit.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};

use crate::clock::Stopwatch;

/// The bytes a [`fill`] writes at a time.
const FILL_CHUNK: usize = 1 << 20;

/// How often a stop looks whether the program has ended.
const STOP_LOOK: Duration = Duration::from_millis(20);

/// Why a step of the operating system failed.
#[derive(Debug, thiserror::Error)]
pub enum OsError {
    /// The program could not be started.
    #[error("{name} could not be started: {source}")]
    Spawn {
        /// The name the rig gave it.
        name: String,
        /// Why.
        source: io::Error,
    },
    /// The program ended before it printed its start line.
    #[error("{name} ended before it printed its start line ({status}); see {name}.err")]
    Ended {
        /// The name the rig gave it.
        name: String,
        /// How it ended.
        status: String,
    },
    /// The program printed no start line within the time it was given, and was killed.
    #[error("{name} printed no start line within {waited:?}, and was killed; see {name}.err")]
    NoStartLine {
        /// The name the rig gave it.
        name: String,
        /// The time it was given.
        waited: Duration,
    },
    /// A file, a signal or a wait failed.
    #[error("{what}: {source}")]
    Io {
        /// What was done.
        what: String,
        /// Why it failed.
        source: io::Error,
    },
    /// `/proc/<pid>/status` names no resident memory.
    #[error("/proc/{pid}/status has no VmRSS line")]
    NoResident {
        /// The process.
        pid: u32,
    },
}

/// A program the rig started, stopped when it is dropped (module docs).
#[derive(Debug)]
pub struct Process {
    name: String,
    child: Child,
    line: String,
    listen: String,
}

impl Process {
    /// Starts `command` as `name`, its output in `logs`, and waits at most `wait` for its start
    /// line.
    ///
    /// # Errors
    /// [`OsError`]: a log file that cannot be opened, a program that cannot be started, one that
    /// ends before its start line, or one that has not printed it within `wait` (it is killed).
    pub fn start(
        mut command: Command,
        name: &str,
        logs: &Path,
        wait: Duration,
    ) -> Result<Self, OsError> {
        let log = |end: &str| {
            let path = logs.join(format!("{name}.{end}"));
            File::options()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|source| OsError::Io {
                    what: path.display().to_string(),
                    source,
                })
        };
        let (out, err) = (log("out")?, log("err")?);
        let spawned = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(err)
            .spawn();
        let mut child = spawned.map_err(|source| OsError::Spawn {
            name: name.to_owned(),
            source,
        })?;
        let (sent, lines) = mpsc::channel();
        let stdout = child.stdout.take();
        std::thread::spawn(move || copy(stdout, out, &sent));
        let (line, listen) = match lines.recv_timeout(wait) {
            Ok(started) => started,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(OsError::NoStartLine {
                    name: name.to_owned(),
                    waited: wait,
                });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = child
                    .wait()
                    .map_or_else(|why| why.to_string(), |s| s.to_string());
                return Err(OsError::Ended {
                    name: name.to_owned(),
                    status,
                });
            }
        };
        Ok(Self {
            name: name.to_owned(),
            child,
            line,
            listen,
        })
    }

    /// Its start line.
    #[must_use]
    pub fn line(&self) -> &str {
        &self.line
    }

    /// The address its start line names.
    #[must_use]
    pub fn listen(&self) -> &str {
        &self.listen
    }

    /// Its process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether it is still running.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Its resident memory, in bytes.
    ///
    /// # Errors
    /// [`OsError`]: its `/proc/<pid>/status` cannot be read (it ended), or has no `VmRSS`.
    pub fn resident_bytes(&self) -> Result<u64, OsError> {
        let pid = self.pid();
        let path = format!("/proc/{pid}/status");
        let text =
            std::fs::read_to_string(&path).map_err(|source| OsError::Io { what: path, source })?;
        vm_rss(&text).ok_or(OsError::NoResident { pid })
    }

    /// Asks it to stop (SIGTERM) and waits for it at most `grace`, then kills it (SIGKILL): how
    /// it ended (by SIGKILL when it did not stop in time).
    ///
    /// # Errors
    /// [`OsError`]: a signal or the wait failed.
    pub fn terminate(mut self, grace: Duration) -> Result<ExitStatus, OsError> {
        let what = format!("SIGTERM to {}", self.name);
        let failed = |source| OsError::Io {
            what: what.clone(),
            source,
        };
        kill_process(Pid::from_child(&self.child), Signal::TERM).map_err(|e| failed(e.into()))?;
        let since = Stopwatch::start();
        loop {
            if let Some(status) = self.child.try_wait().map_err(&failed)? {
                return Ok(status);
            }
            if since.elapsed() >= grace {
                return (self.child.kill())
                    .and_then(|()| self.child.wait())
                    .map_err(&failed);
            }
            std::thread::sleep(STOP_LOOK);
        }
    }

    /// Kills it (SIGKILL), as a crash or an out-of-memory kill would end it, and waits for it.
    ///
    /// # Errors
    /// [`OsError`]: the kill or the wait failed.
    pub fn kill(mut self) -> Result<ExitStatus, OsError> {
        let what = format!("SIGKILL to {}", self.name);
        self.child
            .kill()
            .and_then(|()| self.child.wait())
            .map_err(|source| OsError::Io { what, source })
    }
}

impl Drop for Process {
    /// A program the rig did not stop is killed: no node outlives its run.
    fn drop(&mut self) {
        if self.alive() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Copies `stdout` line by line to `log`, sending the first line with a `listen=` word and the
/// address it names.
fn copy(stdout: Option<ChildStdout>, mut log: File, sent: &mpsc::Sender<(String, String)>) {
    let mut first = true;
    let lines = stdout
        .map(BufReader::new)
        .into_iter()
        .flat_map(BufRead::lines);
    for line in lines.map_while(Result::ok) {
        let _ = writeln!(log, "{line}");
        if let Some(listen) = listen_of(&line).filter(|_| first) {
            first = false;
            let listen = listen.to_owned();
            let _ = sent.send((line, listen));
        }
    }
}

/// The address of the word `listen=<address>` of `line`.
fn listen_of(line: &str) -> Option<&str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix("listen="))
        .filter(|address| !address.is_empty())
}

/// The resident memory a `/proc/<pid>/status` text names, in bytes.
fn vm_rss(text: &str) -> Option<u64> {
    let line = text.lines().find_map(|line| line.strip_prefix("VmRSS:"))?;
    let mut words = line.split_whitespace();
    let kib: u64 = words.next()?.parse().ok()?;
    (words.next() == Some("kB")).then_some(kib * 1024)
}

/// The size in bytes of the file system that holds `path`.
///
/// # Errors
/// [`OsError`]: `statvfs` failed.
pub fn disk_bytes(path: &Path) -> Result<u64, OsError> {
    rustix::fs::statvfs(path)
        .map(|fs| fs.f_blocks.saturating_mul(fs.f_frsize))
        .map_err(|source| OsError::Io {
            what: format!("statvfs of {}", path.display()),
            source: source.into(),
        })
}

/// Fills the file system that holds `path` by writing a new file there until no byte more fits:
/// the bytes written.
///
/// # Errors
/// [`OsError`]: the file cannot be made, or a write fails other than for a full disk.
pub fn fill(path: &Path) -> Result<u64, OsError> {
    File::create_new(path)
        .and_then(fill_with)
        .map_err(|source| OsError::Io {
            what: format!("filling {}", path.display()),
            source,
        })
}

/// Writes zeros to `to` until it is full (`ENOSPC`, or a write of no byte): the bytes written.
fn fill_with(mut to: impl Write) -> io::Result<u64> {
    let chunk = vec![0u8; FILL_CHUNK];
    let mut total = 0u64;
    loop {
        match to.write(&chunk) {
            Ok(0) => return Ok(total),
            Ok(n) => total += n as u64,
            Err(why) if why.kind() == io::ErrorKind::StorageFull => return Ok(total),
            Err(why) if why.kind() == io::ErrorKind::Interrupted => {}
            Err(why) => return Err(why),
        }
    }
}

#[cfg(test)]
#[path = "os_tests.rs"]
mod tests;
