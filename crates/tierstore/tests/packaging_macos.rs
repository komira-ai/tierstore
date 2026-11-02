//! The files of a Mac (`packaging/macos`) against the binary and the update hook: the launchd
//! plist (`com.komira.tierstore.plist`) runs the binary of the release link `current` as
//! `_tierstore`, kept alive and started at load, with the build-node file (`tierstore.toml`)
//! that serves `[local_cache]` on loopback; the binary accepts that command line and that file;
//! and the plist, the file, the declaration (`updater.d/tierstore.toml`) and the hook name the
//! same job, program, account and port, so that none of them can drift from the others.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tierstore::server::update_hook::machine::{
    LAUNCHD_LABEL, LAUNCHD_LINKS_DIR, LAUNCHD_PLIST, LAUNCHD_PROGRAM,
};
use tierstore::server::update_hook::{LaunchdDeclaration, SingleRecord};
use tierstore_testkit::dir::TestDir;

/// The words in the name of a flag or a key that say it holds a secret.
const SECRET_WORDS: [&str; 5] = ["key", "secret", "credential", "token", "password"];

/// A value of a property list.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Text(String),
    Integer(i64),
    Bool(bool),
    Array(Vec<Value>),
    Dict(BTreeMap<String, Value>),
}

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/macos")
}

fn read(file: &str) -> String {
    std::fs::read_to_string(dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"))
}

/// The value an XML element of a property list stands for.
fn value(node: roxmltree::Node<'_, '_>) -> Value {
    let text = || node.text().unwrap_or_default().to_owned();
    match node.tag_name().name() {
        "string" => Value::Text(text()),
        "integer" => Value::Integer(text().parse().expect("an integer")),
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "array" => Value::Array(
            node.children()
                .filter(|n| n.is_element())
                .map(value)
                .collect(),
        ),
        "dict" => {
            let items: Vec<_> = node.children().filter(|n| n.is_element()).collect();
            let mut dict = BTreeMap::new();
            for pair in items.chunks(2) {
                assert_eq!(pair[0].tag_name().name(), "key", "{pair:?}");
                let key = pair[0].text().unwrap_or_default().to_owned();
                assert!(
                    dict.insert(key.clone(), value(pair[1])).is_none(),
                    "{key} twice"
                );
            }
            Value::Dict(dict)
        }
        other => panic!("<{other}> is not a value of a property list"),
    }
}

/// The plist's top-level dictionary.
fn plist() -> BTreeMap<String, Value> {
    let text = read("com.komira.tierstore.plist");
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(&text, options).expect("XML");
    let root = doc.root_element();
    assert_eq!(root.tag_name().name(), "plist");
    assert_eq!(root.attribute("version"), Some("1.0"));
    let mut top = root.children().filter(|n| n.is_element());
    let Some(Value::Dict(dict)) = top.next().map(value) else {
        panic!("the plist holds one dictionary");
    };
    assert!(top.next().is_none());
    dict
}

fn text<'a>(dict: &'a BTreeMap<String, Value>, key: &str) -> &'a str {
    match dict.get(key) {
        Some(Value::Text(text)) => text,
        other => panic!("{key} is {other:?}"),
    }
}

fn integer(dict: &BTreeMap<String, Value>, key: &str) -> i64 {
    match dict.get(key) {
        Some(Value::Integer(n)) => *n,
        other => panic!("{key} is {other:?}"),
    }
}

/// The plist's `ProgramArguments`.
fn arguments() -> Vec<String> {
    match plist().get("ProgramArguments") {
        Some(Value::Array(words)) => words
            .iter()
            .map(|word| match word {
                Value::Text(text) => text.clone(),
                other => panic!("an argument is {other:?}"),
            })
            .collect(),
        other => panic!("ProgramArguments is {other:?}"),
    }
}

/// The value the plist gives `flag`, which it gives once.
fn flag(flag: &str) -> String {
    let words = arguments();
    let at: Vec<usize> = (0..words.len()).filter(|i| words[*i] == flag).collect();
    assert_eq!(at.len(), 1, "{flag} appears once: {words:?}");
    words[at[0] + 1].clone()
}

/// A duration of the binary's flags, whole seconds or minutes (`1s`, `60s`, `15m`).
fn duration(word: &str) -> Duration {
    let (number, unit) = word.split_at(word.len() - 1);
    let number: u64 = number.parse().unwrap_or_else(|_| panic!("{word}"));
    match unit {
        "s" => Duration::from_secs(number),
        "m" => Duration::from_secs(number * 60),
        _ => panic!("{word} is not in seconds or minutes"),
    }
}

