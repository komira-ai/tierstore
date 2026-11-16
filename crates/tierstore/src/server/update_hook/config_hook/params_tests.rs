//! The config hook's params: the update hook's table, with the service's files and service
//! manager filled in for the system when the package leaves them out.

use std::path::{Path, PathBuf};

use super::super::params::{ParamsError, ServiceManager};
use super::{
    Defaults, LINUX_CONFIG_FILE, MACOS_CONFIG_FILE, MACOS_LABEL, parse_params, staged_copy,
    this_system,
};

const STAGED: &str = "\"updater.staged_release\" = \"/var/lib/hostkeeper/packages/tierstore-config-g/0.4.0\"\n\
                      \"updater.target_version\" = \"0.4.0\"\n";

fn linux() -> Defaults {
    Defaults {
        config_file: LINUX_CONFIG_FILE,
        service_manager: ServiceManager::Systemd,
    }
}

fn mac() -> Defaults {
    Defaults {
        config_file: MACOS_CONFIG_FILE,
        service_manager: ServiceManager::Launchd {
            label: MACOS_LABEL.to_owned(),
        },
    }
}

/// Asserts that a config package's params that name only the reserved keys get the Linux file
/// and systemd on Linux.
#[test]
fn params_that_name_no_file_get_the_linux_file_and_systemd() {
    let params = parse_params(STAGED, &linux()).unwrap();
    assert_eq!(
        params.config_file,
        Some(PathBuf::from("/etc/tierstore/config.toml"))
    );
    assert_eq!(params.service_manager, ServiceManager::Systemd);
    assert_eq!(params.target_version, "0.4.0");
}

/// Asserts that the same params get the Mac's file and its launchd job on a Mac.
#[test]
fn params_that_name_no_file_get_the_macs_file_and_job() {
    let params = parse_params(STAGED, &mac()).unwrap();
    assert_eq!(
        params.config_file,
        Some(PathBuf::from("/usr/local/tierstore/etc/tierstore.toml"))
    );
    assert_eq!(
        params.service_manager,
        ServiceManager::Launchd {
            label: "tierstore".to_owned()
        }
    );
}

/// Asserts that a file and a service manager the params name are kept over the defaults.
#[test]
fn what_the_params_name_is_kept() {
    let text = format!(
        "{STAGED}config_file = \"/srv/tierstore.toml\"\nservice_manager = \"launchd\"\nlaunchd_label = \"dev.tierstore\"\n"
    );
    let params = parse_params(&text, &linux()).unwrap();
    assert_eq!(
        params.config_file,
        Some(PathBuf::from("/srv/tierstore.toml"))
    );
    assert_eq!(
        params.service_manager,
        ServiceManager::Launchd {
            label: "dev.tierstore".to_owned()
        }
    );
    let systemd = format!("{STAGED}service_manager = \"systemd\"\n");
    assert_eq!(
        parse_params(&systemd, &mac()).unwrap().service_manager,
        ServiceManager::Systemd
    );
}

/// Asserts that a service manager the hook does not know is kept as such (every step then
/// refuses it), not replaced by the default.
#[test]
fn an_unknown_service_manager_is_not_replaced_by_the_default() {
    let text = format!("{STAGED}service_manager = \"runit\"\n");
    assert_eq!(
        parse_params(&text, &linux()).unwrap().service_manager,
        ServiceManager::Unknown("runit".to_owned())
    );
}

/// Asserts that params the update hook refuses are refused here with the same error.
#[test]
fn params_the_update_hook_refuses_are_refused() {
    assert!(matches!(
        parse_params("\"updater.target_version\" = \"0.4.0\"\n", &linux()),
        Err(ParamsError::Missing(_))
    ));
    assert!(matches!(
        parse_params("not toml =", &linux()),
        Err(ParamsError::Syntax(_))
    ));
}

/// Asserts that the system's defaults are those of the system the test runs on.
#[test]
fn the_defaults_are_the_systems() {
    let expected = if cfg!(target_os = "macos") {
        mac()
    } else {
        linux()
    };
    assert_eq!(this_system(), expected);
}

/// Asserts that the staged copy of a file is under the version directory's `tree/`, whether the
/// file is named absolute or not.
#[test]
fn the_staged_copy_is_under_the_tree() {
    let staged = Path::new("/var/lib/hostkeeper/packages/c/1");
    assert_eq!(
        staged_copy(staged, Path::new("/etc/tierstore/config.toml")),
        Path::new("/var/lib/hostkeeper/packages/c/1/tree/etc/tierstore/config.toml")
    );
    assert_eq!(
        staged_copy(staged, Path::new("etc/x.toml")),
        Path::new("/var/lib/hostkeeper/packages/c/1/tree/etc/x.toml")
    );
}

/// Asserts that the verb hands the step its params with the system's defaults filled in, and
/// that a refusal of the command line is said as `tierstore config-hook` and fails `bad_args`.
#[test]
fn the_verb_reads_the_params_with_the_defaults_and_names_itself_in_a_refusal() {
    use std::cell::RefCell;
    use std::ffi::OsString;

    use super::super::{Done, Invocation, Machine, Params};
    use super::run;

    let dir = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "config-hook-verb");
    let file = dir.path().join("params.toml");
    std::fs::write(&file, STAGED).unwrap();
    let seen = RefCell::new(None);
    let step = |invocation: &Invocation, params: &Params| {
        *seen.borrow_mut() = Some((invocation.step, params.config_file.clone()));
        Done::ok("seen")
    };
    let machine = Machine { step: &step };
    let args = |rest: &[&str]| -> Vec<OsString> {
        ["tierstore", "config-hook"]
            .iter()
            .chain(rest)
            .map(OsString::from)
            .collect()
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let params = file.to_str().unwrap();
    let status = run(
        &args(&["verify", "--params", params]),
        &mut out,
        &mut err,
        &machine,
    );
    assert_eq!(status, std::process::ExitCode::SUCCESS);
    assert_eq!(
        seen.borrow().clone(),
        Some((
            super::super::Step::Verify,
            Some(PathBuf::from(this_system().config_file))
        ))
    );
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = run(
        &args(&["bounce", "--params", params]),
        &mut out,
        &mut err,
        &machine,
    );
    assert_eq!(status, std::process::ExitCode::from(1));
    let said = String::from_utf8(err).unwrap();
    assert!(said.starts_with("tierstore config-hook: "), "{said}");
    let last = String::from_utf8(out).unwrap();
    assert!(last.contains("\"reason\":\"bad_args\""), "{last}");
}
