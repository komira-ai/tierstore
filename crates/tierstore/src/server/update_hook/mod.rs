//! `tierstore update-hook`: tierstore's side of the host updater's hook contract (Linux and
//! macOS).
//!
//! ```text
//! tierstore update-hook <prepare|cutover|verify|rollback|cleanup> [<artifact>] --params <file>
//! tierstore update-hook restore
//! ```
//!
//! The updater runs one step per process and reads two things: the exit status and the last line
//! of standard output, a JSON object ([`Report`]). Exit 0 is success, 1 failure and 75 deferred
//! (the host is not ready, nothing was done, try later). The verb is answered before the
//! server's flags are parsed: a hook is a short-lived process of the updater, not a server.
//!
//! Two instances run side by side during an update, on the private ports of the two slots
//! (`tierstore@9301`, `tierstore@9302`); the public port 9300 is redirected to the serving slot
//! by one nftables rule. The steps keep a record ([`Record`]), set state rather than do things
//! once, and reach the machine only through the ports of [`Ports`]. [`Machine::system`] builds
//! those ports from the machine's implementations ([`machine`]): the declaration it reads gives
//! the state directory and the slot links, and the step runs over them.
//!
//! On macOS one instance runs, under launchd (the job [`machine::LAUNCHD_LABEL`]), on one port
//! of loopback, and an update is a plain restart with a gap in serving: the steps of
//! [`single`](SingleRecord) stop the service, point the release link `current` at the staged
//! release and start it again, through the ports of [`SinglePorts`] (the service, the release
//! links, the instance, the host, the clock and the state directory: no nftables, no slot). The
//! declaration is [`LAUNCHD_DECLARATION_PATH`] ([`LaunchdDeclaration`]). The steps, the launchd
//! calls and the declaration are built and tested on Linux too; what is built on one system
//! alone is the wiring to the real machine (`system` on Linux, the last function of
//! `launchd_system` on macOS) and the `nft` and `systemctl` ports, which only Linux has.

mod args;
mod declaration;
#[cfg(all(target_os = "linux", feature = "hook-conformance"))]
pub mod fake;
mod launchd_system;
pub mod machine;
mod params;
mod plain;
mod ports;
mod probe;
mod record;
mod report;
mod ruleset;
mod single;
mod steps;
#[cfg(target_os = "linux")]
mod system;
mod validate;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

pub use args::{ArgsError, HostArgs, Invocation, ROOT_UID, Step};
pub use declaration::{
    DECLARATION_PATH, Declaration, DeclarationError, LAUNCHD_DECLARATION_PATH, LaunchdDeclaration,
    PUBLIC_PORT, unit_name,
};
pub use params::{
    CONNTRACK_PERCENT_KEY, DEFER_RETRY_KEY, MODE_KEY, PROMOTION_BOUND_KEY, PROMOTION_CEILING_KEY,
    Params, ParamsError, RESTART_MODE, STAGED_RELEASE_KEY, TARGET_VERSION_KEY,
};
pub use ports::{
    Clock, Conntrack, Host, Instances, Link, Lock, Nft, PortError, PortKind, Ports, Releases,
    Reply, Service, SinglePorts, Slots, StateDir, Units,
};
pub use probe::{Build, CacheMode, build_of, readiness};
pub use record::{Marker, Mode, RECORD_MAX, Record, RecordError, SCHEMA};
pub use report::{Class, Done, MAX_REASON_LEN, Outcome, Reason, Report, is_reason_token};
pub use ruleset::{FORWARD_TABLE, GUARD_TABLE, guard_ruleset, redirect_ruleset, redirect_target};
pub use single::{
    Kind as SingleKind, SKIPPED_CLEANUP, SKIPPED_CUTOVER, SKIPPED_PREPARE, SingleRecord,
    run_step as run_single_step,
};
pub use steps::{HANDOVER_CAPABILITY, run_step};
pub use validate::{Invalid, check_params, validate};

/// The first argument that selects the verb.
pub const VERB: &str = "update-hook";

/// What the verb takes from the machine it runs on: the body of the steps.
pub struct Machine<'a> {
    /// Runs the step `invocation` names, with the params already read (`None` for `restore`).
    pub step: &'a dyn Fn(&Invocation, Option<&Params>) -> Done,
}

impl Machine<'static> {
    /// This machine: the two slots of Linux, or the one launchd service of macOS.
    #[must_use]
    pub fn system() -> Self {
        #[cfg(target_os = "linux")]
        let step = &system::on_this_machine;
        #[cfg(target_os = "macos")]
        let step = &launchd_system::on_this_machine;
        Self { step }
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
    /// The failure the updater is told of: nothing was done.
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
    let params = invocation
        .params
        .as_deref()
        .map(Params::read)
        .transpose()
        .map_err(Refusal::Params)?;
    // `prepare` is given the staged tree twice, by argument and by the table; they must agree.
    if let (Some(given), Some(params)) = (&invocation.artifact, &params)
        && given.as_os_str() != params.staged_release.as_os_str()
    {
        return Err(Refusal::Artifact {
            given: given.clone(),
            staged: params.staged_release.clone(),
        });
    }
    Ok((machine.step)(&invocation, params.as_ref()))
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
mod declaration_tests;
#[cfg(test)]
mod docs_tests;
#[cfg(all(test, target_os = "linux", feature = "hook-conformance"))]
mod fake_tests;
#[cfg(test)]
mod launchd_declaration_tests;
#[cfg(test)]
mod launchd_system_tests;
#[cfg(test)]
mod params_tests;
#[cfg(test)]
mod probe_tests;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod ruleset_tests;
#[cfg(test)]
mod single_crash_tests;
#[cfg(test)]
mod single_record_tests;
#[cfg(test)]
mod single_testing;
#[cfg(test)]
mod single_tests;
#[cfg(test)]
mod steps_cleanup_tests;
#[cfg(test)]
mod steps_crash_tests;
#[cfg(test)]
mod steps_cutover_tests;
#[cfg(test)]
mod steps_forged_tests;
#[cfg(test)]
mod steps_prepare_tests;
#[cfg(test)]
mod steps_restore_tests;
#[cfg(test)]
mod steps_rollback_tests;
#[cfg(test)]
mod steps_verify_tests;
#[cfg(all(test, target_os = "linux"))]
mod system_tests;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod validate_tests;
#[cfg(test)]
mod verb_tests;
