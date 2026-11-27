//! The load of each host configuration the build farm runs (`live_configs.rs`): the shape each
//! file makes, the refusal of the Mac's file from before its switch to one process, the mount that
//! the Mac's `fs.toml` and its `fs` interface both name, and the check of the list of sources that
//! sits beside the copies.

use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

use crate::server::config::{PerRun, settings_from_text};
use crate::server::config_cache::CacheSettings;
use crate::server::config_interface::NO_FS;
use crate::server::config_testing::DEFAULTS;
use crate::server::config_testing::live_configs::{self, HOST};
use crate::server::update_hook::launchd::fs::mountpoint_of;

/// The knobs of a run on the farm's accounts.
const RUN: PerRun = PerRun {
    host: HOST,
    ..DEFAULTS
};

/// The build node `text` makes, in a scratch directory that holds the files it reads.
fn cache_of(text: &str) -> CacheSettings {
    settings_from_text(text, RUN)
        .expect("a file that loads")
        .into_cache()
        .expect("a build node")
}

/// A scratch directory with the key files `keys` and the facts file in it.
fn scratch(name: &str, keys: &[&str]) -> TestDir {
    let dir = TestDir::under(std::env::temp_dir(), name);
    live_configs::write_keys(dir.path(), keys);
    dir
}

/// Asserts that the Linux node's file loads as a build node: its `fs` interface has the mount
/// point of the file, the farm's group (the test's own) and the owner that the job user's
/// subordinate ids make of id 1000, for the user and the group both; its put directory is on, for
/// the prefix `cas/b/`, staged beside the cache; its mount provider is served, at that same mount
/// point, on its own socket; and it has no put ingest. Catches a file whose provider answers
/// under another mount than the one the file system is served at, an owner left at the job user's
/// own id, and a put directory that is not served.
#[cfg(target_os = "linux")]
#[test]
fn the_linux_file_loads_as_a_build_node_serving_its_provider_over_fuse() {
    let dir = scratch("lc-linux", &live_configs::LINUX_KEYS);
    let node = cache_of(&live_configs::linux_on(dir.path()));
    let mount = dir.path().join("mnt");
    let fs = node.fs.expect("an fs interface");
    assert_eq!(fs.mountpoint, mount);
    assert_eq!(fs.runtime_dir, dir.path().join("run"));
    assert_eq!(fs.client_group, rustix::process::getegid().as_raw());
    let owner = live_configs::SUBID_START + 1000;
    assert_eq!(fs.owner, (owner, owner));
    assert_eq!(fs.cache_path, dir.path().join("cache/build-cas"));
    let put = fs.put.expect("a put directory");
    assert_eq!(put.store.prefixes, ["cas/b/"]);
    assert_eq!(put.stage_dir, dir.path().join("cache/build-cas/put-stage"));
    let provider = node.provider.expect("a mount provider");
    assert_eq!(provider.mountpoint, mount);
    assert_eq!(provider.socket, dir.path().join("state/provider.sock"));
    assert!(provider.served);
    assert_eq!(node.put_ingest, None);
    assert_eq!(node.front.listen.len(), 1);
}

/// Asserts that the Mac node's file loads as a build node: its `fs` interface is served over NFS
/// on loopback, at the mount point of the file, for the farm's group and its `_tierstore` user and
/// group (the test's own); its mount provider is served at that mount point on a socket of the
/// run directory; its put ingest is the directory the file names; it has no put directory and no
/// handoff. Catches a Mac file whose provider answers directories under a
/// mount that no `fs` interface serves, and a put ingest the load drops.
#[test]
fn the_mac_file_loads_as_a_build_node_serving_its_provider_over_nfs() {
    let dir = scratch("lc-mac", &live_configs::MAC_KEYS);
    let node = cache_of(&live_configs::mac_on(dir.path()));
    let mount = dir.path().join("mnt");
    let fs = node.fs.expect("an fs interface");
    assert_eq!(fs.mountpoint, mount);
    assert_eq!(fs.runtime_dir, dir.path().join("run/fs"));
    let (uid, gid) = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    assert_eq!((fs.client_group, fs.owner), (gid, (uid, gid)));
    assert!(fs.nfs.listen.ip().is_loopback());
    assert!(fs.nfs.mount);
    assert!(fs.put.is_none());
    let provider = node.provider.expect("a mount provider");
    assert_eq!(provider.mountpoint, mount);
    assert_eq!(provider.socket, dir.path().join("run/provider.sock"));
    assert!(provider.served);
    assert_eq!(node.put_ingest, Some(dir.path().join("put")));
    assert!(node.handoff.is_none());
}

/// Asserts that the Mac node's file from before its switch to one process, which has a mount
/// provider and no `fs` interface, is refused at its load with the message that names the
/// provider's entry and says why, in full. Catches a release that loads such a file and starts a
/// node that answers every job's tree `UNAVAILABLE` for ever.
#[test]
fn the_mac_file_before_the_switch_is_refused_for_its_provider_without_an_fs_interface() {
    let dir = scratch("lc-before", &live_configs::MAC_KEYS);
    let text = live_configs::mac_before_switch_on(dir.path());
    let refused = settings_from_text(&text, RUN).expect_err("a file that is refused");
    assert_eq!(refused.to_string(), format!("interface[1].type: {NO_FS}"));
}

