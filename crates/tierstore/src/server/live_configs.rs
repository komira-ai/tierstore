//! The host configurations the build farm runs, as the tests of this crate read them: the files of
//! `tests/live_configs/` (its README says what each is and how it is made from the farm's file),
//! moved onto a scratch directory, with the names of the farm's accounts looked up in a table of
//! the test's own. The tests that hold this release to them: `live_configs_tests.rs` (the load),
//! `treefs/live_config_tests.rs` (the Linux node serves its mount provider over FUSE),
//! `treefs/nfs/live_config_tests.rs` (the Mac node serves it over NFS), and
//! `update_hook/config_hook/steps_tests/live_configs.rs` (the update hook and the config hook).
//!
//! Every edit is a literal replacement whose count in the committed file is stated: a file of the
//! farm that gains a path, or loses one, fails the test that reads it instead of keeping a farm
//! path in the scratch copy.

use std::io;
use std::path::Path;

use crate::server::config::Host;
use crate::server::treefs::{Database, Found};

/// The Linux build node's file (the HostGroup all-in-one).
pub(in crate::server) const LINUX: &str =
    include_str!("../../tests/live_configs/linux-build-node.toml");
/// The Mac build node's file (the HostGroup mac-build), with its `fs` interface.
pub(in crate::server) const MAC: &str =
    include_str!("../../tests/live_configs/mac-build-node.toml");
/// The Mac build node's `fs.toml`, which no process serves from: the config hook reads its
/// `[fs] mountpoint` at the switch to one process.
pub(in crate::server) const MAC_FS: &str = include_str!("../../tests/live_configs/mac-fs.toml");
/// The Mac build node's file before its switch to one process: a mount provider and no `fs`
/// interface, the layout whose mount `tierstore fs` served until 0.3.196.
pub(in crate::server) const MAC_BEFORE_SWITCH: &str =
    include_str!("../../tests/live_configs/mac-build-node-before-switch.toml");

/// The Linux node's mount point, as the farm's file names it.
pub(in crate::server) const LINUX_MOUNT: &str = "/run/tierstore/fs";
/// The Mac node's mount point, as the farm's file names it.
pub(in crate::server) const MAC_MOUNT: &str = "/private/var/tierstore/fs";

/// The key files each file names, below its scratch directory once rebased.
pub(in crate::server) const LINUX_KEYS: [&str; 3] =
    ["etc/access.key", "etc/mac-build-upstream-1.key", SCOPED_KEY];
/// The Linux file's scoped key (`server.scoped_key_files`): JSON with its prefixes, where the
/// others are a key id and a secret on two lines.
const SCOPED_KEY: &str = "etc/kbf-cold.key";
/// The key files of the Mac's file.
pub(in crate::server) const MAC_KEYS: [&str; 2] = ["etc/access.key", "etc/daemons.key"];

/// The id of the job user's first subordinate uid and gid in [`host`]'s `/etc/subuid` and
/// `/etc/subgid`: the owner the Linux file's `{ subids_of = "jsjob", id = 1000 }` resolves to is
/// this plus 1000.
pub(in crate::server) const SUBID_START: u32 = 100_000;

/// `text` with every `from` replaced by `to`, where `from` occurs exactly `count` times.
///
/// # Panics
/// When `from` occurs another number of times: the committed file changed under the test.
pub(in crate::server) fn swap(text: &str, from: &str, to: &str, count: usize) -> String {
    let found = text.matches(from).count();
    assert_eq!(found, count, "{from:?} occurs {found} times, not {count}");
    text.replace(from, to)
}

/// The Linux node's file moved onto `dir`: its sockets, mount, runtime directory (which the file
/// leaves at its default, `/run/tierstore-fs`, here `<dir>/run`), cache, keys and facts file under
/// `dir`, and its S3 interface on a port of the system's choosing instead of 9300.
///
/// # Panics
/// When a replacement does not meet its count, or a value still names a path outside `dir`.
pub(in crate::server) fn linux_on(dir: &Path) -> String {
    let at = dir.display();
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
    assert_rebased(&text, dir);
    text
}

