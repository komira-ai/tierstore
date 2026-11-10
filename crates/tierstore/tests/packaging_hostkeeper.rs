//! The files hostkeeper's package of tierstore installs (`packaging/hostkeeper/units`) against
//! the binary and the update hook: the systemd template `tierstore@.service` runs the binary of the
//! release link `current` with flags the binary parses, starts only under the marker file the hook
//! writes (`marker_path`), is enabled at boot, and waits for the drain; the launchd plist
//! `tierstore.plist` runs the same binary as `_tierstore`, is installed under the name the hook
//! bootstraps (`LAUNCHD_PLIST`), and lets launchd wait for the drain.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use tierstore::server::cli::{Args, Cli};
use tierstore::server::update_hook::{Instance, LAUNCHD_PLIST, RELEASE_BINARY, marker_path};

/// The words in the name of a flag that say it holds a secret.
const SECRET_WORDS: [&str; 5] = ["key", "secret", "credential", "token", "password"];

/// The release link of hostkeeper's package, which both files run the binary of.
const CURRENT: &str = "/var/lib/hostkeeper/packages/tierstore/current";

/// The drain of a process that is not on macOS and is given no part of it: the defaults of
/// `server.drain_lb_wait` and `server.drain_deadline`, which the xtask test of the package holds
/// the template's `TimeoutStopSec=` to (`LINUX_DRAIN_SECS` there).
const LINUX_DRAIN: (Duration, Duration) = (Duration::from_secs(10), Duration::from_secs(15 * 60));

/// The drain of a process on macOS that is given no part of it, which the plist's `ExitTimeOut` is
/// above (`MACOS_DRAIN_SECS` in the xtask test of the package).
const MACOS_DRAIN: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(60));

/// The settings of the example storage node's file, whose drain is the defaults.
fn defaults() -> Cli {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/tierstore.toml");
    let text = std::fs::read_to_string(file).expect("the example file");
    Cli::from_config_text(&text).expect("the example file loads")
}

/// The systemd template.
const TEMPLATE: &str = "tierstore@.service";

/// The directory of the files of the package.
fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/hostkeeper/units")
}

/// The text of a file of the directory.
fn read(file: &str) -> String {
    std::fs::read_to_string(dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"))
}

/// The values of the `key=` lines of a unit, its `\` continuations joined.
fn values(unit: &str, key: &str) -> Vec<String> {
    let joined = unit.replace("\\\n", " ");
    let prefix = format!("{key}=");
    joined
        .lines()
        .filter_map(|line| line.strip_prefix(prefix.as_str()))
        .map(str::to_owned)
        .collect()
}

/// The value of the one `key=` line of a unit.
fn value(unit: &str, key: &str) -> String {
    let mut found = values(unit, key);
    assert_eq!(found.len(), 1, "{key} appears once: {found:?}");
    found.remove(0)
}

/// The words of the template's `ExecStart=`.
fn exec_start(unit: &str) -> Vec<String> {
    value(unit, "ExecStart")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// A value of a property list.
#[derive(Clone, Debug, PartialEq)]
enum Plist {
    Text(String),
    Integer(i64),
    Bool(bool),
    Array(Vec<Plist>),
    Dict(BTreeMap<String, Plist>),
}

/// The value an XML element of a property list stands for.
fn plist_value(node: roxmltree::Node<'_, '_>) -> Plist {
    let text = || node.text().unwrap_or_default().to_owned();
    match node.tag_name().name() {
        "string" => Plist::Text(text()),
        "integer" => Plist::Integer(text().parse().expect("an integer")),
        "true" => Plist::Bool(true),
        "false" => Plist::Bool(false),
        "array" => Plist::Array(
            node.children()
                .filter(|n| n.is_element())
                .map(plist_value)
                .collect(),
        ),
        "dict" => {
            let items: Vec<_> = node.children().filter(|n| n.is_element()).collect();
            let mut dict = BTreeMap::new();
            for pair in items.chunks(2) {
                assert_eq!(pair[0].tag_name().name(), "key", "{pair:?}");
                let key = pair[0].text().unwrap_or_default().to_owned();
                assert!(
                    dict.insert(key.clone(), plist_value(pair[1])).is_none(),
                    "{key} twice"
                );
            }
            Plist::Dict(dict)
        }
        other => panic!("<{other}> is not a value of a property list"),
    }
}

/// The plist's top-level dictionary.
fn plist() -> BTreeMap<String, Plist> {
    let text = read("tierstore.plist");
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(&text, options).expect("XML");
    let root = doc.root_element();
    assert_eq!(root.tag_name().name(), "plist");
    let mut top = root.children().filter(|n| n.is_element());
    let Some(Plist::Dict(dict)) = top.next().map(plist_value) else {
        panic!("the plist holds one dictionary");
    };
    assert!(top.next().is_none());
    dict
}

/// The string the plist gives `key`.
fn text<'a>(dict: &'a BTreeMap<String, Plist>, key: &str) -> &'a str {
    match dict.get(key) {
        Some(Plist::Text(text)) => text,
        other => panic!("{key} is {other:?}"),
    }
}

