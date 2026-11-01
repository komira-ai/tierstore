//! The example unit of the `tierstore` binary (`packaging`) against the binary's flags:
//! the command line parses as the binary parses it, no secret is on it, the stop timeout covers
//! the drain, nothing starts the unit by itself, and it runs as the account that the sysusers.d
//! entry makes. And the files of the update hook's hand-over against the hook and the binary:
//! the declaration (`packaging/updater.d/tierstore.toml`) parses with the hook's own parser
//! and names what the slot unit template (`packaging/tierstore@.service`) is built on; the
//! hook's account, its state directory and the polkit rule of its account give it what the
//! design gives it and no more.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use tierstore::server::cli::Cli;

/// The words in the name of a flag that say it holds a secret.
const SECRET_WORDS: [&str; 5] = ["key", "secret", "credential", "token", "password"];

/// The directory of the example files.
fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging")
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

/// The words of the unit's `ExecStart=`.
fn exec_start(unit: &str) -> Vec<String> {
    value(unit, "ExecStart")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// The unit's command line, parsed as the binary parses its flags (the program name first).
fn parse(unit: &str) -> Cli {
    Cli::try_parse_from(exec_start(unit)).expect("the unit's flags")
}

/// Asserts that the command line of the unit runs the binary of the release as `tierstore`, that
/// it parses with the flags the binary has, and that it describes two different S3 tiers, each
/// with an endpoint, and four key files of absolute paths that are not the same file: the full
/// key, one read-only key (the daemons') and the key of each tier.
#[test]
fn the_command_line_is_the_binarys_flags_for_two_s3_tiers() {
    let unit = read("tierstore.service");
    assert_eq!(exec_start(&unit)[0], "/usr/libexec/kbf/tierstore");
    let cli = parse(&unit);
    assert!(cli.hot.starts_with("s3:"), "{}", cli.hot);
    assert!(cli.cold.starts_with("s3:"), "{}", cli.cold);
    assert_ne!(cli.hot, cli.cold);
    for endpoint in [&cli.hot_endpoint, &cli.cold_endpoint] {
        let endpoint = endpoint.as_deref().expect("an endpoint");
        assert!(endpoint.starts_with("http://"), "{endpoint}");
    }
    let hot_key = cli.hot_credentials_file.as_deref();
    let hot_key = hot_key.expect("--hot-credentials-file");
    let cold_key = cli.cold_credentials_file.as_deref();
    let cold_key = cold_key.expect("--cold-credentials-file");
    let readers = &cli.read_only_key_file;
    assert_eq!(readers.len(), 1, "{readers:?}");
    let read_only_key = readers[0].as_path();
    let files = [
        cli.access_key_file.as_path(),
        read_only_key,
        hot_key,
        cold_key,
    ];
    for file in files {
        assert!(file.is_absolute(), "{file:?}");
    }
    let distinct: BTreeSet<&Path> = files.into_iter().collect();
    assert_eq!(distinct.len(), 4);
}

/// Asserts that no secret is on the command line or in the environment of the unit: the flags
/// whose names speak of a key, a secret, a credential, a token or a password are the four that
/// name a file (`-file`), and the unit sets no environment variable and reads no environment
/// file.
#[test]
fn no_secret_is_on_the_command_line_or_in_the_environment() {
    let unit = read("tierstore.service");
    let flags: Vec<String> = exec_start(&unit)
        .into_iter()
        .filter(|word| word.starts_with("--"))
        .collect();
    let secret_flags: Vec<&String> = flags
        .iter()
        .filter(|flag| SECRET_WORDS.iter().any(|name| flag.contains(*name)))
        .collect();
    assert_eq!(secret_flags.len(), 4, "{secret_flags:?}");
    for flag in secret_flags {
        assert!(flag.ends_with("-file"), "{flag} is not a file");
    }
    let lines = unit.lines();
    let environment = lines.filter(|line| line.starts_with("Environment"));
    assert_eq!(environment.count(), 0);
}

/// Asserts that systemd stops the unit with SIGTERM, which the binary drains on, after a timeout
/// that is no shorter than the wait of the load balancers plus the deadline of the drain plus the
/// 5 s the reconciler's lease release may take (`STOP_WAIT` in `cli.rs`), so that a drain in
/// progress is not cut by SIGKILL; and that a start that fails is tried again, except when the
/// argument parser refused the flags (exit status 2) or a tier answered the start-up self-test
/// wrongly (exit status 3), which a restart cannot mend.
#[test]
fn the_stop_timeout_covers_the_drain_and_a_refused_command_line_is_not_restarted() {
    let unit = read("tierstore.service");
    let cli = parse(&unit);
    assert_eq!(value(&unit, "KillSignal"), "SIGTERM");
    let seconds: u64 = value(&unit, "TimeoutStopSec").parse().expect("seconds");
    let drain = cli.drain_lb_wait + cli.drain_deadline + Duration::from_secs(5);
    let timeout = Duration::from_secs(seconds);
    assert!(timeout >= drain, "{timeout:?} is less than {drain:?}");
    assert_eq!(value(&unit, "Restart"), "on-failure");
    assert_eq!(value(&unit, "RestartPreventExitStatus"), "2 3");
    // A start that fails with exit 1 for ever (a store that answers 500 to a conditional put) is
    // given up on after 5 starts in an hour, not retried for ever; but a store that is down for
    // longer than the few seconds a short restart gap allows (a reboot) is not given up on: the
    // process asks it again for --backend-check-wait, and the restart gap is 30 s, so that the 5
    // starts take about half an hour, well inside the hour.
    assert_eq!(value(&unit, "RestartSec"), "30");
    assert_eq!(value(&unit, "StartLimitIntervalSec"), "1h");
    let burst: u32 = value(&unit, "StartLimitBurst").parse().expect("a count");
    assert_eq!(burst, 5);
    let each = cli.backend_check_wait + Duration::from_secs(30);
    assert!(
        each * burst < Duration::from_secs(3600),
        "{each:?} times {burst}"
    );
    assert!(
        each * burst >= Duration::from_secs(20 * 60),
        "{each:?} times {burst}"
    );
}

/// Asserts that placing the unit starts nothing: it has no `[Install]` section, nothing in it
/// makes another unit want or require it, and it is skipped until the access key file, which
/// its command line names, exists.
#[test]
fn the_unit_is_not_enabled_by_placing_it() {
    let unit = read("tierstore.service");
    let sections = unit.lines().filter(|line| line.trim() == "[Install]");
    assert_eq!(sections.count(), 0);
    for key in ["WantedBy", "RequiredBy", "Alias"] {
        assert_eq!(values(&unit, key), Vec::<String>::new(), "{key}");
    }
    let access_key = parse(&unit).access_key_file;
    let condition = value(&unit, "ConditionPathExists");
    assert_eq!(PathBuf::from(condition), access_key);
}

/// Asserts that the unit runs as an account that is not root, that the sysusers.d entry makes
/// exactly that one account, and that it has no login shell.
#[test]
fn the_unit_runs_as_the_account_the_sysusers_entry_makes() {
    let unit = read("tierstore.service");
    let user = value(&unit, "User");
    assert_ne!(user, "root");
    let sysusers = read("tierstore.conf");
    let entries: Vec<&str> = sysusers
        .lines()
        .filter(|line| line.starts_with("u "))
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    let words: Vec<&str> = entries[0].split_whitespace().collect();
    assert_eq!(words[1], user);
    assert_eq!(words.last(), Some(&"/usr/sbin/nologin"));
}

/// Asserts that the unit keeps the hardening of the repository's other example units that this
/// process can run under: no new privileges, a read-only file system, a private `/tmp`, and no
/// capability.
#[test]
fn the_unit_is_hardened() {
    let unit = read("tierstore.service");
    let wanted = [
        ("NoNewPrivileges", "yes"),
        ("ProtectSystem", "strict"),
        ("PrivateTmp", "yes"),
        ("CapabilityBoundingSet", ""),
    ];
    for (key, want) in wanted {
        assert_eq!(value(&unit, key), want, "{key}");
    }
}

// The slot unit, the declaration, the hook's account and state directory, and the polkit rule:
// the files the host updater's hand-over needs (crates/tierstore/docs/update-hook.md).

/// The slot unit template.
const SLOT_UNIT: &str = "tierstore@.service";
/// The declaration, as the host updater installs it under `/etc/updater.d`.
const DECLARATION: &str = "updater.d/tierstore.toml";
/// The sysusers.d entry of the hook's account.
const HOOK_ACCOUNT: &str = "tierstore-update.conf";
/// The tmpfiles.d entry of the hook's state directory.
const STATE_DIR_ENTRY: &str = "tmpfiles.d/tierstore-update.conf";
/// The polkit rule of the hook's account.
const POLKIT_RULE: &str = "polkit/50-tierstore-update.rules";
/// The hook's account and group.
const HOOK_USER: &str = "tierstore-update";
/// The polkit action that starts and stops a unit.
const MANAGE_UNITS: &str = "org.freedesktop.systemd1.manage-units";
/// The keys of the declaration that must come before any table header: a key after one belongs
/// to that table.
const TOP_LEVEL: [&str; 13] = [
    "name",
    "binary",
    "signer",
    "uid",
    "registration_ttl_secs",
    "health_ports",
    "verify_deadline_secs",
    "promotion_ceiling_secs",
    "cleanup_retries",
    "state_dir",
    "boot",
    "hook",
    // Not a key of the file: the first release that carries the hook sets it.
    "min_version",
];

/// The declaration as a TOML table.
fn declaration() -> toml::Table {
    toml::from_str(&read(DECLARATION)).expect("the declaration is TOML")
}

/// The table at the dotted `path` of `table`.
fn table<'a>(table: &'a toml::Table, path: &str) -> &'a toml::Table {
    path.split('.').fold(table, |at, key| {
        at.get(key)
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| panic!("{path}: no table {key}"))
    })
}

