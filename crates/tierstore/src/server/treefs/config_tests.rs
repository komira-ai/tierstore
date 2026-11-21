//! The `[fs]` table: defaults, every refusal, and the group lookup.

use std::path::PathBuf;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{FsSettings, from_text, ignored, load};

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
        handoff_socket: PathBuf::from("/run/tierstore-fs/handoff.sock"),
        handoff_wait: Duration::from_secs(120),
        recover_wait: Duration::from_secs(30),
        reattach_concurrency: 16,
        tree_idle: Duration::from_secs(900),
        allow_other: true,
        nfs: crate::server::treefs::nfs::config::NfsSettings::default(),
        put: None,
    };
    assert_eq!(got, want);
}

/// Asserts that every key set in the file is taken, the group by number too, and the control socket
/// defaults to the runtime directory that was set.
#[test]
fn every_key_is_taken_when_it_is_set() {
    let text = "[fs]\nmountpoint = \"/m\"\nruntime_dir = \"/r\"\nclient_group = \"77\"\n\
        owner = \"5:6\"\nfill_socket = \"/f\"\ncache_path = \"/c\"\nfetch_timeout = \"5s\"\n\
        threads = 3\n\
        handoff_socket = \"/h.sock\"\nhandoff_wait = \"5s\"\nrecover_wait = \"2m\"\nreattach_concurrency = 3\n\
        tree_idle_secs = 604800\n";
    let got = from_text(text, "").unwrap();
    assert_eq!(got.mountpoint, PathBuf::from("/m"));
    assert_eq!(got.control_socket, PathBuf::from("/r/control.sock"));
    assert_eq!((got.client_group, got.owner), (77, (5, 6)));
    assert_eq!(
        (got.fetch_timeout, got.threads),
        (Duration::from_secs(5), 3)
    );
    assert_eq!(got.reattach_concurrency, 3);
    assert_eq!(got.tree_idle, Duration::from_secs(604_800));
    assert_eq!(got.handoff_socket, PathBuf::from("/h.sock"));
    assert_eq!(
        (got.handoff_wait, got.recover_wait),
        (Duration::from_secs(5), Duration::from_secs(120))
    );
    // Left out, the hand-over socket follows the runtime directory that was set.
    let text = "[fs]\nruntime_dir = \"/r\"\nclient_group = \"77\"\nowner = \"5:6\"\n\
        fill_socket = \"/f\"\ncache_path = \"/c\"\n";
    assert_eq!(
        from_text(text, "").unwrap().handoff_socket,
        PathBuf::from("/r/handoff.sock")
    );
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
        (fs("handoff_socket = \"relative.sock\"\n"), "fs.handoff_socket"),
        (
            fs(&format!("handoff_socket = \"/{}\"\n", "s".repeat(120))),
            "fs.handoff_socket",
        ),
        (fs("handoff_wait = \"999ms\"\n"), "fs.handoff_wait"),
        (fs("handoff_wait = \"2h\"\n"), "fs.handoff_wait"),
        (fs("reattach_concurrency = 0\n"), "fs.reattach_concurrency"),
        (fs("reattach_concurrency = 1025\n"), "fs.reattach_concurrency"),
        (fs("tree_idle_secs = 0\n"), "fs.tree_idle_secs"),
        (fs("tree_idle_secs = 604801\n"), "fs.tree_idle_secs"),
        (fs("recover_wait = \"0s\"\n"), "fs.recover_wait"),
        (fs("recover_wait = \"3h\"\n"), "fs.recover_wait"),
        (fs("fetch_timeout = \"0ms\"\n"), "fs.fetch_timeout"),
        (fs("fetch_timeout = \"2h\"\n"), "fs.fetch_timeout"),
        (fs("threads = 0\n"), "fs.threads"),
        (fs("threads = 253\n"), "fs.threads"),
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
    let text = format!("{}[fs.put]\nenabled = \"yes\"\n", fs(""));
    assert!(refused(&text).contains("expected a boolean"));
}

/// Asserts that a key this release does not know is ignored and the file loads.
#[test]
fn a_key_of_a_later_release_is_ignored() {
    let text = fs("future_knob = 3\n");
    assert!(from_text(&text, GROUPS).is_ok());
    assert_eq!(
        ignored("future_knob"),
        "tierstore fs: fs.future_knob is not a key of this release and is ignored"
    );
}

/// Asserts that a file written for a release that had passthrough starts with its keys, whatever
/// their values (a value the release refused included), with the settings it would have without
/// them, and that the warning of each names the release that removed it and why.
#[test]
fn the_keys_of_passthrough_are_ignored_with_a_warning_that_names_the_release() {
    let plain = from_text(&fs(""), GROUPS).unwrap();
    for removed in [
        "passthrough = false\nmax_backing_ids = 7\n",
        "passthrough = \"yes\"\nmax_backing_ids = 0\n",
    ] {
        assert_eq!(from_text(&fs(removed), GROUPS).unwrap(), plain, "{removed}");
    }
    assert_eq!(
        ignored("passthrough"),
        "tierstore fs: fs.passthrough is ignored: removed after tierstore 0.3.121, with \
         passthrough: every open is a cached open"
    );
    assert_eq!(
        ignored("max_backing_ids"),
        "tierstore fs: fs.max_backing_ids is ignored: removed after tierstore 0.3.121, with \
         passthrough: no backing id is registered"
    );
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

/// Asserts that `fs.threads` takes the most readers a `tsfuse` session holds, and that the bound
/// is that library's: each thread reads its own copy of the connection's descriptor.
#[test]
fn the_most_threads_are_the_descriptors_a_session_holds() {
    let got = from_text(&fs("threads = 252\n"), GROUPS).unwrap();
    assert_eq!(got.threads, 252);
    #[cfg(target_os = "linux")]
    assert_eq!(super::MOST_THREADS as usize, tsfuse::MAX_DEVICES);
}
