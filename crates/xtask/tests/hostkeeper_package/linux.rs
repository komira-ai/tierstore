//! The two Linux packages, read with hostkeeper's own code: the unpacker and the manifest loader
//! of komira-ai/hostkeeper#23, and the placed-path lint of hostkeeper's main with main's loader.

use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use hostkeeper_config::manifest::{
    Expected, Hook, HostkeeperVersion, Manifest, ManifestRefusal, Timeouts,
};
use hostkeeper_config::names::{ServiceName, Version};
use hostkeeper_placed::lint::{LintError, check_unit_text, lint, through_current, wanted_by};
use hostkeeper_placed::{RealFs, Roots};
use xtask::Error;
use xtask::package::Platform;
use xtask::release::hostkeeper_package::{BINARY, hostkeeper_package};

use crate::support::{
    CURRENT_BINARY, LINUX, MIN_HOSTKEEPER, PACKAGES, Scratch, VERSION, binary, flag, hk_platform,
    manifest_toml, package, seconds, text, unit_value, unpack, version_dir, write_tree,
};

/// The unit template of the Linux packages.
const UNIT: &str = "units/tierstore@.service";

fn bound(seconds: u64) -> NonZeroU64 {
    NonZeroU64::new(seconds).expect("not zero")
}

fn running() -> HostkeeperVersion {
    HostkeeperVersion::parse(MIN_HOSTKEEPER).expect("a version")
}

/// What the order of tierstore [`VERSION`] on `platform` expects.
fn expected(platform: Platform) -> Expected {
    Expected {
        name: ServiceName::new("tierstore").expect("a service name"),
        version: Version::new(VERSION).expect("a version"),
        platform: hk_platform(platform),
    }
}

/// The manifest of `platform`'s package, through `edit` first, loaded by the loader from a
/// version directory below `dir` that holds the package's files.
fn load_edited(
    dir: &Path,
    platform: Platform,
    edit: impl Fn(String) -> String,
) -> Result<Manifest, ManifestRefusal> {
    let version = dir.join("version");
    write_tree(&version, &package(platform), |name, bytes| match name {
        "manifest.toml" => edit(String::from_utf8(bytes).expect("UTF-8")).into_bytes(),
        _ => bytes,
    });
    Manifest::load(&version, &expected(platform), &running())
}

#[test]
fn the_linux_packages_unpack_with_hostkeeper_and_their_manifest_is_the_one_the_hook_expects() {
    // Catches: a package hostkeeper's unpacker refuses (its archive rules, its manifest rules),
    // and any key of the manifest other than the update hook and the units are built on: the
    // hook command, its bounds, the two instances, the health URL, the params the hook reads.
    for platform in LINUX {
        let scratch = Scratch::new("hostkeeper-package-linux-unpack");
        let unpacked = unpack(scratch.path(), &package(platform), platform).expect("unpacked");
        let manifest = &unpacked.manifest;
        assert_eq!(manifest.name.as_str(), "tierstore");
        assert_eq!(manifest.version.as_str(), VERSION);
        assert_eq!(manifest.platform, hk_platform(platform));
        assert_eq!(manifest.min_hostkeeper, running());
        assert_eq!(
            manifest.hook,
            Hook {
                command: vec!["bin/tierstore".to_owned(), "update-hook".to_owned()],
                boot: Some("restore".to_owned()),
                verify_deadline_secs: 1600,
                promotion_ceiling_secs: Some(bound(3600)),
                cleanup_retries: 3,
                timeout_secs: Timeouts {
                    prepare: None,
                    cutover: bound(300),
                    verify: bound(1000),
                    rollback: bound(600),
                    cleanup: bound(1300),
                    uninstall: Some(bound(300)),
                },
            }
        );
        assert_eq!(
            manifest.unit_files,
            BTreeSet::from(["tierstore@.service".to_owned()])
        );
        assert_eq!(manifest.run, ["tierstore@a.service", "tierstore@b.service"]);
        assert_eq!(
            manifest.health_url.as_deref(),
            Some("http://127.0.0.1:9300/readyz")
        );
        assert!(manifest.files.is_empty(), "{:?}", manifest.files);
        let params: Vec<(&str, Option<&str>)> = manifest
            .params
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            params,
            [
                ("config_file", Some("/etc/tierstore/config.toml")),
                ("service_manager", Some("systemd")),
            ]
        );
        let mode = |name: &str| {
            let meta = std::fs::metadata(unpacked.dir.join(name)).expect("unpacked file");
            meta.permissions().mode() & 0o7777
        };
        assert_eq!(mode(BINARY), 0o555, "the binary is executable on the host");
        assert_eq!(mode(UNIT), 0o444);
    }
}

