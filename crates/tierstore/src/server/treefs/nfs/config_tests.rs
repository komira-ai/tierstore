//! The `nfs` params of an `fs` interface: defaults, every key, every refusal, and that params
//! without it load.

use std::net::SocketAddr;
use std::time::Duration;

use crate::server::treefs::config::{TEST_PLACE, builders, from_params};

use super::NfsSettings;

/// The keys every `fs` interface needs.
const NEEDED: &str = "client_group = \"77\"\nowner = \"1000:1001\"\n";

/// The name of the key `name` of the `nfs` params, as a refusal says it.
fn key(name: &str) -> String {
    format!("{TEST_PLACE}.nfs.{name}")
}

fn with(nfs: &str) -> String {
    format!("{NEEDED}\n[nfs]\n{nfs}")
}

fn refused(nfs: &str) -> String {
    from_params(&with(nfs), &builders).unwrap_err().to_string()
}

/// Asserts that params without `nfs` load with the defaults, and that an empty table gives the
/// same.
#[test]
fn params_without_the_nfs_table_still_load() {
    let settings = from_params(NEEDED, &builders).unwrap();
    assert_eq!(settings.nfs, NfsSettings::default());
    assert_eq!(
        from_params(&with(""), &builders).unwrap().nfs,
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
    assert_eq!(nfs.jukebox_after, Duration::from_secs(5));
}

/// Asserts that every key set in the table is taken.
#[test]
fn every_nfs_key_is_taken_when_it_is_set() {
    let text = with(
        "listen = \"127.0.0.2:9999\"\nmount = false\nmount_user = \"build.user-1\"\n\
         mount_wait = \"2m\"\nmount_retry = \"1s\"\nfd_cache_shards = 8\nfd_cache_per_shard = 9\n\
         jukebox_after = \"750ms\"\n",
    );
    let nfs = from_params(&text, &builders).unwrap().nfs;
    assert_eq!(nfs.listen, "127.0.0.2:9999".parse::<SocketAddr>().unwrap());
    assert!(!nfs.mount);
    assert_eq!(nfs.mount_user.as_deref(), Some("build.user-1"));
    assert_eq!(
        (nfs.mount_wait, nfs.mount_retry),
        (Duration::from_secs(120), Duration::from_secs(1))
    );
    assert_eq!((nfs.fd_cache_shards, nfs.fd_cache_per_shard), (8, 9));
    assert_eq!(nfs.jukebox_after, Duration::from_millis(750));
}

/// Asserts that every key whose value is out of its range, or is not what the key takes, is
/// refused with the key's name.
#[test]
fn each_nfs_key_rejects_out_of_range_values_by_name() {
    let cases = [
        ("fd_cache_shards = 0", "fd_cache_shards"),
        ("fd_cache_shards = 4097", "fd_cache_shards"),
        ("fd_cache_per_shard = 0", "fd_cache_per_shard"),
        ("fd_cache_per_shard = 65537", "fd_cache_per_shard"),
        ("mount_wait = \"0s\"", "mount_wait"),
        ("mount_wait = \"2h\"", "mount_wait"),
        ("mount_retry = \"0ms\"", "mount_retry"),
        ("mount_retry = \"2m\"", "mount_retry"),
        ("jukebox_after = \"0ms\"", "jukebox_after"),
        ("jukebox_after = \"11m\"", "jukebox_after"),
        ("mount_user = \"\"", "mount_user"),
        ("mount_user = \"-o\"", "mount_user"),
        ("mount_user = \"a b\"", "mount_user"),
        ("mount_user = \"a/b\"", "mount_user"),
        (
            &format!("mount_user = \"{}\"", "u".repeat(65)),
            "mount_user",
        ),
        ("listen = \"not an address\"", "listen"),
    ];
    for (line, name) in cases {
        let why = refused(line);
        assert!(why.contains(&key(name)), "{line}: {why}");
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
            why.contains(&key("listen")) && why.contains("loopback"),
            "{address}: {why}"
        );
    }
    let nfs = from_params(&with("listen = \"127.0.0.1:0\""), &builders)
        .unwrap()
        .nfs;
    assert_eq!(nfs.listen.port(), 0);
}

