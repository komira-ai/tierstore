//! The launchd job of a macOS host, through `launchctl`.
//!
//! Nothing here depends on the system it is compiled on: the calls are argument vectors given to
//! a [`Runner`], so the sequence is built and tested on any host; only the runner that executes
//! them on a Mac is the machine's.
//!
//! - `loaded`: `launchctl print system/<label>`: exit 0 is a job the system domain has, 113 a job
//!   it does not have, and any other status is a failure.
//! - `bootstrap`: `launchctl bootstrap system <plist>` (the plist's `RunAtLoad` starts the job).
//! - `bootout`: nothing when the job is not loaded; otherwise `launchctl bootout system/<label>`
//!   (launchd sends SIGTERM, the process drains and exits), then `print` until the domain no
//!   longer has the job. A `launchctl` call that fails is a failure, never a success.

use std::path::Path;
use std::time::Duration;

use super::super::params::is_label;
use super::super::ports::{Clock, Launchd, PortError, PortKind};
use super::runner::{Call, Output, Runner, checked, run};

/// Where macOS has `launchctl`.
pub const LAUNCHCTL_PROGRAM: &str = "/bin/launchctl";
/// The launchd domain of a LaunchDaemon.
pub const DOMAIN: &str = "system";
/// `launchctl print`'s status for a job its domain does not have.
pub const NOT_FOUND: i32 = 113;

const KIND: PortKind = PortKind::Launchd;

/// The target of `label` in the system domain: `system/<label>`.
#[must_use]
pub fn service_target(label: &str) -> String {
    format!("{DOMAIN}/{label}")
}

/// `print system/<label>`: whether the domain has the job.
#[must_use]
pub fn print_args(label: &str) -> Vec<String> {
    vec!["print".to_owned(), service_target(label)]
}

/// `bootout system/<label>`: stops the job and removes it from the domain.
#[must_use]
pub fn bootout_args(label: &str) -> Vec<String> {
    vec!["bootout".to_owned(), service_target(label)]
}

/// `bootstrap system <plist>`: loads the job into the domain, which starts it (`RunAtLoad`).
#[must_use]
pub fn bootstrap_args(plist: &str) -> Vec<String> {
    vec!["bootstrap".to_owned(), DOMAIN.to_owned(), plist.to_owned()]
}

/// How long the calls of [`LaunchctlJobs`] may take, and how long `bootout` waits for the domain
/// to let the job go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaunchdTimes {
    /// The longest `print` or `bootstrap` may run.
    pub call: Duration,
    /// The longest `bootout` may run: it waits for the process to exit, the drain included.
    pub bootout: Duration,
    /// How many looks `bootout` takes, after the call, for the domain to no longer have the job.
    pub gone_attempts: u32,
    /// The pause between two of those looks.
    pub poll: Duration,
}

/// The jobs of the system domain, through `launchctl`.
pub struct LaunchctlJobs<'a> {
    runner: &'a dyn Runner,
    clock: &'a dyn Clock,
    program: String,
    times: LaunchdTimes,
}

impl<'a> LaunchctlJobs<'a> {
    /// The system domain through [`LAUNCHCTL_PROGRAM`] run by `runner`; `clock` paces the wait
    /// of `bootout`.
    #[must_use]
    pub fn new(runner: &'a dyn Runner, clock: &'a dyn Clock, times: LaunchdTimes) -> Self {
        LaunchctlJobs {
            runner,
            clock,
            program: LAUNCHCTL_PROGRAM.to_owned(),
            times,
        }
    }

    /// The same, calling `program` (an absolute path) instead of the default.
    #[must_use]
    pub fn with_program(mut self, program: &str) -> Self {
        program.clone_into(&mut self.program);
        self
    }

    fn call(&self, args: &[String], timeout: Duration) -> Call {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        Call::new(&self.program, &args, timeout)
    }

    /// Runs `args` and requires exit 0.
    fn must(&self, args: &[String], timeout: Duration) -> Result<Output, PortError> {
        let call = self.call(args, timeout);
        checked(run(self.runner, &call, KIND)?, &call, KIND)
    }
}

/// `label` itself when it is one `launchctl` may be given.
fn checked_label(label: &str) -> Result<&str, PortError> {
    if is_label(label) {
        Ok(label)
    } else {
        Err(PortError::failed(
            KIND,
            format!("{label:?} is not a launchd label"),
        ))
    }
}

impl Launchd for LaunchctlJobs<'_> {
    fn loaded(&self, label: &str) -> Result<bool, PortError> {
        let call = self.call(&print_args(checked_label(label)?), self.times.call);
        let output = run(self.runner, &call, KIND)?;
        if output.code == Some(NOT_FOUND) {
            return Ok(false);
        }
        checked(output, &call, KIND).map(|_| true)
    }

    fn bootstrap(&self, plist: &Path) -> Result<(), PortError> {
        let plist = plist.to_string_lossy();
        self.must(&bootstrap_args(&plist), self.times.call)
            .map(|_| ())
    }

    fn bootout(&self, label: &str) -> Result<(), PortError> {
        if !self.loaded(label)? {
            return Ok(());
        }
        self.must(&bootout_args(label), self.times.bootout)?;
        for _ in 0..self.times.gone_attempts {
            if !self.loaded(label)? {
                return Ok(());
            }
            self.clock.pause(self.times.poll);
        }
        Err(PortError::failed(
            KIND,
            format!(
                "{} is still loaded after bootout and {} looks",
                service_target(label),
                self.times.gone_attempts
            ),
        ))
    }
}