#[test]
fn the_manifest_version_is_the_version_the_binary_embeds() {
    // Catches: a manifest version other than the text the binary reports (no commit, another
    // commit), which hostkeeper would install and the update hook's check of the build gauge
    // would then refuse on every host.
    let scratch = Scratch::new("hostkeeper-package-version");
    for platform in LINUX {
        let dir = scratch.path().join(platform.name());
        let unpacked = unpack(&dir, &package(platform), platform).expect("unpacked");
        let held = std::fs::read(unpacked.dir.join(BINARY)).expect("the unpacked binary");
        let version = unpacked.manifest.version.as_str().as_bytes();
        assert!(held.windows(version.len()).any(|w| w == version));
    }
    let mac = package(Platform::MacosArm64);
    let version = manifest_toml(&mac)["version"]
        .as_str()
        .expect("a string")
        .to_owned();
    assert_eq!(version, VERSION);
    let held = crate::support::file(&mac, BINARY);
    assert!(held.windows(version.len()).any(|w| w == version.as_bytes()));
    // A version the binary does not hold is refused, and nothing is written.
    let bin = scratch.path().join("tierstore");
    std::fs::write(&bin, binary()).expect("binary");
    let out = scratch.path().join("out");
    for platform in Platform::ALL {
        let err = hostkeeper_package(&bin, "0.4.0+fedcba987654", MIN_HOSTKEEPER, platform, &out)
            .unwrap_err();
        assert!(matches!(err, Error::Failed(_)), "{err:?}");
    }
    assert!(!out.exists());
}

#[test]
fn the_loader_refuses_an_install_bound_or_an_unknown_key_and_the_package_has_neither() {
    // Catches: a manifest with an `install` bound (hostkeeper has no install step) or any key
    // the loader does not know: the loader refuses each, so the package must hold none. The
    // unedited manifest loading is the control that the edits are what is refused.
    let scratch = Scratch::new("hostkeeper-package-keys");
    for platform in LINUX {
        let dir = scratch.path().join(platform.name());
        let loaded = load_edited(&dir.join("as-built"), platform, |text| text);
        assert_eq!(
            loaded.expect("the manifest loads").version.as_str(),
            VERSION
        );
        let install = load_edited(&dir.join("install"), platform, |text| {
            text.replace(
                "[hook.timeout_secs]\n",
                "[hook.timeout_secs]\ninstall = 300\n",
            )
        });
        assert_eq!(
            install.unwrap_err(),
            ManifestRefusal::UnknownKey {
                table: "hook.timeout_secs".to_owned(),
                key: "install".to_owned(),
            }
        );
        let unknown = load_edited(&dir.join("unknown"), platform, |text| {
            format!("user = \"root\"\n{text}")
        });
        assert_eq!(
            unknown.unwrap_err(),
            ManifestRefusal::UnknownKey {
                table: String::new(),
                key: "user".to_owned(),
            }
        );
        let toml = manifest_toml(&package(platform));
        let steps: Vec<&str> = toml["hook"]["timeout_secs"]
            .as_table()
            .expect("a table")
            .keys()
            .map(String::as_str)
            .collect();
        let mut bounds = [
            "cleanup",
            "cutover",
            "prepare",
            "rollback",
            "uninstall",
            "verify",
        ];
        bounds.sort_unstable();
        assert_eq!(steps, bounds);
    }
}