/// The strings of the array `key` of `table`.
fn strings(table: &toml::Table, key: &str) -> Vec<String> {
    let array = table.get(key).and_then(toml::Value::as_array);
    let array = array.unwrap_or_else(|| panic!("{key} is an array"));
    let each = array.iter().map(|v| v.as_str().map(str::to_owned));
    each.collect::<Option<_>>()
        .unwrap_or_else(|| panic!("{key} holds strings"))
}

/// The integer `key` of `table`.
fn integer(table: &toml::Table, key: &str) -> i64 {
    let value = table.get(key).and_then(toml::Value::as_integer);
    value.unwrap_or_else(|| panic!("{key} is an integer"))
}

/// The slot unit's command line for the slot `port`, `%i` replaced as systemd does.
fn slot_exec_start(port: u16) -> Vec<String> {
    let unit = read(SLOT_UNIT).replace("%i", &port.to_string());
    exec_start(&unit)
}

/// The slot unit's command line for the slot `port`, parsed as the binary parses it.
fn slot_cli(port: u16) -> Cli {
    Cli::try_parse_from(slot_exec_start(port)).expect("the slot unit's flags")
}

/// What is wrong with the rights the declaration's `[hook]` gives: `CAP_NET_ADMIN` must be
/// declared hook-wide (every step and the boot entry list nftables rules over netlink), with no
/// table per step, and netlink must be among the address families.
fn hook_rights_problems(declaration: &toml::Table) -> Vec<String> {
    let hook = table(declaration, "hook");
    let mut problems = Vec::new();
    if strings(hook, "capabilities") != ["CAP_NET_ADMIN"] {
        problems.push("hook.capabilities is not [\"CAP_NET_ADMIN\"]".to_owned());
    }
    if hook.contains_key("step_capabilities") {
        problems.push("hook.step_capabilities is there".to_owned());
    }
    let families = strings(hook, "address_families");
    if families != ["AF_UNIX", "AF_INET", "AF_INET6", "AF_NETLINK"] {
        problems.push(format!("hook.address_families is {families:?}"));
    }
    problems
}

