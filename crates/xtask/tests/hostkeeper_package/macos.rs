//! The macOS package, on the Linux runner: its manifest and plist read as data, the placed-path
//! rule of a plist's program, and, through hostkeeper's unpacker, where hostkeeper stands today.

use std::path::Path;

use hostkeeper_config::manifest::ManifestRefusal;
use hostkeeper_config::names::UnitError;
use hostkeeper_placed::lint::{LintError, check_unit_text};
use hostkeeper_unpack::UnpackError;
use xtask::package::Platform;
use xtask::release::hostkeeper_package::{BINARY, LAUNCHD_LABEL};

use crate::support::{
    CURRENT_BINARY, LINUX, PACKAGES, Scratch, VERSION, Value, hk_platform, integer, listing,
    manifest_toml, package, plist, seconds, string, text, unpack,
};

/// The launchd job of the macOS package.
const PLIST: &str = "units/tierstore.plist";

fn mac() -> Vec<u8> {
    package(Platform::MacosArm64)
}

/// The `[units] run` entries of a manifest.
fn run(toml: &toml::Table) -> Vec<&str> {
    toml["units"]["run"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|unit| unit.as_str().expect("a string"))
        .collect()
}

#[test]
fn the_macos_manifest_names_macos_arm64_and_one_job_whose_label_is_the_plists() {
    // Catches: a macOS manifest that names another platform, two run entries (there is no
    // hand-over on a Mac), or a run entry, `launchd_label` and plist Label that differ, so the
    // daemon would observe one job and the hook restart another.
    let archive = mac();
    let toml = manifest_toml(&archive);
    assert_eq!(toml["platform"].as_str(), Some("macos-arm64"));
    assert_eq!(run(&toml), [LAUNCHD_LABEL]);
    let params = toml["params"].as_table().expect("a table");
    assert_eq!(params["launchd_label"].as_str(), Some(LAUNCHD_LABEL));
    assert_eq!(params["service_manager"].as_str(), Some("launchd"));
    assert_eq!(
        params["config_file"].as_str(),
        Some("/usr/local/tierstore/etc/tierstore.toml")
    );
    let job = plist(&text(&archive, PLIST));
    assert_eq!(string(&job["Label"]), LAUNCHD_LABEL);
    // The unit file is linked as <unit dir>/<label>.plist, and its name starts with the service
    // name, as hostkeeper requires of every unit file.
    assert_eq!(PLIST, format!("units/{LAUNCHD_LABEL}.plist"));
    assert!(LAUNCHD_LABEL.starts_with("tierstore"));
}

#[test]
fn the_macos_manifest_differs_from_the_linux_one_only_in_platform_run_and_params() {
    // Catches: a bound, the hook, the health URL or any other key that differs between the two
    // systems by mistake, and a Linux key (`service_manager = "systemd"`, the a/b pair) left in
    // the macOS manifest.
    let strip = |mut toml: toml::Table| {
        for key in ["platform", "units", "params"] {
            toml.remove(key).expect("the key");
        }
        toml
    };
    let mac_toml = manifest_toml(&mac());
    for platform in LINUX {
        let linux_toml = manifest_toml(&package(platform));
        assert_eq!(
            strip(linux_toml.clone()),
            strip(mac_toml.clone()),
            "{platform:?}"
        );
        let keys = |toml: &toml::Table| -> Vec<String> {
            toml["params"]
                .as_table()
                .expect("a table")
                .keys()
                .cloned()
                .collect()
        };
        assert_eq!(keys(&linux_toml), ["config_file", "service_manager"]);
        assert_eq!(
            keys(&mac_toml),
            ["config_file", "launchd_label", "service_manager"]
        );
        assert_eq!(run(&linux_toml).len(), 2);
        assert_eq!(run(&mac_toml).len(), 1);
    }
}

