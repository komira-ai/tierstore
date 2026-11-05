//! The io keys of `[server]`: their defaults, that each is read into the settings it names, and
//! the refusal of a value outside its range or of two keys that contradict each other.

use std::time::Duration;

use super::IoTuning;
use crate::server::cli::Cli;
use crate::server::config::{from_text, settings_from_text};
use crate::server::config_testing::{DEFAULTS, MINIMAL};
use crate::server::config_unknown::unknown_key_warnings;
use crate::server::listen::ListenTuning;

/// `MINIMAL` with the lines `extra` added to its `[server]` table.
fn file(extra: &str) -> String {
    let line = format!("access_key_file = \"key\"\n{extra}\n");
    MINIMAL.replacen("access_key_file = \"key\"", &line, 1)
}

/// The settings of the file with `extra` in its `[server]` table.
fn cli(extra: &str) -> Cli {
    from_text(&file(extra), DEFAULTS).expect("a file")
}

/// The message with which the file with `extra` in its `[server]` table is refused.
fn refusal(extra: &str) -> String {
    let why = from_text(&file(extra), DEFAULTS).expect_err("a refusal");
    why.to_string()
}

/// Asserts that a file without the keys makes the values the code had before they were keys, in
/// the server's settings and in those of the tiers.
#[test]
fn without_the_keys_the_settings_are_the_values_the_code_had() {
    let cli = cli("");
    let listen = ListenTuning {
        backlog: 1024,
        accept_pause: Duration::from_millis(100),
        door_grace: Duration::from_secs(3),
        door_look: Duration::from_millis(10),
    };
    let want = IoTuning {
        header_timeout: Duration::from_secs(60),
        listen,
        put_pieces_queued: 4,
        oci_handshake_timeout: Duration::from_secs(10),
    };
    assert_eq!(cli.io_tuning, want);
    assert_eq!(cli.io_tuning, IoTuning::default());
    assert_eq!(cli.s3_client.chunks_waiting, 1);
    assert_eq!(cli.s3_client.dir_copy_chunk, 1 << 20);
    assert_eq!(cli.s3_client.dir_read_chunk, 64 << 10);
}

