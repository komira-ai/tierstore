//! Running a program: the argument vector, optional standard input and a time limit. The
//! real runner never involves a shell.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

/// Reads a pipe to its end on a thread of its own, so that a program that writes more than a
/// pipe holds is not stuck while this one waits for it to exit.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.map(|mut pipe| pipe.read_to_end(&mut bytes));
        bytes
    })
}

fn execute(call: &Call) -> io::Result<Output> {
    let mut child = Command::new(&call.program)
        .args(&call.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // The input goes in from a thread too, and the pipe closes after it: the program sees the
    // end of its input. A program that exits without reading it all makes the write fail,
    // which is its own affair.
    let input = call.stdin.clone().unwrap_or_default();
    let pipe = child.stdin.take();
    let writer = thread::spawn(move || {
        let _ = pipe.map(|mut pipe| pipe.write_all(&input));
    });
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + call.timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            break None;
        }
        thread::sleep(POLL);
    };
    let _ = writer.join();
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    match status {
        Some(status) => Ok(Output {
            code: status.code(),
            stdout,
            stderr,
        }),
        None => Err(io::ErrorKind::TimedOut.into()),
    }
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