/// The plist's `ProgramArguments`.
fn arguments() -> Vec<String> {
    match plist().get("ProgramArguments") {
        Some(Plist::Array(words)) => words
            .iter()
            .map(|word| match word {
                Plist::Text(text) => text.clone(),
                other => panic!("an argument is {other:?}"),
            })
            .collect(),
        other => panic!("ProgramArguments is {other:?}"),
    }
}

/// Asserts that the template runs `bin/tierstore` of the release link `current` of hostkeeper's
/// package (the path the hook's `RELEASE_BINARY` names under a release), and that the flags parse
/// as the binary parses them: `--config` is a file under `/etc/tierstore`, and the two flags of
/// the drain are given. It would catch a template that runs a binary no release provides, or a
/// flag the binary refuses, which would leave the instance restarting for ever.
#[test]
fn the_template_runs_the_current_release_with_flags_the_binary_parses() {
    let unit = read(TEMPLATE);
    let words = exec_start(&unit);
    assert_eq!(words[0], format!("{CURRENT}/{RELEASE_BINARY}"));
    let args = Args::try_parse_from(&words).expect("the template's flags");
    assert_eq!(
        args.config.as_deref(),
        Some(Path::new("/etc/tierstore/config.toml"))
    );
    assert_eq!(args.drain_lb_wait, None);
    assert_eq!(args.drain_deadline, None);
}

/// Asserts that a process on Linux (or anywhere but macOS) with no drain key waits 10 s for the
/// load balancers and 15 min for the requests in flight, which is the drain the template's
/// `TimeoutStopSec=` is sized for. It would catch a default raised past the stop timeout, which
/// would let systemd kill a drain in progress.
#[cfg(not(target_os = "macos"))]
#[test]
fn the_units_stop_timeouts_cover_the_drain_the_defaults_make() {
    let cli = defaults();
    assert_eq!((cli.drain_lb_wait, cli.drain_deadline), LINUX_DRAIN);
}

/// Asserts that a process on macOS with no drain key waits 1 s and 60 s, the drain the plist's
/// `ExitTimeOut` is sized for (a loopback service has no load balancer to wait for). It would
/// catch a macOS default raised past `ExitTimeOut`.
#[cfg(target_os = "macos")]
#[test]
fn the_macos_defaults_are_the_drain_the_plist_waits_for() {
    let cli = defaults();
    assert_eq!((cli.drain_lb_wait, cli.drain_deadline), MACOS_DRAIN);
}

/// Asserts that an instance starts only under its marker file: the template's
/// `ConditionPathExists=` is `/var/lib/tierstore/run-%i`, and with `%i` replaced by each
/// instance's letter it is the path the hook writes (`marker_path`). It would catch a template
/// and a hook that disagree about the marker, which would start the wrong instance at boot or
/// none.
#[test]
fn the_template_starts_only_under_the_marker_the_hook_writes() {
    let unit = read(TEMPLATE);
    let condition = value(&unit, "ConditionPathExists");
    assert_eq!(condition, "/var/lib/tierstore/run-%i");
    for instance in Instance::ALL {
        assert_eq!(
            PathBuf::from(condition.replace("%i", instance.letter())),
            marker_path(instance)
        );
    }
}

/// Asserts that the template is enabled at boot (`WantedBy=multi-user.target`, the only
/// `[Install]` key), that the state directory it makes is `tierstore`, private to the account, and
/// that the unit runs as an account that is not root. The marker and the hand-over socket live in
/// that directory.
#[test]
fn the_template_is_enabled_at_boot_with_a_private_state_directory() {
    let unit = read(TEMPLATE);
    assert_eq!(value(&unit, "WantedBy"), "multi-user.target");
    for key in ["RequiredBy", "Alias"] {
        assert_eq!(values(&unit, key), Vec::<String>::new(), "{key}");
    }
    assert_eq!(value(&unit, "StateDirectory"), "tierstore");
    assert_eq!(value(&unit, "StateDirectoryMode"), "0700");
    assert_eq!(value(&unit, "UMask"), "0077");
    assert_ne!(value(&unit, "User"), "root");
}

