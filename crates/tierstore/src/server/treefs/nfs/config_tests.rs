//! The `[fs.nfs]` table: defaults, every key, every refusal, and that a file without it loads.

use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::Level;
use tracing::subscriber::with_default;

use crate::server::build_info::stderr_log::LineLog;
use crate::server::treefs::config::{builders, from_text};

use super::NfsSettings;

/// The keys every file needs.
const NEEDED: &str = "client_group = \"77\"\nowner = \"1000:1001\"\n\
    cache_path = \"/var/cache/tierstore\"\n";

fn with(nfs: &str) -> String {
    format!("[fs]\n{NEEDED}\n[fs.nfs]\n{nfs}")
}

fn refused(nfs: &str) -> String {
    from_text(&with(nfs), &builders).unwrap_err().to_string()
}

/// Asserts that a file without `[fs.nfs]` loads (B8: a file written before the table existed), with
/// the defaults, and that an empty table gives the same.
#[test]
fn a_config_without_the_nfs_table_still_loads() {
    let text = format!("[fs]\n{NEEDED}");
    let settings = from_text(&text, &builders).unwrap();
    assert_eq!(settings.nfs, NfsSettings::default());
    assert_eq!(
        from_text(&with(""), &builders).unwrap().nfs,
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
    let nfs = from_text(&text, &builders).unwrap().nfs;
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
    let nfs = from_text(&with("listen = \"127.0.0.1:0\""), &builders)
        .unwrap()
        .nfs;
    assert_eq!(nfs.listen.port(), 0);
}

/// A writer whose bytes the test reads back: the operator's stderr.
#[derive(Clone, Default)]
struct Stderr(Arc<Mutex<Vec<u8>>>);

impl Write for Stderr {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Asserts that a key this release does not know, in the table or beside it, is ignored with one
/// warning that names it and the file loads, as the other keys of `[fs]` are (B8: a release does
/// not refuse the file of the next).
#[test]
fn an_unknown_nfs_key_is_ignored_and_the_file_loads() {
    let stderr = Stderr::default();
    let text = with("futuristic = 3\nmount_wait = \"3s\"\n");
    let nfs = with_default(LineLog::new(stderr.clone(), Level::WARN), || {
        from_text(&text, &builders).unwrap().nfs
    });
    assert_eq!(nfs.mount_wait, Duration::from_secs(3));
    let said = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        said,
        "tierstore fs: fs.nfs.futuristic is not a key of this release; ignored\n"
    );
}

/// Asserts that a file that still sets `fs.nfs.allowed_uids`, the key of the releases up to
/// 0.3.166, loads whatever its value (a list, the empty list that release refused, a value of
/// another type) to the settings the file has without it, and that the start says so in one line,
/// exact, that names the key and why it is ignored. Catches a release that refuses, or that stops,
/// on a file written for the release before it.
#[test]
fn a_file_that_still_sets_allowed_uids_loads_with_one_warning() {
    let plain = from_text(&with("mount_wait = \"3s\"\n"), &builders)
        .unwrap()
        .nfs;
    for value in ["[501]", "[501, 502, 0]", "[]", "\"501\"", "true"] {
        let stderr = Stderr::default();
        let text = with(&format!("allowed_uids = {value}\nmount_wait = \"3s\"\n"));
        let nfs = with_default(LineLog::new(stderr.clone(), Level::WARN), || {
            from_text(&text, &builders).unwrap().nfs
        });
        assert_eq!(nfs, plain, "{value}");
        let said = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            said,
            "tierstore fs: fs.nfs.allowed_uids is ignored: removed after tierstore 0.3.166: the \
             file service answers every local user, as the S3 front answers every client that \
             holds a key\n",
            "{value}"
        );
    }
}