/// The build-node file.
fn config() -> toml::Table {
    read("tierstore.toml").parse().expect("TOML")
}

fn table<'a>(table: &'a toml::Table, key: &str) -> &'a toml::Table {
    table[key].as_table().unwrap_or_else(|| panic!("[{key}]"))
}

fn declaration() -> LaunchdDeclaration {
    LaunchdDeclaration::parse(&read("updater.d/tierstore.toml")).expect("the declaration")
}

/// Asserts that the plist is a LaunchDaemon of the job the hook stops and starts: its label is
/// the hook's, it runs as `_tierstore` (the account the declaration names) in that account's
/// group, launchd starts it at load and keeps it alive, its program is the binary of the release
/// `current` names, its output goes to the log directory, and it sets no environment variable.
/// It would catch a plist that runs as root, one that launchd does not restart, and one that
/// hands the process configuration in its environment.
#[test]
fn the_plist_runs_the_current_release_as_the_service_account_kept_alive() {
    let dict = plist();
    assert_eq!(text(&dict, "Label"), LAUNCHD_LABEL);
    assert_eq!(text(&dict, "UserName"), "_tierstore");
    assert_eq!(text(&dict, "UserName"), declaration().service);
    assert_eq!(text(&dict, "GroupName"), "_tierstore");
    assert_eq!(dict.get("RunAtLoad"), Some(&Value::Bool(true)));
    assert_eq!(dict.get("KeepAlive"), Some(&Value::Bool(true)));
    assert_eq!(arguments()[0], LAUNCHD_PROGRAM);
    for key in ["StandardOutPath", "StandardErrorPath"] {
        let log = Path::new(text(&dict, key));
        assert!(
            log.starts_with(Path::new(LAUNCHD_LINKS_DIR).join("log")),
            "{key}: {log:?}"
        );
    }
    for key in dict.keys() {
        assert!(!key.contains("Environment"), "{key}");
    }
    assert_eq!(integer(&dict, "Umask"), 0o077);
}

/// Asserts that the plist's command line is `--config` with the file's place and the two flags
/// of the drain, each once and nothing else (a start refuses any other flag beside a file; the
/// test below runs the binary on it), and that no key of the file holds a secret (each names a
/// file); and that the file is a build node that serves on loopback only: `[local_cache]` and `[cold_store]` and no
/// `[hot_store]`, `listen` exactly `127.0.0.1:9300`, and `slot` the same port. It would catch a
/// Mac that serves the network, or that runs a storage node instead of a cache.
#[test]
fn the_plist_runs_a_local_cache_on_loopback_from_its_file() {
    let words = arguments();
    assert_eq!(words.len(), 7, "{words:?}");
    assert_eq!(
        Path::new(&flag("--config")),
        Path::new(LAUNCHD_LINKS_DIR).join("etc/tierstore.toml")
    );
    let flags: Vec<&String> = words.iter().filter(|w| w.starts_with("--")).collect();
    assert_eq!(flags, ["--config", "--drain-lb-wait", "--drain-deadline"]);
    let file = config();
    assert!(file.contains_key("local_cache"), "a build node");
    assert!(file.contains_key("cold_store"));
    assert!(!file.contains_key("hot_store"), "not a storage node");
    let server = table(&file, "server");
    assert_eq!(server["listen"].as_str(), Some("127.0.0.1:9300"));
    assert_eq!(server["slot"].as_integer(), Some(9300));
    for (name, section) in [
        ("server", server),
        ("cold_store", table(&file, "cold_store")),
    ] {
        for (key, value) in section {
            if SECRET_WORDS.iter().any(|word| key.contains(word)) {
                let paths: Vec<&str> = match value {
                    toml::Value::String(path) => vec![path.as_str()],
                    toml::Value::Array(paths) => {
                        paths.iter().filter_map(toml::Value::as_str).collect()
                    }
                    other => panic!("{name}.{key} is {other}"),
                };
                assert!(key.ends_with("_file") || key.ends_with("_files"), "{key}");
                for path in paths {
                    assert!(Path::new(path).is_absolute(), "{name}.{key}: {path}");
                }
            }
        }
    }
}

