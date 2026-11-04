//! The steps on a machine: builds the ports of the arm the params name from the machine's
//! implementations and runs the step over them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::process::geteuid;

use super::args::Invocation;
use super::look::POLL_INTERVAL;
use super::machine::{
    LaunchctlJobs, LaunchdTimes, LoopbackInstances, MarkerFiles, MonotonicClock, Runner,
    StateFiles, SystemHost, SystemRunner, SystemdUnits,
};
use super::params::{Params, ServiceManager};
use super::ports::{LaunchdPorts, SystemdPorts};
use super::report::{Done, Reason};
use super::{MARKER_DIR, STATE_DIR};

/// Where the Debian and Ubuntu package puts `systemctl`.
pub(super) const SYSTEMCTL_PROGRAM: &str = super::machine::SYSTEMCTL_PROGRAM;
/// Where macOS has `launchctl`.
pub(super) const LAUNCHCTL_PROGRAM: &str = super::machine::LAUNCHCTL_PROGRAM;
/// The longest `systemctl start` may run.
pub(super) const START_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest `systemctl stop` may run: it returns when the drain is over.
pub(super) const STOP_TIMEOUT: Duration = Duration::from_secs(1000);
/// The longest `systemctl show` may run.
pub(super) const SHOW_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest a `launchctl` call but `bootout` may run.
pub(super) const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest `launchctl bootout` may run: it returns when the drain is over.
pub(super) const BOOTOUT_TIMEOUT: Duration = Duration::from_secs(120);
/// How many looks `bootout` takes for the domain to let a job go.
pub(super) const GONE_ATTEMPTS: u32 = 60;
/// The longest `<release>/bin/tierstore --capabilities` may run.
pub(super) const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest a connect to the service may take.
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest a whole request to the service may take: its write and every read of the answer.
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the machine is and what runs its programs.
pub(super) struct SystemConfig<'a> {
    /// The root the host's files are under (`/` on a host).
    pub root: PathBuf,
    /// The user id that must own the state directory and the record.
    pub owner: u32,
    /// The user id that must own the binary of a staged release.
    pub release_owner: u32,
    /// The `systemctl` program.
    pub systemctl: &'a str,
    /// The `launchctl` program.
    pub launchctl: &'a str,
    /// What every pause waits: the interval of the looks.
    pub poll: Duration,
    /// What runs the programs.
    pub runner: &'a dyn Runner,
}

/// The path of `absolute` under `root`.
fn under(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

/// Runs the step on the machine described by `config`: the ports are the machine's, and the
/// arm is the one the params name. A service manager that is none of the two refuses every
/// step, `prepare` included, before anything is touched.
pub(super) fn run_on(config: &SystemConfig<'_>, invocation: &Invocation, params: &Params) -> Done {
    let runner = config.runner;
    let host = SystemHost::new(config.root.clone(), runner, CAPABILITIES_TIMEOUT)
        .with_release_owner(config.release_owner);
    let instances = LoopbackInstances::new(CONNECT_TIMEOUT, READ_TIMEOUT);
    let clock = MonotonicClock::new().with_pause(config.poll);
    let state = StateFiles::new(under(&config.root, STATE_DIR), config.owner);
    match &params.service_manager {
        ServiceManager::Unknown(value) => Done::fail(
            Reason::ServiceManagerUnknown,
            format!("service_manager {value:?} is neither \"systemd\" nor \"launchd\""),
        ),
        ServiceManager::Systemd => {
            let units = SystemdUnits::new(runner, START_TIMEOUT, STOP_TIMEOUT, SHOW_TIMEOUT)
                .with_program(config.systemctl);
            let markers = MarkerFiles::new(under(&config.root, MARKER_DIR));
            let ports = SystemdPorts {
                units: &units,
                markers: &markers,
                instances: &instances,
                host: &host,
                clock: &clock,
                state: &state,
            };
            super::systemd::run_step(invocation.step, params, &ports)
        }
        ServiceManager::Launchd { label } => {
            let times = LaunchdTimes {
                call: LAUNCHCTL_TIMEOUT,
                bootout: BOOTOUT_TIMEOUT,
                gone_attempts: GONE_ATTEMPTS,
                poll: config.poll,
            };
            let launchd = LaunchctlJobs::new(runner, &clock, times).with_program(config.launchctl);
            let ports = LaunchdPorts {
                launchd: &launchd,
                instances: &instances,
                host: &host,
                clock: &clock,
                state: &state,
            };
            super::launchd::run_step(invocation.step, params, label, &ports)
        }
    }
}

/// The machine this process runs on: the host's own files under `/`, a state directory owned by
/// the user the hook runs as, a staged release owned by root, and the host's own `systemctl`
/// and `launchctl`.
pub(super) fn system_config(runner: &dyn Runner) -> SystemConfig<'_> {
    SystemConfig {
        root: PathBuf::from("/"),
        owner: geteuid().as_raw(),
        release_owner: 0,
        systemctl: SYSTEMCTL_PROGRAM,
        launchctl: LAUNCHCTL_PROGRAM,
        poll: POLL_INTERVAL,
        runner,
    }
}

/// Runs the step on the host the process is running on.
pub(super) fn on_this_machine(invocation: &Invocation, params: &Params) -> Done {
    run_on(&system_config(&SystemRunner), invocation, params)
}
