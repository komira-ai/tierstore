//! The `[fs.nfs]` table: defaults, every key, every refusal, and that a file without it loads.

use std::net::SocketAddr;
use std::time::Duration;

use crate::server::treefs::config::from_text;

use super::NfsSettings;

/// The keys every file needs.
const NEEDED: &str = "client_group = \"77\"\nowner = \"1000:1001\"\n\
    fill_socket = \"/run/tierstore/fill.sock\"\ncache_path = \"/var/cache/tierstore\"\n";

fn with(nfs: &str) -> String {
    format!("[fs]\n{NEEDED}\n[fs.nfs]\n{nfs}")
}

fn refused(nfs: &str) -> String {
    from_text(&with(nfs), "").unwrap_err().to_string()
}

/// Asserts that a file without `[fs.nfs]` loads (B8: a file written before the table existed), with
/// the defaults, and that an empty table gives the same.
#[test]
fn a_config_without_the_nfs_table_still_loads() {
    let text = format!("[fs]\n{NEEDED}");
    let settings = from_text(&text, "").unwrap();
    assert_eq!(settings.nfs, NfsSettings::default());
    assert_eq!(
        from_text(&with(""), "").unwrap().nfs,
        NfsSettings::default()
    );
}

/// Asserts the defaults of the table.
#[test]
fn nfs_table_defaults() {
    let nfs = NfsSettings::default();
    assert_eq!(nfs.listen, "127.0.0.1:12049".parse::<SocketAddr>().unwrap());
    assert!(nfs.mount);
    assert_eq!(nfs.mount_user, None);
    assert_eq!(nfs.mount_wait, Duration::from_secs(30));
    assert_eq!(nfs.mount_retry, Duration::from_millis(500));
    assert_eq!((nfs.fd_cache_shards, nfs.fd_cache_per_shard), (64, 64));
    assert_eq!(nfs.allowed_uids, None);
    assert_eq!(nfs.jukebox_after, Duration::from_secs(5));
}

/// Asserts that every key set in the table is taken.
#[test]
fn every_nfs_key_is_taken_when_it_is_set() {
    let text = with(
        "listen = \"127.0.0.2:9999\"\nmount = false\nmount_user = \"build.user-1\"\n\
         mount_wait = \"2m\"\nmount_retry = \"1s\"\nfd_cache_shards = 8\nfd_cache_per_shard = 9\n\
         allowed_uids = [501, 502]\njukebox_after = \"750ms\"\n",
    );
    let nfs = from_text(&text, "").unwrap().nfs;
    assert_eq!(nfs.listen, "127.0.0.2:9999".parse::<SocketAddr>().unwrap());
    assert!(!nfs.mount);
    assert_eq!(nfs.mount_user.as_deref(), Some("build.user-1"));
    assert_eq!(
        (nfs.mount_wait, nfs.mount_retry),
        (Duration::from_secs(120), Duration::from_secs(1))
    );
    assert_eq!((nfs.fd_cache_shards, nfs.fd_cache_per_shard), (8, 9));
    assert_eq!(nfs.allowed_uids, Some(vec![501, 502]));
    assert_eq!(nfs.jukebox_after, Duration::from_millis(750));
}

/// Asserts that every key whose value is out of its range, or is not what the key takes, is
/// refused with the key's name.
#[test]
fn each_nfs_key_rejects_out_of_range_values_by_name() {
    let cases = [
        ("fd_cache_shards = 0", "fs.nfs.fd_cache_shards"),
        ("fd_cache_shards = 4097", "fs.nfs.fd_cache_shards"),
        ("fd_cache_per_shard = 0", "fs.nfs.fd_cache_per_shard"),
        ("fd_cache_per_shard = 65537", "fs.nfs.fd_cache_per_shard"),
        ("mount_wait = \"0s\"", "fs.nfs.mount_wait"),
        ("mount_wait = \"2h\"", "fs.nfs.mount_wait"),
        ("mount_retry = \"0ms\"", "fs.nfs.mount_retry"),
        ("mount_retry = \"2m\"", "fs.nfs.mount_retry"),
        ("jukebox_after = \"0ms\"", "fs.nfs.jukebox_after"),
        ("jukebox_after = \"11m\"", "fs.nfs.jukebox_after"),
        ("allowed_uids = []", "fs.nfs.allowed_uids"),
        ("mount_user = \"\"", "fs.nfs.mount_user"),
        ("mount_user = \"-o\"", "fs.nfs.mount_user"),
        ("mount_user = \"a b\"", "fs.nfs.mount_user"),
        ("mount_user = \"a/b\"", "fs.nfs.mount_user"),
        (
            &format!("mount_user = \"{}\"", "u".repeat(65)),
            "fs.nfs.mount_user",
        ),
        ("listen = \"not an address\"", "fs.nfs.listen"),
    ];
    for (line, key) in cases {
        let why = refused(line);
        assert!(why.contains(key), "{line}: {why}");
    }
}

/// Asserts that the address must be on loopback, IPv4: the protocol has no authentication a
/// network could rely on.
#[test]
fn nfs_listen_must_be_loopback() {
    for address in [
        "0.0.0.0:12049",
        "192.168.1.5:12049",
        "[::1]:12049",
        "[::]:12049",
        "10.0.0.1:1",
    ] {
        let why = refused(&format!("listen = \"{address}\""));
        assert!(
            why.contains("fs.nfs.listen") && why.contains("loopback"),
            "{address}: {why}"
        );
    }
    let nfs = from_text(&with("listen = \"127.0.0.1:0\""), "")
        .unwrap()
        .nfs;
    assert_eq!(nfs.listen.port(), 0);
}

/// Asserts that a key this release does not know, in the table or beside it, is ignored with a
/// warning and the file loads, as the other keys of `[fs]` are (B8: a release does not refuse the
/// file of the next).
#[test]
fn an_unknown_nfs_key_is_ignored_and_the_file_loads() {
    let nfs = from_text(&with("futuristic = 3\nmount_wait = \"3s\"\n"), "")
        .unwrap()
        .nfs;
    assert_eq!(nfs.mount_wait, Duration::from_secs(3));
}
