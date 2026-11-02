//! The service of a macOS host under launchd, through `launchctl`, and the names the service and
//! its packaging share: the label, the plist, the program and the directory of the release
//! links.
//!
//! Nothing here depends on the system it is compiled on: the calls are argument vectors given to
//! a [`Runner`], so the sequence is built and tested on any host; only the runner that executes
//! them on a Mac is the machine's.
//!
//! - `stop`: `launchctl print system/<label>`; if the domain has the service,
//!   `launchctl bootout system/<label>` (launchd sends SIGTERM, the process drains and exits),
//!   then `print` again until the domain no longer has it.
//! - `start`: `print`; if the domain does not have the service, `launchctl bootstrap system
//!   <plist>` (the plist's `RunAtLoad` starts it), and otherwise `launchctl kickstart
//!   system/<label>`, which starts it if it is not running and does nothing if it runs.
//!
//! `print` exits 0 for a service the domain has and 113 for one it does not; any other status,
//! of any call, is a failure.

use std::time::Duration;

use super::super::ports::{Clock, PortError, PortKind, Service};
use super::runner::{Call, Output, Runner, checked, run};

/// The label of the service's launchd job.
pub const LAUNCHD_LABEL: &str = "com.komira.tierstore";
/// The job's plist: a LaunchDaemon, run by the system domain.
pub const LAUNCHD_PLIST: &str = "/Library/LaunchDaemons/com.komira.tierstore.plist";
/// Where macOS has `launchctl`.
pub const LAUNCHCTL_PROGRAM: &str = "/bin/launchctl";
/// The directory of the release links, `current` and `previous`.
pub const LAUNCHD_LINKS_DIR: &str = "/usr/local/tierstore";
/// The program the plist runs: the binary of the release `current` points at.
pub const LAUNCHD_PROGRAM: &str = "/usr/local/tierstore/current/tierstore";
/// The launchd domain of a LaunchDaemon.
pub const DOMAIN: &str = "system";
/// `launchctl print`'s status for a service its domain does not have.
pub const NOT_FOUND: i32 = 113;

const KIND: PortKind = PortKind::Units;

/// The service target of `label` in the system domain: `system/<label>`.
#[must_use]
pub fn service_target(label: &str) -> String {
    format!("{DOMAIN}/{label}")
}

/// `print system/<label>`: whether the domain has the service.
#[must_use]
pub fn print_args(label: &str) -> Vec<String> {
    vec!["print".to_owned(), service_target(label)]
}

/// `bootout system/<label>`: stops the service and removes it from the domain.
#[must_use]
pub fn bootout_args(label: &str) -> Vec<String> {
    vec!["bootout".to_owned(), service_target(label)]
}

/// `bootstrap system <plist>`: loads the job into the domain, which starts it (`RunAtLoad`).
#[must_use]
pub fn bootstrap_args(plist: &str) -> Vec<String> {
    vec!["bootstrap".to_owned(), DOMAIN.to_owned(), plist.to_owned()]
}

/// `kickstart system/<label>`: starts a loaded service that is not running.
#[must_use]
pub fn kickstart_args(label: &str) -> Vec<String> {
    vec!["kickstart".to_owned(), service_target(label)]
}

/// How long the calls of [`LaunchdService`] may take, and how long `stop` waits for the domain
/// to let the service go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaunchdTimes {
    /// The longest `print`, `bootstrap` or `kickstart` may run.
    pub call: Duration,
    /// The longest `bootout` may run: it waits for the process to exit, the drain included.
    pub bootout: Duration,
    /// How many looks `stop` takes, after `bootout`, for the domain to no longer have the
    /// service.
    pub gone_attempts: u32,
    /// The pause between two of those looks.
    pub poll: Duration,
}

/// The service `label` of the system domain, whose plist is `plist`, through `launchctl`.
pub struct LaunchdService<'a> {
    runner: &'a dyn Runner,
    clock: &'a dyn Clock,
    program: String,
    label: String,
    plist: String,
    times: LaunchdTimes,
}

impl<'a> LaunchdService<'a> {
    /// The service [`LAUNCHD_LABEL`] of [`LAUNCHD_PLIST`], through [`LAUNCHCTL_PROGRAM`] run by
    /// `runner`; `clock` paces the wait of `stop`.
    #[must_use]
    pub fn new(runner: &'a dyn Runner, clock: &'a dyn Clock, times: LaunchdTimes) -> Self {
        LaunchdService {
            runner,
            clock,
            program: LAUNCHCTL_PROGRAM.to_owned(),
            label: LAUNCHD_LABEL.to_owned(),
            plist: LAUNCHD_PLIST.to_owned(),
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

    /// Whether the system domain has the service.
    fn loaded(&self) -> Result<bool, PortError> {
        let call = self.call(&print_args(&self.label), self.times.call);
        let output = run(self.runner, &call, KIND)?;
        if output.code == Some(NOT_FOUND) {
            return Ok(false);
        }
        checked(output, &call, KIND).map(|_| true)
    }

    /// Runs `args` and requires exit 0.
    fn must(&self, args: &[String], timeout: Duration) -> Result<Output, PortError> {
        let call = self.call(args, timeout);
        checked(run(self.runner, &call, KIND)?, &call, KIND)
    }
}

impl Service for LaunchdService<'_> {
    fn start(&self) -> Result<(), PortError> {
        let args = if self.loaded()? {
            kickstart_args(&self.label)
        } else {
            bootstrap_args(&self.plist)
        };
        self.must(&args, self.times.call).map(|_| ())
    }

    fn stop(&self) -> Result<(), PortError> {
        if !self.loaded()? {
            return Ok(());
        }
        // A bootout that fails while the service is gone anyway (it exited on its own in
        // between) has done what was asked; one that fails while it is still there is the error.
        let bootout = self.must(&bootout_args(&self.label), self.times.bootout);
        for _ in 0..self.times.gone_attempts {
            if !self.loaded()? {
                return Ok(());
            }
            self.clock.pause(self.times.poll);
        }
        bootout?;
        Err(PortError::failed(
            KIND,
            format!(
                "{} is still loaded after bootout and {} looks",
                service_target(&self.label),
                self.times.gone_attempts
            ),
        ))
    }
}
