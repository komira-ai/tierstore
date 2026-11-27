//! The commands of an example: each run in the work directory with an environment the runner
//! builds, never the caller's.
//!
//! The environment of a command is `PATH` (the caller's, so that the client and the tools it
//! calls are found), `HOME` (`${work}/home`), `TMPDIR` (`${work}/tmp`) and `LC_ALL=C`, then the
//! example's `[env]`, then its step's `env`. So a setting of the caller's (an `AWS_PROFILE`, a
//! configuration file in its home directory) reaches no client.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};

use crate::error::{Error, invalid_at, io_at};
use crate::spec::{Client, Command, SPEC_FILE, expand};

/// The step name of the closing commands, in errors and in their log's name.
pub const CLEANUP: &str = "cleanup";

/// What the commands of a run share.
pub struct Runner<'a> {
    /// The work directory: every command runs in it.
    pub work: &'a Path,
    /// The directory of the example, for errors that name its spec.
    pub example: &'a Path,
    /// The values `${name}` stands for.
    pub vars: &'a BTreeMap<String, String>,
    /// The environment of every command: the runner's own and the example's `[env]`, expanded.
    pub env: BTreeMap<String, OsString>,
    /// The directory of the logs, one for each step.
    pub logs: PathBuf,
}

impl<'a> Runner<'a> {
    /// The runner of the commands of an example whose `[env]` is `env`.
    ///
    /// # Errors
    /// A value of `env` names an unknown value of the run.
    pub fn new(
        work: &'a Path,
        example: &'a Path,
        vars: &'a BTreeMap<String, String>,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, Error> {
        let mut base = BTreeMap::new();
        base.insert("PATH".to_owned(), search_path().unwrap_or_default());
        base.insert("HOME".to_owned(), work.join("home").into_os_string());
        base.insert("TMPDIR".to_owned(), work.join("tmp").into_os_string());
        base.insert("LC_ALL".to_owned(), OsString::from("C"));
        let logs = work.join("logs");
        let mut runner = Self {
            work,
            example,
            vars,
            env: base,
            logs,
        };
        runner.env = runner.with(env)?;
        Ok(runner)
    }

    /// The environment of the runner with `more` on top, expanded.
    ///
    /// # Errors
    /// A value names an unknown value of the run.
    pub fn with(
        &self,
        more: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, OsString>, Error> {
        let mut env = self.env.clone();
        for (name, value) in more {
            let value =
                expand(value, self.vars).map_err(invalid_at(&self.example.join(SPEC_FILE)))?;
            env.insert(name.clone(), OsString::from(value));
        }
        Ok(env)
    }

    /// Runs `command` of `step` with `env`. Its standard error, and its standard output unless it
    /// names a file of its own, are added to the step's log.
    ///
    /// # Errors
    /// An argument names an unknown value, the program cannot be started, a file cannot be
    /// opened, or the command ends with a status other than 0.
    pub fn run(
        &self,
        step: &str,
        command: &Command,
        env: &BTreeMap<String, OsString>,
    ) -> Result<(), Error> {
        let spec = self.example.join(SPEC_FILE);
        let argv: Vec<String> = command
            .argv
            .iter()
            .map(|word| expand(word, self.vars))
            .collect::<Result<_, _>>()
            .map_err(invalid_at(&spec))?;
        let log = self.logs.join(format!("{step}.log"));
        let errors = append(&log)?;
        let out = match &command.stdout {
            Some(file) => {
                let path = self.work.join(file);
                File::create(&path).map_err(io_at(&path))?
            }
            None => append(&log)?,
        };
        // Every argv has a program (`Example::check`).
        let program = &argv[0];
        let status = Process::new(program)
            .args(&argv[1..])
            .env_clear()
            .envs(env)
            .current_dir(self.work)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(errors)
            .status()
            .map_err(|source| Error::Spawn {
                step: step.to_owned(),
                program: program.clone(),
                source,
            })?;
        if status.success() {
            return Ok(());
        }
        Err(Error::Command {
            step: step.to_owned(),
            command: argv.join(" "),
            status: status.to_string(),
            log,
        })
    }

    /// Checks that the client is installed and prints the version the example pins, run as a
    /// command is but with the runner's environment alone.
    ///
    /// # Errors
    /// The client cannot be started, or prints another first line.
    pub fn check_client(&self, client: &Client) -> Result<(), Error> {
        let output = Process::new(&client.program)
            .args(&client.version_args)
            .env_clear()
            .envs(&self.env)
            .current_dir(self.work)
            .stdin(Stdio::null())
            .output()
            .map_err(|source| Error::ClientMissing {
                program: client.program.clone(),
                source,
            })?;
        let text = String::from_utf8_lossy(&output.stdout);
        let found = text.lines().next().unwrap_or_default().trim();
        if found == client.version {
            return Ok(());
        }
        Err(Error::ClientVersion {
            program: client.program.clone(),
            expected: client.version.clone(),
            found: found.to_owned(),
        })
    }

    /// Runs the closing commands, each whatever the one before did; a failure is written to the
    /// log of the closing commands and does not end the run.
    pub fn cleanup(&self, commands: &[Command]) {
        let log = self.logs.join(format!("{CLEANUP}.log"));
        for command in commands {
            if let Err(error) = self.run(CLEANUP, command, &self.env) {
                let line = format!("{error}\n");
                let _ = append(&log).and_then(|mut file| {
                    use std::io::Write as _;
                    file.write_all(line.as_bytes()).map_err(io_at(&log))
                });
            }
        }
    }
}

/// The search path of the runner, which the commands are given so that they find the client.
#[expect(
    clippy::disallowed_methods,
    reason = "PATH is where the system looks for the example's client and its tools; it configures nothing of the runner, and is all of the caller's environment a command gets"
)]
fn search_path() -> Option<OsString> {
    std::env::var_os("PATH")
}

/// `path` opened to add to its end, made if it is not there.
fn append(path: &Path) -> Result<File, Error> {
    let opened = OpenOptions::new().create(true).append(true).open(path);
    opened.map_err(io_at(path))
}
