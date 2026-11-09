//! The two hand-off keys of `[server]`: what a file without them makes, their defaults, the values
//! they refuse at start, and the build node and the storage node with `[local_cache]`, which read
//! them as a storage node does.

use std::path::PathBuf;
use std::time::Duration;

use super::super::config::{PerRun, ServerTable, from_text, settings_from_text};
use super::super::config_testing::{DEFAULTS, MINIMAL};
use super::super::config_unknown::fields_of;
use super::super::handoff::Handoff;
use super::{DEFAULT_HANDOFF_WAIT, MOST_SOCKET_PATH};

/// `MINIMAL` with `lines` as more keys of `[server]`.
fn with_server(lines: &str) -> String {
    MINIMAL.replacen(
        "access_key_file = \"key\"",
        &format!("access_key_file = \"key\"\n{lines}"),
        1,
    )
}

/// What a start of `text` is refused with.
fn refusal(text: &str) -> String {
    settings_from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// The hand-off a storage node's file `text` makes.
fn handoff_of(text: &str) -> Option<Handoff> {
    from_text(text, DEFAULTS).unwrap().handoff
}

/// Asserts that a file that gives neither key has no hand-off, which is the start of the releases
/// before the keys: it binds its own sockets and makes no control socket.
#[test]
fn a_file_without_the_keys_has_no_handoff() {
    assert_eq!(handoff_of(MINIMAL), None);
}

/// Asserts that `handoff_socket` alone turns the hand-off on with the default wait of 10 seconds,
/// and that `handoff_wait` sets the wait, at both ends of its range.
#[test]
fn the_socket_turns_the_handoff_on_and_the_wait_has_a_default() {
    let socket = PathBuf::from("/var/lib/tierstore/handoff.sock");
    assert_eq!(DEFAULT_HANDOFF_WAIT, Duration::from_secs(10));
    let alone = with_server("handoff_socket = \"/var/lib/tierstore/handoff.sock\"");
    assert_eq!(
        handoff_of(&alone),
        Some(Handoff {
            socket: socket.clone(),
            wait: Duration::from_secs(10),
        })
    );
    for (given, wait) in [
        ("1ms", Duration::from_millis(1)),
        ("45s", Duration::from_secs(45)),
        ("1h", Duration::from_secs(3600)),
    ] {
        let text = with_server(&format!(
            "handoff_socket = \"/var/lib/tierstore/handoff.sock\"\nhandoff_wait = \"{given}\""
        ));
        let expected = Handoff {
            socket: socket.clone(),
            wait,
        };
        assert_eq!(handoff_of(&text), Some(expected), "{given}");
    }
}

/// Asserts that a `handoff_wait` outside 1ms to 1h is refused at start, naming the key and the
/// range in the exact words.
#[test]
fn a_wait_out_of_range_is_refused_by_name() {
    for (given, ms) in [("0s", "0"), ("61m", "3660000")] {
        let text = with_server(&format!(
            "handoff_socket = \"/run/x.sock\"\nhandoff_wait = \"{given}\""
        ));
        assert_eq!(
            refusal(&text),
            format!("server.handoff_wait: {ms}ms is not between 1ms and 3600000ms"),
            "{given}"
        );
    }
}

/// Asserts that a wait without a socket is refused, naming the key: it would change nothing, and
/// the operator who wrote it meant a hand-off.
#[test]
fn a_wait_without_a_socket_is_refused() {
    assert_eq!(
        refusal(&with_server("handoff_wait = \"5s\"")),
        "server.handoff_wait: it is the wait for the running instance of server.handoff_socket, \
         which is not given"
    );
}

/// Asserts that a socket path that is relative, empty or names no file is refused at start, with
/// the path and the reason in the exact words.
#[test]
fn a_socket_that_is_not_an_absolute_file_path_is_refused() {
    for (path, why) in [
        ("handoff.sock", "must be an absolute path"),
        ("", "must be an absolute path"),
        ("/", "must name a file, not a directory"),
    ] {
        let text = with_server(&format!("handoff_socket = \"{path}\""));
        assert_eq!(
            refusal(&text),
            format!("server.handoff_socket: \"{path}\" {why}"),
            "{path:?}"
        );
    }
}

/// Asserts that the longest path a Unix socket can have on every platform (103 bytes) is taken and
/// one byte more is refused at start, naming its length, rather than failing at the bind after the
/// proxy is open.
#[test]
fn a_socket_path_longer_than_a_unix_socket_can_have_is_refused() {
    assert_eq!(MOST_SOCKET_PATH, 103);
    let longest = format!("/{}", "a".repeat(102));
    let text = with_server(&format!("handoff_socket = \"{longest}\""));
    let taken = handoff_of(&text).expect("a hand-off");
    assert_eq!(taken.socket.as_os_str().len(), 103);
    let over = format!("/{}", "a".repeat(103));
    assert_eq!(
        refusal(&with_server(&format!("handoff_socket = \"{over}\""))),
        format!(
            "server.handoff_socket: \"{over}\" is 104 bytes; the path of a Unix socket is at most 103"
        )
    );
}

/// Asserts that the control socket may not be the lock file of the hot tier: the offer replaces
/// the file at its path, which would take the lock away from the node.
#[test]
fn a_socket_that_is_the_lock_file_is_refused() {
    let text = with_server("handoff_socket = \"/var/lib/tierstore/instance.lock\"").replacen(
        "budget = 1000",
        "budget = 1000\nlock_file = \"/var/lib/tierstore/instance.lock\"",
        1,
    );
    assert_eq!(
        refusal(&text),
        "server.handoff_socket: \"/var/lib/tierstore/instance.lock\" is hot_store.lock_file too; \
         the control socket replaces the file at its path"
    );
    let beside = text.replacen(
        "handoff_socket = \"/var/lib/tierstore/instance.lock\"",
        "handoff_socket = \"/var/lib/tierstore/handoff.sock\"",
        1,
    );
    assert!(handoff_of(&beside).is_some());
}

/// The least a build node's file can say, with `server` as more lines of `[server]`.
fn build_node(server: &str) -> String {
    format!(
        "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\n\
         [server]\nproxy_id = \"c1\"\nbucket = \"tier-bucket\"\naccess_key_file = \"key\"\n{server}\n\n\
         [local_cache]\npath = \"/c\"\nbudget = 1000\n\n[cold_store]\nkind = \"memory\"\n\n\
         [[policy]]\nprefix = \"cas/\"\ncache = true\n"
    )
}

/// A build node's file with the cache at `path` and `server` as more lines of `[server]`.
fn build_node_at(path: &str, server: &str) -> String {
    build_node(server).replacen("path = \"/c\"", &format!("path = \"{path}\""), 1)
}

/// A storage node's file with `[local_cache]` at `path` and `server` as more lines of `[server]`.
fn combined_at(path: &str, server: &str) -> String {
    format!(
        "{}\n[local_cache]\npath = \"{path}\"\nbudget = 1000\n\n[[policy]]\nprefix = \"cas/\"\ncache = true\n",
        with_server(server)
    )
}

/// The hand-off a build node's file `text` makes.
fn cache_handoff(text: &str) -> Option<Handoff> {
    settings_from_text(text, DEFAULTS)
        .unwrap()
        .into_cache()
        .expect("a build node")
        .handoff
}

/// The hand-off the file `text` of a storage node with `[local_cache]` makes.
fn combined_handoff(text: &str) -> Option<Handoff> {
    settings_from_text(text, DEFAULTS)
        .unwrap()
        .into_combined()
        .expect("a combined node")
        .node
        .handoff
}

const SOCKET_LINE: &str = "handoff_socket = \"/var/lib/tierstore/handoff.sock\"";

/// Asserts that a build node reads both keys: no hand-off without them, the socket with the
/// default wait of 10 seconds, and the socket with the wait given.
#[test]
fn a_build_node_reads_the_hand_off_keys() {
    assert_eq!(cache_handoff(&build_node("")), None);
    let socket = PathBuf::from("/var/lib/tierstore/handoff.sock");
    assert_eq!(
        cache_handoff(&build_node(SOCKET_LINE)),
        Some(Handoff {
            socket: socket.clone(),
            wait: Duration::from_secs(10),
        })
    );
    let waiting = build_node(&format!("{SOCKET_LINE}\nhandoff_wait = \"5s\""));
    assert_eq!(
        cache_handoff(&waiting),
        Some(Handoff {
            socket,
            wait: Duration::from_secs(5),
        })
    );
}

/// Asserts that a storage node with `[local_cache]` reads both keys: no hand-off without them,
/// the socket with the default wait, and the socket with the wait given.
#[test]
fn a_storage_node_with_a_local_cache_reads_the_hand_off_keys() {
    assert_eq!(combined_handoff(&combined_at("/c", "")), None);
    let socket = PathBuf::from("/var/lib/tierstore/handoff.sock");
    assert_eq!(
        combined_handoff(&combined_at("/c", SOCKET_LINE)),
        Some(Handoff {
            socket: socket.clone(),
            wait: Duration::from_secs(10),
        })
    );
    let waiting = combined_at("/c", &format!("{SOCKET_LINE}\nhandoff_wait = \"7s\""));
    assert_eq!(
        combined_handoff(&waiting),
        Some(Handoff {
            socket,
            wait: Duration::from_secs(7),
        })
    );
}

/// Asserts that on both cache shapes a wait without a socket is refused by name, as on a storage
/// node without a cache.
#[test]
fn a_wait_without_a_socket_is_refused_on_both_cache_shapes() {
    let words = "server.handoff_wait: it is the wait for the running instance of \
                 server.handoff_socket, which is not given";
    assert_eq!(refusal(&build_node("handoff_wait = \"5s\"")), words);
    assert_eq!(refusal(&combined_at("/c", "handoff_wait = \"5s\"")), words);
}

/// Asserts that a control socket inside the cache directory is refused on both cache shapes with
/// the exact words, whether it is directly in the directory or deeper, and that a socket beside
/// the directory (another directory or a file next to it that only shares the name's start) loads:
/// the paths are compared by component, not by string prefix.
#[test]
fn a_socket_inside_the_cache_directory_is_refused_on_both_cache_shapes() {
    let words = "local_cache.path: must not hold server.handoff_socket: it is not a place for the \
                 cache's files";
    for socket in ["/c/x.sock", "/c/deeper/x.sock", "/c/../c/x.sock"] {
        let line = format!("handoff_socket = \"{socket}\"");
        assert_eq!(refusal(&build_node(&line)), words, "{socket}");
        assert_eq!(refusal(&combined_at("/c", &line)), words, "{socket}");
    }
    for socket in ["/c-other/x.sock", "/c.sock", "/other/c/x.sock"] {
        let line = format!("handoff_socket = \"{socket}\"");
        assert!(cache_handoff(&build_node(&line)).is_some(), "{socket}");
        assert!(
            combined_handoff(&combined_at("/c", &line)).is_some(),
            "{socket}"
        );
    }
}

/// Asserts that a build node whose cache path is relative cannot be compared with a socket and is
/// refused with the exact words, and that without a socket the same relative path loads.
#[test]
fn a_relative_cache_path_with_a_socket_is_refused_on_a_build_node() {
    assert_eq!(
        refusal(&build_node_at("cache", SOCKET_LINE)),
        "local_cache.path: must be an absolute path to be compared with server.handoff_socket"
    );
    assert_eq!(cache_handoff(&build_node_at("cache", "")), None);
}

/// Asserts that on a storage node with `[local_cache]` the control socket may not be the lock file
/// of the hot tier, and a socket beside the lock file loads.
#[test]
fn a_socket_that_is_the_lock_file_is_refused_on_a_combined_node() {
    let lock = "/var/lib/tierstore/instance.lock";
    let text = combined_at("/c", &format!("handoff_socket = \"{lock}\"")).replacen(
        "budget = 1000",
        &format!("budget = 1000\nlock_file = \"{lock}\""),
        1,
    );
    assert_eq!(
        refusal(&text),
        format!(
            "server.handoff_socket: \"{lock}\" is hot_store.lock_file too; \
             the control socket replaces the file at its path"
        )
    );
    let beside = text.replacen(&format!("handoff_socket = \"{lock}\""), SOCKET_LINE, 1);
    assert!(combined_handoff(&beside).is_some());
}

/// Asserts that both keys are keys of `[server]` this release knows, so a file that sets them has
/// no unknown-key warning.
#[test]
fn the_keys_are_known() {
    let fields = fields_of::<ServerTable>();
    for key in ["handoff_socket", "handoff_wait"] {
        assert!(fields.contains(&key), "{key}");
    }
}

/// Asserts (T01) that on both cache shapes a `handoff_wait` outside 1ms to 1h is refused by name
/// with the exact words, and that the ends of the range load.
#[test]
fn a_wait_out_of_range_is_refused_by_name_on_both_cache_shapes() {
    for (given, ms) in [("0s", "0"), ("61m", "3660000")] {
        let line = format!("{SOCKET_LINE}\nhandoff_wait = \"{given}\"");
        let words = format!("server.handoff_wait: {ms}ms is not between 1ms and 3600000ms");
        assert_eq!(refusal(&build_node(&line)), words, "{given}");
        assert_eq!(refusal(&combined_at("/c", &line)), words, "{given}");
    }
    for (given, wait) in [("1ms", 1), ("1h", 3_600_000)] {
        let line = format!("{SOCKET_LINE}\nhandoff_wait = \"{given}\"");
        let expected = Some(Duration::from_millis(wait));
        assert_eq!(
            cache_handoff(&build_node(&line)).map(|h| h.wait),
            expected,
            "{given}"
        );
        assert_eq!(
            combined_handoff(&combined_at("/c", &line)).map(|h| h.wait),
            expected,
            "{given}"
        );
    }
}

/// Asserts (T01) that on both cache shapes a socket path that is relative, empty or a directory is
/// refused with the path and the reason in the exact words.
#[test]
fn a_socket_that_is_not_an_absolute_file_path_is_refused_on_both_cache_shapes() {
    for (path, why) in [
        ("handoff.sock", "must be an absolute path"),
        ("", "must be an absolute path"),
        ("/", "must name a file, not a directory"),
    ] {
        let line = format!("handoff_socket = \"{path}\"");
        let words = format!("server.handoff_socket: \"{path}\" {why}");
        assert_eq!(refusal(&build_node(&line)), words, "{path:?}");
        assert_eq!(refusal(&combined_at("/c", &line)), words, "{path:?}");
    }
}

/// Asserts (T01) that on both cache shapes a socket path of 103 bytes loads and one of 104 bytes is
/// refused naming its length.
#[test]
fn a_socket_path_over_103_bytes_is_refused_on_both_cache_shapes() {
    let longest = format!("/{}", "a".repeat(102));
    let line = format!("handoff_socket = \"{longest}\"");
    for taken in [
        cache_handoff(&build_node(&line)),
        combined_handoff(&combined_at("/c", &line)),
    ] {
        assert_eq!(taken.expect("a hand-off").socket.as_os_str().len(), 103);
    }
    let over = format!("/{}", "a".repeat(103));
    let line = format!("handoff_socket = \"{over}\"");
    let words = format!(
        "server.handoff_socket: \"{over}\" is 104 bytes; the path of a Unix socket is at most 103"
    );
    assert_eq!(refusal(&build_node(&line)), words);
    assert_eq!(refusal(&combined_at("/c", &line)), words);
}

/// Asserts (T01) that a plain storage node, the control, still loads with both keys and reads the
/// socket and the wait.
#[test]
fn a_plain_storage_node_reads_both_keys_as_the_control() {
    let text = with_server(&format!("{SOCKET_LINE}\nhandoff_wait = \"3s\""));
    assert_eq!(
        handoff_of(&text),
        Some(Handoff {
            socket: PathBuf::from("/var/lib/tierstore/handoff.sock"),
            wait: Duration::from_secs(3),
        })
    );
}

/// Asserts that a service with a state directory has the hand-off without the key: the socket is
/// `handoff.sock` in the directory, with the default wait; that the key, when given, wins; that
/// `handoff_wait` is then allowed without the key; and that a build node gets the same.
#[test]
fn a_state_directory_gives_the_socket_its_default() {
    let per_run = PerRun {
        state_dir: Some(PathBuf::from("/var/lib/tierstore")),
        ..PerRun::default()
    };
    let default = Handoff {
        socket: PathBuf::from("/var/lib/tierstore/handoff.sock"),
        wait: Duration::from_secs(10),
    };
    assert_eq!(
        from_text(MINIMAL, per_run.clone()).unwrap().handoff,
        Some(default)
    );

    let named = with_server("handoff_socket = \"/run/other.sock\"");
    let taken = from_text(&named, per_run.clone()).unwrap().handoff.unwrap();
    assert_eq!(taken.socket, PathBuf::from("/run/other.sock"));

    let waiting = with_server("handoff_wait = \"5s\"");
    let taken = from_text(&waiting, per_run.clone())
        .unwrap()
        .handoff
        .unwrap();
    assert_eq!(taken.wait, Duration::from_secs(5));

    let build = build_node("");
    let taken = settings_from_text(&build, per_run.clone()).unwrap();
    assert_eq!(
        taken
            .into_cache()
            .expect("a build node")
            .handoff
            .map(|handoff| handoff.socket),
        Some(PathBuf::from("/var/lib/tierstore/handoff.sock"))
    );
    let combined = combined_at("/var/lib/tierstore/cache", "");
    let taken = settings_from_text(&combined, per_run).unwrap();
    assert_eq!(
        taken
            .into_combined()
            .expect("a combined node")
            .node
            .handoff
            .map(|handoff| handoff.socket),
        Some(PathBuf::from("/var/lib/tierstore/handoff.sock"))
    );
}

/// Asserts that a default socket is held to the checks of a named one: a state directory that
/// makes the path too long is refused, naming the key.
#[test]
fn a_state_directory_that_makes_the_socket_path_too_long_is_refused() {
    let per_run = PerRun {
        state_dir: Some(PathBuf::from(format!("/{}", "a".repeat(100)))),
        ..PerRun::default()
    };
    let why = from_text(MINIMAL, per_run).unwrap_err().to_string();
    assert!(why.starts_with("server.handoff_socket: \""), "{why}");
    assert!(
        why.ends_with("the path of a Unix socket is at most 103"),
        "{why}"
    );
}