#[test]
fn the_unit_runs_the_binary_of_current_as_tierstore_and_its_stop_covers_the_drain() {
    // Catches: an ExecStart through a version directory or another path (the unit would not
    // follow the switch of `current`), a configuration file other than the manifest's
    // `config_file`, a stop timeout or cleanup bound shorter than the drain (systemd would kill a
    // drain in progress), a unit that runs as root, a run unit hostkeeper cannot enable (no
    // WantedBy=), and instances not gated on their marker.
    for platform in LINUX {
        let archive = package(platform);
        let unit = text(&archive, UNIT);
        let exec = unit_value(&unit, "[Service]", "ExecStart");
        let words: Vec<&str> = exec.split_whitespace().collect();
        assert_eq!(words[0], CURRENT_BINARY);
        assert!(CURRENT_BINARY.ends_with(&format!("/current/{BINARY}")));
        assert!(through_current(Path::new(PACKAGES), "tierstore", words[0]));
        let toml = manifest_toml(&archive);
        assert_eq!(
            flag(&words, "--config"),
            toml["params"]["config_file"].as_str().expect("a string")
        );
        assert_eq!(unit_value(&unit, "[Service]", "User"), "tierstore");
        assert_eq!(unit_value(&unit, "[Service]", "Restart"), "on-failure");
        let lease = 5;
        let drain = seconds(flag(&words, "--drain-lb-wait"))
            + seconds(flag(&words, "--drain-deadline"))
            + lease;
        let stop: u64 = unit_value(&unit, "[Service]", "TimeoutStopSec")
            .parse()
            .expect("seconds");
        assert!(stop > drain, "TimeoutStopSec {stop} s, drain {drain} s");
        let cleanup = toml["hook"]["timeout_secs"]["cleanup"]
            .as_integer()
            .expect("an integer");
        assert!(u64::try_from(cleanup).expect("positive") > stop);
        assert_eq!(wanted_by(&unit), ["multi-user.target"]);
        assert_eq!(
            unit_value(&unit, "[Unit]", "ConditionPathExists"),
            "/var/lib/tierstore/start-%i"
        );
        assert_eq!(
            check_unit_text(
                Path::new(PACKAGES),
                "tierstore",
                "tierstore@.service",
                &unit
            ),
            Ok(())
        );
    }
}

/// The placed-path lint of hostkeeper's main on `platform`'s package, through `edit` (of the
/// unit's text) first, in a host tree below `root`. Main's loader predates the manifest's
/// `platform` key, so that one line is left out for it.
fn placed_lint(
    root: &Path,
    platform: Platform,
    edit: impl Fn(String) -> String,
) -> Result<(), LintError> {
    let version = version_dir(root);
    std::fs::create_dir_all(&version).expect("the version directory");
    let utf8 = |bytes: Vec<u8>| String::from_utf8(bytes).expect("UTF-8");
    write_tree(&version, &package(platform), |name, bytes| match name {
        "manifest.toml" => utf8(bytes)
            .lines()
            .filter(|line| !line.starts_with("platform "))
            .map(|line| format!("{line}\n"))
            .collect::<String>()
            .into_bytes(),
        UNIT => edit(utf8(bytes)).into_bytes(),
        _ => bytes,
    });
    let manifest = hostkeeper_config_main::manifest::Manifest::load(
        &version,
        &hostkeeper_config_main::manifest::Expected {
            name: hostkeeper_config_main::names::ServiceName::new("tierstore").expect("a name"),
            version: hostkeeper_config_main::names::Version::new(VERSION).expect("a version"),
        },
        &hostkeeper_config_main::manifest::HostkeeperVersion::parse(MIN_HOSTKEEPER)
            .expect("a version"),
    )
    .expect("main's loader takes the manifest without its platform");
    for unit in &manifest.run {
        let name = hostkeeper_config_main::names::UnitName::of("tierstore", unit, false)
            .expect("a unit of tierstore");
        let template = name.template().expect("an instance of a template");
        assert!(
            manifest.unit_files.contains(&template),
            "{unit}: {template}"
        );
    }
    lint(&RealFs::new(root), &Roots::linux(), &manifest)
}

#[test]
fn each_run_entry_is_an_instance_of_the_linked_template_and_the_units_pass_the_placed_lint() {
    // Catches: a run entry that is not an instance of a unit file of the package, a unit that
    // names a package file other than through `current`, and a run unit with no WantedBy=. The
    // two edited units, refused, are the control that the lint reads the package's units.
    let scratch = Scratch::new("hostkeeper-package-placed");
    for platform in LINUX {
        let dir = scratch.path().join(platform.name());
        assert_eq!(
            placed_lint(&dir.join("as-built"), platform, |unit| unit),
            Ok(())
        );
        let pinned = placed_lint(&dir.join("pinned"), platform, |unit| {
            unit.replace("/current/bin/", &format!("/{VERSION}/bin/"))
        });
        assert!(
            matches!(&pinned, Err(LintError::PackagePath { key, .. }) if key == "ExecStart="),
            "{pinned:?}"
        );
        let unwanted = placed_lint(&dir.join("unwanted"), platform, |unit| {
            unit.replace("WantedBy=multi-user.target", "")
        });
        assert_eq!(
            unwanted,
            Err(LintError::NoWantedBy {
                unit: "tierstore@a.service".to_owned(),
                file: "tierstore@.service".to_owned(),
            })
        );
    }
}