/// The double-quoted strings of a polkit rule.
fn quoted(text: &str) -> Vec<&str> {
    text.split('"').skip(1).step_by(2).collect()
}

/// What is wrong with a polkit rule for the hook's account: it must name the account, grant
/// only the action that starts and stops a unit, for exactly the units `units`, and never name
/// the action over unit files (which systemd checks without naming the unit).
fn polkit_problems(text: &str, units: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    let words = quoted(text);
    if !words.contains(&HOOK_USER) {
        problems.push(format!("{HOOK_USER} is not named"));
    }
    let actions: BTreeSet<&str> = words
        .iter()
        .copied()
        .filter(|w| w.starts_with("org.freedesktop."))
        .collect();
    if actions != BTreeSet::from([MANAGE_UNITS]) {
        problems.push(format!("the actions are {actions:?}"));
    }
    if text.contains("manage-unit-files") {
        problems.push("manage-unit-files is named".to_owned());
    }
    let named: BTreeSet<&str> = words
        .iter()
        .copied()
        .filter(|w| w.ends_with(".service"))
        .collect();
    let wanted: BTreeSet<&str> = units.iter().map(String::as_str).collect();
    if named != wanted {
        problems.push(format!("the units are {named:?}, not {wanted:?}"));
    }
    if words
        .iter()
        .any(|w| w.ends_with(".service") && w.contains('*'))
    {
        problems.push("a unit pattern is named".to_owned());
    }
    problems
}

