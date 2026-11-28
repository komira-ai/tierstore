//! `tierstore config-hook`: the hook of a **config package**, a package that replaces only the
//! node's configuration files (`/etc/tierstore/config.toml` and `/etc/tierstore/guard.nft` on
//! Linux, `/usr/local/tierstore/etc/tierstore.toml` on a Mac), while the binary stays the one
//! the service package installed.
//!
//! The command line, the params file, the exit statuses and the last line are those of
//! `update-hook` ([`super::run`]); the steps are `prepare`, `cutover`, `verify`, `rollback`,
//! `cleanup` and `uninstall` (there is no boot step: `restore` is refused). The steps move the
//! same instances as an update of the release, with the release unchanged:
//!
//! - `prepare` has the binary that serves check the file the package stages
//!   (`<staged release>/tree/<config file>`) before it is installed, refuses while an update of
//!   the release is in flight, and chooses the way: a hand-over when the file that serves and the
//!   staged one listen alike (their `fs` interfaces apart) and name the same hand-over socket, a
//!   restart otherwise. It refuses with `mode_unsupported`, unless the params say
//!   `mode = "restart"`, a staged file whose `fs` interface moves the mount point, the control
//!   socket or the runtime directory of the one that serves, and a change that must restart a
//!   node that holds a mount. On Linux it refuses the switch of a node to one process (below).
//! - `cutover` reloads the network guard (Linux, when the node has `guard.nft`) and starts the
//!   other instance on the new file, by the hand-over (or the restart) of an update; on a Mac it
//!   restarts the one job.
//! - `verify` has the binary check the installed file (`--check-config`) and reads `/readyz`
//!   and the build of the instance that serves; for the switch of a Mac it also requires the
//!   mount listed at the mount point of the installed file.
//! - `rollback`, after hostkeeper has put the previous package's files back, reloads the guard
//!   and hands back to the instance that served; for the switch of a Mac the mount goes back
//!   first. A node's first config package has no files to go back to: its rollback reloads nothing, leaves the instance that runs serving and fails
//!   `no_previous_release`.
//! - `cleanup` retires the old instance, as for an update.
//! - `uninstall` changes nothing: the service keeps what it read.
//!
//! On systemd a staged file that differs from the one that serves in nothing but
//! `tree_list_ram_bytes` of the `fs` interface, or in nothing at all, moves no instance: `prepare`
//! records it as a change applied in place, `cutover` has the control socket of the instance that
//! serves set the new value (and reloads the guard only when its file changed), `verify` reads it
//! back from the gauge of the mount, `rollback` sets the value of the file that served again, and
//! `cleanup` commits with the same instance serving (`reload.rs`).
//!
//! A node's fs process of the old layout is left alone by every config update. The **switch** of
//! a node to one process, a package whose staged file has an `fs` interface where the file that
//! serves has none and the old fs process runs (on a Mac: the `tierstore-fs` job is loaded), is
//! a Mac's alone: it is a restart, with the fs job booted out and disabled first (`switch.rs`).
//! On Linux this release has no step for the fs unit, so `prepare` refuses it with
//! `mode_unsupported` (`old_layout.rs`), and the node is switched to the `fs` interface by the
//! release that does that first. Where no old fs process runs, the package is a plain config
//! update.
//!
//! Its record is its own, beside the update hook's and under the same lock
//! (`config-state.json`), so that the release's record (`to`, `version`, the promotion) is never touched; `cleanup`
//! writes the instance that now serves into the release's record, which the boot step reads.
//! The build an instance must show is this binary's own version: the config hook is run from
//! the service's installed binary, which is the one that serves.

mod reload;
mod steps;
mod switch;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::params::{Params, ParamsError, SERVICE_MANAGER_KEY, ServiceManager};
use super::{Machine, run_verb};

pub use steps::{Common, GUARD_FILE, GUARD_UNIT, Running, launchd_step, systemd_step};

/// The first argument that selects the verb.
pub const VERB: &str = "config-hook";

/// The configuration file of a Linux node, when the params name none.
pub const LINUX_CONFIG_FILE: &str = "/etc/tierstore/config.toml";
/// The configuration file of a Mac, when the params name none.
pub const MACOS_CONFIG_FILE: &str = "/usr/local/tierstore/etc/tierstore.toml";
/// The label of the launchd job of a Mac, when the params name no service manager.
pub const MACOS_LABEL: &str = "tierstore";

/// What a config package's params may leave out, by the system the hook runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Defaults {
    /// The configuration file, when `config_file` is absent.
    pub config_file: &'static str,
    /// The service manager, when `service_manager` is absent.
    pub service_manager: ServiceManager,
}

/// The defaults of the system this binary is built for: systemd and
/// [`LINUX_CONFIG_FILE`] on Linux, launchd with the job [`MACOS_LABEL`] and
/// [`MACOS_CONFIG_FILE`] on macOS.
#[must_use]
pub fn this_system() -> Defaults {
    if cfg!(target_os = "macos") {
        Defaults {
            config_file: MACOS_CONFIG_FILE,
            service_manager: ServiceManager::Launchd {
                label: MACOS_LABEL.to_owned(),
                fs_label: None,
            },
        }
    } else {
        Defaults {
            config_file: LINUX_CONFIG_FILE,
            service_manager: ServiceManager::Systemd,
        }
    }
}

/// Reads the text of a config package's params file as `update-hook` does, then fills in what it
/// leaves out from `defaults`: a config package built for a group need not name the service's
/// files.
///
/// # Errors
/// [`ParamsError`], as for the update hook.
pub fn parse_params(text: &str, defaults: &Defaults) -> Result<Params, ParamsError> {
    let mut params = Params::parse(text)?;
    // `Params::parse` has read the text as a table already.
    let named_manager = text
        .parse::<toml::Table>()
        .is_ok_and(|table| table.contains_key(SERVICE_MANAGER_KEY));
    if !named_manager {
        params.service_manager = defaults.service_manager.clone();
    }
    if params.config_file.is_none() {
        params.config_file = Some(PathBuf::from(defaults.config_file));
    }
    Ok(params)
}

fn read_params(path: &Path) -> Result<Params, ParamsError> {
    parse_params(&std::fs::read_to_string(path)?, &this_system())
}

/// Where a config package's staged copy of the file `config` is: hostkeeper unpacks a package
/// into its version directory's `tree/`.
#[must_use]
pub fn staged_copy(staged_release: &Path, config: &Path) -> PathBuf {
    staged_release
        .join("tree")
        .join(config.strip_prefix("/").unwrap_or(config))
}

/// Runs the verb on `args` (the program name and the verb first): the last line goes to `out`,
/// the words for a person to `err`.
pub fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    machine: &Machine<'_>,
) -> ExitCode {
    run_verb(VERB, args, out, err, machine, read_params)
}

/// The verb as the binary runs it.
#[must_use]
pub fn main(args: &[OsString]) -> ExitCode {
    let machine = Machine {
        step: &super::system::config_on_this_machine,
    };
    run(
        args,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &machine,
    )
}

#[cfg(test)]
mod params_tests;
#[cfg(test)]
mod steps_tests;
#[cfg(test)]
mod system_tests;