/// Asserts that systemd stops an instance with SIGTERM after a timeout no shorter than the wait of
/// the load balancers plus the deadline of the drain plus the 5 s the reconciler's lease release
/// may take; that a failed start is tried again, except for the exit statuses 2 and 3, which a
/// restart cannot mend; and that an exit 0, the end of a drain after a hand-over, is not a failure
/// (`Restart=on-failure`). It would catch a SIGKILL of a drain in progress, and a drained
/// instance that systemd starts again.
#[test]
fn the_template_waits_for_the_drain_and_does_not_restart_a_clean_exit() {
    let unit = read(TEMPLATE);
    let cli = defaults();
    assert_eq!(value(&unit, "KillSignal"), "SIGTERM");
    let seconds: u64 = value(&unit, "TimeoutStopSec").parse().expect("seconds");
    let drain = cli.drain_lb_wait + cli.drain_deadline + Duration::from_secs(5);
    let timeout = Duration::from_secs(seconds);
    assert!(timeout >= drain, "{timeout:?} is less than {drain:?}");
    assert_eq!(value(&unit, "Restart"), "on-failure");
    assert_eq!(value(&unit, "RestartPreventExitStatus"), "2 3");
    assert_eq!(value(&unit, "RestartSec"), "30");
    assert_eq!(value(&unit, "StartLimitIntervalSec"), "1h");
    assert_eq!(value(&unit, "StartLimitBurst"), "5");
}

/// Asserts that no secret is on the command line of the template or in its environment: no flag's
/// name speaks of a key, a secret, a credential, a token or a password, and the unit sets no
/// environment variable and reads no environment file.
#[test]
fn no_secret_is_on_the_templates_command_line_or_in_its_environment() {
    let unit = read(TEMPLATE);
    for flag in exec_start(&unit)
        .iter()
        .filter(|word| word.starts_with("--"))
    {
        assert!(
            !SECRET_WORDS.iter().any(|secret| flag.contains(secret)),
            "{flag}"
        );
    }
    let environment = unit.lines().filter(|line| line.starts_with("Environment"));
    assert_eq!(environment.count(), 0);
}

/// Asserts that the template keeps the hardening of `tierstore.service`: no new privileges, a
/// read-only file system, a private `/tmp`, no capability, and a system call filter.
#[test]
fn the_template_is_hardened() {
    let unit = read(TEMPLATE);
    let wanted = [
        ("NoNewPrivileges", "yes"),
        ("ProtectSystem", "strict"),
        ("ProtectHome", "yes"),
        ("PrivateTmp", "yes"),
        ("PrivateDevices", "yes"),
        ("CapabilityBoundingSet", ""),
        ("SystemCallFilter", "@system-service"),
    ];
    for (key, want) in wanted {
        assert_eq!(value(&unit, key), want, "{key}");
    }
}

/// Asserts that the plist is a LaunchDaemon of the one job the hook boots out and bootstraps: it
/// is a file named as the hook's `LAUNCHD_PLIST` names it, its program is the binary of the
/// release link `current`, its flags parse as the binary parses them, it runs as `_tierstore`, and
/// launchd starts it at load and keeps it alive. It would catch a plist the hook cannot find by
/// name, one that runs a binary no release provides, and one that launchd does not restart.
#[test]
fn the_plist_runs_the_current_release_as_the_service_account_kept_alive() {
    let installed = Path::new(LAUNCHD_PLIST).file_name().unwrap();
    assert_eq!(installed, "tierstore.plist");
    assert!(dir().join(installed).is_file());
    let dict = plist();
    assert_eq!(text(&dict, "Label"), "tierstore");
    assert_eq!(text(&dict, "UserName"), "_tierstore");
    assert_eq!(text(&dict, "GroupName"), "_tierstore");
    assert_eq!(dict.get("RunAtLoad"), Some(&Plist::Bool(true)));
    assert_eq!(dict.get("KeepAlive"), Some(&Plist::Bool(true)));
    let words = arguments();
    assert_eq!(words[0], format!("{CURRENT}/{RELEASE_BINARY}"));
    let args = Args::try_parse_from(&words).expect("the plist's flags");
    assert!(args.config.is_some_and(|file| file.is_absolute()));
    for key in dict.keys() {
        assert!(!key.contains("Environment"), "{key}");
    }
}

/// Asserts that launchd lets the drain finish before it kills the process: `ExitTimeOut` is at
/// least the plist's `--drain-lb-wait` plus `--drain-deadline`. It would catch a SIGKILL of a
/// drain in progress.
#[test]
fn launchd_waits_for_the_drain() {
    let args = Args::try_parse_from(arguments()).expect("the plist's flags");
    assert_eq!(args.drain_lb_wait, None, "the drain is the file's keys");
    assert_eq!(args.drain_deadline, None, "the drain is the file's keys");
    let Some(Plist::Integer(seconds)) = plist().get("ExitTimeOut").cloned() else {
        panic!("ExitTimeOut is an integer");
    };
    let exit = Duration::from_secs(seconds.try_into().unwrap());
    let drain = MACOS_DRAIN.0 + MACOS_DRAIN.1;
    assert!(exit >= drain, "{exit:?} is less than {drain:?}");
}