/// Asserts the keys of the hand-off: none by default (an update is a restart), a control socket
/// that is an absolute path of a file that fits a Unix socket's address, a wait that is a time in
/// range and needs the socket, and a drain wait in range.
#[test]
fn the_handoff_keys_are_taken_and_each_refusal_names_its_key() {
    let defaults = NfsSettings::default();
    assert_eq!(defaults.handoff, None);
    assert_eq!(defaults.drain_wait, Duration::from_secs(10));
    let set = from_text(
        &with("handoff_socket = \"/var/run/tierstore/fs-handoff.sock\"\nhandoff_wait = \"3s\"\ndrain_wait = \"2s\"\n"),
        &builders,
    )
    .unwrap()
    .nfs;
    let handoff = set.handoff.unwrap();
    assert_eq!(
        handoff.socket,
        std::path::Path::new("/var/run/tierstore/fs-handoff.sock")
    );
    assert_eq!(handoff.wait, Duration::from_secs(3));
    assert_eq!(set.drain_wait, Duration::from_secs(2));
    let waits = from_text(&with("handoff_socket = \"/run/x.sock\""), &builders)
        .unwrap()
        .nfs;
    assert_eq!(waits.ready_wait, Duration::from_secs(120));
    assert_eq!(waits.handoff.unwrap().wait, Duration::from_secs(10));
    let ready = from_text(
        &with("handoff_socket = \"/run/x.sock\"\nhandoff_ready_wait = \"7s\""),
        &builders,
    )
    .unwrap()
    .nfs;
    assert_eq!(ready.ready_wait, Duration::from_secs(7));
    let long = format!("handoff_socket = \"/{}\"", "x".repeat(120));
    for (text, key) in [
        ("handoff_wait = \"3s\"".to_owned(), "fs.nfs.handoff_wait"),
        (
            "handoff_ready_wait = \"3s\"".to_owned(),
            "fs.nfs.handoff_ready_wait",
        ),
        (
            "handoff_socket = \"/run/x\"\nhandoff_ready_wait = \"0s\"".to_owned(),
            "fs.nfs.handoff_ready_wait",
        ),
        (
            "handoff_socket = \"run/x.sock\"".to_owned(),
            "fs.nfs.handoff_socket",
        ),
        ("handoff_socket = \"/\"".to_owned(), "fs.nfs.handoff_socket"),
        (long, "fs.nfs.handoff_socket"),
        (
            "handoff_socket = \"/run/x\"\nhandoff_wait = \"0s\"".to_owned(),
            "fs.nfs.handoff_wait",
        ),
        ("drain_wait = \"0s\"".to_owned(), "fs.nfs.drain_wait"),
        ("drain_wait = \"2h\"".to_owned(), "fs.nfs.drain_wait"),
    ] {
        let why = refused(&text);
        assert!(why.contains(key), "{text}: {why}");
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
        let text = format!("[fs]\n{NEEDED}fetch_timeout = \"{fetch}\"\n\n[fs.nfs]\n{nfs}");
        from_text(&text, &builders).map(|settings| settings.nfs.dead_timeout)
    };
    let without_table = format!("[fs]\n{NEEDED}fetch_timeout = \"2m\"\n");
    assert_eq!(
        from_text(&without_table, &builders)
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
        "fs.nfs.dead_timeout: 60000ms is not more than fs.fetch_timeout (60000ms): a fill that \
         the server still waits for would end the mount"
    );
    assert!(
        fetch("2m", "dead_timeout = \"90s\"")
            .unwrap_err()
            .to_string()
            .starts_with("fs.nfs.dead_timeout: 90000ms is not more than fs.fetch_timeout"),
    );
    assert_eq!(
        fetch("60s", "dead_timeout = \"3h\"")
            .unwrap_err()
            .to_string(),
        "fs.nfs.dead_timeout: 10800000ms is more than 7200000ms"
    );
}

/// Asserts the limit of each command of the mount and the unmount: 20 s by default, a value in
/// range taken, and one below 1 s or above 25 s (four steps of 25 s, and the wait to collect a
/// killed command, are what the package's `ExitTimeOut` covers) refused by name.
#[test]
fn the_unmount_wait_is_taken_in_its_range() {
    assert_eq!(NfsSettings::default().unmount_wait, Duration::from_secs(20));
    let nfs = from_text(&with("unmount_wait = \"5s\""), &builders)
        .unwrap()
        .nfs;
    assert_eq!(nfs.unmount_wait, Duration::from_secs(5));
    for line in ["unmount_wait = \"500ms\"", "unmount_wait = \"26s\""] {
        let why = refused(line);
        assert!(why.starts_with("fs.nfs.unmount_wait: "), "{line}: {why}");
    }
}