/// Asserts the drain wait: 10 s by default, a value in range taken, and one of 0 or above an hour
/// refused by name.
#[test]
fn the_drain_wait_is_taken_in_its_range() {
    assert_eq!(NfsSettings::default().drain_wait, Duration::from_secs(10));
    let set = from_params(&with("drain_wait = \"2s\"\n"), &builders)
        .unwrap()
        .nfs;
    assert_eq!(set.drain_wait, Duration::from_secs(2));
    for line in ["drain_wait = \"0s\"", "drain_wait = \"2h\""] {
        let why = refused(line);
        assert!(why.contains(&key("drain_wait")), "{line}: {why}");
    }
}

/// Asserts the dead timeout of the mount: by default `fs.fetch_timeout` (rounded up to whole
/// seconds) and 30 s, so it follows a file's own fetch timeout; a value given is taken when it is
/// more than `fs.fetch_timeout`; one at or below it, or above two hours, is refused at load with
/// the key's name and why. Catches a mount that the client gives up on during a fill the server
/// still waits for, and two settings that can be set against each other.
#[test]
fn the_dead_timeout_is_more_than_the_fetch_timeout() {
    assert_eq!(NfsSettings::default().dead_timeout, Duration::from_secs(90));
    let fetch = |fetch: &str, nfs: &str| {
        let text = format!("{NEEDED}fetch_timeout = \"{fetch}\"\n\n[nfs]\n{nfs}");
        from_params(&text, &builders).map(|settings| settings.nfs.dead_timeout)
    };
    let without_table = format!("{NEEDED}fetch_timeout = \"2m\"\n");
    assert_eq!(
        from_params(&without_table, &builders)
            .unwrap()
            .nfs
            .dead_timeout,
        Duration::from_secs(150)
    );
    assert_eq!(fetch("2m", "").unwrap(), Duration::from_secs(150));
    assert_eq!(fetch("1500ms", "").unwrap(), Duration::from_secs(32));
    assert_eq!(
        fetch("60s", "dead_timeout = \"61s\"").unwrap(),
        Duration::from_secs(61)
    );
    assert_eq!(
        fetch("60s", "dead_timeout = \"60s\"")
            .unwrap_err()
            .to_string(),
        format!(
            "{}: 60000ms is not more than {TEST_PLACE}.fetch_timeout (60000ms): a fill that the \
             server still waits for would end the mount",
            key("dead_timeout")
        )
    );
    assert!(
        fetch("2m", "dead_timeout = \"90s\"")
            .unwrap_err()
            .to_string()
            .starts_with(&format!(
                "{}: 90000ms is not more than {TEST_PLACE}.fetch_timeout",
                key("dead_timeout")
            )),
    );
    assert_eq!(
        fetch("60s", "dead_timeout = \"3h\"")
            .unwrap_err()
            .to_string(),
        format!("{}: 10800000ms is more than 7200000ms", key("dead_timeout"))
    );
}

/// Asserts the limit of each command of the mount and the unmount: 20 s by default, a value in
/// range taken, and one below 1 s or above 25 s (four steps of 25 s, and the wait to collect a
/// killed command, are what the package's `ExitTimeOut` covers) refused by name.
#[test]
fn the_unmount_wait_is_taken_in_its_range() {
    assert_eq!(NfsSettings::default().unmount_wait, Duration::from_secs(20));
    let nfs = from_params(&with("unmount_wait = \"5s\""), &builders)
        .unwrap()
        .nfs;
    assert_eq!(nfs.unmount_wait, Duration::from_secs(5));
    for line in ["unmount_wait = \"500ms\"", "unmount_wait = \"26s\""] {
        let why = refused(line);
        assert!(
            why.starts_with(&format!("{}: ", key("unmount_wait"))),
            "{line}: {why}"
        );
    }
}