/// Asserts that every key is read, into the setting it names.
#[test]
fn every_io_key_is_read_into_the_setting_it_names() {
    let cli = cli(r#"
header_timeout         = "90s"
listen_backlog         = 256
accept_pause           = "250ms"
door_grace             = "5s"
door_look              = "20ms"
put_pieces_queued      = 9
oci_handshake_timeout  = "15s"
s3_chunks_waiting      = 3
dir_copy_chunk         = "2MiB"
dir_read_chunk         = "128KiB"
"#);
    let io = cli.io_tuning;
    assert_eq!(io.header_timeout, Duration::from_secs(90));
    assert_eq!(io.listen.backlog, 256);
    assert_eq!(io.listen.accept_pause, Duration::from_millis(250));
    assert_eq!(io.listen.door_grace, Duration::from_secs(5));
    assert_eq!(io.listen.door_look, Duration::from_millis(20));
    assert_eq!(io.put_pieces_queued, 9);
    assert_eq!(io.oci_handshake_timeout, Duration::from_secs(15));
    let client = cli.s3_client;
    assert_eq!(client.chunks_waiting, 3);
    assert_eq!(client.dir_copy_chunk, 2 << 20);
    assert_eq!(client.dir_read_chunk, 128 << 10);
}

/// Asserts that a value just outside its range, at either end, is refused with a message that
/// names the key, and that the value on the edge inside it loads.
#[test]
fn a_value_outside_its_range_is_refused_naming_the_key() {
    let inside = [
        ("header_timeout", "\"1ms\""),
        ("header_timeout", "\"1h\""),
        ("listen_backlog", "1"),
        ("listen_backlog", "65535"),
        ("accept_pause", "\"1m\""),
        ("door_grace", "\"1m\""),
        ("door_look", "\"10ms\""),
        ("put_pieces_queued", "4096"),
        ("oci_handshake_timeout", "\"1h\""),
        ("s3_chunks_waiting", "1024"),
        ("dir_copy_chunk", "1"),
        ("dir_copy_chunk", "\"1GiB\""),
        ("dir_read_chunk", "1"),
        ("dir_read_chunk", "\"1GiB\""),
    ];
    for (key, value) in inside {
        assert!(
            from_text(&file(&format!("{key} = {value}")), DEFAULTS).is_ok(),
            "{key} = {value}"
        );
    }
    let outside = [
        ("header_timeout", "\"0s\""),
        ("header_timeout", "\"2h\""),
        ("listen_backlog", "0"),
        ("listen_backlog", "65536"),
        ("accept_pause", "\"0s\""),
        ("accept_pause", "\"2m\""),
        ("door_grace", "\"0s\""),
        ("door_grace", "\"2m\""),
        ("door_look", "\"0s\""),
        ("door_look", "\"11s\""),
        ("put_pieces_queued", "0"),
        ("put_pieces_queued", "4097"),
        ("oci_handshake_timeout", "\"0s\""),
        ("oci_handshake_timeout", "\"2h\""),
        ("s3_chunks_waiting", "0"),
        ("s3_chunks_waiting", "1025"),
        ("dir_copy_chunk", "0"),
        ("dir_copy_chunk", "\"1025MiB\""),
        ("dir_read_chunk", "0"),
        ("dir_read_chunk", "\"1025MiB\""),
    ];
    for (key, value) in outside {
        let why = refusal(&format!("{key} = {value}"));
        assert!(
            why.contains(&format!("server.{key}")),
            "{key} = {value}: {why}"
        );
    }
}

/// Asserts the pair `door_look` and `door_grace`: a look longer than the grace is refused, naming
/// `server.door_look`, equal ones load, and a look that is left out is the default, or the grace
/// if that is shorter, so that a file that shortens the grace alone loads.
#[test]
fn a_door_look_longer_than_the_grace_is_refused() {
    let why = refusal("door_grace = \"50ms\"\ndoor_look = \"51ms\"");
    assert!(why.contains("server.door_look"), "{why}");
    assert!(why.contains("server.door_grace"), "{why}");
    let equal = cli("door_grace = \"50ms\"\ndoor_look = \"50ms\"");
    assert_eq!(equal.io_tuning.listen.door_look, Duration::from_millis(50));
    let short = cli("door_grace = \"5ms\"");
    assert_eq!(short.io_tuning.listen.door_look, Duration::from_millis(5));
    let least = cli("door_grace = \"1ms\"");
    assert_eq!(least.io_tuning.listen.door_look, Duration::from_millis(1));
    let long = cli("door_grace = \"3s\"");
    assert_eq!(long.io_tuning.listen.door_look, Duration::from_millis(10));
}

/// Asserts that a value of the wrong type is refused and never shows the text written.
#[test]
fn a_value_of_the_wrong_type_is_refused_without_showing_it() {
    for (line, shown) in [
        ("header_timeout = 4242", "4242"),
        ("listen_backlog = \"bigbacklog\"", "bigbacklog"),
        ("put_pieces_queued = -7731", "7731"),
    ] {
        let why = refusal(line);
        assert!(why.starts_with("line "), "{line}: {why}");
        assert!(!why.contains(shown), "{line}: {why}");
    }
}

/// Asserts that the keys are in the table of `[server]` that this release reads: none is warned of
/// as unknown, and a key this release does not have still is, so a file that sets them loads in a
/// release that lacks them with one warning each (the release before ignores unknown keys, see
/// `config_unknown`).
#[test]
fn the_io_keys_are_known_to_this_release() {
    let text = file("header_timeout = \"61s\"\ndir_read_chunk = 4096\nnot_an_io_knob = 1");
    let table: toml::Table = toml::from_str(&text).expect("toml");
    assert_eq!(
        unknown_key_warnings(&table),
        ["tierstore: config key server.not_an_io_knob is not known to this release and is ignored"]
    );
    let node = settings_from_text(&text, DEFAULTS).expect("a file");
    assert!(matches!(node, crate::server::config::Settings::Storage(_)));
}
