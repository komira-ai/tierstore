//! `tierstore update-hook`: tierstore's side of hostkeeper's hook contract, on Linux (systemd)
//! and on macOS (launchd).
//!
//! ```text
//! tierstore update-hook prepare <staged release> --params <file>
//! tierstore update-hook <cutover|verify|rollback|cleanup|uninstall|restore> --params <file>
//! ```
//!
//! hostkeeper runs one step per process, as root, and reads two things: the exit status and the
//! last line of standard output, a JSON object ([`Report`]). Exit 0 is success, 1 failure and 75
//! deferred (not now; the step is run again later). The verb is answered before the server's
//! flags are parsed: a hook is a short-lived process of hostkeeper, not a server.
//!
//! The params file names the service manager (`service_manager`, [`ServiceManager`]):
//!
//! - **systemd** ([`systemd`]): the package ships one template, `tierstore@.service`, and the hook
//!   alternates between its two instances, `tierstore@a` and `tierstore@b`. Both run
//!   `current/bin/tierstore` and serve port 9300. In `cutover` the idle one is started and takes
//!   the listening sockets of the live one over the hand-over socket (`server.handoff_socket`),
//!   and the live one drains and exits on its own. Each instance starts only when its marker file
//!   ([`MARKER_DIR`]) is there, so at boot only the recorded one does.
//! - **launchd** ([`launchd`]): one job, the label in `launchd_label`, and an update is a restart
//!   with a gap: `bootout`, then `bootstrap` of the linked plist ([`LAUNCHD_PLIST`]).
//!
//! hostkeeper switches the release link `current` before `cutover` and back before `rollback`;
//! the hook never moves it. The steps keep a [`Record`] in [`STATE_DIR`], set state rather than do
//! things once, and reach the machine only through the ports of [`SystemdPorts`] and
//! [`LaunchdPorts`]. [`Machine::system`] builds those ports from the machine's implementations
//! ([`machine`]). Both arms are built and tested on every system: the arm is chosen by the
//! params, not by the system the hook was built for.

mod args;
mod launchd;
mod look;
pub mod machine;
mod params;
mod plain;
mod ports;
mod probe;
mod record;
mod report;
mod system;
mod systemd;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

pub use args::{ArgsError, Invocation, Step};
pub use look::{POLL_INTERVAL, PROBE_ATTEMPTS, PROBE_HITS, READY_ATTEMPTS};
pub use params::{
    CONFIG_FILE_KEY, LAUNCHD, LAUNCHD_LABEL_KEY, MODE_KEY, PREVIOUS_RELEASE_KEY,
    PREVIOUS_VERSION_KEY, PROMOTION_CEILING_KEY, Params, ParamsError, Previous, RESTART_MODE,
    SERVICE_MANAGER_KEY, STAGED_RELEASE_KEY, SYSTEMD, ServiceManager, TARGET_VERSION_KEY,
};
pub use ports::{
    Clock, Host, Instance, Instances, Launchd, LaunchdPorts, Lock, Markers, PortError, PortKind,
    Reply, StateDir, SystemdPorts, UnitState, Units,
};
pub use probe::{Build, CacheMode, build_of, readiness};
pub use record::{Invalid, Kind, Marker, Mode, RECORD_MAX, Record, RecordError, SCHEMA};
pub use report::{Class, Done, MAX_REASON_LEN, Outcome, Reason, Report, is_reason_token};

/// The first argument that selects the verb.
pub const VERB: &str = "update-hook";

