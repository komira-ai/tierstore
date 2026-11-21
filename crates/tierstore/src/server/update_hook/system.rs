//! The steps on a machine: builds the ports of the arm the params name from the machine's
//! implementations and runs the step over them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::process::geteuid;

use super::args::Invocation;
use super::config_hook::{Common, GUARD_FILE, GUARD_UNIT, Running, launchd_step, systemd_step};
use super::look::POLL_INTERVAL;
use super::machine::{
    CONFIG_STATE_FILE, GuardUnit, LaunchctlJobs, LaunchdTimes, LoopbackInstances, MarkerFiles,
    MonotonicClock, Runner, StateFiles, SystemHost, SystemRunner, SystemdUnits,
};
use super::params::{Params, ServiceManager};
use super::ports::{LaunchdPorts, SystemdPorts};
use super::report::{Done, Reason};
use super::{MARKER_DIR, STATE_DIR};
use crate::server::build_info::VERSION as BUILD_VERSION;

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
    /// The user id the hook runs as: a root hook (0) runs a release's check of the configuration
    /// as the service account of the arm.
    pub hook_uid: u32,
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

/// The host of `config` for the arm `manager` names: on launchd (a Mac) the service account is
/// `_tierstore` in the directory service, on systemd `tierstore` in `etc/passwd`.
fn host_for<'a>(config: &SystemConfig<'a>, manager: &ServiceManager) -> SystemHost<'a> {
    let host = SystemHost::new(config.root.clone(), config.runner, CAPABILITIES_TIMEOUT)
        .with_release_owner(config.release_owner)
        .with_hook_user(config.hook_uid);
    match manager {
        ServiceManager::Launchd { .. } => host.with_directory_service(),
        ServiceManager::Systemd | ServiceManager::Unknown(_) => host,
    }
}

/// Runs the step on the machine described by `config`: the ports are the machine's, and the
/// arm is the one the params name. A service manager that is none of the two refuses every
/// step, `prepare` included, before anything is touched.
pub(super) fn run_on(config: &SystemConfig<'_>, invocation: &Invocation, params: &Params) -> Done {
    let runner = config.runner;
    let host = host_for(config, &params.service_manager);
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

/// Runs a step of the config hook on the machine described by `config`: the ports are the
/// machine's, every release check runs `binary` (the binary of the service that serves, the one
/// running this hook), and the record is the config hook's own.
pub(super) fn config_run_on(
    config: &SystemConfig<'_>,
    binary: &Path,
    invocation: &Invocation,
    params: &Params,
) -> Done {
    let runner = config.runner;
    // The staged package is closed to every user but root: its copy of the configuration goes
    // to the check on standard input.
    let host = host_for(config, &params.service_manager)
        .with_binary(binary.to_path_buf())
        .with_root_only(params.staged_release.clone());
    let release = Path::new("/");
    let running = Running {
        host: &host,
        release,
    };
    let instances = LoopbackInstances::new(CONNECT_TIMEOUT, READ_TIMEOUT);
    let clock = MonotonicClock::new().with_pause(config.poll);
    let state = StateFiles::new(under(&config.root, STATE_DIR), config.owner)
        .with_record(CONFIG_STATE_FILE);
    let service = StateFiles::new(under(&config.root, STATE_DIR), config.owner);
    let common = Common {
        service: &service,
        release,
        build: BUILD_VERSION,
    };
    match &params.service_manager {
        ServiceManager::Unknown(value) => Done::fail(
            Reason::ServiceManagerUnknown,
            format!("service_manager {value:?} is neither \"systemd\" nor \"launchd\""),
        ),
        ServiceManager::Systemd => {
            let units = SystemdUnits::new(runner, START_TIMEOUT, STOP_TIMEOUT, SHOW_TIMEOUT)
                .with_program(config.systemctl);
            let markers = MarkerFiles::new(under(&config.root, MARKER_DIR));
            let guard = GuardUnit::new(&units, GUARD_UNIT, under(&config.root, GUARD_FILE));
            let ports = SystemdPorts {
                units: &units,
                markers: &markers,
                instances: &instances,
                host: &running,
                clock: &clock,
                state: &state,
            };
            systemd_step(invocation.step, params, &ports, &guard, &common)
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
                host: &running,
                clock: &clock,
                state: &state,
            };
            launchd_step(invocation.step, params, label, &ports, &common)
        }
    }
}

/// Runs a step of the config hook on the host the process is running on, with the binary of
/// this process as the one that serves.
pub(super) fn config_on_this_machine(invocation: &Invocation, params: &Params) -> Done {
    config_run_as(
        &system_config(&SystemRunner),
        std::env::current_exe(),
        invocation,
        params,
    )
}

/// Runs a step of the config hook with `binary`, the path of the binary of this process as the
/// system gave it, as the one that serves.
pub(super) fn config_run_as(
    config: &SystemConfig<'_>,
    binary: std::io::Result<PathBuf>,
    invocation: &Invocation,
    params: &Params,
) -> Done {
    match binary {
        Ok(binary) => config_run_on(config, &binary, invocation, params),
        Err(error) => Done::fail(
            Reason::HostFailed,
            format!("the path of this binary cannot be read: {error}"),
        ),
    }
}

/// The machine this process runs on: the host's own files under `/`, a state directory owned by
/// the user the hook runs as, a staged release owned by root, and the host's own `systemctl`
/// and `launchctl`.
pub(super) fn system_config(runner: &dyn Runner) -> SystemConfig<'_> {
    let uid = geteuid().as_raw();
    SystemConfig {
        root: PathBuf::from("/"),
        owner: uid,
        release_owner: 0,
        hook_uid: uid,
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
