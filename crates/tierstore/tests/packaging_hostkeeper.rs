//! The files hostkeeper's package of tierstore installs (`packaging/hostkeeper/units`) against
//! the binary and the update hook: the systemd template `tierstore@.service` runs the copy of the
//! release's binary the hook puts in place (`SERVICE_BINARY`, outside hostkeeper's package
//! directories, which only root may enter) with flags the binary parses, starts only under the
//! marker file the hook writes (`marker_path`), is enabled at boot, and waits for the drain; the
//! launchd plist `tierstore.plist` runs the same binary as `_tierstore`, is installed under the name the hook
//! bootstraps (`LAUNCHD_PLIST`), and lets launchd wait for the drain. The template the deb and rpm
//! packages install (`packaging/tierstore@.service`) is held to this one but for the binary, which
//! is the package's, at the path the hook finds in a staged release of the package.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use tierstore::server::cli::{Args, Cli};
#[path = "packaging/unit_checks.rs"]
mod unit_checks;

use unit_checks::{
    MOUNT_NAMESPACE_OPTIONS, grants_cap_sys_admin_alone, mount_namespace_options, values,
    with_service_line,
};

use tierstore::server::update_hook::{
    Instance, LAUNCHD_PLIST, PACKAGE_RELEASE_BINARY, POLL_INTERVAL, PROBE_ATTEMPTS, READY_ATTEMPTS,
    SERVICE_BINARY, marker_path,
};

/// The words in the name of a flag that say it holds a secret.
const SECRET_WORDS: [&str; 5] = ["key", "secret", "credential", "token", "password"];

/// The directory hostkeeper keeps its packages in, which only root may enter: no unit runs a
/// program under it, since every unit runs as an account of its own.
const PACKAGES: &str = "/var/lib/hostkeeper/packages/";

/// The drain of a process that is not on macOS and is given no part of it: the defaults of
/// `server.drain_lb_wait` and `server.drain_deadline`, which the xtask test of the package holds
/// the template's `TimeoutStopSec=` to (`LINUX_DRAIN_SECS` there).
#[cfg(not(target_os = "macos"))]
const LINUX_DRAIN: (Duration, Duration) = (Duration::from_secs(10), Duration::from_secs(15 * 60));

/// The drain of a process on macOS that is given no part of it, which the plist's `ExitTimeOut` is
/// above (`MACOS_DRAIN_SECS` in the xtask test of the package).
const MACOS_DRAIN: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(60));

/// The settings of the example storage node's file, whose drain is the defaults.
fn defaults() -> Cli {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/config.toml");
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
    plist_of("tierstore.plist")
}