/// The port the service serves on, through both instances and the launchd job alike.
pub const PUBLIC_PORT: u16 = 9300;
/// The hook's own state directory: the record and its lock. root's alone (mode 0700); the hook
/// makes it when it is absent. hostkeeper does not remove it, and nor does `uninstall`.
pub const STATE_DIR: &str = "/var/lib/tierstore-hook";
/// The directory of the start markers of the systemd instances: the template's
/// `StateDirectory=` (`/var/lib/tierstore`), which also holds the hand-over socket.
pub const MARKER_DIR: &str = "/var/lib/tierstore";
/// The start marker of `instance`, in [`MARKER_DIR`]: the path the template's
/// `ConditionPathExists=` names with `%i`.
#[must_use]
pub fn marker_path(instance: Instance) -> PathBuf {
    PathBuf::from(MARKER_DIR).join(format!("run-{}", instance.letter()))
}
/// The account the service runs as: the template's `User=`. A check of the configuration by a
/// release runs as this account when the hook runs as root, so that the release judges the files
/// as the unit will.
pub const SERVICE_ACCOUNT: &str = "tierstore";
/// The binary of a release, under its version directory (hostkeeper's package layout).
pub const RELEASE_BINARY: &str = "bin/tierstore";
/// The plist of the launchd job, as hostkeeper links it from the package's `units/`.
pub const LAUNCHD_PLIST: &str = "/Library/LaunchDaemons/tierstore.plist";
/// The capability (`tierstore --capabilities`) of a release that offers and takes listening
/// sockets over `server.handoff_socket`. Both releases need it for a hand-over.
pub const HANDOFF_CAPABILITY: &str = crate::server::build_info::HANDOFF_SOCKET_CAPABILITY;
/// The capability of a release whose build node and storage node with `[local_cache]` hand their
/// sockets over too. Both releases need it, beside [`HANDOFF_CAPABILITY`], for a hand-over of a
/// node whose configuration has `[local_cache]`.
pub const CACHE_HANDOFF_CAPABILITY: &str = crate::server::build_info::HANDOFF_CACHE_CAPABILITY;
/// The capability of a release whose `server.handoff_socket` defaults to `handoff.sock` in the
/// service's state directory (`STATE_DIRECTORY`, which the template's `StateDirectory=` sets).
/// Both releases need it, beside [`HANDOFF_CAPABILITY`], for a hand-over of a node whose
/// configuration leaves the key out.
pub const HANDOFF_STATE_DIR_CAPABILITY: &str =
    crate::server::build_info::HANDOFF_STATE_DIR_CAPABILITY;
/// The table of the configuration that holds the hot tier.
pub const HOT_STORE_TABLE: &str = "hot_store";
/// The `kind` of a tier that is a directory of the file system.
pub const DIR_TIER_KIND: &str = "dir";
/// The table of the configuration that makes a node one with a local cache.
pub const LOCAL_CACHE_TABLE: &str = "local_cache";
/// The configuration key of the hand-over socket, in the `[server]` table.
pub const HANDOFF_SOCKET_KEY: &str = "handoff_socket";

/// What the verb takes from the machine it runs on: the body of the steps.
pub struct Machine<'a> {
    /// Runs the step `invocation` names, with the params already read.
    pub step: &'a dyn Fn(&Invocation, &Params) -> Done,
}

impl Machine<'static> {
    /// This machine: its programs, files and loopback port.
    #[must_use]
    pub fn system() -> Self {
        Self {
            step: &system::on_this_machine,
        }
    }
}

/// Why no step ran.
#[derive(Debug, thiserror::Error)]
enum Refusal {
    #[error("{0}")]
    Args(ArgsError),
    #[error("{0}")]
    Params(ParamsError),
    #[error(
        "the artifact {given:?} is not the staged release {STAGED_RELEASE_KEY} names ({staged:?})"
    )]
    Artifact { given: PathBuf, staged: PathBuf },
}

impl Refusal {
    /// The failure hostkeeper is told of: nothing was done.
    fn done(&self) -> Done {
        let reason = match self {
            Refusal::Args(_) | Refusal::Artifact { .. } => Reason::BadArgs,
            Refusal::Params(_) => Reason::BadParams,
        };
        Done::fail(reason, self.to_string())
    }
}

/// Reads the command line and the params file and runs the step they name.
fn step_of(args: &[OsString], machine: &Machine<'_>) -> Result<Done, Refusal> {
    let invocation = Invocation::parse(args.iter().skip(2).cloned()).map_err(Refusal::Args)?;
    let params = Params::read(&invocation.params).map_err(Refusal::Params)?;
    // `prepare` is given the staged tree twice, by argument and by the table; they must agree.
    if let Some(given) = &invocation.artifact
        && given.as_os_str() != params.staged_release.as_os_str()
    {
        return Err(Refusal::Artifact {
            given: given.clone(),
            staged: params.staged_release.clone(),
        });
    }
    Ok((machine.step)(&invocation, &params))
}

/// Runs the verb on `args` (the program name and the verb first): the last line goes to `out`,
/// the words for a person to `err`.
pub fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    machine: &Machine<'_>,
) -> ExitCode {
    let done = step_of(args, machine).unwrap_or_else(|refusal| {
        let _ = writeln!(err, "tierstore update-hook: {refusal}");
        refusal.done()
    });
    // A closed standard output cannot be reported anywhere; the status still is.
    let _ = done.report.write_to(out);
    done.outcome.into()
}

/// The verb as the binary runs it.
#[must_use]
pub fn main(args: &[OsString]) -> ExitCode {
    run(
        args,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &Machine::system(),
    )
}

#[cfg(test)]
mod args_tests;
#[cfg(test)]
mod docs_tests;
#[cfg(test)]
mod launchd_tests;
#[cfg(test)]
mod look_tests;
#[cfg(test)]
mod params_tests;
#[cfg(test)]
mod probe_tests;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod system_tests;
#[cfg(test)]
mod systemd_tests;
#[cfg(test)]
mod verb_tests;
