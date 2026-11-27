//! The host configurations the build farm runs (`tests/live_configs/`, whose README says what each
//! is) against the binary: each current file, moved onto a scratch directory with fixture key
//! files, is checked by `tierstore --config <file> --check-config` run with an empty environment,
//! and the Mac's file from before its switch to one process is refused with the one line the
//! binary prints.
//!
//! The binary reads the accounts of the host it runs on, which a test cannot rely on: the farm's
//! account names are replaced by the numeric ids of the test's own user and group. The Linux
//! file's `owner = { subids_of = "jsjob", id = 1000 }` is replaced the same way, because the binary
//! reads the host's `/etc/subuid`; `src/server/live_configs_tests.rs` loads it with a table of its
//! own.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use tierstore_testkit::dir::TestDir;

/// The farm's files, as `tests/live_configs/` holds them.
#[cfg(target_os = "linux")]
const LINUX: &str = include_str!("live_configs/linux-build-node.toml");
const MAC: &str = include_str!("live_configs/mac-build-node.toml");
const MAC_BEFORE_SWITCH: &str = include_str!("live_configs/mac-build-node-before-switch.toml");

/// Why the loader refuses a mount provider in a file with no `fs` interface (the text of
/// `NO_FS` in `src/server/config_interface.rs`, which the binary prints after the entry's place).
const NO_FS: &str = "a mount-provider needs an [[interface]] with type = \"fs\" in the same file: the provider answers directories under that interface's mount, and no other process serves one (tierstore fs is no longer a process of its own)";

/// `text` with every `from` replaced by `to`, where `from` occurs exactly `count` times.
fn swap(text: &str, from: &str, to: &str, count: usize) -> String {
    let found = text.matches(from).count();
    assert_eq!(found, count, "{from:?} occurs {found} times, not {count}");
    text.replace(from, to)
}

/// The numeric ids of the user and the group of this process.
fn ids() -> (u32, u32) {
    (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    )
}