/// Asserts that the hook's own parser takes the declaration and reads from it what the slot
/// unit is built on: the two ports 9301 and 9302, the slot units of those ports, the state
/// directory whose slot links the unit runs, and the service account the unit runs as; and that
/// the boot entry is `restore`, which starts the slot the record names, since the unit has no
/// `[Install]` section.
#[cfg(target_os = "linux")]
#[test]
fn the_declaration_parses_and_names_what_the_slot_unit_is_built_on() {
    use tierstore::server::update_hook::{Declaration, unit_name};
    let text = read(DECLARATION);
    let parsed = Declaration::parse(&text).expect("the hook takes the declaration");
    assert_eq!(parsed.ports, [9301, 9302]);
    let state_dir = Path::new("/var/lib/updater/hooks/tierstore");
    assert_eq!(parsed.state_dir, state_dir);
    let unit = read(SLOT_UNIT);
    assert_eq!(
        parsed.service.as_deref(),
        Some(value(&unit, "User").as_str())
    );
    let declared = declaration();
    let units = strings(table(&declared, "hook"), "units");
    let built: Vec<String> = parsed.ports.iter().map(|p| unit_name(*p)).collect();
    assert_eq!(units, built);
    for (port, name) in parsed.ports.iter().zip(&units) {
        // The template's file name with the port as its instance.
        assert_eq!(*name, SLOT_UNIT.replace("@.", &format!("@{port}.")));
        let program = PathBuf::from(&slot_exec_start(*port)[0]);
        let link = state_dir.join("slots").join(port.to_string());
        assert_eq!(program, link.join("tierstore"));
        assert_eq!(slot_cli(*port).slot, Some(*port));
    }
    assert_eq!(
        declared.get("boot").and_then(toml::Value::as_str),
        Some("restore")
    );
    assert_eq!(
        declared.get("name").and_then(toml::Value::as_str),
        Some("tierstore")
    );
    assert_eq!(
        declared.get("binary").and_then(toml::Value::as_str),
        Some("tierstore")
    );
}

/// Asserts that every top-level key of the declaration comes before the first table header
/// (a key after one belongs to that table), that the file has exactly the top-level keys of the
/// design and no `min_version`, and that the hook's parser refuses the file when `state_dir` or
/// `health_ports` is moved under `[hook]`.
#[cfg(target_os = "linux")]
#[test]
fn every_top_level_key_comes_before_a_table_and_one_after_is_refused() {
    use tierstore::server::update_hook::Declaration;
    let text = read(DECLARATION);
    let declared = declaration();
    let keys: BTreeSet<&str> = declared.keys().map(String::as_str).collect();
    let wanted: BTreeSet<&str> = TOP_LEVEL[..TOP_LEVEL.len() - 1].iter().copied().collect();
    assert_eq!(keys, wanted);
    let lines: Vec<&str> = text.lines().collect();
    let first_table = lines.iter().position(|l| l.starts_with('['));
    let first_table = first_table.expect("a table");
    for key in TOP_LEVEL.iter().filter(|k| **k != "hook") {
        let at = lines
            .iter()
            .position(|l| l.starts_with(&format!("{key} =")));
        if *key == "min_version" {
            assert_eq!(
                at, None,
                "min_version is set by the release that first carries it"
            );
            continue;
        }
        let at = at.unwrap_or_else(|| panic!("{key} is a line of its own"));
        assert!(at < first_table, "{key} is after a table header");
    }
    for key in ["state_dir", "health_ports"] {
        let line = *lines
            .iter()
            .find(|l| l.starts_with(&format!("{key} =")))
            .expect(key);
        let moved = text.replacen(&format!("{line}\n"), "", 1).replacen(
            "[hook]\n",
            &format!("[hook]\n{line}\n"),
            1,
        );
        assert!(Declaration::parse(&moved).is_err(), "{key} under [hook]");
    }
}