/// The top-level dictionary of the plist `file`.
fn plist_of(file: &str) -> BTreeMap<String, Plist> {
    let text = read(file);
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

/// Asserts that the template runs the copy the hook puts at `SERVICE_BINARY` (not the binary
/// inside hostkeeper's package, which its account `tierstore` cannot reach), and that the flags
/// parse as the binary parses them: `--config` is a file under `/etc/tierstore`, and the two flags
/// of the drain are given. It would catch a template that runs a binary its account cannot run,
/// or a flag the binary refuses, either of which would leave the instance failing at every start.
#[test]
fn the_template_runs_the_installed_binary_with_flags_the_binary_parses() {
    let unit = read(TEMPLATE);
    let words = exec_start(&unit);
    assert_eq!(words[0], SERVICE_BINARY);
    assert_eq!(value(&unit, "User"), "tierstore");
    let args = Args::try_parse_from(&words).expect("the template's flags");
    assert_eq!(
        args.config.as_deref(),
        Some(Path::new("/etc/tierstore/config.toml"))
    );
    assert_eq!(args.drain_lb_wait, None);
    assert_eq!(args.drain_deadline, None);
}

/// The example file `packaging/config.toml` copied into `scratch`, with each file it names under
/// `/etc/tierstore` (the key files) made in `scratch` (a key of the form the process reads, mode
/// 0600, each with an access key id of its own) and named there instead; the path of the copy.
fn example_file_in(scratch: &Path) -> PathBuf {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/config.toml");
    let mut text = std::fs::read_to_string(example).expect("the example file");
    let names = [
        "access.key",
        "daemons.key",
        "hot.credentials",
        "cold.credentials",
    ];
    for (index, name) in names.into_iter().enumerate() {
        let on_host = format!("/etc/tierstore/{name}");
        assert!(text.contains(&on_host), "{on_host}");
        let key = scratch.join(name);
        std::fs::write(&key, format!("KEYID{index}\nsecret-{name}-0123456789\n")).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        text = text.replace(&on_host, &key.display().to_string());
    }
    let left: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#') && line.contains("/etc/tierstore/"))
        .collect();
    assert_eq!(left, Vec::<&str>::new(), "every file it names is made");
    let file = scratch.join("config.toml");
    std::fs::write(&file, text).unwrap();
    file
}

/// Asserts that the binary itself takes the command line of both templates (hostkeeper's and the
/// deb and rpm packages') and the example file: run with the words of `ExecStart=` after the
/// program, the file `--config` names replaced by a copy of `packaging/config.toml` with its key
/// files, and `--check-config` (which reads the file and the key files and binds nothing), it
/// exits 0 and says nothing. And that a word of the command line the binary does not take fails
/// the same run (status 2, naming it). It would catch a flag or a file the binary refuses, with
/// which every start of an instance fails and the unit gives up after 5 starts in 60 s.
#[test]
fn the_binary_accepts_the_units_command_line_and_the_example_file() {
    let scratch = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "unit-check");
    let file = example_file_in(scratch.path());
    let check = |words: &[String]| {
        let args: Vec<String> = words[1..]
            .iter()
            .map(|word| match word.as_str() {
                "/etc/tierstore/config.toml" => file.display().to_string(),
                other => other.to_owned(),
            })
            .chain(["--check-config".to_owned()])
            .collect();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_tierstore"))
            .args(&args)
            .output()
            .expect("the binary runs");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    for (name, unit) in [(TEMPLATE, read(TEMPLATE)), ("package", package_template())] {
        let words = exec_start(&unit);
        assert!(
            words.contains(&"/etc/tierstore/config.toml".to_owned()),
            "{name}"
        );
        assert_eq!(
            check(&words),
            (Some(0), String::new(), String::new()),
            "{name}"
        );
    }
    let mut wrong = exec_start(&read(TEMPLATE));
    wrong.push("--no-such-flag".to_owned());
    let (code, stdout, stderr) = check(&wrong);
    assert_eq!((code, stdout), (Some(2), String::new()));
    assert!(stderr.contains("--no-such-flag"), "{stderr}");
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
/// may take; that a failed start is tried again after 2 s, except for the exit statuses 2 and 3,
/// which a restart cannot mend, and given up after 5 starts in 60 s (the file system's values: a
/// mount whose server is down holds its jobs' requests, so it comes back fast, and one that cannot
/// come back lets systemd drop the descriptor store); and that an exit 0, the end of a drain after
/// a hand-over, is not a failure (`Restart=on-failure`). It would catch a SIGKILL of a drain in
/// progress, a drained instance that systemd starts again, and the S3 unit's 30 s and 5 in an hour
/// left in the one unit.
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
    assert_eq!(value(&unit, "RestartSec"), "2");
    assert_eq!(value(&unit, "StartLimitIntervalSec"), "60");
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

/// Asserts the hardening the template keeps without a mount namespace (docs/design/one-service.md
/// section 2.2): no new privileges, a device filter that allows the standard pseudo devices and
/// `/dev/fuse` alone, no namespace the process may make, the network families of its fronts, no
/// real-time scheduling, no change of personality, no writable and executable memory, the native
/// system calls of a service and of a mount alone, and a umask that keeps its files its own.
#[test]
fn the_template_is_hardened() {
    let unit = read(TEMPLATE);
    let wanted = [
        ("NoNewPrivileges", "yes"),
        ("DevicePolicy", "closed"),
        ("DeviceAllow", "/dev/fuse rw"),
        ("RestrictNamespaces", "yes"),
        ("RestrictAddressFamilies", "AF_UNIX AF_INET AF_INET6"),
        ("RestrictRealtime", "yes"),
        ("LockPersonality", "yes"),
        ("MemoryDenyWriteExecute", "yes"),
        ("SystemCallArchitectures", "native"),
        (
            "SystemCallFilter",
            "@system-service @mount pidfd_open pidfd_getfd",
        ),
        ("UMask", "0077"),
    ];
    for (key, want) in wanted {
        assert_eq!(value(&unit, key), want, "{key}");
    }
}

/// Asserts that the plist is a LaunchDaemon of the one job the hook boots out and bootstraps: it
/// is a file named as the hook's `LAUNCHD_PLIST` names it, its program is the copy the hook puts at
/// `SERVICE_BINARY`, its flags parse as the binary parses them, it runs as `_tierstore`, and
/// launchd starts it at load and keeps it alive. It would catch a plist the hook cannot find by
/// name, one that runs a binary `_tierstore` cannot reach, and one that launchd does not restart.
#[test]
fn the_plist_runs_the_installed_binary_as_the_service_account_kept_alive() {
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
    assert_eq!(words[0], SERVICE_BINARY);
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

/// The check of the marker of the unit of `tierstore fs`, the one command of it that runs as root.
const FS_MARKER_CHECK: &str = "ExecCondition=+/usr/bin/test -e /var/lib/tierstore/run-fs-%i\n";

/// Every systemd unit that makes or will make the node's FUSE mount: the template of hostkeeper's
/// package, the template of the deb and rpm packages, and the unit of `tierstore fs`.
fn mounting_units() -> [(&'static str, String); 3] {
    [
        (TEMPLATE, read(TEMPLATE)),
        ("packaging/tierstore@.service", package_template()),
        (FS_TEMPLATE, read(FS_TEMPLATE)),
    ]
}

/// Asserts that no unit that makes the mount sets an option that gives the service a mount
/// namespace of its own (docs/design/one-service.md section 2.2: the table's options and the path
/// options), with any value: the template of hostkeeper's package, the template of the deb and rpm
/// packages and the unit of `tierstore fs` set none. And that the check finds each of them: a copy
/// of the template with `ProtectSystem=strict` added fails it, as does a copy with any other of
/// the options. It would catch an option put back in the template, with which the mount the
/// process makes stays in the service's own namespace and the job runtime and the next instance
/// never see it.
#[test]
fn the_unit_sets_no_option_that_makes_a_mount_namespace() {
    for (name, unit) in mounting_units() {
        assert_eq!(
            mount_namespace_options(&unit),
            Vec::<String>::new(),
            "{name}"
        );
    }
    let template = read(TEMPLATE);
    assert_eq!(
        mount_namespace_options(&with_service_line(&template, "ProtectSystem=strict")),
        ["ProtectSystem"]
    );
    for option in MOUNT_NAMESPACE_OPTIONS {
        let copy = with_service_line(&template, &format!("{option}=yes"));
        assert_eq!(mount_namespace_options(&copy), [option], "{option}");
    }
    let commented = with_service_line(&template, "#ProtectSystem=strict");
    assert_eq!(mount_namespace_options(&commented), Vec::<String>::new());
}

/// Asserts that every unit that makes the mount grants `CAP_SYS_ADMIN`, the capability of the
/// mount (docs/design/one-service.md section 2.1), and no other: the bounding and the ambient set
/// are that one capability, as an account that is not root that gains no privilege by `execve`.
/// And that the check finds each way to grant more: a second capability on the line, a second
/// line that adds one, a bounding set of every capability but some, an ambient set left empty or
/// left out, a service run as root or that may gain privileges, and a command run with raised
/// privileges (`+`, `!`, `!!`, alone or after other prefixes), but not a `+` in an argument. It
/// would catch a capability added to the template beyond the one the mount needs, and the
/// capability dropped, with which the process cannot mount.
///
/// The unit of `tierstore fs` has one command that runs with raised privileges: the check of its
/// marker, `test -e` as root, so that the marker is seen whoever owns `/var/lib/tierstore` (the
/// unit's own comment). It grants the process nothing; that one line, once, is left out of the
/// judgement, and any other raised command in the unit still fails it.
#[test]
fn the_unit_grants_cap_sys_admin_and_no_other() {
    for (name, unit) in mounting_units() {
        let judged = if name == FS_TEMPLATE {
            assert_eq!(unit.matches(FS_MARKER_CHECK).count(), 1, "{name}");
            unit.replace(FS_MARKER_CHECK, "")
        } else {
            unit
        };
        assert_eq!(grants_cap_sys_admin_alone(&judged), Ok(()), "{name}");
    }
    let fs = read(FS_TEMPLATE).replace(FS_MARKER_CHECK, "");
    assert_eq!(
        grants_cap_sys_admin_alone(&with_service_line(&fs, "ExecStartPre=+/usr/bin/true")),
        Err("ExecStartPre=+/usr/bin/true runs with raised privileges".to_owned()),
        "another raised command of the fs unit"
    );
    let template = read(TEMPLATE);
    let bounding = "CapabilityBoundingSet=CAP_SYS_ADMIN\n";
    let ambient = "AmbientCapabilities=CAP_SYS_ADMIN\n";
    let defects = [
        (
            template.replace(
                bounding,
                "CapabilityBoundingSet=CAP_SYS_ADMIN CAP_NET_ADMIN\n",
            ),
            "CapabilityBoundingSet is {\"CAP_NET_ADMIN\", \"CAP_SYS_ADMIN\"}",
        ),
        (
            with_service_line(&template, "AmbientCapabilities=CAP_NET_RAW"),
            "AmbientCapabilities is {\"CAP_NET_RAW\", \"CAP_SYS_ADMIN\"}",
        ),
        (
            template.replace(bounding, "CapabilityBoundingSet=~CAP_SYS_MODULE\n"),
            "CapabilityBoundingSet=~CAP_SYS_MODULE keeps every capability but those",
        ),
        (
            template.replace(ambient, "AmbientCapabilities=\n"),
            "AmbientCapabilities is {}",
        ),
        (template.replace(ambient, ""), "AmbientCapabilities is {}"),
        (
            template.replace("User=tierstore\n", "User=root\n"),
            "User is [\"root\"]",
        ),
        (
            template.replace("NoNewPrivileges=yes\n", ""),
            "NoNewPrivileges is not yes",
        ),
        (
            with_service_line(&template, "ExecStartPre=+/usr/bin/true"),
            "ExecStartPre=+/usr/bin/true runs with raised privileges",
        ),
        (
            with_service_line(&template, "ExecStartPost=!!/usr/bin/true"),
            "ExecStartPost=!!/usr/bin/true runs with raised privileges",
        ),
        (
            with_service_line(&template, "ExecStartPre=-+/usr/bin/true"),
            "ExecStartPre=-+/usr/bin/true runs with raised privileges",
        ),
        (
            with_service_line(&template, "ExecStopPost= :!/usr/bin/true"),
            "ExecStopPost= :!/usr/bin/true runs with raised privileges",
        ),
    ];
    for (unit, why) in defects {
        assert_ne!(unit, template);
        assert_eq!(grants_cap_sys_admin_alone(&unit), Err(why.to_owned()));
    }
    // A `+` or `!` in an argument is not a prefix: the command runs as the unit's user.
    let argument = with_service_line(&template, "ExecStartPre=-/usr/bin/test +x != y");
    assert_eq!(grants_cap_sys_admin_alone(&argument), Ok(()));
}

/// Asserts that the template has what the unit of `tierstore fs` gives its process to make and
/// keep the mount, with the same values (docs/design/one-service.md section 1.4): the device and
/// the system calls of the mount, the descriptor store that keeps it across a restart, the start
/// limit and restart wait that let a mount without a server fail fast, the runtime directories of
/// the file system's state and of the mount point's parent, and the account, privilege, memory,
/// umask and file limits the two units already shared. It would catch the template and the unit
/// of `tierstore fs` drifting apart on what the mount needs, so that one process of the template
/// cannot do what the unit of `tierstore fs` does.
#[test]
fn the_template_has_what_the_fs_unit_gives_the_mount() {
    let (template, fs) = (read(TEMPLATE), read(FS_TEMPLATE));
    for key in [
        "DeviceAllow",
        "CapabilityBoundingSet",
        "AmbientCapabilities",
        "SystemCallFilter",
        "FileDescriptorStoreMax",
        "FileDescriptorStorePreserve",
        "StartLimitIntervalSec",
        "StartLimitBurst",
        "RestartSec",
        "RuntimeDirectory",
        "RuntimeDirectoryPreserve",
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
        assert_eq!(value(&template, key), value(&fs, key), "{key}");
    }
    assert_eq!(
        value(&template, "RuntimeDirectory"),
        "tierstore-fs tierstore"
    );
    assert_eq!(value(&template, "FileDescriptorStoreMax"), "1");
}

/// Asserts what the unit of `tierstore fs` has that the template has not: it is `Type=notify`
/// (the process says `READY=1` once it serves the mount, so a unit ordered after it finds the
/// mount there), with a start timeout above the 120 s default of `fs.handoff_wait` (a start that
/// takes the mount over must not be cut by the unit's timeout), and its process has no network
/// (`AF_UNIX` alone), where the template's serves S3.
#[test]
fn the_fs_template_is_a_notify_unit_on_unix_sockets_alone() {
    let (template, fs) = (read(TEMPLATE), read(FS_TEMPLATE));
    assert_eq!(value(&fs, "Type"), "notify");
    assert_eq!(value(&fs, "NotifyAccess"), "main");
    // The default of `fs.handoff_wait` is 120 s (`DEFAULT_HANDOFF_WAIT` in the crate, not public
    // to this test).
    let start_secs: u64 = value(&fs, "TimeoutStartSec")
        .parse()
        .expect("TimeoutStartSec is whole seconds");
    assert!(
        start_secs > 120,
        "TimeoutStartSec is above the default fs.handoff_wait"
    );
    assert_eq!(value(&fs, "RestrictAddressFamilies"), "AF_UNIX");
    assert_ne!(value(&template, "RestrictAddressFamilies"), "AF_UNIX");
}

/// Asserts that the units of `tierstore fs` and of the store process set `LimitNOFILE` to exactly
/// 1048576, and that the launchd jobs of both set the soft and the hard `NumberOfFiles` to the
/// same. It would catch any one of the four files going back to a smaller limit, with which a
/// build's open files run the process out of descriptors and every open after them fails.
#[test]
fn every_unit_and_job_allows_1048576_open_files() {
    for file in ["tierstore@.service", "tierstore-fs@.service"] {
        assert_eq!(value(&read(file), "LimitNOFILE"), "1048576", "{file}");
    }
    for file in ["tierstore.plist", "tierstore-fs.plist"] {
        let job = plist_of(file);
        for limits in ["SoftResourceLimits", "HardResourceLimits"] {
            let Some(Plist::Dict(limit)) = job.get(limits) else {
                panic!("{file}: {limits} is a dictionary");
            };
            assert_eq!(
                limit.get("NumberOfFiles"),
                Some(&Plist::Integer(1_048_576)),
                "{file}: {limits}"
            );
        }
    }
}

/// Asserts that the unit of `tierstore fs` runs the `fs` verb of the installed binary with
/// a `--config` the binary reads (a file that is not there is status 2, naming it, which a flag the
/// binary did not parse would not be), starts only under a marker of its own after the S3
/// instances, and is enabled at boot. It would catch a verb or a flag that is renamed under the
/// unit, which would leave the instance failing at every start.
#[test]
fn the_fs_template_runs_the_fs_verb_of_the_installed_binary() {
    let unit = read(FS_TEMPLATE);
    let words = exec_start(&unit);
    assert_eq!(words[0], SERVICE_BINARY);
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

/// Asserts that the unit of `tierstore fs` looks at its marker at every start, its automatic
/// restarts included: one `ExecCondition=` that tests, as root (`+`, as systemd tests the
/// condition), for exactly the file of `ConditionPathExists=`, beside `Restart=on-failure`.
/// systemd does not evaluate the conditions of `[Unit]` again for a restart of `Restart=`, so
/// without it a taker whose take failed is started again after the update hook removed its
/// marker, again and again, and the rollback, which waits for it to end, never ends. It would catch the line left out, and one that tests another file.
#[test]
fn the_fs_template_looks_at_its_marker_at_every_restart() {
    let unit = read(FS_TEMPLATE);
    let marker = value(&unit, "ConditionPathExists");
    assert_eq!(
        value(&unit, "ExecCondition"),
        format!("+/usr/bin/test -e {marker}")
    );
    assert_eq!(value(&unit, "Restart"), "on-failure");
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

/// Asserts the launchd job of `tierstore fs`: its label is its file's name, it runs the `fs` verb of
/// the installed binary with a `--config` the binary reads (a file that is not there is
/// status 2, naming it), as the user and group of the store's job (the fill socket answers only a
/// peer of its own user, so a job of any other user, root included, has every fill refused), it is
/// restarted when it exits, launchd waits 150 seconds after SIGTERM (the process's unmount is at
/// most 120) before it kills it and kills the whole process group (no `AbandonProcessGroup`), it
/// sets no environment variable, and no word of it names a secret. It would catch a verb or a flag
/// renamed under the job, a job that runs as root, and a stop time that kills an unmount in
/// progress.
#[test]
fn the_fs_launchd_job_runs_the_fs_verb_of_the_installed_binary() {
    let job = plist_of("tierstore-fs.plist");
    assert_eq!(
        job.get("Label"),
        Some(&Plist::Text("tierstore-fs".to_owned()))
    );
    let store = plist();
    for key in ["UserName", "GroupName"] {
        assert_eq!(text(&job, key), "_tierstore", "{key}");
        assert_eq!(text(&job, key), text(&store, key), "{key}");
    }
    let Some(Plist::Array(words)) = job.get("ProgramArguments") else {
        panic!("ProgramArguments is an array");
    };
    let words: Vec<String> = words
        .iter()
        .map(|word| match word {
            Plist::Text(text) => text.clone(),
            other => panic!("an argument is {other:?}"),
        })
        .collect();
    assert_eq!(words[0], SERVICE_BINARY);
    assert_eq!(
        words[1..],
        ["fs", "--config", "/usr/local/tierstore/etc/fs.toml"]
    );
    assert_eq!(job.get("KeepAlive"), Some(&Plist::Bool(true)));
    assert_eq!(job.get("RunAtLoad"), Some(&Plist::Bool(true)));
    assert_eq!(job.get("ExitTimeOut"), Some(&Plist::Integer(150)));
    assert!(!job.contains_key("AbandonProcessGroup"));
    for key in job.keys() {
        assert!(!key.contains("Environment"), "{key}");
    }
    let scratch = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tfs-plist");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .args(["fs", "--config"])
        .arg(scratch.path().join("fs.toml"))
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("fs.toml"));
}

/// The template the deb and rpm packages install.
fn package_template() -> String {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/tierstore@.service");
    std::fs::read_to_string(file).expect("the package's template")
}

/// The lines of a unit that systemd reads: no comment, no blank line.
fn settings_of(unit: &str) -> Vec<String> {
    unit.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// Asserts that the template of the deb and rpm packages says what the archive's template says,
/// line for line, but for the binary of `ExecStart=`: the same marker condition, state directory,
/// account, drain, restarts, hardening and `[Install]`. It would catch a package template that
/// drifts from the one the update hook was written against (a condition the hook's markers do not
/// meet, a state directory without the hand-over socket, a hardening the hook's hand-over cannot
/// run under).
#[test]
fn the_package_template_is_the_archive_template_but_for_the_binary() {
    let archive = settings_of(&read(TEMPLATE));
    let package = settings_of(&package_template());
    let package_binary = format!("/{}", PACKAGE_RELEASE_BINARY.trim_start_matches("tree/"));
    let as_archive: Vec<String> = package
        .iter()
        .map(|line| line.replace(&package_binary, SERVICE_BINARY))
        .collect();
    assert_eq!(as_archive, archive);
    assert_ne!(
        package, archive,
        "the package's template runs its own binary"
    );
}

/// Asserts that both templates want the nft guard unit and start after it, besides the network
/// ordering they already have, and that neither requires it or is stopped with it: the guard is
/// its own unit, so the stop of the old instance in a hand-over leaves it under the new one, and a
/// host without `guard.nft` (whose guard unit is skipped) still starts its instances.
#[test]
fn both_templates_want_the_guard_and_start_after_it_without_requiring_it() {
    for unit in [read(TEMPLATE), package_template()] {
        assert_eq!(
            values(&unit, "Wants"),
            ["network-online.target", "tierstore-guard.service"]
        );
        assert_eq!(
            values(&unit, "After"),
            ["network-online.target", "tierstore-guard.service"]
        );
        for key in ["Requires", "BindsTo", "PartOf", "Before"] {
            assert_eq!(values(&unit, key), Vec::<String>::new(), "{key}");
        }
    }
}

/// Asserts that the template of the packages runs the binary the hook asks in a staged release of
/// the package (`PACKAGE_RELEASE_BINARY` below the version's `tree/`, which is the root of the
/// package's files), on the host's `/etc/tierstore/config.toml`, with flags the binary parses.
/// It would catch a template that runs another binary than the one the hook checked the
/// configuration with, or that reads the example.
#[test]
fn the_package_template_runs_the_binary_the_hook_finds_in_a_release_of_the_package() {
    let unit = package_template();
    let words = exec_start(&unit);
    let in_tree = PACKAGE_RELEASE_BINARY
        .strip_prefix("tree")
        .expect("the binary is under the tree");
    assert_eq!(words[0], in_tree);
    let args = Args::try_parse_from(&words).expect("the template's flags");
    assert_eq!(
        args.config.as_deref(),
        Some(Path::new("/etc/tierstore/config.toml"))
    );
    assert_eq!(
        value(&unit, "ConditionPathExists"),
        marker_path(Instance::A)
            .to_str()
            .expect("utf-8")
            .replace("run-a", "run-%i")
    );
}

/// The `[hook.timeout_secs]` table of the package's manifest (`packaging/hostkeeper/manifest.toml.in`,
/// whose other tables have placeholders that are not TOML), in seconds by step.
fn step_bounds() -> BTreeMap<String, u64> {
    let file =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/hostkeeper/manifest.toml.in");
    let text = std::fs::read_to_string(file).expect("the manifest");
    let (_, rest) = text
        .split_once("[hook.timeout_secs]\n")
        .expect("a [hook.timeout_secs] table");
    let table = rest.split("\n[").next().expect("its lines");
    let table: toml::Table = table.parse().expect("the table is TOML");
    table
        .into_iter()
        .map(|(step, secs)| {
            (
                step,
                u64::try_from(secs.as_integer().expect("seconds")).expect("not negative"),
            )
        })
        .collect()
}

/// Whether a step bound of `secs` is above the looks of a step that starts a release and waits
/// for it: up to `READY_ATTEMPTS` looks for it to serve and `PROBE_ATTEMPTS` of the probe, a
/// `POLL_INTERVAL` apart; the rest of the bound is the start itself and the time of the requests.
fn above_the_looks(secs: u64) -> bool {
    let looks = POLL_INTERVAL * (READY_ATTEMPTS + PROBE_ATTEMPTS);
    Duration::from_secs(secs) > looks
}

/// Asserts that the manifest's bounds of the steps that start a release and wait for it to serve
/// (`cutover` and `rollback`) and of the one that confirms it again (`cleanup`) are above the
/// hook's looks, and that the `cutover` bound of 300 s that the manifest had before the wait
/// covered a slow open of the cache is not. It would catch a wait raised without its bound, after
/// which hostkeeper would kill a step that was still within its looks.
#[test]
fn the_manifests_step_bounds_are_above_the_hooks_looks() {
    let bounds = step_bounds();
    for step in ["cutover", "rollback", "cleanup"] {
        let secs = bounds[step];
        assert!(above_the_looks(secs), "{step} = {secs} s");
    }
    assert!(!above_the_looks(300));
}

/// The `ExitTimeOut` of the launchd job of `file`, in hostkeeper's package.
fn exit_timeout(file: &str) -> Duration {
    let Some(Plist::Integer(seconds)) = plist_of(file).get("ExitTimeOut").cloned() else {
        panic!("{file}: ExitTimeOut is an integer");
    };
    Duration::from_secs(seconds.try_into().expect("not negative"))
}

/// Asserts that the manifest's bounds of `cutover` and `rollback`, the same on both systems, cover
/// what those steps wait for on a Mac at the longest: the bootout of the file service's job (its
/// `ExitTimeOut`), the bootout of the main job (its `ExitTimeOut`, 210 s), and the hook's looks
/// for the release to serve and its probe. It would catch an `ExitTimeOut` raised past what the
/// bound leaves room for, after which hostkeeper would kill a step that launchd and the hook were
/// still within.
#[test]
fn the_manifests_cutover_and_rollback_cover_both_bootouts_and_the_looks() {
    let looks = POLL_INTERVAL * (READY_ATTEMPTS + PROBE_ATTEMPTS);
    let longest = exit_timeout("tierstore-fs.plist") + exit_timeout("tierstore.plist") + looks;
    let bounds = step_bounds();
    for step in ["cutover", "rollback"] {
        let bound = Duration::from_secs(bounds[step]);
        assert!(
            bound > longest,
            "{step}: {bound:?} is not above {longest:?}"
        );
    }
    assert_eq!(exit_timeout("tierstore.plist"), Duration::from_secs(210));
}

/// Asserts that no unit and no launchd job of hostkeeper's package names a path inside
/// hostkeeper's package directories: each runs the copy at `SERVICE_BINARY`, as an account that
/// is not root (the units' `tierstore` and the jobs' `_tierstore`). hostkeeper
/// keeps its packages closed to every user but root, so it would catch a unit or a job that runs
/// the binary through the release link `current`, which its account cannot reach.
#[test]
fn no_unit_or_job_runs_a_program_inside_the_package_directories() {
    for file in [TEMPLATE, FS_TEMPLATE] {
        let unit = read(file);
        assert_eq!(exec_start(&unit)[0], SERVICE_BINARY, "{file}");
        assert_eq!(value(&unit, "User"), "tierstore", "{file}");
        assert!(!unit.contains(PACKAGES), "{file}");
    }
    for file in ["tierstore.plist", "tierstore-fs.plist"] {
        let job = plist_of(file);
        let Some(Plist::Array(words)) = job.get("ProgramArguments") else {
            panic!("{file}: ProgramArguments is an array");
        };
        assert_eq!(
            words.first(),
            Some(&Plist::Text(SERVICE_BINARY.to_owned())),
            "{file}"
        );
        assert!(!read(file).contains(PACKAGES), "{file}");
    }
}
