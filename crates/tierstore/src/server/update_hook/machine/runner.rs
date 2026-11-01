//! Running a program: the argument vector, optional standard input and a time limit. The
//! real runner never involves a shell.

use std::io;
use std::time::Duration;

use super::super::ports::{PortError, PortKind};

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
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        execute(call).map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => RunError::Timeout(call.timeout),
            _ => RunError::Spawn(error.to_string()),
        })
    }
}

fn execute(_call: &Call) -> io::Result<Output> {
    Err(io::Error::other("stub"))
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
