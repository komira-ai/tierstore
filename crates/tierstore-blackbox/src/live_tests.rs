//! The parts of the live rig that read no process: the control commands it sends the upstream,
//! and the placements it counts in a cache directory (the rig itself, over running programs, is
//! in `tests/live_integration.rs`).

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