/// Writes the key files `names` (mode 0600, a key id and a secret of their own each) and the
/// facts file of hostkeeper in `dir`, and returns the secrets.
fn write_keys(dir: &Path, names: &[&str]) -> Vec<String> {
    std::fs::create_dir_all(dir.join("etc")).unwrap();
    let mut secrets = Vec::new();
    for (n, name) in names.iter().enumerate() {
        let path = dir.join("etc").join(name);
        let secret = format!("integration-fixture-secret-{n:04}");
        std::fs::write(&path, format!("AKIDINTEG{n}\n{secret}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        secrets.push(secret);
    }
    std::fs::write(
        dir.join("facts.json"),
        "{\"schema\":1,\"host_id\":\"node-1\"}\n",
    )
    .unwrap();
    secrets
}

/// Writes the scoped key file `name` (mode 0600; the JSON of a key id, a secret and the prefixes
/// it may read, write and delete) in `dir`, and returns its secret.
#[cfg(target_os = "linux")]
fn write_scoped_key(dir: &Path, name: &str) -> String {
    let secret = "integration-scoped-secret-0000".to_owned();
    let path = dir.join("etc").join(name);
    let json = format!(
        "{{\"access_key_id\":\"AKIDSCOPED\",\"secret_access_key\":\"{secret}\",\"read_prefixes\":[\"cas/\"],\"write_prefixes\":[],\"delete_prefixes\":[]}}\n"
    );
    std::fs::write(&path, json).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    secret
}

/// The Linux file moved onto `dir`, its farm accounts the ids of this process.
#[cfg(target_os = "linux")]
fn linux_on(dir: &Path) -> String {
    let at = dir.display();
    let (uid, gid) = ids();
    let text = swap(
        LINUX,
        "params = { port = 9300 }",
        "params = { addresses = [\"127.0.0.1:0\"] }",
        1,
    );
    let text = swap(&text, "\"/var/lib/tierstore/", &format!("\"{at}/state/"), 2);
    let text = swap(&text, "\"/run/tierstore/fs\"", &format!("\"{at}/mnt\""), 2);
    let text = swap(
        &text,
        "tree_list_ram_bytes = 2147483648\n",
        &format!("tree_list_ram_bytes = 2147483648\nruntime_dir = \"{at}/run\"\n"),
        1,
    );
    let text = swap(
        &text,
        "\"/var/lib/tierstore-cache/",
        &format!("\"{at}/cache/"),
        2,
    );
    let text = swap(&text, "\"/etc/tierstore/", &format!("\"{at}/etc/"), 4);
    let text = swap(
        &text,
        "\"/var/lib/hostkeeper/facts.json\"",
        &format!("\"{at}/facts.json\""),
        1,
    );
    let text = swap(
        &text,
        "client_group = \"tierstore\"",
        &format!("client_group = \"{gid}\""),
        1,
    );
    let text = swap(
        &text,
        "owner        = { subids_of = \"jsjob\", id = 1000 }",
        &format!("owner        = \"{uid}:{gid}\""),
        1,
    );
    assert_rebased(&text, dir);
    text
}

/// The Mac file `text` (the one of today, or the one before the switch, which names its mount
/// `mounts` times) moved onto `dir`, its farm accounts the ids of this process.
fn mac_on(text: &str, dir: &Path, mounts: usize) -> String {
    let at = dir.display();
    let (uid, gid) = ids();
    let text = swap(text, "\"127.0.0.1:9300\"", "\"127.0.0.1:0\"", 1);
    let text = swap(
        &text,
        "listen = \"127.0.0.1:12049\"",
        "listen = \"127.0.0.1:0\"",
        mounts - 1,
    );
    let text = swap(
        &text,
        "\"/private/var/tierstore/fs\"",
        &format!("\"{at}/mnt\""),
        mounts,
    );
    let text = swap(
        &text,
        "\"/var/lib/hostkeeper/facts.json\"",
        &format!("\"{at}/facts.json\""),
        1,
    );
    let text = text.replace("\"/usr/local/tierstore/", &format!("\"{at}/"));
    let text = swap(
        &text,
        "client_group = \"_tierstore\"",
        &format!("client_group = \"{gid}\""),
        mounts - 1,
    );
    let text = swap(
        &text,
        "owner        = \"_tierstore:_tierstore\"",
        &format!("owner        = \"{uid}:{gid}\""),
        mounts - 1,
    );
    assert_rebased(&text, dir);
    text
}

/// Asserts that every quoted absolute path of `text` is below `dir`.
fn assert_rebased(text: &str, dir: &Path) {
    let inside = format!("\"{}/", dir.display());
    for (at, _) in text.match_indices("\"/") {
        assert!(
            text[at..].starts_with(&inside),
            "a path outside the scratch directory: {}",
            &text[at..]
        );
    }
}

/// What a run of the binary printed, and how it ended.
struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs `tierstore --config <file> --check-config` with an empty environment.
fn check(file: &Path) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .env_clear()
        .arg("--config")
        .arg(file)
        .arg("--check-config")
        .output()
        .expect("the binary runs");
    Run {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Asserts that no secret of the key files is in what `run` printed.
fn assert_no_secret(run: &Run, secrets: &[String]) {
    for secret in secrets {
        assert!(
            !run.stdout.contains(secret) && !run.stderr.contains(secret),
            "{secret}"
        );
    }
}

/// Asserts that the binary accepts the Linux build node's file: moved onto a scratch directory,
/// with its farm accounts as this process's ids and fixture key files of mode 0600,
/// `--check-config` exits 0 with nothing on standard output and nothing on standard error, with
/// an empty environment, and prints no secret. Catches a key, a value or a combination of the
/// farm's file that a release refuses, which would leave a host restarting for ever after the
/// update.
#[cfg(target_os = "linux")]
#[test]
fn the_binary_accepts_the_linux_build_node_file() {
    let dir = TestDir::under(std::env::temp_dir(), "lc-bin-linux");
    let mut secrets = write_keys(
        dir.path(),
        &["access.key", "mac-build-upstream-1.key", "swfs.credentials"],
    );
    secrets.push(write_scoped_key(dir.path(), "kbf-cold.key"));
    std::fs::create_dir_all(dir.path().join("cache/kbf-cas")).unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, linux_on(dir.path())).unwrap();
    let run = check(&file);
    assert_eq!(
        (run.status, run.stdout.as_str(), run.stderr.as_str()),
        (Some(0), "", "")
    );
    assert_no_secret(&run, &secrets);
}

/// Asserts that the binary accepts the Mac build node's file: moved onto a scratch directory, with
/// its farm accounts as this process's ids and fixture key files of mode 0600, `--check-config`
/// exits 0 with nothing on standard output and nothing on standard error, with an empty
/// environment, and prints no secret. Catches a key, a value or a combination of the farm's file
/// that a release refuses, which would leave a Mac restarting for ever after the update.
#[test]
fn the_binary_accepts_the_mac_build_node_file() {
    let dir = TestDir::under(std::env::temp_dir(), "lc-bin-mac");
    let secrets = write_keys(dir.path(), &["access.key", "daemons.key", "upstream.key"]);
    std::fs::create_dir_all(dir.path().join("cache/kbf-cas")).unwrap();
    let file = dir.path().join("tierstore.toml");
    std::fs::write(&file, mac_on(MAC, dir.path(), 2)).unwrap();
    let run = check(&file);
    assert_eq!(
        (run.status, run.stdout.as_str(), run.stderr.as_str()),
        (Some(0), "", "")
    );
    assert_no_secret(&run, &secrets);
}

/// Asserts that the binary refuses the Mac's file from before its switch to one process: the file
/// has a mount provider and no `fs` interface, so `--check-config` exits 2 with nothing on
/// standard output and, on standard error, the binary's note that the file's
/// `local_cache.fill_socket` is ignored and then the line that names the file and the provider's
/// entry and says why, and prints no secret. Catches a release that starts a node whose provider
/// answers every job `UNAVAILABLE`, and a refusal that does not name what to change.
#[test]
fn the_binary_refuses_the_mac_file_from_before_the_switch() {
    let dir = TestDir::under(std::env::temp_dir(), "lc-bin-before");
    let secrets = write_keys(dir.path(), &["access.key", "daemons.key", "upstream.key"]);
    std::fs::create_dir_all(dir.path().join("cache/kbf-cas")).unwrap();
    let file = dir.path().join("tierstore.toml");
    std::fs::write(&file, mac_on(MAC_BEFORE_SWITCH, dir.path(), 1)).unwrap();
    let run = check(&file);
    let line = format!(
        "tierstore: local_cache.fill_socket is ignored: no fill socket is served, the file system of an fs interface calls the cache in this process\ntierstore: config file {}: interface[1].type: {NO_FS}\n",
        file.display()
    );
    assert_eq!(
        (run.status, run.stdout.as_str(), run.stderr.as_str()),
        (Some(2), "", line.as_str())
    );
    assert_no_secret(&run, &secrets);
}