/// Asserts that the declaration gives the hook `CAP_NET_ADMIN` hook-wide, with no table per
/// step, and the netlink family nft needs; that its timeouts and bounds are the design's and
/// hold together (the deadline covers verify and rollback, cleanup covers the unit's stop
/// timeout); and that the check is not vacuous: without the capability, or with a
/// `step_capabilities` table, it fails.
#[test]
fn the_hook_holds_cap_net_admin_hook_wide_with_netlink() {
    let declared = declaration();
    assert_eq!(hook_rights_problems(&declared), Vec::<String>::new());
    let hook = table(&declared, "hook");
    for key in ["user", "group"] {
        assert_eq!(hook.get(key).and_then(toml::Value::as_str), Some(HOOK_USER));
    }
    assert_ne!(
        declared.get("uid").and_then(toml::Value::as_str),
        Some(HOOK_USER)
    );
    let timeouts = table(&declared, "hook.timeout_secs");
    let steps = ["prepare", "cutover", "verify", "rollback", "cleanup"];
    let got: Vec<i64> = steps.iter().map(|s| integer(timeouts, s)).collect();
    assert_eq!(got, [600, 300, 1000, 600, 1300]);
    assert_eq!(timeouts.len(), steps.len());
    let deadline = integer(&declared, "verify_deadline_secs");
    assert_eq!(deadline, 1600);
    assert!(deadline >= integer(timeouts, "verify") + integer(timeouts, "rollback"));
    assert_eq!(integer(&declared, "promotion_ceiling_secs"), 3600);
    assert_eq!(integer(&declared, "cleanup_retries"), 3);
    let params = table(&declared, "hook.params");
    assert_eq!(integer(params, "promotion_bound_secs"), 900);
    assert!(integer(timeouts, "verify") > integer(params, "promotion_bound_secs"));
    let stop: i64 = value(&read(SLOT_UNIT), "TimeoutStopSec")
        .parse()
        .expect("seconds");
    assert!(
        integer(timeouts, "cleanup") > stop,
        "cleanup outlasts the old slot's stop"
    );

    let text = read(DECLARATION);
    let without = text.replacen("capabilities = [\"CAP_NET_ADMIN\"]", "capabilities = []", 1);
    assert_ne!(without, text);
    let without: toml::Table = toml::from_str(&without).expect("TOML");
    assert!(!hook_rights_problems(&without).is_empty(), "no capability");
    let per_step = format!("{text}\n[hook.step_capabilities]\nverify = []\n");
    let per_step: toml::Table = toml::from_str(&per_step).expect("TOML");
    assert!(
        !hook_rights_problems(&per_step).is_empty(),
        "a table per step"
    );
    let no_netlink = text.replacen(", \"AF_NETLINK\"", "", 1);
    assert_ne!(no_netlink, text);
    let no_netlink: toml::Table = toml::from_str(&no_netlink).expect("TOML");
    assert!(!hook_rights_problems(&no_netlink).is_empty(), "no netlink");
}

