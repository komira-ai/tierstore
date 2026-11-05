//! The two hand-off keys of `[server]`: what a file without them makes, their defaults, the values
//! they refuse at start, and the shapes of node that refuse them.

use std::path::PathBuf;
use std::time::Duration;

use super::super::config::{ServerTable, from_text, settings_from_text};
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

/// Asserts that a build node and a storage node with `[local_cache]`, which do not serve through
/// the hand-off, refuse each key by name instead of reading past it, and load without them.
#[test]
fn the_shapes_that_do_not_hand_off_refuse_the_keys() {
    let socket = "handoff_socket = \"/var/lib/tierstore/handoff.sock\"";
    let wait = "handoff_wait = \"5s\"";
    for (key, line) in [("handoff_socket", socket), ("handoff_wait", wait)] {
        assert_eq!(
            refusal(&build_node(line)),
            format!("server.{key}: a build node does not hand off its sockets; leave it out")
        );
        let combined = format!(
            "{}\n[local_cache]\npath = \"/c\"\nbudget = 1000\n\n[[policy]]\nprefix = \"cas/\"\ncache = true\n",
            with_server(line)
        );
        assert_eq!(
            refusal(&combined),
            format!(
                "server.{key}: a storage node with [local_cache] does not hand off its sockets; \
                 leave it out"
            )
        );
    }
    assert!(settings_from_text(&build_node(""), DEFAULTS).is_ok());
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