#[test]
fn the_plist_runs_the_binary_of_current_as_tierstore_and_its_exit_timeout_covers_the_drain() {
    // Catches: a program other than the package's binary through `current` (the job would not
    // follow the switch), another configuration file than the manifest's, a job that runs as
    // root or is not started and kept alive, an ExitTimeOut shorter than the drain (launchd would
    // kill a drain in progress), and cutover or rollback bounds shorter than ExitTimeOut.
    let archive = mac();
    let job = plist(&text(&archive, PLIST));
    let Value::Array(arguments) = &job["ProgramArguments"] else {
        panic!("ProgramArguments is {:?}", job["ProgramArguments"]);
    };
    let words: Vec<&str> = arguments.iter().map(string).collect();
    let toml = manifest_toml(&archive);
    let config = toml["params"]["config_file"].as_str().expect("a string");
    assert_eq!(
        words,
        [
            CURRENT_BINARY,
            "--config",
            config,
            "--drain-lb-wait",
            "1s",
            "--drain-deadline",
            "60s"
        ]
    );
    assert!(CURRENT_BINARY.ends_with(&format!("/current/{BINARY}")));
    assert_eq!(string(&job["UserName"]), "_tierstore");
    assert_eq!(string(&job["GroupName"]), "_tierstore");
    assert_eq!(job["RunAtLoad"], Value::Bool(true));
    assert_eq!(job["KeepAlive"], Value::Bool(true));
    assert_eq!(integer(&job["ThrottleInterval"]), 30);
    let exit_timeout = u64::try_from(integer(&job["ExitTimeOut"])).expect("positive");
    let drain = seconds(words[4]) + seconds(words[6]);
    assert!(
        exit_timeout > drain,
        "ExitTimeOut {exit_timeout} s, drain {drain} s"
    );
    for step in ["cutover", "rollback"] {
        let bound = toml["hook"]["timeout_secs"][step]
            .as_integer()
            .expect("a bound");
        assert!(
            u64::try_from(bound).expect("positive") > exit_timeout,
            "{step}"
        );
    }
}

#[test]
fn the_plist_keeps_hostkeepers_rule_for_the_program_of_a_unit() {
    // Catches: a plist whose program names a version directory, which hostkeeper's placed-path
    // lint refuses for a plist as for ExecStart=. The edited plist, refused, is the control.
    let plist = text(&mac(), PLIST);
    let packages = Path::new(PACKAGES);
    assert_eq!(
        check_unit_text(packages, "tierstore", "tierstore.plist", &plist),
        Ok(())
    );
    let pinned = plist.replace("/current/bin/", &format!("/{VERSION}/bin/"));
    let refused = check_unit_text(packages, "tierstore", "tierstore.plist", &pinned);
    assert!(
        matches!(&refused, Err(LintError::PackagePath { key, .. }) if key == "ProgramArguments"),
        "{refused:?}"
    );
}

#[test]
fn the_macos_package_has_one_job_no_service_file_and_the_linux_ones_no_plist() {
    // Catches: a systemd unit or the a/b template in the macOS package, or a plist in a Linux
    // one, which hostkeeper would link into the other system's unit directory.
    let units = |archive: &[u8]| -> Vec<String> {
        listing(archive)
            .into_iter()
            .map(|entry| entry.name)
            .filter(|name| name.starts_with("units/"))
            .collect()
    };
    assert_eq!(units(&mac()), [PLIST]);
    for platform in LINUX {
        let linux = units(&package(platform));
        assert_eq!(linux, ["units/tierstore@.service"], "{platform:?}");
        assert!(linux.iter().all(|name| !name.ends_with(".plist")));
    }
    assert!(
        units(&mac())
            .iter()
            .all(|name| !name.ends_with(".service") && !name.contains('@'))
    );
}

#[test]
fn today_the_macos_package_keeps_every_archive_rule_and_only_the_unit_name_rule_refuses_it() {
    // Catches: a macOS archive that breaks one of hostkeeper's archive rules: the unpacker reads
    // and writes every entry before it loads the manifest, so a refusal of the plist's name by the
    // loader means every archive rule held. The loader takes `.service`, `.socket` and `.timer`
    // unit files only until hostkeeper accepts a launchd plist and label on macOS; then this test
    // fails and the one below, ignored until then, takes its place.
    let scratch = Scratch::new("hostkeeper-package-mac-today");
    let refused = unpack(scratch.path(), &mac(), Platform::MacosArm64).unwrap_err();
    assert!(
        matches!(
            &refused,
            UnpackError::Manifest(ManifestRefusal::UnitFile { file, why: UnitError::NotAUnit })
                if file == "tierstore.plist"
        ),
        "{refused:?}"
    );
}

#[test]
#[ignore = "waits for a hostkeeper manifest loader that accepts a launchd plist unit file and a launchd label as a run entry on macOS"]
fn the_macos_package_unpacks_with_hostkeeper_and_its_manifest_loads() {
    // Catches: a macOS package hostkeeper refuses once its loader reads launchd jobs.
    let scratch = Scratch::new("hostkeeper-package-mac-unpack");
    let unpacked = unpack(scratch.path(), &mac(), Platform::MacosArm64).expect("unpacked");
    let manifest = &unpacked.manifest;
    assert_eq!(manifest.version.as_str(), VERSION);
    assert_eq!(manifest.platform, hk_platform(Platform::MacosArm64));
    assert_eq!(manifest.run, [LAUNCHD_LABEL]);
    assert!(manifest.unit_files.contains("tierstore.plist"));
    assert_eq!(
        manifest
            .params
            .get("launchd_label")
            .and_then(toml::Value::as_str),
        Some(LAUNCHD_LABEL)
    );
}