/// Asserts that the slot unit's command line parses with the binary's flags for each slot, and
/// that each slot listens on the wildcard of both families at its own port, names that port as
/// its slot and has a proxy id of its own; that the two slots share one lock file; and that the
/// unit runs the binary of the slot's link and is skipped when the link is absent.
#[test]
fn the_slot_unit_runs_the_slots_binary_on_its_own_port_and_proxy_id() {
    let mut proxy_ids = BTreeSet::new();
    let mut locks = BTreeSet::new();
    for port in [9301, 9302] {
        let cli = slot_cli(port);
        let listen: Vec<String> = cli.listen.iter().map(ToString::to_string).collect();
        assert_eq!(listen, [format!("0.0.0.0:{port}"), format!("[::]:{port}")]);
        assert_eq!(cli.slot, Some(port));
        assert!(cli.listen.iter().all(|a| Some(a.port()) == cli.slot));
        assert_eq!(cli.proxy_id, format!("tierstore-{port}"));
        proxy_ids.insert(cli.proxy_id);
        locks.insert(cli.lock_file.expect("--lock-file"));
        assert!(cli.hot.starts_with("s3:"), "{}", cli.hot);
        assert!(cli.cold.starts_with("s3:"), "{}", cli.cold);
    }
    assert_eq!(proxy_ids.len(), 2, "a proxy id per slot");
    assert_eq!(locks.len(), 1, "one lock file for the node");
    let unit = read(SLOT_UNIT);
    let program = exec_start(&unit).remove(0);
    assert!(program.contains("/slots/%i/"), "{program}");
    assert_eq!(value(&unit, "ConditionPathExists"), program);
}

/// Asserts that the slot unit is started by nothing but the hook: no `[Install]` section and
/// nothing that makes another unit want it; that its stop timeout covers the settle, the drain
/// and the lease; that it runs as the service account with the hook's group, so that it can
/// traverse the state directory; and that no secret is on its command line or in its
/// environment.
#[test]
fn the_slot_unit_is_not_enabled_covers_the_drain_and_keeps_secrets_off_the_command_line() {
    let unit = read(SLOT_UNIT);
    assert_eq!(unit.lines().filter(|l| l.trim() == "[Install]").count(), 0);
    for key in ["WantedBy", "RequiredBy", "Alias"] {
        assert_eq!(values(&unit, key), Vec::<String>::new(), "{key}");
    }
    let cli = slot_cli(9301);
    assert_eq!(cli.drain_lb_wait, Duration::from_secs(70));
    assert_eq!(cli.drain_deadline, Duration::from_secs(15 * 60));
    assert_eq!(value(&unit, "KillSignal"), "SIGTERM");
    let seconds: u64 = value(&unit, "TimeoutStopSec").parse().expect("seconds");
    assert_eq!(seconds, 1100);
    let drain = cli.drain_lb_wait + cli.drain_deadline + Duration::from_secs(5);
    assert!(
        Duration::from_secs(seconds) >= drain,
        "{seconds} s < {drain:?}"
    );
    assert_eq!(value(&unit, "User"), "tierstore");
    assert_eq!(value(&unit, "SupplementaryGroups"), HOOK_USER);
    let flags: Vec<String> = slot_exec_start(9301)
        .into_iter()
        .filter(|word| word.starts_with("--"))
        .collect();
    let secret_flags: Vec<&String> = flags
        .iter()
        .filter(|flag| SECRET_WORDS.iter().any(|name| flag.contains(*name)))
        .collect();
    assert_eq!(secret_flags.len(), 4, "{secret_flags:?}");
    assert!(
        secret_flags.iter().all(|f| f.ends_with("-file")),
        "{secret_flags:?}"
    );
    assert_eq!(
        unit.lines()
            .filter(|l| l.starts_with("Environment"))
            .count(),
        0
    );
    for (key, want) in [
        ("NoNewPrivileges", "yes"),
        ("ProtectSystem", "strict"),
        ("CapabilityBoundingSet", ""),
    ] {
        assert_eq!(value(&unit, key), want, "{key}");
    }
}

/// Asserts that the sysusers.d entry makes `tierstore-update` an account of its own, with its
/// own group, no login and no home, apart from the service's `tierstore`; that the service is
/// listed in the hook's group (which the hook reads from `/etc/group`), and that the hook's
/// account is put in no group of the service's.
#[test]
fn the_hook_account_is_its_own_and_the_service_is_in_its_group() {
    let entries = read(HOOK_ACCOUNT);
    let lines: Vec<Vec<&str>> = entries
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| l.split_whitespace().collect())
        .collect();
    let users: Vec<&Vec<&str>> = lines.iter().filter(|w| w[0] == "u").collect();
    assert_eq!(users.len(), 1, "{users:?}");
    let user = users[0];
    assert_eq!(user[1], HOOK_USER);
    // `-`: a uid of the system range, and a group of the same name.
    assert_eq!(user[2], "-");
    assert_eq!(user.last(), Some(&"/usr/sbin/nologin"));
    assert!(user.contains(&"/nonexistent"), "{user:?}");
    let service = read("tierstore.conf");
    let service = service
        .lines()
        .find(|l| l.starts_with("u "))
        .expect("u line");
    let service = service.split_whitespace().nth(1).expect("a name");
    assert_ne!(service, HOOK_USER);
    let members: Vec<(&str, &str)> = lines
        .iter()
        .filter(|w| w[0] == "m")
        .map(|w| (w[1], w[2]))
        .collect();
    assert_eq!(members, [(service, HOOK_USER)]);
    let kinds: BTreeSet<&str> = lines.iter().map(|w| w[0]).collect();
    assert_eq!(kinds, BTreeSet::from(["m", "u"]));
}

