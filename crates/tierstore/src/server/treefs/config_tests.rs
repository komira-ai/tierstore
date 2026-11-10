//! The `[fs]` table: defaults, every refusal, and the group lookup.

use std::path::PathBuf;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{FsSettings, from_text, load};

const GROUPS: &str = "root:x:0:\nbad line\nbuilders:x:2001:alice\nodd:x:notanumber:\n";

/// The keys every file needs.
const NEEDED: &str = "client_group = \"builders\"\nowner = \"1000:1001\"\n\
    fill_socket = \"/run/tierstore/fill.sock\"\ncache_path = \"/var/cache/tierstore\"\n";

fn fs(extra: &str) -> String {
    format!("[fs]\n{NEEDED}{extra}")
}

fn refused(text: &str) -> String {
    from_text(text, GROUPS).unwrap_err().to_string()
}

/// Asserts that a file with only the needed keys gets the defaults of the table, and the group by
/// name from the group file.
#[test]
fn the_needed_keys_alone_give_the_defaults() {
    let got = from_text(&fs(""), GROUPS).unwrap();
    let want = FsSettings {
        mountpoint: PathBuf::from("/run/tierstore/fs"),
        runtime_dir: PathBuf::from("/run/tierstore-fs"),
        control_socket: PathBuf::from("/run/tierstore-fs/control.sock"),
        client_group: 2001,
        owner: (1000, 1001),
        fill_socket: PathBuf::from("/run/tierstore/fill.sock"),
        cache_path: PathBuf::from("/var/cache/tierstore"),
        fetch_timeout: Duration::from_secs(60),
        threads: 16,
        passthrough: true,
        max_backing_ids: 65_536,
        allow_other: true,
        nfs: crate::server::treefs::nfs::config::NfsSettings::default(),
    };
    assert_eq!(got, want);
}

/// Asserts that every key set in the file is taken, the group by number too, and the control socket
/// defaults to the runtime directory that was set.
#[test]
fn every_key_is_taken_when_it_is_set() {
    let text = "[fs]\nmountpoint = \"/m\"\nruntime_dir = \"/r\"\nclient_group = \"77\"\n\
        owner = \"5:6\"\nfill_socket = \"/f\"\ncache_path = \"/c\"\nfetch_timeout = \"5s\"\n\
        threads = 3\npassthrough = false\nmax_backing_ids = 7\n";
    let got = from_text(text, "").unwrap();
    assert_eq!(got.mountpoint, PathBuf::from("/m"));
    assert_eq!(got.control_socket, PathBuf::from("/r/control.sock"));
    assert_eq!((got.client_group, got.owner), (77, (5, 6)));
    assert_eq!(
        (got.fetch_timeout, got.threads),
        (Duration::from_secs(5), 3)
    );
    assert_eq!((got.passthrough, got.max_backing_ids), (false, 7));
    let own = fs("control_socket = \"/elsewhere.sock\"\n");
    assert_eq!(
        from_text(&own, GROUPS).unwrap().control_socket,
        PathBuf::from("/elsewhere.sock")
    );
}

/// Asserts each refusal and that it names the key.
#[test]
fn every_refusal_names_its_key() {
    let cases: Vec<(String, &str)> = vec![
        (fs("mountpoint = \"relative\"\n"), "fs.mountpoint"),
        (fs("runtime_dir = \"relative\"\n"), "fs.runtime_dir"),
        (fs("control_socket = \"relative.sock\"\n"), "fs.control_socket"),
        (
            fs(&format!("control_socket = \"/{}\"\n", "s".repeat(120))),
            "fs.control_socket",
        ),
        (fs("fetch_timeout = \"0ms\"\n"), "fs.fetch_timeout"),
        (fs("fetch_timeout = \"2h\"\n"), "fs.fetch_timeout"),
        (fs("threads = 0\n"), "fs.threads"),
        (fs("threads = 1025\n"), "fs.threads"),
        (fs("max_backing_ids = 0\n"), "fs.max_backing_ids"),
        (fs("max_backing_ids = 1048577\n"), "fs.max_backing_ids"),
        (
            "[fs]\nowner = \"1:2\"\nfill_socket = \"/f\"\ncache_path = \"/c\"\n".to_owned(),
            "fs.client_group",
        ),
        (
            "[fs]\nclient_group = \"1\"\nfill_socket = \"/f\"\ncache_path = \"/c\"\n".to_owned(),
            "fs.owner",
        ),
        (
            "[fs]\nclient_group = \"1\"\nowner = \"1:2\"\ncache_path = \"/c\"\n".to_owned(),
            "fs.fill_socket",
        ),
        (
            "[fs]\nclient_group = \"1\"\nowner = \"1:2\"\nfill_socket = \"/f\"\n".to_owned(),
            "fs.cache_path",
        ),
        (
            "[fs]\nclient_group = \"1\"\nowner = \"1:2\"\nfill_socket = \"f\"\ncache_path = \"/c\"\n"
                .to_owned(),
            "fs.fill_socket",
        ),
        (
            "[fs]\nclient_group = \"1\"\nowner = \"1:2\"\nfill_socket = \"/f\"\ncache_path = \"c\"\n"
                .to_owned(),
            "fs.cache_path",
        ),
        (fs("").replace("1000:1001", "1000"), "fs.owner"),
        (fs("").replace("1000:1001", "a:b"), "fs.owner"),
        (fs("").replace("1000:1001", "1:-2"), "fs.owner"),
        (fs("").replace("builders", "nobody-here"), "fs.client_group"),
        (fs("").replace("builders", "odd"), "fs.client_group"),
    ];
    for (text, key) in cases {
        let why = refused(&text);
        assert!(why.starts_with(key), "{key}: {why}");
    }
}

/// Asserts that a file that is not TOML, has no `[fs]` table or a value of the wrong type is a
/// syntax error that shows no value.
#[test]
fn a_file_that_is_not_the_table_is_a_syntax_error_without_values() {
    assert!(refused("not toml [").contains("line 1"));
    assert!(refused("[server]\nid = 1\n").contains("fs"));
    let why = refused("[fs]\nthreads = \"seventeen\"\n");
    assert!(!why.contains("seventeen"), "{why}");
    // A switch is true or false, nothing else.
    assert!(refused(&fs("passthrough = \"yes\"\n")).contains("expected a boolean"));
}

/// Asserts that a key this release does not know is ignored and the file loads.
#[test]
fn a_key_of_a_later_release_is_ignored() {
    let text = fs("future_knob = 3\n");
    assert!(from_text(&text, GROUPS).is_ok());
}

/// Asserts that a file is read from a path, a missing one is an error naming it, and the group file
/// of the host is read (a group by number needs none).
#[test]
fn a_file_is_read_from_its_path() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-config");
    let path = dir.path().join("fs.toml");
    std::fs::write(&path, fs("").replace("builders", "4242")).unwrap();
    assert_eq!(load(&path).unwrap().client_group, 4242);
    let missing = load(&dir.path().join("none.toml")).unwrap_err().to_string();
    assert!(missing.contains("none.toml"), "{missing}");
}
