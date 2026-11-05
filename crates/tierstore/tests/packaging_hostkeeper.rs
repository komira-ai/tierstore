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
use tierstore::server::cli::Args;
use tierstore::server::update_hook::{Instance, LAUNCHD_PLIST, RELEASE_BINARY, marker_path};

/// The words in the name of a flag that say it holds a secret.
const SECRET_WORDS: [&str; 5] = ["key", "secret", "credential", "token", "password"];

/// The release link of hostkeeper's package, which both files run the binary of.
const CURRENT: &str = "/var/lib/hostkeeper/packages/tierstore/current";

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
    assert_eq!(args.drain_lb_wait, Duration::from_secs(10));
    assert_eq!(args.drain_deadline, Duration::from_secs(15 * 60));
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
    let args = Args::try_parse_from(exec_start(&unit)).expect("the template's flags");
    assert_eq!(value(&unit, "KillSignal"), "SIGTERM");
    let seconds: u64 = value(&unit, "TimeoutStopSec").parse().expect("seconds");
    let drain = args.drain_lb_wait + args.drain_deadline + Duration::from_secs(5);
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
    assert_eq!(text(&dict, "Label"), "com.komira.tierstore");
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
    let Some(Plist::Integer(seconds)) = plist().get("ExitTimeOut").cloned() else {
        panic!("ExitTimeOut is an integer");
    };
    let exit = Duration::from_secs(seconds.try_into().unwrap());
    let drain = args.drain_lb_wait + args.drain_deadline;
    assert!(exit >= drain, "{exit:?} is less than {drain:?}");
}