/// The `mountpoint` of each `[[interface]]` of type `kind` in the text of a farm file.
fn mounts(text: &str, kind: &str) -> Vec<String> {
    let file: toml::Table = text.parse().expect("TOML");
    file["interface"]
        .as_array()
        .expect("[[interface]]")
        .iter()
        .filter(|entry| entry["type"].as_str() == Some(kind))
        .map(|entry| {
            let mount = entry["params"]["mountpoint"].as_str();
            mount.expect("a mountpoint").to_owned()
        })
        .collect()
}

/// Asserts that each file with a mount provider (the Linux and the Mac node's) names one mount
/// point, the farm's, for its provider and for its `fs` interface alike, wherever the file
/// writes them. Catches a copy whose provider answers directories under a mount that its file
/// system does not serve.
#[test]
fn each_file_names_one_mount_for_its_provider_and_its_fs_interface() {
    for (text, mount) in [
        (live_configs::LINUX, live_configs::LINUX_MOUNT),
        (live_configs::MAC, live_configs::MAC_MOUNT),
    ] {
        assert_eq!(mounts(text, "fs"), [mount]);
        assert_eq!(mounts(text, "mount-provider"), [mount]);
    }
}

/// Asserts that the mount point `[fs] mountpoint` of the Mac's `fs.toml`, which the config hook
/// reads at the switch to one process, is the `mountpoint` of the `fs` interface of the Mac's node
/// file, as the farm writes both. Catches a switch that waits for a mount at a place where the
/// node does not serve one.
#[test]
fn the_mac_fs_file_names_the_mount_of_the_fs_interface() {
    let hook = mountpoint_of(live_configs::MAC_FS).expect("an fs.toml that the hook reads");
    assert_eq!(hook, Path::new(live_configs::MAC_MOUNT));
    assert_eq!(
        mounts(live_configs::MAC, "fs"),
        [hook.display().to_string()]
    );
}

/// The files of `tests/live_configs`, by name, that end in `.toml`.
fn copies() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/live_configs");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("the directory of the copies")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".toml"))
        .collect();
    names.sort();
    names
}

/// The defects of `sums`, the text of `sources.sha256`, against `files`, the names of the
/// `.toml` copies beside it: a line that is not a SHA-256 and a name in the form of `sha256sum`
/// (`<64 lowercase hex digits>  <name>`), a name twice, a copy with no line and a line with no copy.
fn defects(sums: &str, files: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut named: Vec<&str> = Vec::new();
    for (at, line) in sums.lines().enumerate() {
        let Some((hash, name)) = line.split_once("  ") else {
            problems.push(format!("line {}: not a hash and a name", at + 1));
            continue;
        };
        if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            problems.push(format!("{name}: {hash:?} is not 64 lowercase hex digits"));
        }
        if named.contains(&name) {
            problems.push(format!("{name}: two lines"));
        }
        named.push(name);
    }
    for file in files {
        if !named.contains(&file.as_str()) {
            problems.push(format!("{file}: no line"));
        }
    }
    for name in named {
        if !files.iter().any(|file| file == name) {
            problems.push(format!("{name}: no file"));
        }
    }
    problems
}

/// Asserts that `sources.sha256` has one line for each `.toml` copy of the directory and none
/// other, each a lowercase SHA-256 in the form of `sha256sum`, and that the four copies are the
/// four the README names. Catches a copy added or removed without its line.
#[test]
fn the_sources_file_has_one_sha256_line_for_each_copy() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/live_configs");
    let sums = std::fs::read_to_string(dir.join("sources.sha256")).expect("sources.sha256");
    let files = copies();
    assert_eq!(
        files,
        [
            "linux-build-node.toml",
            "mac-build-node-before-switch.toml",
            "mac-build-node.toml",
            "mac-fs.toml"
        ]
    );
    assert_eq!(defects(&sums, &files), Vec::<String>::new());
}

/// Asserts that the check of the sources file names each defect it is made to find: a copy with no
/// line, a line with no copy, a hash of capitals, a hash too short, a name twice and a line that
/// is no hash and name at all; and that it names none for a file that is as it should be.
/// Catches a check that passes a list that does not match the directory.
#[test]
fn the_sources_check_names_each_defect() {
    let hash = "0a7f1461dfed630f68dbaa707d69b718ed175e9b30cbca702c72a6ad3565ab6f";
    let files = ["a.toml".to_owned(), "b.toml".to_owned()];
    let good = format!("{hash}  a.toml\n{hash}  b.toml\n");
    assert_eq!(defects(&good, &files), Vec::<String>::new());
    let capitals = hash.to_uppercase();
    let short = &hash[1..];
    let cases = [
        (
            format!("{hash}  a.toml\n"),
            vec!["b.toml: no line".to_owned()],
        ),
        (
            format!("{good}{hash}  c.toml\n"),
            vec!["c.toml: no file".to_owned()],
        ),
        (
            format!("{capitals}  a.toml\n{hash}  b.toml\n"),
            vec![format!(
                "a.toml: {capitals:?} is not 64 lowercase hex digits"
            )],
        ),
        (
            format!("{short}  a.toml\n{hash}  b.toml\n"),
            vec![format!("a.toml: {short:?} is not 64 lowercase hex digits")],
        ),
        (
            format!("{good}{hash}  a.toml\n"),
            vec!["a.toml: two lines".to_owned()],
        ),
        (
            format!("{good}{hash} c.toml\n"),
            vec!["line 3: not a hash and a name".to_owned()],
        ),
    ];
    for (text, want) in cases {
        assert_eq!(defects(&text, &files), want, "{text}");
    }
}
