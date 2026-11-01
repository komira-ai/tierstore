//! Running a program: the argument vector, optional standard input and a time limit. The
//! real runner never involves a shell. The child gets an environment of its own (a fixed
//! `PATH` and `LC_ALL=C`, so that the words of its messages do not depend on the host's
//! locale), runs as the leader of its own process group, and is killed with that group, and
//! reaped, on a timeout, on an error and when it has ended (what it left running goes too).
//! At most 1 MiB of each of its output streams is kept.

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

use super::super::ports::{PortError, PortKind};

/// How often a running program is looked at for its end.
const POLL: Duration = Duration::from_millis(5);
/// The most of a program's standard error that goes into a message.
const STDERR_SHOWN: usize = 300;

/// One program to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The program, by name or path.
    pub program: String,
    /// Its arguments, one element each.
    pub args: Vec<String>,
    /// What it reads on standard input; none is an empty input.
    pub stdin: Option<Vec<u8>>,
    /// How long it may run before it is killed.
    pub timeout: Duration,
}

impl Call {
    /// A call of `program` with `args`, no input.
    #[must_use]
    pub fn new(program: &str, args: &[&str], timeout: Duration) -> Call {
        Call {
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            stdin: None,
            timeout,
        }
    }

    /// The same call, with `bytes` on standard input.
    #[must_use]
    pub fn with_stdin(mut self, bytes: &[u8]) -> Call {
        self.stdin = Some(bytes.to_vec());
        self
    }

    /// The command line as words, for a message.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut words = vec![self.program.as_str()];
        words.extend(self.args.iter().map(String::as_str));
        words.join(" ")
    }
}

/// What a program that ran to its end left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The exit status; `None` when a signal ended it.
    pub code: Option<i32>,
    /// What it wrote on standard output.
    pub stdout: Vec<u8>,
    /// What it wrote on standard error.
    pub stderr: Vec<u8>,
}

impl Output {
    /// The start of standard error as text, for a message.
    #[must_use]
    pub fn stderr_text(&self) -> String {
        let text = String::from_utf8_lossy(&self.stderr);
        text.trim().chars().take(STDERR_SHOWN).collect()
    }
}

/// Why a program did not run to its end.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    /// It could not be started or waited for.
    #[error("cannot run: {0}")]
    Spawn(String),
    /// It was still running when its time was up, and was killed.
    #[error("no end after {0:?}, killed")]
    Timeout(Duration),
}

/// Runs programs.
pub trait Runner {
    /// Runs `call` to its end.
    ///
    /// # Errors
    /// It could not be started, or ran past its time.
    fn run(&self, call: &Call) -> Result<Output, RunError>;
}

/// Runs the program as a child process of this one.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        execute(call).map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => RunError::Timeout(call.timeout),
            _ => RunError::Spawn(error.to_string()),
        })
    }
}

/// The most of each of a program's output streams that is kept; the rest is read and dropped.
pub const MAX_OUTPUT: usize = 1 << 20;
/// How long, after a program has ended, its output streams may take to reach their end.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// The `PATH` of every child: the environment of the hook is not handed on.
const SEARCH_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// A child, started as the leader of a process group of its own. When this is dropped, on every
/// path out of [`execute`] (an error included), the whole group is killed and the child reaped,
/// so no child, zombie or helper of the child outlives the call.
pub(super) struct Group {
    pub(super) child: Child,
}

impl Group {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Group> {
        Ok(Group {
            child: command.process_group(0).spawn()?,
        })
    }

    /// Kills every process of the group, the child's helpers too.
    fn kill(&self) {
        let _ = kill_process_group(Pid::from_child(&self.child), Signal::KILL);
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
        let _ = self.child.wait();
    }
}

/// Keeps the first [`MAX_OUTPUT`] bytes written to it and takes the rest without keeping it.
struct Capped(Vec<u8>);

impl Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = MAX_OUTPUT - self.0.len();
        self.0.extend_from_slice(&bytes[..bytes.len().min(room)]);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads a pipe to its end on a thread of its own, so that a program that writes more than a
/// pipe holds is not stuck while this one waits for it to exit. The first [`MAX_OUTPUT`] bytes
/// are sent back when it ends; a program that writes without end cannot fill the memory. A
/// reader that fails to the point of a panic sends nothing, which [`collect`] reports.
pub(super) fn drain(pipe: Option<Box<dyn Read + Send>>) -> Receiver<Vec<u8>> {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Capped(Vec::new());
        let _ = pipe.map(|mut pipe| io::copy(&mut pipe, &mut kept));
        let _ = kept.flush();
        let _ = send.send(kept.0);
    });
    receive
}

/// What [`drain`] read, if its pipe reached the end by `deadline`.
pub(super) fn collect(drained: &Receiver<Vec<u8>>, deadline: Instant) -> io::Result<Vec<u8>> {
    drained
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| match error {
            RecvTimeoutError::Timeout => io::Error::other("the output was not drained in time"),
            RecvTimeoutError::Disconnected => io::Error::other("the output reader failed"),
        })
}

fn execute(call: &Call) -> io::Result<Output> {
    let mut command = Command::new(&call.program);
    command
        .args(&call.args)
        .env_clear()
        .env("PATH", SEARCH_PATH)
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut group = Group::spawn(&mut command)?;
    // The input goes in from a thread too, and the pipe closes after it: the program sees the
    // end of its input. A program that exits without reading it all makes the write fail,
    // which is its own affair.
    let input = call.stdin.clone().unwrap_or_default();
    let pipe = group.child.stdin.take();
    let writer = thread::spawn(move || {
        let _ = pipe.map(|mut pipe| pipe.write_all(&input));
    });
    let stdout = drain(
        group
            .child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        group
            .child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + call.timeout;
    // A return from here on drops `group`, which kills what is left and reaps the child.
    let status = loop {
        if let Some(status) = group.child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        thread::sleep(POLL);
    };
    // Whatever the program left running in its group holds the pipes open: it goes now.
    group.kill();
    let _ = writer.join();
    let drained = Instant::now() + DRAIN_GRACE;
    Ok(Output {
        code: status.code(),
        stdout: collect(&stdout, drained)?,
        stderr: collect(&stderr, drained)?,
    })
}

/// Runs `call`, and says of a failure to run which port asked.
pub(super) fn run(runner: &dyn Runner, call: &Call, kind: PortKind) -> Result<Output, PortError> {
    runner
        .run(call)
        .map_err(|error| PortError::failed(kind, format!("{}: {error}", call.describe())))
}

/// `output` itself when the program exited 0, and otherwise the failure that says how it did not.
pub(super) fn checked(output: Output, call: &Call, kind: PortKind) -> Result<Output, PortError> {
    let how = match output.code {
        Some(0) => return Ok(output),
        Some(code) => format!("exited {code}"),
        None => "ended by a signal".to_owned(),
    };
    Err(PortError::failed(
        kind,
        format!("{}: {how}: {}", call.describe(), output.stderr_text()),
    ))
}
