//! The build-node file of a Mac (`packaging/macos/tierstore.toml`) against the binary and the
//! launchd plist of hostkeeper's package (`packaging/hostkeeper/units/tierstore.plist`): the plist
//! names the file, the binary accepts that command line and that
//! file, and the file serves `[local_cache]` on loopback only, on the port the update hook probes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tierstore_testkit::dir::TestDir;

/// The directory of the Mac's release links, configuration, cache and logs.
const LAUNCHD_LINKS_DIR: &str = "/usr/local/tierstore";
/// Where hostkeeper writes the facts file that the proxy id is read from.
const FACTS_PATH: &str = "/var/lib/hostkeeper/facts.json";
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

/// The launchd plist of hostkeeper's package, which starts the job on a Mac.
fn plist_text() -> String {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/hostkeeper/units/tierstore.plist");
    std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()))
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
    plist_in(&plist_text())
}

/// The launchd plist of the installer package (`cargo xtask release macos-pkg`).
fn package_plist_text() -> String {
    let file =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/macos-pkg/tierstore.plist");
    std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()))
}

/// The top-level dictionary of the property list `text`.
fn plist_in(text: &str) -> BTreeMap<String, Value> {
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(text, options).expect("XML");
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

/// The build-node file.
fn config() -> toml::Table {
    read("tierstore.toml").parse().expect("TOML")
}

/// The entries of the file's `[[interface]]` of type `kind`.
fn interfaces<'a>(file: &'a toml::Table, kind: &str) -> Vec<&'a toml::Value> {
    let entries = file["interface"].as_array().expect("[[interface]]");
    entries
        .iter()
        .filter(|entry| entry["type"].as_str() == Some(kind))
        .collect()
}

/// The addresses of the one `s3` interface of the file, which has, besides it, only its mount
/// provider, which listens on no address.
fn interface_addresses(file: &toml::Table) -> Vec<String> {
    let entries = file["interface"].as_array().expect("[[interface]]");
    assert_eq!(entries.len(), 2, "the s3 interface and the mount provider");
    let s3 = interfaces(file, "s3");
    assert_eq!(s3.len(), 1, "one s3 interface");
    let addresses = s3[0]["params"]["addresses"].as_array().expect("addresses");
    addresses
        .iter()
        .map(|address| address.as_str().expect("an address").to_owned())
        .collect()
}

fn table<'a>(table: &'a toml::Table, key: &str) -> &'a toml::Table {
    table[key].as_table().unwrap_or_else(|| panic!("[{key}]"))
}

/// Asserts that the plist's command line is `--config` with the file's place
/// of the drain, each once and nothing else (a start refuses any other flag beside a file; the
/// test below runs the binary on it), and that no key of the file holds a secret (each names a
/// file); and that the file is a build node that serves on loopback only: `[local_cache]` and `[cold_store]` and no
/// `[hot_store]`, one `s3` interface at exactly `127.0.0.1:9300`, and `slot` the same port (the port the update hook probes; it does not read `slot`). It would catch a
/// Mac that serves the network, or that runs a storage node instead of a cache.
#[test]
fn the_plist_runs_a_local_cache_on_loopback_from_its_file() {
    let words = arguments();
    assert_eq!(words.len(), 3, "{words:?}");
    assert_eq!(
        Path::new(&flag("--config")),
        Path::new(LAUNCHD_LINKS_DIR).join("etc/tierstore.toml")
    );
    let flags: Vec<&String> = words.iter().filter(|w| w.starts_with("--")).collect();
    assert_eq!(flags, ["--config"]);
    let file = config();
    assert!(file.contains_key("local_cache"), "a build node");
    assert!(file.contains_key("cold_store"));
    assert!(!file.contains_key("hot_store"), "not a storage node");
    let server = table(&file, "server");
    assert_eq!(interface_addresses(&file), ["127.0.0.1:9300"]);
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

/// Asserts that the file is only a local read cache: the proxy id is made from the facts file
/// hostkeeper writes, with the prefix `tierstore-cache-` (and no literal `proxy_id`); there is no
/// hot tier, no `handoff_socket` and no `[oci]` or `[trees]` role; `[local_cache]` fills whole
/// objects up to 128MiB; `[cold_store]` is the `s3` store of the six storage nodes' S3 servers
/// (six distinct documentation addresses at port 9300, tried in turn); and the one policy is
/// `cas/`, cached. It would catch a Mac that took a storage node's role, a proxy id that two Macs
/// share, or a cache that reaches only one storage node.
#[test]
fn the_file_is_a_cache_of_the_storage_nodes_with_a_proxy_id_from_the_facts_file() {
    let file = config();
    assert!(!file.contains_key("hot_store"));
    assert!(!file.contains_key("oci"));
    assert!(!file.contains_key("trees"));
    let server = table(&file, "server");
    assert!(!server.contains_key("proxy_id"));
    assert!(!server.contains_key("handoff_socket"));
    let facts = table(server, "proxy_id_from_facts");
    assert_eq!(facts.len(), 2);
    assert_eq!(facts["path"].as_str(), Some(FACTS_PATH));
    assert_eq!(facts["prefix"].as_str(), Some("tierstore-cache-"));
    let cache = table(&file, "local_cache");
    assert_eq!(cache["whole_fill_limit"].as_str(), Some("128MiB"));
    assert!(
        cache["path"]
            .as_str()
            .is_some_and(|path| Path::new(path).starts_with(format!("{LAUNCHD_LINKS_DIR}/cache")))
    );
    let cold = table(&file, "cold_store");
    assert_eq!(cold["kind"].as_str(), Some("s3"));
    assert!(!cold.contains_key("endpoint"));
    let endpoints: Vec<&str> = cold["endpoints"]
        .as_array()
        .expect("endpoints")
        .iter()
        .map(|endpoint| endpoint.as_str().expect("an endpoint"))
        .collect();
    assert_eq!(endpoints.len(), 6, "{endpoints:?}");
    for (at, endpoint) in endpoints.iter().enumerate() {
        assert_eq!(*endpoint, format!("http://192.0.2.{}:9300", at + 1));
    }
    let policies = file["policy"].as_array().expect("[[policy]]");
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0]["prefix"].as_str(), Some("cas/"));
    assert_eq!(policies[0]["cache"].as_bool(), Some(true));
}

