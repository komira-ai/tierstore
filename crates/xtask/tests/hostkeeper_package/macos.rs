//! The macOS package, on the Linux runner: its manifest and plist read as data, the placed-path
//! rule of a plist's program, its manifest and plist loaded by hostkeeper's loader, which reads
//! both the manifest's `platform` key and launchd jobs, and its archive through hostkeeper's
//! unpacker (both at the one commit the root `Cargo.toml` pins).

use std::path::Path;

use hostkeeper_config::manifest::{Expected, HostkeeperVersion, Manifest, ManifestRefusal};
use hostkeeper_config::names::{ServiceName, Version};
use hostkeeper_placed::lint::{LintError, check_unit_text};
use xtask::package::Platform;
use xtask::release::hostkeeper_package::{BINARY, LAUNCHD_LABEL};

use crate::support::{
    CURRENT_BINARY, LINUX, MIN_HOSTKEEPER, PACKAGES, Scratch, VERSION, Value, hk_platform, integer,
    listing, manifest_toml, package, plist, seconds, string, text, unpack, version_dir, write_tree,
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

/// The macOS package's files below `root`, as a host keeps the version, its manifest through
/// `edit` first, loaded with hostkeeper's manifest loader.
fn load_with_hostkeepers_loader(
    root: &Path,
    edit: impl Fn(String) -> String,
) -> Result<Manifest, ManifestRefusal> {
    let version = version_dir(root);
    std::fs::create_dir_all(&version).expect("the version directory");
    write_tree(&version, &mac(), |name, bytes| match name {
        "manifest.toml" => edit(String::from_utf8(bytes).expect("UTF-8")).into_bytes(),
        _ => bytes,
    });
    Manifest::load(
        &version,
        &Expected {
            name: ServiceName::new("tierstore").expect("a name"),
            version: Version::new(VERSION).expect("a version"),
            platform: hk_platform(Platform::MacosArm64),
        },
        &HostkeeperVersion::parse(MIN_HOSTKEEPER).expect("a version"),
    )
}

/// The manifest `text` with its run entry replaced by `entry`.
fn run_entry(text: String, entry: &str) -> String {
    let built = format!("run = [\"{LAUNCHD_LABEL}\"]");
    assert!(text.contains(&built), "the built run entry");
    text.replace(&built, &format!("run = [\"{entry}\"]"))
}

#[test]
fn the_macos_manifest_and_its_plist_load_with_hostkeepers_launchd_loader() {
    // Catches: a macOS package hostkeeper's loader refuses now that it reads launchd jobs: a plist
    // file that is not `<label>.plist` for a label of tierstore, or a run entry that is not the
    // label of the package's plist. The two edited manifests, refused, are the control that the
    // loader checks the run entry against the plist: a label of tierstore with no plist of its
    // own, and a label that is not tierstore's.
    let scratch = Scratch::new("hostkeeper-package-mac-load");
    let manifest = load_with_hostkeepers_loader(&scratch.path().join("as-built"), |text| text)
        .expect("loaded");
    assert_eq!(manifest.version.as_str(), VERSION);
    assert_eq!(manifest.run, [LAUNCHD_LABEL]);
    assert_eq!(
        manifest.unit_files.iter().collect::<Vec<_>>(),
        ["tierstore.plist"]
    );
    assert_eq!(
        manifest
            .params
            .get("launchd_label")
            .and_then(toml::Value::as_str),
        Some(LAUNCHD_LABEL)
    );
    assert_eq!(manifest.hook.command, ["bin/tierstore", "update-hook"]);
    for (dir, entry) in [
        ("no-plist", "tierstore.other"),
        ("not-tierstores", "com.example.tierstore"),
    ] {
        let refused =
            load_with_hostkeepers_loader(&scratch.path().join(dir), |text| run_entry(text, entry))
                .unwrap_err();
        assert!(
            matches!(&refused, ManifestRefusal::RunUnit { unit, .. } if unit == entry),
            "{entry}: {refused:?}"
        );
    }
}

#[test]
fn the_macos_package_unpacks_with_hostkeeper_and_its_manifest_loads() {
    // Catches: a macOS package hostkeeper's unpacker refuses: an archive rule broken, or a
    // manifest or launchd job its loader, which knows both the manifest's `platform` key and
    // launchd jobs (komira-ai/hostkeeper#32), does not take.
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