/// The Mac node's file moved onto `dir` (see [`linux_on`]): its NFS front on a port of the
/// system's choosing too, instead of 12049.
///
/// # Panics
/// As [`linux_on`].
pub(in crate::server) fn mac_on(dir: &Path) -> String {
    mac_file_on(MAC, dir, 2)
}

/// The Mac node's file before its switch moved onto `dir`, as [`mac_on`] moves the file of today.
///
/// # Panics
/// As [`linux_on`].
pub(in crate::server) fn mac_before_switch_on(dir: &Path) -> String {
    mac_file_on(MAC_BEFORE_SWITCH, dir, 1)
}

/// `text`, a Mac file that names its mount point `mounts` times, moved onto `dir`.
fn mac_file_on(text: &str, dir: &Path, mounts: usize) -> String {
    let at = dir.display();
    let text = swap(text, "\"127.0.0.1:9300\"", "\"127.0.0.1:0\"", 1);
    let text = swap(
        &text,
        "listen = \"127.0.0.1:12049\"",
        "listen = \"127.0.0.1:0\"",
        mounts - 1,
    );
    let text = swap(
        &text,
        &format!("\"{MAC_MOUNT}\""),
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
    assert_rebased(&text, dir);
    text
}

/// Asserts that every quoted absolute path of `text` is below `dir`.
fn assert_rebased(text: &str, dir: &Path) {
    let inside = format!("\"{}/", dir.display());
    for (at, _) in text.match_indices("\"/") {
        assert!(
            text[at..].starts_with(&inside),
            "a value names a path outside the scratch directory: {}",
            text[at..].lines().next().unwrap_or_default()
        );
    }
}

/// Writes in `dir` what the rebased file reads at its load: the key files `keys` (mode 0600, a key
/// id and a secret of more than the 16 bytes a key needs, each its own; the scoped key as JSON that
/// may read `cas/`) and hostkeeper's facts file.
///
/// # Panics
/// When a file cannot be written.
pub(in crate::server) fn write_keys(dir: &Path, keys: &[&str]) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir.join("etc")).unwrap();
    for (n, key) in keys.iter().enumerate() {
        let path = dir.join(key);
        let text = if *key == SCOPED_KEY {
            format!(
                "{{\"access_key_id\": \"AKIDLIVE{n}\", \"secret_access_key\": \"live-fixture-secret-{n:04}\", \"read_prefixes\": [\"cas/\"], \"write_prefixes\": [], \"delete_prefixes\": []}}\n"
            )
        } else {
            format!("AKIDLIVE{n}\nlive-fixture-secret-{n:04}\n")
        };
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::write(
        dir.join("facts.json"),
        "{\"schema\":1,\"host_id\":\"node-1\"}\n",
    )
    .unwrap();
}

/// The system the farm's files are loaded on: its accounts are the test's own (the groups
/// `tierstore` and `_tierstore` are the test's group, the users `_tierstore` and `jsjob` its user),
/// the job user has the subordinate ids from [`SUBID_START`], and file systems are the test's.
pub(in crate::server) const HOST: Host = Host {
    names,
    volume: Host::SYSTEM.volume,
    read,
};

/// The id of an account of the farm's files: the test's own group or user.
fn names(database: Database, name: &str) -> Found {
    Ok(match (database, name) {
        (Database::Group, "tierstore" | "_tierstore") => Some(rustix::process::getegid().as_raw()),
        (Database::User, "_tierstore" | "jsjob") => Some(rustix::process::geteuid().as_raw()),
        _ => None,
    })
}

/// The job user's subordinate ids, in both files.
fn read(path: &Path) -> io::Result<String> {
    match path.to_str() {
        Some("/etc/subuid" | "/etc/subgid") => Ok(format!("jsjob:{SUBID_START}:65536\n")),
        _ => Err(io::Error::from(io::ErrorKind::NotFound)),
    }
}

#[cfg(test)]
#[path = "live_configs_tests.rs"]
mod tests;