/// Asserts that the Mac runs the mount provider jobscheduler's agent asks for a job's tree: one
/// `mount-provider` interface, on a socket in a directory of its own under the Mac's tierstore
/// directory, writing each tree whole into `job-trees` there (which the package's postinstall
/// makes) rather than answering a directory of `tierstore fs`, whose network volume a Mac's jobs
/// cannot read. It would catch a Mac whose jobs cannot have their inputs made local before they
/// start, or are answered a directory they cannot read.
#[test]
fn the_mac_runs_the_mount_provider_on_its_socket() {
    let file = config();
    let providers = interfaces(&file, "mount-provider");
    assert_eq!(providers.len(), 1);
    let params = providers[0]["params"].as_table().expect("params");
    assert_eq!(params.len(), 2, "{params:?}");
    assert_eq!(
        params["socket"].as_str(),
        Some(format!("{LAUNCHD_LINKS_DIR}/run/provider.sock").as_str())
    );
    assert_eq!(
        params["local_trees"].as_str(),
        Some(format!("{LAUNCHD_LINKS_DIR}/job-trees").as_str())
    );
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
        std::fs::write(
            &key,
            format!("ID{}\nsecret-{name}-0123456789\n", name.len()),
        )
        .unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let shipped = format!("{LAUNCHD_LINKS_DIR}/etc/{name}");
        assert!(text.contains(&shipped), "{shipped}");
        text = text.replace(&shipped, &key.display().to_string());
    }
    let facts = scratch.path().join("facts.json");
    std::fs::write(&facts, r#"{"schema":1,"host_id":"mac-7","labels":{}}"#).unwrap();
    assert!(text.contains(FACTS_PATH), "{FACTS_PATH}");
    text = text.replace(FACTS_PATH, &facts.display().to_string());
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

/// The program of the installer package's plist: the binary the package lays down.
const PACKAGE_PROGRAM: &str = "/Library/Application Support/tierstore/bin/tierstore";

/// The `ProgramArguments` of the property list `dict`.
fn words(dict: &BTreeMap<String, Value>) -> Vec<String> {
    match dict.get("ProgramArguments") {
        Some(Value::Array(words)) => words
            .iter()
            .map(|w| match w {
                Value::Text(t) => t.clone(),
                other => panic!("an argument is {other:?}"),
            })
            .collect(),
        other => panic!("ProgramArguments is {other:?}"),
    }
}

/// Asserts that the plist of the installer package, and the plist of hostkeeper's archive package,
/// set the soft and the hard `NumberOfFiles` to exactly 1048576. It would catch either plist going
/// back to a smaller limit, with which the open files of a build's jobs would run the process out
/// of descriptors.
#[test]
fn both_plists_allow_1048576_open_files() {
    for (name, plist) in [
        ("archive", plist()),
        ("package", plist_in(&package_plist_text())),
    ] {
        for limits in ["SoftResourceLimits", "HardResourceLimits"] {
            let Some(Value::Dict(limit)) = plist.get(limits) else {
                panic!("{name}: {limits} is a dictionary");
            };
            assert_eq!(
                limit.get("NumberOfFiles"),
                Some(&Value::Integer(1_048_576)),
                "{name}: {limits}"
            );
        }
    }
}

/// Asserts that the plist of the installer package is the plist of hostkeeper's archive package
/// but for the program: every key is the same, and the first of `ProgramArguments` is the
/// package's binary instead of the copy the update hook installs for the archive, the rest of the
/// command line unchanged. It
/// would catch a change to one plist that the other does not follow, which would run a Mac with
/// another account, limit or drain than the archive's.
#[test]
fn the_package_plist_differs_from_the_archive_plist_only_in_the_program() {
    let archive = plist();
    let package = plist_in(&package_plist_text());
    let mut changed = archive.clone();
    let Some(Value::Array(program)) = changed.get_mut("ProgramArguments") else {
        panic!("ProgramArguments is an array");
    };
    assert_eq!(
        program[0],
        Value::Text("/usr/local/libexec/tierstore/tierstore".to_owned())
    );
    program[0] = Value::Text(PACKAGE_PROGRAM.to_owned());
    assert_eq!(package, changed);
}

/// Asserts that the binary accepts the command line of the installer package's plist: run with
/// the plist's flags and `--check-config`, it gets past the flags and refuses the one thing a
/// scratch machine lacks, the file `--config` names (it says so, and does not say it met an
/// argument it does not know). It would catch a flag the binary refuses, which would leave the job
/// restarting for ever.
#[test]
fn the_binary_accepts_the_package_plists_command_line() {
    let words = words(&plist_in(&package_plist_text()));
    assert_eq!(words[0], PACKAGE_PROGRAM);
    assert_eq!(words[1], "--config");
    let mut args = words[1..].to_vec();
    args.push("--check-config".to_owned());
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .args(&args)
        .output()
        .expect("the binary runs");
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success());
    assert!(!said.contains("unexpected argument"), "{said}");
    assert!(said.contains("tierstore.toml"), "{said}");
}