/// The systemd template of `tierstore fs`.
const FS_TEMPLATE: &str = "tierstore-fs@.service";

/// The options of `tierstore@.service` that give a service a mount namespace of its own, from which
/// a mount does not reach the host (`docs/design/virtual-build-fs.md` section 6.2): the unit of
/// `tierstore fs` leaves each out.
const LEFT_OUT: [&str; 9] = [
    "PrivateDevices",
    "PrivateTmp",
    "ProtectSystem",
    "ProtectHome",
    "ProtectKernelTunables",
    "ProtectKernelModules",
    "ProtectKernelLogs",
    "ProtectControlGroups",
    "RestrictNamespaces",
];

/// Asserts the diff of the unit of `tierstore fs` against `tierstore@.service`, as the design lists
/// it: each option that gives a mount namespace of its own is in the S3 unit and not in this one;
/// the mount's device, capability and system calls, the notify type, the start limit and the
/// runtime directory are added with their values; the address families are the Unix ones alone;
/// and the user, the privilege, memory and umask settings, the restart settings and the release
/// link are the S3 unit's. It would catch an option left in that makes the mount invisible to the
/// job runtime, and a capability the unit does not need.
#[test]
fn the_fs_template_differs_from_the_s3_template_as_the_design_lists() {
    let (s3, fs) = (read(TEMPLATE), read(FS_TEMPLATE));
    for key in LEFT_OUT {
        assert_eq!(values(&s3, key).len(), 1, "the S3 unit has {key}");
        assert!(
            values(&fs, key).is_empty(),
            "the unit of tierstore fs has {key}"
        );
    }
    let added = [
        ("DeviceAllow", "/dev/fuse rw"),
        ("CapabilityBoundingSet", "CAP_SYS_ADMIN"),
        ("AmbientCapabilities", "CAP_SYS_ADMIN"),
        ("SystemCallFilter", "@system-service @mount"),
        ("Type", "notify"),
        ("NotifyAccess", "main"),
        ("StartLimitIntervalSec", "60"),
        ("StartLimitBurst", "5"),
        ("RuntimeDirectory", "tierstore-fs tierstore"),
        ("RuntimeDirectoryPreserve", "yes"),
        ("RestrictAddressFamilies", "AF_UNIX"),
    ];
    for (key, want) in added {
        assert_eq!(value(&fs, key), want, "{key}");
    }
    assert_ne!(value(&s3, "RestrictAddressFamilies"), "AF_UNIX");
    for key in [
        "User",
        "NoNewPrivileges",
        "MemoryDenyWriteExecute",
        "UMask",
        "Restart",
        "RestartPreventExitStatus",
        "KillSignal",
        "SystemCallArchitectures",
        "LockPersonality",
        "RestrictRealtime",
        "LimitNOFILE",
    ] {
        assert_eq!(value(&fs, key), value(&s3, key), "{key}");
    }
}

/// Asserts that the unit of `tierstore fs` runs the `fs` verb of the binary of the release link with
/// a `--config` the binary reads (a file that is not there is status 2, naming it, which a flag the
/// binary did not parse would not be), starts only under a marker of its own after the S3
/// instances, and is enabled at boot. It would catch a verb or a flag that is renamed under the
/// unit, which would leave the instance failing at every start.
#[test]
fn the_fs_template_runs_the_fs_verb_of_the_current_release() {
    let unit = read(FS_TEMPLATE);
    let words = exec_start(&unit);
    assert_eq!(words[0], format!("{CURRENT}/{RELEASE_BINARY}"));
    assert_eq!(words[1..], ["fs", "--config", "/etc/tierstore/fs.toml"]);
    assert_eq!(
        value(&unit, "ConditionPathExists"),
        "/var/lib/tierstore/run-fs-%i"
    );
    assert_eq!(
        value(&unit, "After"),
        "tierstore@a.service tierstore@b.service"
    );
    assert_eq!(value(&unit, "WantedBy"), "multi-user.target");
    let scratch = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tfs-unit");
    let missing = scratch.path().join("fs.toml");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .args(["fs", "--config"])
        .arg(&missing)
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("fs.toml"), "{stderr}");
}

/// Asserts that the marker of the unit of `tierstore fs` is not one of the markers the update hook
/// writes for the S3 instances, so the hook cannot start or retire a mount it does not know.
#[test]
fn the_fs_marker_is_not_a_marker_of_the_s3_instances() {
    let unit = read(FS_TEMPLATE);
    let marker = value(&unit, "ConditionPathExists");
    for instance in [Instance::A, Instance::B] {
        let theirs = marker_path(instance);
        let ours = marker.replace("%i", instance.letter());
        assert_ne!(Path::new(&ours), theirs.as_path());
    }
}
