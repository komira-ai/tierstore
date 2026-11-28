//! The parts of the live rig that read no process: the control commands it sends the upstream,
//! and the placements it counts in a cache directory (the rig itself, over running programs, is
//! in `tests/live_integration.rs`).

use std::process::ExitStatus;

use tierstore_testkit::dir::TestDir;

use super::*;

/// Asserts the control command of each way the upstream can serve.
#[test]
fn each_serving_is_its_control_command() {
    assert_eq!(command_of(&Serving::All), "serve");
    assert_eq!(
        command_of(&Serving::Stalled {
            prefix: "cas/p/".to_owned()
        }),
        "stall cas/p/"
    );
    assert_eq!(
        command_of(&Serving::Failing {
            prefix: String::new(),
            status: 503
        }),
        "fail 503 "
    );
}

/// Asserts that the placements are the files of `placements` named by a placement id, other
/// names left out; none when the cache has no such directory; and that one that cannot be read
/// says so.
#[test]
fn the_placements_are_the_ids_in_the_cache() {
    let dir = TestDir::under(std::env::temp_dir(), "blackbox-placements");
    let cache = dir.path().join("cache");
    assert_eq!(placements_in(&cache), Ok(0));
    let placements = cache.join("placements");
    std::fs::create_dir_all(&placements).unwrap();
    for name in [
        "0123456789abcdef0123456789abcdef",
        "ffffffffffffffffffffffffffffffff",
        "readme",
        "0123456789abcdef0123456789abcdeg",
        ".new-0123456789abcdef0123456789ab",
    ] {
        std::fs::write(placements.join(name), b"{}").unwrap();
    }
    assert_eq!(placements_in(&cache), Ok(2));

    // A `placements` that is a file, not a directory.
    let not_a_dir = dir.path().join("other");
    std::fs::create_dir_all(&not_a_dir).unwrap();
    std::fs::write(not_a_dir.join("placements"), b"x").unwrap();
    let refused = placements_in(&not_a_dir).unwrap_err();
    assert!(
        refused.starts_with(&format!("{}: ", not_a_dir.join("placements").display())),
        "{refused}"
    );
}

/// Asserts that a node ended as asked when it exited 0 or died of the SIGTERM, or was not
/// running; and not when it exited otherwise or died of another signal, which is named.
#[test]
fn a_node_ends_as_asked_with_status_0_or_by_the_sigterm() {
    assert_eq!(ended_as_asked(None), Ok(()));
    assert_eq!(ended_as_asked(Some(ExitStatus::from_raw(0))), Ok(())); // exit 0
    assert_eq!(ended_as_asked(Some(ExitStatus::from_raw(15))), Ok(())); // SIGTERM
    assert_eq!(
        ended_as_asked(Some(ExitStatus::from_raw(9))),
        Err("the node ended signal: 9 (SIGKILL) when asked to stop".to_owned())
    );
    assert_eq!(
        ended_as_asked(Some(ExitStatus::from_raw(1 << 8))),
        Err("the node ended exit status: 1 when asked to stop".to_owned())
    );
}

/// Asserts that `/metricz` is read only from an answer 200, and that another status is named
/// with the node's address.
#[test]
fn the_gauges_are_read_from_an_answer_200_only() {
    let body = b"tierstore_provider_preparing 3\n";
    assert_eq!(
        gauges_of(200, body, "127.0.0.1:9").unwrap().preparing,
        Some(3)
    );
    assert_eq!(
        gauges_of(404, body, "127.0.0.1:9"),
        Err("GET http://127.0.0.1:9/metricz answered 404".to_owned())
    );
}

/// Asserts that the held requests are the `stalled=` word of fake-s3's `stats`, and that an
/// answer without one, or with a count that is no number, is named.
#[test]
fn the_held_requests_are_the_stalled_word_of_stats() {
    assert_eq!(stalled_of("stalled=7 begun=9"), Ok(7));
    for answer in ["begun=9", "stalled=x begun=1", "refused: unknown command"] {
        assert_eq!(
            stalled_of(answer),
            Err(format!("fake-s3 answered stats with {answer:?}"))
        );
    }
}