/// Asserts that the tmpfiles.d entry makes the declaration's state directory and its `slots`
/// directory with mode exactly 0750 (the mode the hook requires, with no group or other write
/// bit), owned by the hook's user and group, and never cleaned.
#[test]
fn the_state_dir_is_0750_of_the_hook_with_no_group_write() {
    let declared = declaration();
    let state_dir = declared.get("state_dir").and_then(toml::Value::as_str);
    let state_dir = state_dir.expect("state_dir");
    let entries = read(STATE_DIR_ENTRY);
    let lines: Vec<Vec<&str>> = entries
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| l.split_whitespace().collect())
        .collect();
    let paths: Vec<&str> = lines.iter().map(|w| w[1]).collect();
    assert_eq!(paths, [state_dir.to_owned(), format!("{state_dir}/slots")]);
    for words in &lines {
        assert_eq!(words.len(), 6, "{words:?}");
        assert_eq!(words[0], "d");
        let mode = u32::from_str_radix(words[2], 8).expect("an octal mode");
        assert_eq!(mode, 0o750);
        assert_eq!(mode & 0o022, 0, "a group or other write bit");
        assert_eq!((words[3], words[4]), (HOOK_USER, HOOK_USER));
        assert_eq!(words[5], "-", "an age would clean it");
    }
}

/// Asserts that the polkit rule grants the hook's account the action that starts and stops a
/// unit for exactly the declaration's two slot units, and no right over unit files; and that
/// the check fails for a rule that names that right or a third unit.
#[test]
fn the_polkit_rule_grants_manage_units_for_the_two_slots_only() {
    let units = strings(table(&declaration(), "hook"), "units");
    let rule = read(POLKIT_RULE);
    assert_eq!(polkit_problems(&rule, &units), Vec::<String>::new());
    let verbs: BTreeSet<&str> = quoted(&rule)
        .into_iter()
        .filter(|w| ["start", "stop", "restart", "reload"].contains(w))
        .collect();
    assert_eq!(verbs, BTreeSet::from(["start", "stop"]));
    let files = rule.replacen(
        MANAGE_UNITS,
        "org.freedesktop.systemd1.manage-unit-files",
        1,
    );
    assert!(
        !polkit_problems(&files, &units).is_empty(),
        "manage-unit-files"
    );
    let third = rule.replacen(
        "\"tierstore@9302.service\"",
        "\"tierstore@9302.service\", \"tierstore@9303.service\"",
        1,
    );
    assert_ne!(third, rule);
    assert!(!polkit_problems(&third, &units).is_empty(), "a third unit");
    let other_user = rule.replace(HOOK_USER, "tierstore");
    assert!(
        !polkit_problems(&other_user, &units).is_empty(),
        "another account"
    );
}

/// Asserts that the record the first install writes (`packaging/README.md`) is one the hook
/// reads and validates against the declaration: a host with no update in flight, serving on
/// slot 9301, which `restore` then starts.
#[cfg(target_os = "linux")]
#[test]
fn the_first_install_record_is_one_the_hook_takes() {
    use tierstore::server::update_hook::{Declaration, Record, validate};
    let readme = read("README.md");
    let line = readme
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("{\"schema\""))
        .expect("the record of the first install");
    let record = Record::decode(line.as_bytes()).expect("a record");
    assert_eq!(record, Record::idle(9301));
    let parsed = Declaration::parse(&read(DECLARATION)).expect("the declaration");
    validate(&record, &parsed).expect("valid against the declaration");
}
