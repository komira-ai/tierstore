//! The single-instance steps on a launchd host: builds the [`SinglePorts`] from the machine's
//! implementations and runs the step over them.
//!
//! Everything here but [`on_this_machine`] is built and tested on every system: the calls to
//! `launchctl` go through a [`Runner`], the files are read under a root. Only
//! [`on_this_machine`], which gives the real runner and the host's own root, is built on macOS
//! alone.

// Off macOS nothing but the tests calls into this module: the wiring is built there so that its
// tests run under the coverage gate, and a non-test build has no caller for it.
#![cfg_attr(
    all(not(target_os = "macos"), not(test)),
    expect(
        dead_code,
        reason = "the launchd wiring runs on macOS; elsewhere only its tests call it"
    )
)]

use std::path::PathBuf;
use std::time::Duration;

use rustix::process::geteuid;

use super::args::{HostArgs, Invocation};
use super::declaration::{LAUNCHD_DECLARATION_PATH, LaunchdDeclaration};
use super::machine::{
    LAUNCHCTL_PROGRAM, LAUNCHD_LINKS_DIR, LaunchdService, LaunchdTimes, LoopbackInstances,
    MonotonicClock, ReleaseLinks, Runner, StateFiles, SystemHost,
};
use super::params::Params;
use super::ports::{Host, SinglePorts};
use super::report::Done;
use super::single::run_step;

/// The longest `launchctl print`, `bootstrap` or `kickstart` may run.
pub(super) const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest `launchctl bootout` may run: it returns when the drain is over.
pub(super) const BOOTOUT_TIMEOUT: Duration = Duration::from_secs(300);
/// How many looks, a poll apart, `stop` gives launchd to let the service go after `bootout`.
pub(super) const GONE_ATTEMPTS: u32 = 60;
/// The longest `<release>/tierstore --capabilities` may run (not asked on this host).
const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest a connect to the instance may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest the instance may leave a request unanswered.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The pause between two looks at launchd or the instance, unless the command line sets one.
const POLL: Duration = Duration::from_secs(1);

/// Where the machine is and who owns the hook's files.
pub(super) struct LaunchdConfig<'a> {
    /// The root the host's files are read under (`/` on a host).
    pub root: PathBuf,
    /// The user id that must own the state directory, the record and the links directory: the
    /// hook's (root on a host: only root may bootstrap a LaunchDaemon).
    pub owner: u32,
    /// The user id that must own the declaration.
    pub declaration_owner: u32,
    /// The `launchctl` program.
    pub launchctl: PathBuf,
    /// What a step waits between two looks; `None` is a second.
    pub poll: Option<Duration>,
    /// What runs the programs.
    pub runner: &'a dyn Runner,
}

/// Runs the step on the launchd host described by `config`: the declaration names the state
/// directory and the port, the links are in [`LAUNCHD_LINKS_DIR`] under the root, and the step
/// is run over them.
pub(super) fn run_on(
    config: &LaunchdConfig<'_>,
    invocation: &Invocation,
    params: Option<&Params>,
) -> Done {
    let under = |path: &str| config.root.join(path.trim_start_matches('/'));
    let runner = config.runner;
    let host = SystemHost::new(config.root.clone(), runner, CAPABILITIES_TIMEOUT)
        .with_declaration_owner(config.declaration_owner)
        .with_declaration_path(LAUNCHD_DECLARATION_PATH);
    // The declaration names the state directory, so it is read before the state port exists;
    // the step reads it again, under the lock.
    let decl = match host
        .declaration()
        .map_err(Done::from)
        .and_then(|text| LaunchdDeclaration::parse(&text).map_err(Done::from))
    {
        Ok(decl) => decl,
        Err(done) => return done,
    };
    let poll = config.poll.unwrap_or(POLL);
    let clock = match config.poll {
        Some(poll) => MonotonicClock::new().with_pause(poll),
        None => MonotonicClock::new(),
    };
    let times = LaunchdTimes {
        call: LAUNCHCTL_TIMEOUT,
        bootout: BOOTOUT_TIMEOUT,
        gone_attempts: GONE_ATTEMPTS,
        poll,
    };
    let service = LaunchdService::new(runner, &clock, times)
        .with_program(&config.launchctl.to_string_lossy());
    let releases = ReleaseLinks::new(under(LAUNCHD_LINKS_DIR), config.owner);
    let instances = LoopbackInstances::new(CONNECT_TIMEOUT, READ_TIMEOUT);
    let state = StateFiles::private(under(&decl.state_dir.to_string_lossy()), config.owner);
    let ports = SinglePorts {
        service: &service,
        releases: &releases,
        instances: &instances,
        host: &host,
        clock: &clock,
        state: &state,
    };
    run_step(invocation.step, params, &ports)
}

/// The launchd host this process runs on: the host's own files under the root the command line
/// names (`/` unless `--root` says otherwise), the state directory and the links owned by the
/// user the hook runs as, a declaration owned by root (or the user `--root-uid` names), and
/// `/bin/launchctl`.
pub(super) fn launchd_config<'a>(runner: &'a dyn Runner, host: &HostArgs) -> LaunchdConfig<'a> {
    LaunchdConfig {
        root: host.root.clone(),
        owner: geteuid().as_raw(),
        declaration_owner: host.root_uid,
        launchctl: PathBuf::from(LAUNCHCTL_PROGRAM),
        poll: host.poll,
        runner,
    }
}

/// Runs the step on the macOS host the process is running on.
#[cfg(target_os = "macos")]
pub(super) fn on_this_machine(invocation: &Invocation, params: Option<&Params>) -> Done {
    let runner = super::machine::SystemRunner;
    run_on(
        &launchd_config(&runner, &invocation.host),
        invocation,
        params,
    )
}