/// Asserts that the binary itself takes the plist's command line and the shipped file: the
/// file, copied into a scratch directory with its key files and its cache there (keys of the
/// form the process reads, mode 0600), is checked by the binary run with the plist's own
/// arguments and `--check-config`, which reads the file and the key files and binds nothing, and
/// it exits 0 and says nothing. It would catch a file or a flag the binary refuses, which would
/// leave the job restarting for ever.
#[test]
fn the_binary_accepts_the_plists_command_line_and_file() {
    let scratch = TestDir::under(std::env::temp_dir(), "packaging-macos-check");
    let mut text = read("tierstore.toml");
    for name in ["access.key", "daemons.key", "upstream-read.key"] {
        let key = scratch.path().join(name);
        std::fs::write(&key, format!("ID{}\nsecret-{name}\n", name.len())).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let shipped = format!("{LAUNCHD_LINKS_DIR}/etc/{name}");
        assert!(text.contains(&shipped), "{shipped}");
        text = text.replace(&shipped, &key.display().to_string());
    }
    let cache = format!("{LAUNCHD_LINKS_DIR}/cache");
    text = text.replace(&cache, &scratch.path().join("cache").display().to_string());
    let file = scratch.path().join("tierstore.toml");
    std::fs::write(&file, text).unwrap();
    let mut words = arguments();
    words[2] = file.display().to_string();
    words.push("--check-config".to_owned());
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .args(&words[1..])
        .output()
        .expect("the binary runs");
    assert_eq!(
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned()
        ),
        (Some(0), String::new(), String::new())
    );
}

/// Asserts that the plist, the declaration, the file and the hook cannot drift: the plist is
/// installed under the name the hook bootstraps (`LAUNCHD_PLIST`), its label is the one the
/// declaration's `hook.service` must name (the hook's own parser takes the declaration), its
/// program is the binary of the link the hook swaps, and the one port of the declaration, which
/// the hook probes, is the port the file listens on and the slot label it reports. It would catch
/// a hook that restarts a job the plist does not define, or probes a port nothing serves.
#[test]
fn the_plist_the_declaration_the_file_and_the_hook_name_one_job() {
    let installed = Path::new(LAUNCHD_PLIST);
    assert_eq!(
        installed.file_name().unwrap(),
        "com.komira.tierstore.plist",
        "the file in packaging/macos"
    );
    assert!(dir().join(installed.file_name().unwrap()).is_file());
    assert_eq!(
        installed.parent(),
        Some(Path::new("/Library/LaunchDaemons"))
    );
    let decl = read("updater.d/tierstore.toml");
    assert!(decl.contains(&format!("service = \"{LAUNCHD_LABEL}\"")));
    assert_eq!(text(&plist(), "Label"), LAUNCHD_LABEL);
    assert_eq!(arguments()[0], LAUNCHD_PROGRAM);
    let port = declaration().port;
    let file = config();
    let server = table(&file, "server");
    let listen = server["listen"].as_str().unwrap();
    assert_eq!(listen, format!("127.0.0.1:{port}"));
    assert_eq!(server["slot"].as_integer(), Some(i64::from(port)));
}

/// Asserts that launchd lets the drain finish before it kills the process: `ExitTimeOut` is at
/// least the plist's `--drain-lb-wait` plus `--drain-deadline` plus the 5 s a lease release may
/// take, and the declaration keeps the Mac on a plain restart. The declaration's step timeouts
/// against every bound of the hook (the calls, the requests, the rollback) are checked where those
/// bounds are, in `src/server/update_hook/launchd_system_tests.rs`. It would catch a SIGKILL of a
/// drain in progress.
#[test]
fn launchd_and_the_updater_wait_for_the_drain() {
    let drain = duration(&flag("--drain-lb-wait"))
        + duration(&flag("--drain-deadline"))
        + Duration::from_secs(5);
    let exit = Duration::from_secs(integer(&plist(), "ExitTimeOut").try_into().unwrap());
    assert!(exit >= drain, "{exit:?} is less than {drain:?}");
    let decl: toml::Table = read("updater.d/tierstore.toml").parse().unwrap();
    assert_eq!(
        table(&decl, "hook")["params"]["mode"].as_str(),
        Some("restart")
    );
}

/// Asserts that the record the first install of a Mac lays down, as the install guide shows it,
/// is one the hook's steps take: an idle single-instance record. It would catch a guide whose
/// first record every step refuses.
#[test]
fn the_first_install_record_of_a_mac_is_one_the_hook_takes() {
    let guide =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/update-hook.md"))
            .unwrap();
    let line = guide
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("{\"schema\":1,\"kind\":\"single\""))
        .expect("the record of the first install of a Mac");
    let record = SingleRecord::decode(line.as_bytes()).expect("a record");
    assert_eq!(record, SingleRecord::idle());
    assert_eq!(record.check(), Ok(()));
}
